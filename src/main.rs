#![cfg(unix)]

use clap::Parser;
use nix::errno::Errno;
use nix::libc;
use nix::libc::ptrace_syscall_info;
use nix::sys::wait::WaitStatus;
use nix::sys::{ptrace, wait};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::process::Command;
use syscalls::Sysno;

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

// TODO proper error handling around command spawning
fn main() {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    // start the child process
    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..]);
    unsafe {
        cmd.pre_exec(|| {
            ptrace::traceme().map_err(errno_to_io)?;
            Ok(())
        })
    };

    println!("Spawning child process");
    let child = cmd.spawn().expect("failed to spawn child");
    let child_pid = Pid::from_raw(child.id() as i32);

    // options:
    // * PTRACE_O_TRACESYSGOOD: add mask to syscall stops, allows parsing WaitStatus::PtraceSyscall
    // * PTRACE_O_EXITKILL: kill traced process if tracer exits to avoid orphaned processes
    // * PTRACE_O_TRACE*: trace children through fork syscalls?
    let ptrace_options = ptrace::Options::PTRACE_O_TRACESYSGOOD | ptrace::Options::PTRACE_O_EXITKILL;
    // | ptrace::Options::PTRACE_O_TRACEFORK
    // | ptrace::Options::PTRACE_O_TRACEVFORK
    // | ptrace::Options::PTRACE_O_TRACECLONE;
    ptrace::setoptions(child_pid, ptrace_options).expect("failed to set ptrace options");

    let mut partial_syscall = None;

    loop {
        ptrace::syscall(child_pid, None).expect("failed ptrace::syscall");
        let status = wait::waitpid(child_pid, None).expect("failed wait::waitpid");
        match status {
            WaitStatus::Exited(pid, _status) => {
                // root child exited, we can stop tracing
                if pid == child_pid {
                    break;
                }
            }
            WaitStatus::PtraceSyscall(pid) => {
                let info = ptrace_syscall_info(pid).expect("failed ptrace::syscall_info");

                println!("syscall {pid} {:?}", info);

                match info.op {
                    libc::PTRACE_SYSCALL_INFO_ENTRY => {
                        assert!(partial_syscall.is_none());

                        let entry = unsafe { &info.u.entry };
                        let nr = Sysno::new(entry.nr as usize);

                        let next_partial_syscall = if let Some(nr) = nr {
                            match nr {
                                Sysno::clone | Sysno::fork | Sysno::vfork | Sysno::clone3 => SyscallEntry::Fork,
                                Sysno::execve | Sysno::execveat => SyscallEntry::Exec,
                                Sysno::exit | Sysno::exit_group => SyscallEntry::Exit,
                                _ => SyscallEntry::Ignore,
                            }
                        } else {
                            // ignore unknown syscalls
                            SyscallEntry::Ignore
                        };

                        println!("  entry {:?}", nr);

                        partial_syscall = Some(next_partial_syscall);
                    }
                    libc::PTRACE_SYSCALL_INFO_EXIT => {
                        let partial = partial_syscall.take().unwrap();

                        let entry = unsafe { &info.u.exit };

                        match partial {
                            SyscallEntry::Ignore => {}
                            SyscallEntry::Fork => {
                                // TODO record forked process pid
                            }
                            SyscallEntry::Exec => {
                                // TODO record new process info (only if successful!)
                            }
                            SyscallEntry::Exit => {
                                // TODO record process exit
                            }
                        }

                        println!("  exit {}", entry.sval);
                    }
                    _ => {}
                }
            }
            // ignore these, we only care about (some) syscalls
            WaitStatus::Signaled(_, _, _) => todo!(),
            WaitStatus::Stopped(_, _) => {}
            WaitStatus::PtraceEvent(pid, signal, extra) => {
                println!("ptrace event: pid={pid} signal={signal:?} extra={extra}");
            }
            WaitStatus::Continued(_) => todo!(),
            WaitStatus::StillAlive => todo!(),
        }
    }
}

enum SyscallEntry {
    Ignore,
    Fork,
    Exec,
    Exit,
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
