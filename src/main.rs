#![cfg(unix)]

use clap::Parser;
use std::ffi::CString;
use std::process::ExitCode;
use wtf::gui::main_gui;
use wtf::layout::place_processes;
use wtf::record::Recording;
use wtf::trace::record_trace;

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<CString>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    let mut recording = Recording::new();
    let record_result = unsafe { record_trace(&args.command[0], &args.command[0..], |event| recording.report(event)) };
    match record_result {
        Ok(rec) => rec,
        Err(e) => {
            eprintln!("Failed to spawn child process: {}", e.0);
            return ExitCode::FAILURE;
        }
    };

    println!("Recording complete:");
    for info in recording.processes.values() {
        println!("  {:?}", info);
    }

    let placed = place_processes(&recording, false);

    if let Some(placed) = placed {
        main_gui(recording, placed).expect("GUI failed");
    }

    ExitCode::SUCCESS
}
