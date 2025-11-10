use crate::record::{ProcessExec, ProcessInfo, ProcessKind, Recording};
use crate::util::MapExt;
use indexmap::IndexMap;
use itertools::{Either, Itertools};
use nix::unistd::Pid;
use ordered_float::OrderedFloat;
use std::cmp::min;
use std::ops::{Range, RangeInclusive};

pub struct PlacedProcess {
    pub pid: Pid,

    pub depth: usize,
    pub row_offset: usize,
    pub row_height: usize,

    pub children: Vec<PlacedProcess>,

    // bounds
    pub max_depth: usize,
    pub time_bound: RangeInclusive<f32>,
}

pub fn place_processes(rec: &Recording, include_threads: bool) -> Option<PlacedProcess> {
    // TODO what about orphans?
    rec.root_pid.and_then(|root_pid| {
        let mut cache = TimeCache::new();
        place_process(rec, include_threads, &mut cache, root_pid, 0)
    })
}

impl PlacedProcess {
    pub fn visit(&self, f: &mut impl FnMut(&PlacedProcess, usize)) {
        fn visit_impl(slf: &PlacedProcess, offset_start: usize, f: &mut impl FnMut(&PlacedProcess, usize)) {
            let offset = offset_start + slf.row_offset;
            f(slf, offset);
            for child in &slf.children {
                visit_impl(child, offset, f);
            }
        }

        visit_impl(self, 0, f)
    }
}

fn place_process(
    rec: &Recording,
    include_threads: bool,
    cache: &mut TimeCache,
    pid: Pid,
    depth: usize,
) -> Option<PlacedProcess> {
    let info = rec.processes.get(&pid)?;

    // filter/flatten children
    let children = if include_threads {
        Either::Left(info.children.iter().map(|&(_, c)| c))
    } else {
        let mut children = vec![];
        fn f(rec: &Recording, children: &mut Vec<Pid>, curr: Pid) {
            if let Some(info) = rec.processes.get(&curr) {
                for &(child_kind, child_pid) in &info.children {
                    match child_kind {
                        ProcessKind::Process => children.push(child_pid),
                        ProcessKind::Thread => f(rec, children, child_pid),
                    }
                }
            }
        }
        f(rec, &mut children, pid);
        Either::Right(children.into_iter())
    };

    // collect all relevant time points and the processes that start/end that happen at those times
    let mut time_to_events: IndexMap<OrderedFloat<f32>, (Vec<Pid>, Vec<Pid>)> = IndexMap::new();
    for c in children {
        let cb = process_time_bound(rec, cache, c);
        if cb.start() == cb.end() {
            // TODO can we leave these in? they're tricky because they start and stop in the same cycle
            continue;
        }
        time_to_events.entry(OrderedFloat(*cb.start())).or_default().0.push(c);
        time_to_events.entry(OrderedFloat(*cb.end())).or_default().1.push(c);
    }
    let sorted_events = time_to_events
        .into_iter()
        .sorted_by_key(|&(k, _)| k)
        .map(|(_, v)| v)
        .collect_vec();

    // simulate time from left to right
    let mut free = FreeList::new();
    let mut children_active: IndexMap<Pid, Range<usize>> = IndexMap::new();
    let mut placed_children = vec![];
    let mut max_depth = depth;

    for (children_start, children_end) in sorted_events {
        // handle child ends (first to allow immediately reusing rows)
        for child in children_end {
            if let Some(range) = children_active.swap_remove(&child) {
                free.release(range)
            }
        }

        // handle child starts
        for child in children_start {
            if let Some(mut child_placed) = place_process(rec, include_threads, cache, child, depth + 1) {
                assert_eq!(child_placed.row_offset, 0);
                max_depth = max_depth.max(child_placed.max_depth);

                let child_height = child_placed.row_height;
                let child_row = free.allocate(child_height);
                child_placed.row_offset = 1 + child_row;
                children_active.insert_first(child, child_row..child_row + child_height);
                placed_children.push(child_placed);
            }
        }
    }

    // combine everything
    Some(PlacedProcess {
        pid,
        depth,
        row_offset: 0,
        row_height: 1 + free.len(),
        children: placed_children,
        max_depth,
        time_bound: process_time_bound(rec, cache, pid),
    })
}

type TimeCache = IndexMap<Pid, RangeInclusive<f32>>;

fn process_time_bound(rec: &Recording, cache: &mut TimeCache, pid: Pid) -> RangeInclusive<f32> {
    if let Some(res) = cache.get(&pid) {
        return res.clone();
    }

    let mut bound_min = f32::MAX;
    let mut bound_max = f32::MIN;

    if let Some(info) = rec.processes.get(&pid) {
        for &(_, c) in &info.children {
            let c_bound = process_time_bound(rec, cache, c);
            bound_min = bound_min.min(*c_bound.start());
            bound_max = bound_max.max(*c_bound.end());
        }
        process_for_each_time(info, |t| {
            bound_min = bound_min.min(t);
            bound_max = bound_max.max(t);
        });
    }

    let res = bound_min..=bound_max;
    cache.insert_first(pid, res.clone());
    res
}

fn process_for_each_time(proc: &ProcessInfo, mut f: impl FnMut(f32)) {
    let &ProcessInfo {
        pid: _,
        time_start,
        time_end,
        ref execs,
        children: _,
    } = proc;
    f(time_start);
    if let Some(time_end) = time_end {
        f(time_end);
    }
    for exec in execs {
        let &ProcessExec { time, path: _, argv: _ } = exec;
        f(time);
    }
}

struct FreeList {
    mask: Vec<bool>,
}

impl FreeList {
    fn new() -> Self {
        Self { mask: vec![] }
    }

    fn len(&self) -> usize {
        self.mask.len()
    }

    fn allocate(&mut self, len: usize) -> usize {
        // find start
        let mut start = None;
        for s in 0..self.len() {
            if (s..min(s + len, self.len())).all(|i| self.mask[i]) {
                start = Some(s);
                break;
            }
        }
        let start = start.unwrap_or(self.len());

        // extend if needed
        while self.len() < start + len {
            self.mask.push(true);
        }

        // clear allocated range
        for i in start..start + len {
            self.mask[i] = false;
        }

        start
    }

    fn release(&mut self, range: Range<usize>) {
        for i in range {
            assert!(!self.mask[i]);
            self.mask[i] = true;
        }
    }
}
