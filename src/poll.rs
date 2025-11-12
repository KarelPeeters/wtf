use crate::record::ProcessKind;
use crate::trace::TraceEvent;
use nix::unistd::Pid;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

macro_rules! try_control {
    ($e:expr) => {
        match ($e) {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(b) => return Ok(ControlFlow::Break(b)),
        }
    };
}

pub fn poll_proc<B>(
    child_path: &OsStr,
    child_argv: &[OsString],
    step: Duration,
    mut callback: impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> io::Result<ControlFlow<B, ExitStatus>> {
    let time_start = Instant::now();
    let mut root_handle = Command::new(child_path).args(child_argv).spawn()?;
    let root_pid = Pid::from_raw(root_handle.id() as i32);

    let mut prev_active: HashSet<Pid> = HashSet::new();
    let mut curr_active: HashSet<Pid> = HashSet::new();

    try_control!(callback(TraceEvent::TraceStart { time: time_start }));
    try_control!(callback(TraceEvent::ProcessStart {
        pid: root_pid,
        time: 0.0
    }));
    prev_active.insert(root_pid);

    loop {
        let time_now = Instant::now();
        let time_now_f = (time_now - time_start).as_secs_f32();

        // check if the child is done
        if let Some(status) = root_handle.try_wait()? {
            try_control!(callback(TraceEvent::TraceEnd { time: time_now_f }));
            return Ok(ControlFlow::Continue(status));
        }

        // start polling from the root process
        assert!(curr_active.is_empty());
        try_control!(poll_proc_all(
            time_now_f,
            root_pid,
            &prev_active,
            &mut curr_active,
            &mut callback
        ));

        // collect dead processes
        for &pid in &prev_active {
            if !curr_active.contains(&pid) {
                try_control!(callback(TraceEvent::ProcessExit { pid, time: time_now_f }));
            }
        }
        std::mem::swap(&mut curr_active, &mut prev_active);
        curr_active.clear();

        // wait for leftover time if any
        let time_left = step.checked_sub(time_now.elapsed());
        if let Some(time_left) = time_left {
            std::thread::sleep(time_left);
        }
    }
}

fn poll_proc_all<B>(
    time: f32,
    pid: Pid,
    prev_active: &HashSet<Pid>,
    curr_active: &mut HashSet<Pid>,
    callback: &mut impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> ControlFlow<B> {
    // mark process as active
    if !prev_active.contains(&pid) {
        callback(TraceEvent::ProcessStart { pid, time })?;
    }
    curr_active.insert(pid);

    // visit children
    if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for child in children.split(" ") {
            if child.is_empty() {
                continue;
            }
            let child_pid = Pid::from_raw(child.parse::<i32>().unwrap());

            // report child process
            if !prev_active.contains(&child_pid) {
                callback(TraceEvent::ProcessChild {
                    parent: pid,
                    child: child_pid,
                    kind: ProcessKind::Process,
                })?;
            }

            // recurse into child process
            poll_proc_all(time, child_pid, &prev_active, curr_active, callback)?;
        }
    }

    // visit threads
    if let Ok(dirs) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for dir in dirs {
            if let Ok(dir) = dir {
                let task_pid = Pid::from_raw(dir.file_name().to_str().unwrap().parse::<i32>().unwrap());
                if task_pid != pid {
                    // report child thread
                    if !prev_active.contains(&task_pid) {
                        callback(TraceEvent::ProcessChild {
                            parent: pid,
                            child: task_pid,
                            kind: ProcessKind::Thread,
                        })?;
                    }

                    // recurse into threads
                    poll_proc_all(time, task_pid, &prev_active, curr_active, callback)?;
                }
            }
        }
    }

    ControlFlow::Continue(())
}
