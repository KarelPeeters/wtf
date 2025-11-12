#![cfg(unix)]

use clap::Parser;
use crossbeam::channel::{Receiver, RecvError, SendError, TryRecvError};
use itertools::Itertools;
use std::ffi::{CString, OsString};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wtf::gui::{main_gui, DataToGui, GuiHandle};
use wtf::layout::place_processes;
use wtf::poll::poll_proc;
use wtf::record::Recording;
use wtf::trace::{record_trace, TraceEvent};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    /// Polling frequency in Hz. If not passed, ptrace-based tracing is used.
    poll: Option<f32>,

    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

fn main() -> ExitCode {
    // parse args
    let args = Args::parse();

    assert!(args.command.len() >= 1);
    let mut command_args = args.command;
    let command = command_args.remove(0);

    // create shared state and channels
    let stopped = Arc::new(AtomicBool::new(false));
    let (event_tx, event_rx) = crossbeam::channel::unbounded::<TraceEvent>();
    let (gui_handle_tx, gui_handle_rx) = crossbeam::channel::bounded::<GuiHandle>(1);

    // spawn tracing thread
    let handle_tracer = {
        let stopped = stopped.clone();
        let callback = move |event| {
            if stopped.load(Ordering::Relaxed) {
                return ControlFlow::Break(());
            }

            match event_tx.send(event) {
                Ok(()) => ControlFlow::Continue(()),
                Err(SendError(_)) => ControlFlow::Break(()),
            }
        };

        match args.poll {
            None => {
                // TODO does fork/exec work fine with the extra spawned thread?  if not, split this up into start/run
                let command = CString::new(command.as_bytes()).expect("Failed to convert command to CString");
                let command_args = command_args
                    .iter()
                    .map(|s| CString::new(s.as_bytes()).expect("Failed to convert command to CString"))
                    .collect_vec();

                std::thread::spawn(move || {
                    let trace_result = unsafe { record_trace(&command, &command_args, callback) };
                    if let Err(e) = &trace_result {
                        eprintln!("Failed to spawn child process: {}", e.0);
                    }
                })
            }
            Some(poll_freq) => std::thread::spawn(move || {
                let poll_result = poll_proc(&command, &command_args, Duration::from_secs_f32(poll_freq), callback);
                if let Err(e) = &poll_result {
                    eprintln!("Failed to spawn child process: {}", e);
                }
            }),
        }
    };

    // spawn collector thread
    let handle_collector = {
        let stopped = stopped.clone();
        std::thread::spawn(move || thread_collector(stopped, event_rx, gui_handle_rx))
    };

    // start gui (egui wants this to be on the main thread)
    main_gui(gui_handle_tx).expect("GUI failed");
    stopped.store(true, Ordering::Relaxed);

    let _ = handle_tracer.join();
    let _ = handle_collector.join();

    ExitCode::SUCCESS
}

fn thread_collector(stopped: Arc<AtomicBool>, event_rx: Receiver<TraceEvent>, gui_handle_rx: Receiver<GuiHandle>) {
    let gui_handle = match gui_handle_rx.recv() {
        Ok(handle) => handle,
        Err(RecvError) => return,
    };
    drop(gui_handle_rx);

    let mut recording = Recording::new();

    loop {
        if stopped.load(Ordering::Relaxed) {
            break;
        }

        // wait for next event
        match event_rx.recv() {
            Ok(event) => recording.report(event),
            Err(RecvError) => break,
        }
        // batch collect all available events
        // (we can't exit immediately on disconnect, we want to send the last remaining data first)
        let disconnected = loop {
            match event_rx.try_recv() {
                Ok(event) => recording.report(event),
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => break true,
            }
        };

        // compute a new mapping
        // TODO make thread inclusion configurable from the GUI
        // TODO avoid deep cloning here?
        let placed_threads_no = place_processes(&recording, false);
        let placed_threads_yes = place_processes(&recording, true);

        let data = DataToGui {
            recording: recording.clone(),
            placed_threads_no,
            placed_threads_yes,
        };

        *gui_handle.data_to_gui.lock().unwrap() = Some(data);
        gui_handle.ctx.request_repaint();

        if disconnected {
            break;
        }
    }
}
