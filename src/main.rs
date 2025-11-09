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
    let mut child = cmd.spawn().expect("failed to spawn child");
    let child_pid = Pid::from_raw(child.id() as i32);

    ptrace::setoptions(child_pid, ptrace::Options::PTRACE_O_TRACESYSGOOD)
        .expect("failed to set ptrace options");

    loop {
        ptrace::syscall(child_pid, None).expect("failed ptrace::syscall");
        let status = wait::waitpid(child_pid, None).expect("failed wait::waitpid");
        match status {
            WaitStatus::Exited(_pid, _status) => {
                // child exited, we can stop tracing
                break;
            }
            WaitStatus::Signaled(_, _, _) => todo!(),
            WaitStatus::Stopped(_, _) => todo!(),
            WaitStatus::PtraceEvent(_, _, _) => todo!(),
            WaitStatus::PtraceSyscall(_) => {
                let info = ptrace_syscall_info(child_pid).expect("failed ptrace::syscall_info");
                println!("syscall {:?}", info);
            }
            WaitStatus::Continued(_) => todo!(),
            WaitStatus::StillAlive => todo!(),
        }
    }
}

/// Fixed version of ptrace::syscall_info.
/// Based on https://github.com/nix-rust/nix/issues/2660.
fn ptrace_syscall_info(pid: Pid) -> Result<ptrace_syscall_info, Errno> {
    let mut data = std::mem::MaybeUninit::<libc::ptrace_syscall_info>::uninit();

    let res = unsafe {
        libc::ptrace(
            ptrace::Request::PTRACE_GET_SYSCALL_INFO as libc::c_uint,
            libc::pid_t::from(pid),
            std::mem::size_of::<libc::ptrace_syscall_info>(),
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
