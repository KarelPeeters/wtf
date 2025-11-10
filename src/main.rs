#![cfg(unix)]

use clap::Parser;
use crossbeam::channel::{Receiver, RecvError, SendError, Sender, TryRecvError};
use std::ffi::CString;
use std::ops::ControlFlow;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wtf::gui::{main_gui, DataToGui, GuiHandle};
use wtf::layout::place_processes;
use wtf::record::Recording;
use wtf::trace::{record_trace, SpawnFailed, TraceEvent};

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<CString>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    let stopped = Arc::new(AtomicBool::new(false));
    let (event_tx, event_rx) = crossbeam::channel::unbounded::<TraceEvent>();
    let (gui_handle_tx, gui_handle_rx) = crossbeam::channel::bounded::<GuiHandle>(1);

    // spawn tracing thread
    // TODO does fork/exec work fine with the extra spawned thread?  if not, split this up into start/run
    // TODO result handling would also be nicer with split, then we get the result in the first half already
    let handle_tracer = {
        let stopped = stopped.clone();
        std::thread::spawn(move || unsafe { thread_tracer(&args, stopped, event_tx) })
    };

    // spawn collector thread
    let handle_collector = {
        let stopped = stopped.clone();
        std::thread::spawn(move || thread_collector(stopped, event_rx, gui_handle_rx))
    };

    // start gui (egui wants this to be on the main thread)
    main_gui(gui_handle_tx).expect("GUI failed");
    stopped.store(true, Ordering::Relaxed);

    let record_result = handle_tracer.join().expect("Failed to join tracer thread");
    let _ = handle_collector.join();

    match record_result {
        Ok(rec) => rec,
        Err(e) => {
            eprintln!("Failed to spawn child process: {}", e.0);
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

unsafe fn thread_tracer(
    args: &Args,
    stopped: Arc<AtomicBool>,
    event_tx: Sender<TraceEvent>,
) -> Result<(), SpawnFailed> {
    let record_result = unsafe {
        record_trace(&args.command[0], &args.command[0..], |event| {
            if stopped.load(Ordering::Relaxed) {
                return ControlFlow::Break(());
            }

            match event_tx.send(event) {
                Ok(()) => ControlFlow::Continue(()),
                Err(SendError(_)) => ControlFlow::Break(()),
            }
        })
    };
    drop(event_tx);

    if record_result.is_err() {
        stopped.store(true, Ordering::Relaxed);
    }

    record_result
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
        let mut disconnected = false;
        match event_rx.try_recv() {
            Ok(event) => recording.report(event),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => disconnected = true,
        }

        // compute a new mapping
        // TODO make thread inclusion configurable from the GUI
        // TODO avoid deep cloning here?
        let placed = place_processes(&recording, false);
        let data = DataToGui {
            recording: recording.clone(),
            placed,
        };

        *gui_handle.data_to_gui.lock().unwrap() = Some(data);
        gui_handle.ctx.request_repaint();

        if disconnected {
            break;
        }
    }
}
