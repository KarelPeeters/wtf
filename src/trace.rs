#![cfg(unix)]

use crate::util::MapExt;
use indexmap::IndexMap;
use nix::errno::Errno;
use nix::libc;
use nix::libc::ptrace_syscall_info;
use nix::sys::signal::SIGSTOP;
use nix::sys::wait::WaitStatus;
use nix::sys::{ptrace, wait};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Instant;
use syscalls::Sysno;

#[derive(Debug)]
pub struct Recording {
    pub root_pid: Pid,
    pub processes: IndexMap<Pid, ProcessInfo>,
}

#[derive(Debug)]
pub struct ProcessInfo {
    pub pid: Pid,

    pub time_start: f32,
    pub time_end: Option<f32>,

    pub execs: Vec<ProcessExec>,
    // note: children might be reported here before they actually exist as ProcessInfo entries
    pub children: Vec<Pid>,
}

#[derive(Debug)]
pub struct ProcessExec {
    pub time: f32,
    pub path: String,
    pub argv: Vec<String>,
}

impl ProcessInfo {
    pub fn new(pid: Pid, time_start: f32) -> Self {
        Self {
            pid,
            time_start,
            time_end: None,
            execs: Vec::new(),
            children: Vec::new(),
        }
    }
}

// TODO better error handling
pub fn record_trace(mut cmd: Command) -> Recording {
    // start the child process
    unsafe {
        // tell the child process to start being traced
        cmd.pre_exec(|| {
            ptrace::traceme().map_err(errno_to_io)?;
            Ok(())
        })
    };
    let root = cmd.spawn().expect("failed to spawn child");
    let root_pid = Pid::from_raw(root.id() as i32);

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

    // result data structure
    // TODO is this time info accurate enough?
    let time_start = Instant::now();
    let mut recording = Recording {
        root_pid,
        processes: IndexMap::new(),
    };
    recording
        .processes
        .insert_first(root_pid, ProcessInfo::new(root_pid, 0.0));

    // track in-progress syscall per child
    let mut partial_syscalls: HashMap<Pid, SyscallEntry> = HashMap::new();

    // main tracing event loop
    loop {
        let status = wait::waitpid(None, None).expect("failed wait::waitpid");

        let resume_pid = match status {
            // handle syscall
            WaitStatus::PtraceSyscall(pid) => {
                let info = ptrace_syscall_info(pid).expect("failed ptrace::syscall_info");

                match info.op {
                    libc::PTRACE_SYSCALL_INFO_ENTRY => {
                        let info_entry = unsafe { &info.u.entry };
                        let nr = Sysno::new(info_entry.nr as usize);

                        let next_partial_syscall = if let Some(nr) = nr {
                            let res = match nr {
                                // handle fork-like
                                Sysno::clone | Sysno::fork | Sysno::vfork | Sysno::clone3 => SyscallEntry::Fork,
                                // handle exec-like
                                Sysno::execve => {
                                    let args_ptr = ExecArgPointers {
                                        path: info_entry.args[0],
                                        argv: info_entry.args[1],
                                        envp: info_entry.args[2],
                                    };
                                    let args =
                                        ptrace_extract_exec_args(pid, args_ptr).expect("failed to extract exec args");
                                    SyscallEntry::Exec(args)
                                }
                                Sysno::execveat => {
                                    let args_ptr = ExecArgPointers {
                                        path: info_entry.args[1],
                                        argv: info_entry.args[2],
                                        envp: info_entry.args[3],
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

                            if !matches!(res, SyscallEntry::Ignore) {
                                println!("[{pid}] syscall entry {nr:?}");
                            }

                            res
                        } else {
                            // ignore unknown syscalls
                            SyscallEntry::Ignore
                        };

                        partial_syscalls.insert_first(pid, next_partial_syscall);
                    }
                    libc::PTRACE_SYSCALL_INFO_EXIT => {
                        let info_exir = unsafe { &info.u.exit };

                        let partial = partial_syscalls.remove(&pid).unwrap_or(SyscallEntry::Ignore);
                        match partial {
                            SyscallEntry::Ignore => {}
                            SyscallEntry::Fork => {
                                println!("[{pid}] syscall exit fork-like");
                                if info_exir.sval > 0 {
                                    let child_pid = Pid::from_raw(info_exir.sval as i32);
                                    recording.processes.get_mut(&pid).unwrap().children.push(child_pid);
                                }
                            }
                            SyscallEntry::Exec(ref args) => {
                                println!("[{pid}] syscall exit exec-like");
                                if info_exir.sval == 0 {
                                    let proc_exec = ProcessExec {
                                        time: time_start.elapsed().as_secs_f32(),
                                        path: String::from_utf8_lossy(&args.path).into_owned(),
                                        argv: vec![],
                                    };
                                    recording.processes.get_mut(&pid).unwrap().execs.push(proc_exec);
                                }
                            }
                        }
                    }
                    _ => {}
                }

                Some(pid)
            }
            // ignore events
            //    these get reported for the parent process when children are created due to the ptrace options,
            //    but we don't care about them
            WaitStatus::PtraceEvent(pid, _signal, _event) => Some(pid),
            // process exited, cleanup and maybe stop tracing
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => {
                recording.processes.get_mut(&pid).unwrap().time_end = Some(time_start.elapsed().as_secs_f32());
                partial_syscalls.remove(&pid);
                if pid == root_pid {
                    break;
                }
                None
            }
            // stopped by some signal, just continue
            WaitStatus::Stopped(pid, signal) => {
                if signal == SIGSTOP && !recording.processes.contains_key(&pid) {
                    // initial stop for new child process, create it
                    let proc_info = ProcessInfo::new(pid, time_start.elapsed().as_secs_f32());
                    recording.processes.insert_first(pid, proc_info);
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

    recording
}

#[derive(Debug)]
enum SyscallEntry {
    Ignore,
    Fork,
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

/// Fixed version of ptrace::syscall_info.
/// Based on https://github.com/nix-rust/nix/issues/2660.
fn ptrace_syscall_info(pid: Pid) -> Result<ptrace_syscall_info, Errno> {
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

    Ok(ExecArgs { path, argv: Vec::new() })
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

fn errno_to_io(e: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}
