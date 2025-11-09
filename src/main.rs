#![cfg(unix)]

use clap::Parser;
use indexmap::IndexMap;
use nix::errno::Errno;
use nix::libc;
use nix::libc::ptrace_syscall_info;
use nix::sys::wait::WaitStatus;
use nix::sys::{ptrace, wait};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Instant;
use syscalls::Sysno;
use wtf::util::MapExt;

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

#[derive(Debug)]
struct Recording {
    processes: IndexMap<Pid, ProcessInfo>,
}

#[derive(Debug)]
struct ProcessInfo {
    pid: Pid,
    parent_pid: Option<Pid>,

    time_start: Instant,
    time_end: Option<Instant>,

    execs: Vec<ProcessExec>,
    children: Vec<Pid>,
}

#[derive(Debug)]
struct ProcessExec {
    time: Instant,
    name: String,
    argv: Vec<String>,
}

impl ProcessInfo {
    pub fn new_start_now(pid: Pid, parent_pid: Option<Pid>) -> Self {
        Self {
            pid,
            parent_pid,
            time_start: Instant::now(),
            time_end: None,
            execs: Vec::new(),
            children: Vec::new(),
        }
    }
}

// TODO proper error handling around command spawning
fn main() {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    // start the child process
    let mut root_cmd = Command::new(&args.command[0]);
    root_cmd.args(&args.command[1..]);
    unsafe {
        // tell the child process to start being traced
        root_cmd.pre_exec(|| {
            ptrace::traceme().map_err(errno_to_io)?;
            Ok(())
        })
    };
    let root = root_cmd.spawn().expect("failed to spawn child");
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
    let mut recording = Recording {
        processes: IndexMap::new(),
    };
    recording
        .processes
        .insert_first(root_pid, ProcessInfo::new_start_now(root_pid, None));

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
                        let entry = unsafe { &info.u.entry };
                        let nr = Sysno::new(entry.nr as usize);

                        let next_partial_syscall = if let Some(nr) = nr {
                            let res = match nr {
                                Sysno::clone | Sysno::fork | Sysno::vfork | Sysno::clone3 => SyscallEntry::Fork,
                                Sysno::execve | Sysno::execveat => SyscallEntry::Exec,
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
                        let partial = partial_syscalls.remove(&pid).unwrap_or(SyscallEntry::Ignore);

                        let exit_info = unsafe { &info.u.exit };

                        match partial {
                            SyscallEntry::Ignore => {}
                            SyscallEntry::Fork => {
                                // TODO record forked process pid
                            }
                            SyscallEntry::Exec => {
                                // TODO record new process info (only if successful!)
                            }
                        }

                        if !matches!(partial, SyscallEntry::Ignore) {
                            println!("[{pid}] syscall exit {:?} -> {}", partial, exit_info.sval);
                        }
                    }
                    _ => {}
                }

                Some(pid)
            }
            // handle event
            WaitStatus::PtraceEvent(pid, _signal, event) => {
                match event {
                    // handle fork-like events at the start of the child process
                    // note: these don't necessarily correspond to exact original syscall, depending on the flags
                    libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                        let child_pid = ptrace::getevent(pid).expect("ptrace::getevent failed");
                        let child_pid = Pid::from_raw(child_pid as i32);

                        recording
                            .processes
                            .insert_first(child_pid, ProcessInfo::new_start_now(child_pid, Some(pid)));
                    }
                    // ignore other events
                    _ => {}
                }

                Some(pid)
            }
            // process exited, cleanup and maybe stop tracing
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => {
                recording.processes.get_mut(&pid).unwrap().time_end = Some(Instant::now());
                partial_syscalls.remove(&pid);
                if pid == root_pid {
                    break;
                }
                None
            }
            // stopped by some signal, just continue
            WaitStatus::Stopped(pid, _signal) => Some(pid),
            // cases that shouldn't happen
            WaitStatus::Continued(_) => unreachable!("we didn't set WaitPidFlag::WCONTINUE"),
            WaitStatus::StillAlive => unreachable!("we didn't set WaitPidFlag::WNOHANG"),
        };

        if let Some(resume_pid) = resume_pid {
            ptrace::syscall(resume_pid, None).expect("failed ptrace::syscall");
        }
    }

    println!("Recording complete:");
    for info in recording.processes.values() {
        println!("  {:?}", info);
    }
}

#[derive(Debug)]
enum SyscallEntry {
    Ignore,
    Fork,
    Exec,
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

fn errno_to_io(e: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}
