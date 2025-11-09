#![cfg(unix)]

use clap::Parser;
use nix::errno::Errno;
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
            println!("before traceme");
            nix::sys::ptrace::traceme().map_err(errno_to_io)?;
            println!("before kill");
            nix::sys::signal::kill(nix::unistd::getpid(), nix::sys::signal::Signal::SIGSTOP)
                .map_err(errno_to_io)?;
            println!("after kill");
            Ok(())
        })
    };

    println!("Spawning child process");
    let mut child = cmd.spawn().expect("failed to spawn child");

    println!("Waiting for child pid");
    nix::sys::wait::waitpid(Some(Pid::from_raw(child.id() as i32)), None).unwrap();

    // start capturing trace
    // TODO

    // wait for child to finish?
    child.wait().expect("failed to wait for child");
}

fn errno_to_io(e: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}
