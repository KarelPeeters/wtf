use crate::record::ProcessKind;
use crate::trace::TraceEvent;
use nix::unistd::Pid;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
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

type ProcMap = HashMap<Pid, ProcessExecInfo>;

pub fn poll_proc<B>(
    child_path: &OsStr,
    child_argv: &[OsString],
    step: Duration,
    mut callback: impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> io::Result<ControlFlow<B, ExitStatus>> {
    // build root command
    let mut cmd = Command::new(child_path);
    if let Some((child_argv_0, child_argv_rest)) = child_argv.split_first() {
        cmd.arg0(child_argv_0);
        cmd.args(child_argv_rest);
    };

    // start root process
    let time_start = Instant::now();
    let mut root_handle = cmd.spawn()?;
    let root_pid = Pid::from_raw(root_handle.id() as i32);

    let mut prev_active: ProcMap = HashMap::new();
    let mut curr_active: ProcMap = HashMap::new();

    try_control!(callback(TraceEvent::TraceStart { time: time_start }));
    try_control!(callback(TraceEvent::ProcessStart {
        pid: root_pid,
        time: 0.0
    }));
    prev_active.insert(root_pid, get_process_exec_info(root_pid));

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

        // report dead processes
        for &pid in prev_active.keys() {
            if !curr_active.contains_key(&pid) {
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
    prev_active: &ProcMap,
    curr_active: &mut ProcMap,
    callback: &mut impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> ControlFlow<B> {
    // maybe report process start
    if !prev_active.contains_key(&pid) {
        callback(TraceEvent::ProcessStart { pid, time })?;
    }

    // maybe report process exec change
    let new_info = get_process_exec_info(pid);
    if prev_active.get(&pid).map_or(true, |info| info != &new_info) {
        callback(TraceEvent::ProcessExec {
            pid,
            time,
            cwd: new_info.cwd.clone(),
            path: new_info.path.clone(),
            argv: new_info.argv.clone(),
        })?;
    }
    curr_active.insert(pid, new_info);

    // visit children
    if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for child in children.split(" ") {
            if child.is_empty() {
                continue;
            }
            let child_pid = Pid::from_raw(child.parse::<i32>().unwrap());

            // report child process
            if !prev_active.contains_key(&child_pid) {
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
                    if !prev_active.contains_key(&task_pid) {
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

#[derive(Debug, Eq, PartialEq)]
struct ProcessExecInfo {
    cwd: Option<String>,
    path: String,
    argv: Vec<String>,
}

fn get_process_exec_info(pid: Pid) -> ProcessExecInfo {
    let cwd = if let Ok(cwd) = std::fs::read_link(format!("/proc/{}/cwd", pid)) {
        Some(cwd.into_os_string().to_string_lossy().into_owned())
    } else {
        None
    };

    let path = if let Ok(exe_path) = std::fs::read_link(format!("/proc/{}/exe", pid)) {
        exe_path.to_string_lossy().into_owned()
    } else {
        String::new()
    };

    let argv = if let Ok(argv) = std::fs::read(format!("/proc/{}/cmdline", pid)) {
        argv.split(|&b| b == 0)
            .map(|s| OsString::from_vec(s.to_owned()).to_string_lossy().into_owned())
            .collect()
    } else {
        Vec::new()
    };

    ProcessExecInfo { cwd, path, argv }
}
