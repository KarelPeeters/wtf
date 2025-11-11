use crate::trace::TraceEvent;
use crate::util::MapExt;
use indexmap::IndexMap;
use nix::unistd::Pid;

#[derive(Debug, Clone)]
pub struct Recording {
    pub root_pid: Option<Pid>,
    pub processes: IndexMap<Pid, ProcessInfo>,
    pub last_time: f32,
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: Pid,

    pub time_start: f32,
    pub time_end: Option<f32>,

    pub execs: Vec<ProcessExec>,
    // note: children might be reported here before they actually exist as ProcessInfo entries
    pub children: Vec<(ProcessKind, Pid)>,
}

#[derive(Debug, Clone)]
pub struct ProcessExec {
    pub time: f32,
    pub path: String,
    pub argv: Vec<String>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ProcessKind {
    Process,
    Thread,
}

impl Recording {
    pub fn new() -> Self {
        Self {
            root_pid: None,
            processes: IndexMap::new(),
            last_time: 0.0,
        }
    }

    pub fn report(&mut self, event: TraceEvent) {
        match event {
            TraceEvent::ProcessStart { pid, time } => {
                let info = ProcessInfo {
                    pid,
                    time_start: time,
                    time_end: None,
                    execs: Vec::new(),
                    children: Vec::new(),
                };
                self.processes.insert_first(pid, info);

                if self.root_pid.is_none() {
                    self.root_pid = Some(pid);
                }
            }
            TraceEvent::ProcessExit { pid, time } => {
                self.processes.get_mut(&pid).unwrap().time_end = Some(time);
            }
            TraceEvent::ProcessChild { parent, child, kind } => {
                self.processes.get_mut(&parent).unwrap().children.push((kind, child));
            }
            TraceEvent::ProcessExec { pid, time, path, argv } => {
                let exec = ProcessExec { time, path, argv };
                self.processes.get_mut(&pid).unwrap().execs.push(exec);
            }
            TraceEvent::Time { time } => {
                self.last_time = time;
            }
        }
    }
}
