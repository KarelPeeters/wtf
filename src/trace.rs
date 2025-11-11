#![cfg(unix)]

use crate::record::ProcessKind;
use crate::util::MapExt;
use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::sys::{ptrace, wait};
use nix::unistd::{ForkResult, Pid};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::ops::ControlFlow;
use std::time::Instant;
use syscalls::Sysno;

#[derive(Debug)]
pub struct SpawnFailed(pub Errno);

#[derive(Debug)]
pub enum TraceEvent {
    ProcessStart {
        pid: Pid,
        time: f32,
    },
    ProcessExit {
        pid: Pid,
        time: f32,
    },
    ProcessChild {
        parent: Pid,
        child: Pid,
        kind: ProcessKind,
    },
    ProcessExec {
        pid: Pid,
        time: f32,
        path: String,
        argv: Vec<String>,
    },
    Time {
        time: f32,
    },
}

// TODO better error handling
pub unsafe fn record_trace(
    child_path: &CStr,
    child_argv: &[CString],
    mut callback: impl FnMut(TraceEvent) -> ControlFlow<()>,
) -> Result<(), SpawnFailed> {
    let r = unsafe { record_trace_impl(child_path, child_argv, |event| callback(event)) };
    match r {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break(()) => Ok(()),
    }
}

const CHECK_PTRACE_SYSCALL_INFO_NEW: bool = false;

pub unsafe fn record_trace_impl(
    child_path: &CStr,
    child_argv: &[CString],
    mut callback: impl FnMut(TraceEvent) -> ControlFlow<()>,
) -> ControlFlow<(), Result<(), SpawnFailed>> {
    // start the child process
    let root_pid = unsafe {
        let fork_result = nix::unistd::fork().expect("failed fork");
        match fork_result {
            ForkResult::Parent { child } => child,
            ForkResult::Child => match run_child(child_path, child_argv) {
                Ok(()) => unreachable!("after exec"),
                Err(_) => {
                    // we don't need to send the error to the parent,
                    //   it will see it anyway because it's recording syscalls!
                    libc::exit(1)
                }
            },
        }
    };

    // wait for child to stop, so we know for sure that it exists and has called traceme
    let s = wait::waitpid(root_pid, None).expect("failed initial wait::waitpid");
    assert!(matches!(s, WaitStatus::Stopped(pid, Signal::SIGSTOP) if pid == root_pid));

    // start ptrace
    // options:
    // * PTRACE_O_TRACESYSGOOD: add mask to syscall stops, allows parsing WaitStatus::PtraceSyscall
    // * PTRACE_O_EXITKILL: kill traced process if tracer exits to avoid orphaned processes
    // * PTRACE_O_TRACE*: trace children through fork syscalls?
    let ptrace_options = ptrace::Options::PTRACE_O_TRACESYSGOOD
        | ptrace::Options::PTRACE_O_EXITKILL
        | ptrace::Options::PTRACE_O_TRACECLONE
        | ptrace::Options::PTRACE_O_TRACEFORK
        | ptrace::Options::PTRACE_O_TRACEVFORK;
    ptrace::setoptions(root_pid, ptrace_options).expect("failed to set ptrace options");

    // report initial process start
    // TODO is this time info accurate enough?
    let time_start = Instant::now();
    callback(TraceEvent::ProcessStart {
        pid: root_pid,
        time: 0.0,
    })?;

    // resume after earlier stop
    ptrace::syscall(root_pid, None).expect("failed initial ptrace resume");

    // track in-progress syscall per child
    let mut partial_syscalls: HashMap<Pid, SyscallEntry> = HashMap::new();
    let mut active_processes: HashSet<Pid> = HashSet::new();
    active_processes.insert(root_pid);

    // main tracing event loop
    let mut root_exec_any_success = false;
    let mut root_exec_last_error = None;

    loop {
        let status = wait::waitpid(None, None).expect("failed wait::waitpid");

        let time_status = time_start.elapsed().as_secs_f32();
        callback(TraceEvent::Time { time: time_status })?;

        let resume_pid = match status {
            // handle syscall
            WaitStatus::PtraceSyscall(pid) => {
                match partial_syscalls.remove(&pid) {
                    None => {
                        // syscall entry
                        let info = ptrace_syscall_info_entry(CHECK_PTRACE_SYSCALL_INFO_NEW, pid)
                            .expect("failed to get syscall entry info");
                        let nr = Sysno::new(info.nr as usize);

                        let next_partial_syscall = if let Some(nr) = nr {
                            let res = match nr {
                                // handle fork-like
                                Sysno::clone => {
                                    let flags = info.args[0];
                                    SyscallEntry::Fork(process_kind_from_clone_flags(flags as _))
                                }
                                Sysno::clone3 => {
                                    let clone_args_ptr = info.args[0];
                                    let clone_args_size = info.args[1] as usize;
                                    let flags = if clone_args_size >= 8 {
                                        ptrace::read(pid, clone_args_ptr as *mut libc::c_void)
                                            .expect("failed to read clone_args")
                                    } else {
                                        0
                                    };

                                    SyscallEntry::Fork(process_kind_from_clone_flags(flags as _))
                                }
                                Sysno::fork | Sysno::vfork => SyscallEntry::Fork(ProcessKind::Process),
                                // handle exec-like
                                Sysno::execve => {
                                    let args_ptr = ExecArgPointers {
                                        path: info.args[0],
                                        argv: info.args[1],
                                        envp: info.args[2],
                                    };
                                    let args =
                                        ptrace_extract_exec_args(pid, args_ptr).expect("failed to extract exec args");
                                    SyscallEntry::Exec(args)
                                }
                                Sysno::execveat => {
                                    let args_ptr = ExecArgPointers {
                                        path: info.args[1],
                                        argv: info.args[2],
                                        envp: info.args[3],
                                    };
                                    let args =
                                        ptrace_extract_exec_args(pid, args_ptr).expect("failed to extract exec args");
                                    SyscallEntry::Exec(args)
                                }
                                // ignore exit syscalls, we'll record the actual exit on process termination
                                Sysno::exit | Sysno::exit_group => SyscallEntry::Ignore,
                                // ignore other syscalls, we're only interested in fork/exec
                                _ => SyscallEntry::Ignore,
                            };

                            res
                        } else {
                            // ignore unknown syscalls
                            SyscallEntry::Ignore
                        };

                        partial_syscalls.insert_first(pid, next_partial_syscall);
                    }
                    Some(partial) => {
                        let info = ptrace_syscall_info_exit(CHECK_PTRACE_SYSCALL_INFO_NEW, pid)
                            .expect("failed to get syscall exit info");

                        match partial {
                            SyscallEntry::Ignore => {}
                            SyscallEntry::Fork(fork_kind) => {
                                if info.sval > 0 {
                                    callback(TraceEvent::ProcessChild {
                                        parent: pid,
                                        child: Pid::from_raw(info.sval as i32),
                                        kind: fork_kind,
                                    })?;
                                }
                            }
                            SyscallEntry::Exec(args) => {
                                // check for errors when spawning the child process
                                // there can be multiple exec attempts due to $PATH, it's fine if any of them succeeds
                                if !root_exec_any_success && pid == root_pid {
                                    if info.sval < 0 {
                                        root_exec_last_error = Some(Errno::from_raw(-info.sval as i32));
                                    } else {
                                        root_exec_any_success = true;
                                    }
                                }

                                if info.sval == 0 {
                                    // TODO populate arv
                                    callback(TraceEvent::ProcessExec {
                                        pid,
                                        time: time_status,
                                        path: String::from_utf8_lossy(&args.path).into_owned(),
                                        argv: args
                                            .argv
                                            .iter()
                                            .map(|arg| String::from_utf8_lossy(&arg).into_owned())
                                            .collect(),
                                    })?;
                                }
                            }
                        }
                    }
                }

                Some(pid)
            }
            // ignore events
            //    these get reported for the parent process when children are created due to the ptrace options,
            //    but we don't care about them
            WaitStatus::PtraceEvent(pid, _signal, _event) => Some(pid),
            // process exited, cleanup and maybe stop tracing
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => {
                callback(TraceEvent::ProcessExit { pid, time: time_status })?;

                partial_syscalls.remove(&pid);
                if pid == root_pid {
                    break;
                }
                None
            }
            // stopped by some signal, just continue
            WaitStatus::Stopped(pid, signal) => {
                if matches!(signal, Signal::SIGSTOP | Signal::SIGTRAP) && !active_processes.contains(&pid) {
                    // initial stop for new child process, create it
                    callback(TraceEvent::ProcessStart { pid, time: time_status })?;
                    active_processes.insert(pid);
                }

                Some(pid)
            }
            // cases that shouldn't happen
            WaitStatus::Continued(_) => unreachable!("we didn't set WaitPidFlag::WCONTINUE"),
            WaitStatus::StillAlive => unreachable!("we didn't set WaitPidFlag::WNOHANG"),
        };

        if let Some(resume_pid) = resume_pid {
            ptrace::syscall(resume_pid, None).expect("failed ptrace::syscall");
        }
    }

    // check if at least the root process managed to start
    if !root_exec_any_success {
        let err = root_exec_last_error.expect("there wasn't any exec attempt");
        return ControlFlow::Continue(Err(SpawnFailed(err)));
    }

    ControlFlow::Continue(Ok(()))
}

pub unsafe fn run_child(child_path: &CStr, child_argv: &[CString]) -> Result<(), nix::Error> {
    // mark this process as traceable
    ptrace::traceme()?;
    // pause this process, to give the parent a change to start tracing without any race conditions
    nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGSTOP)?;
    // actually execute the target program
    nix::unistd::execvp(child_path, child_argv)?;
    Ok(())
}

#[derive(Debug)]
enum SyscallEntry {
    Ignore,
    Fork(ProcessKind),
    Exec(ExecArgs),
}

#[derive(Debug, Copy, Clone)]
struct ExecArgPointers {
    path: u64,
    #[allow(dead_code)]
    argv: u64,
    #[allow(dead_code)]
    envp: u64,
}

#[derive(Debug)]
struct ExecArgs {
    path: Vec<u8>,
    #[allow(dead_code)]
    argv: Vec<Vec<u8>>,
}

fn process_kind_from_clone_flags(flags: libc::c_long) -> ProcessKind {
    if (flags & libc::CLONE_THREAD as libc::c_long) != 0 {
        ProcessKind::Thread
    } else {
        ProcessKind::Process
    }
}

struct PtraceSyscallInfoEntry {
    nr: u64,
    args: [u64; 6],
}

struct PtraceSyscallInfoExit {
    sval: i64,
}

fn ptrace_syscall_info_entry(check_using_syscall: bool, pid: Pid) -> nix::Result<PtraceSyscallInfoEntry> {
    // get info manually
    let regs = ptrace::getregs(pid)?;
    let info = PtraceSyscallInfoEntry {
        nr: regs.orig_rax,
        args: [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9],
    };

    // check that info matches the kernel-provided function
    if check_using_syscall {
        let info_new = ptrace_syscall_info(pid)?;

        assert_eq!(info_new.op, libc::PTRACE_SYSCALL_INFO_ENTRY);
        let info_new = unsafe { &info_new.u.entry };
        assert_eq!(info_new.nr, info.nr);
        assert_eq!(info_new.args, info.args);
    }

    Ok(info)
}

fn ptrace_syscall_info_exit(check_using_syscall: bool, pid: Pid) -> nix::Result<PtraceSyscallInfoExit> {
    // get info manually
    let regs = ptrace::getregs(pid)?;
    let info = PtraceSyscallInfoExit { sval: regs.rax as i64 };

    // check that info matches the kernel-provided function
    if check_using_syscall {
        let info_new = ptrace_syscall_info(pid)?;

        assert_eq!(info_new.op, libc::PTRACE_SYSCALL_INFO_EXIT);
        let info_new = unsafe { &info_new.u.exit };
        assert_eq!(info_new.sval, info.sval);
    }

    Ok(info)
}

/// Fixed version of ptrace::syscall_info.
/// Based on https://github.com/nix-rust/nix/issues/2660.
fn ptrace_syscall_info(pid: Pid) -> Result<libc::ptrace_syscall_info, Errno> {
    let mut data = std::mem::MaybeUninit::<libc::ptrace_syscall_info>::uninit();

    let res = unsafe {
        libc::ptrace(
            ptrace::Request::PTRACE_GET_SYSCALL_INFO as libc::c_uint,
            libc::pid_t::from(pid),
            size_of::<libc::ptrace_syscall_info>(),
            data.as_mut_ptr(),
        )
    };

    Errno::result(res)?;
    let info = unsafe { data.assume_init() };
    Ok(info)
}

fn ptrace_extract_exec_args(pid: Pid, args: ExecArgPointers) -> nix::Result<ExecArgs> {
    let ExecArgPointers { path, argv: _, envp: _ } = args;

    let path = ptrace_read_str(pid, path as *mut _)?;
    let argv = ptrace_read_str_list(pid, args.argv as *mut _)?;

    Ok(ExecArgs { path, argv })
}

fn ptrace_read_str(pid: Pid, start: *mut libc::c_void) -> nix::Result<Vec<u8>> {
    // TODO is there really no batch memory read?
    // TODO limit max length?
    let mut result = Vec::new();

    for offset_word in 0isize.. {
        let offset_byte = offset_word * size_of::<libc::c_long>() as isize;
        let word = ptrace::read(pid, unsafe { start.offset(offset_byte) })?;
        for b in word.to_ne_bytes() {
            if b == 0 {
                return Ok(result);
            }
            result.push(b);
        }
    }

    Ok(result)
}

fn ptrace_read_str_list(pid: Pid, start: *mut libc::c_void) -> nix::Result<Vec<Vec<u8>>> {
    let mut result = Vec::new();

    for index in 0isize.. {
        let ptr_addr = unsafe { start.offset(index * size_of::<*mut libc::c_void>() as isize) };
        let ptr_value = ptrace::read(pid, ptr_addr)? as *mut libc::c_void;
        if ptr_value.is_null() {
            break;
        }
        result.push(ptrace_read_str(pid, ptr_value)?);
    }

    Ok(result)
}
