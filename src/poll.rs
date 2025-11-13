use crate::record::ProcessKind;
use crate::trace::TraceEvent;
use nix::unistd::Pid;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

macro_rules! try_control {
    ($e:expr) => {
        match ($e) {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(b) => return Ok(ControlFlow::Break(b)),
        }
    };
}

type ProcSet = HashSet<Pid>;
type ProcMap = HashMap<Pid, Option<ProcessExecInfo>>;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let pid = Pid::from_raw(self.0.id() as i32);
        let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = self.0.kill();
    }
}

pub fn record_poll<B>(
    child_path: &OsStr,
    child_argv: &[OsString],
    period: Duration,
    mut callback: impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> io::Result<ControlFlow<B, ExitStatus>> {
    // build root command
    let mut cmd = Command::new(child_path);
    if let Some((child_argv_0, child_argv_rest)) = child_argv.split_first() {
        cmd.arg0(child_argv_0);
        cmd.args(child_argv_rest);
    };
    unsafe {
        // set process group so we can kill all children later
        cmd.pre_exec(|| {
            nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(|e| io::Error::from_raw_os_error(e as i32))
        });
    }

    // start root process
    let time_start = Instant::now();
    let root_handle = cmd.spawn()?;
    let root_pid = Pid::from_raw(root_handle.id() as i32);
    let mut root_handle = KillOnDrop(root_handle);

    let mut ever_active: HashMap<Pid, Option<ProcessExecInfo>> = HashMap::new();
    let mut prev_active: ProcSet = HashSet::new();
    let mut curr_active: ProcSet = HashSet::new();

    try_control!(callback(TraceEvent::TraceStart { time: time_start }));

    loop {
        let time_now = Instant::now();
        let time_now_f = (time_now - time_start).as_secs_f32();

        try_control!(callback(TraceEvent::None));

        // check if the child is done
        if let Some(status) = root_handle.0.try_wait()? {
            for &pid in &prev_active {
                try_control!(callback(TraceEvent::ProcessExit { pid, time: time_now_f }));
            }
            try_control!(callback(TraceEvent::TraceEnd { time: time_now_f }));
            return Ok(ControlFlow::Continue(status));
        }

        // start polling from the root process
        assert!(curr_active.is_empty());
        try_control!(poll_proc_all(
            time_now_f,
            root_pid,
            &mut ever_active,
            &mut curr_active,
            &mut callback
        ));

        // report dead processes
        for &pid in &prev_active {
            if !curr_active.contains(&pid) {
                try_control!(callback(TraceEvent::ProcessExit { pid, time: time_now_f }));
            }
        }
        std::mem::swap(&mut curr_active, &mut prev_active);
        curr_active.clear();

        // wait for leftover time if any
        let time_left = period.checked_sub(time_now.elapsed());
        if let Some(time_left) = time_left {
            std::thread::sleep(time_left);
        }
    }
}

fn poll_proc_all<B>(
    time: f32,
    pid: Pid,
    ever_active: &mut ProcMap,
    curr_active: &mut ProcSet,
    callback: &mut impl FnMut(TraceEvent) -> ControlFlow<B>,
) -> ControlFlow<B> {
    assert!(!curr_active.contains(&pid));

    // maybe report process start
    if !ever_active.contains_key(&pid) {
        callback(TraceEvent::ProcessStart { pid, time })?;
    }
    curr_active.insert(pid);

    // maybe report process exec change, if there is new good info
    let new_info = get_process_exec_info(pid);
    let old_info = ever_active.get(&pid).and_then(Option::as_ref);
    match (old_info, new_info) {
        (old_info, Ok(new_info)) => {
            if old_info.is_none_or(|old_info| old_info.path != new_info.path || old_info.argv != new_info.argv) {
                callback(TraceEvent::ProcessExec {
                    pid,
                    time,
                    cwd: new_info.cwd.clone(),
                    path: new_info.path.clone(),
                    argv: new_info.argv.clone(),
                })?;
            }

            // replace with new info
            ever_active.insert(pid, Some(new_info));
        }
        (None, Err(_)) => {
            // mark as active but without good info yet
            ever_active.insert(pid, None);
        }
        (Some(_), Err(_)) => {
            // leave old info as is, we don't have anything better
        }
    };
    assert!(ever_active.contains_key(&pid));

    // visit threads
    if let Ok(dirs) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for dir in dirs {
            if let Ok(dir) = dir {
                let task_pid = Pid::from_raw(dir.file_name().to_str().unwrap().parse::<i32>().unwrap());

                if task_pid != pid {
                    // report child thread
                    if let Entry::Vacant(e) = ever_active.entry(task_pid) {
                        e.insert(None);
                        curr_active.insert(task_pid);

                        callback(TraceEvent::ProcessStart { pid: task_pid, time })?;
                        callback(TraceEvent::ProcessChild {
                            parent: pid,
                            child: task_pid,
                            kind: ProcessKind::Thread,
                        })?;
                    }
                }

                // visit children
                if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{task_pid}/children")) {
                    for child in children.split(" ") {
                        if child.is_empty() {
                            continue;
                        }
                        let child_pid = Pid::from_raw(child.parse::<i32>().unwrap());

                        // report child process
                        if !ever_active.contains_key(&child_pid) {
                            callback(TraceEvent::ProcessChild {
                                parent: task_pid,
                                child: child_pid,
                                kind: ProcessKind::Process,
                            })?;
                        }

                        // recurse into child process
                        poll_proc_all(time, child_pid, ever_active, curr_active, callback)?;
                    }
                }
            }
        }
    }

    ControlFlow::Continue(())
}

#[derive(Debug)]
struct ProcessExecInfo {
    cwd: Option<String>,
    path: String,
    argv: Vec<String>,
}

fn get_process_exec_info(pid: Pid) -> io::Result<ProcessExecInfo> {
    let cwd = std::fs::read_link(format!("/proc/{}/cwd", pid))?
        .into_os_string()
        .to_string_lossy()
        .into_owned();

    let path = std::fs::read_link(format!("/proc/{}/exe", pid))?
        .to_string_lossy()
        .into_owned();

    let argv = std::fs::read(format!("/proc/{}/cmdline", pid))?
        .split(|&b| b == 0)
        .map(|s| OsString::from_vec(s.to_owned()).to_string_lossy().into_owned())
        .collect();

    Ok(ProcessExecInfo {
        cwd: Some(cwd),
        path,
        argv,
    })
}
