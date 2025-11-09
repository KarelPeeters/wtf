#![cfg(unix)]

use clap::Parser;
use std::process::Command;
use wtf::trace::record_trace;

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

fn main() {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..]);
    let rec = record_trace(cmd);

    println!("Recording complete:");
    for info in rec.processes.values() {
        println!("  {:?}", info);
    }
}
