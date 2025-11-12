use crate::record::{ProcessKind, Recording, TimeRange};
use crate::util::MapExt;
use indexmap::IndexMap;
use itertools::{Either, Itertools};
use nix::unistd::Pid;
use ordered_float::OrderedFloat;
use std::cmp::min;
use std::ops::{ControlFlow, Range};

pub struct PlacedProcess {
    pub pid: Pid,
    pub time_bound: TimeRange,

    pub row_offset: usize,
    pub row_height: usize,

    pub children: Vec<PlacedProcess>,
}

pub fn place_processes(rec: &Recording, include_threads: bool) -> Option<PlacedProcess> {
    // TODO what about orphans?
    rec.root_pid.and_then(|root_pid| {
        let mut cache = TimeCache::new();
        place_process(rec, include_threads, &mut cache, root_pid)
    })
}

impl PlacedProcess {
    pub fn visit<R>(
        &self,
        mut f_before: impl FnMut(&PlacedProcess, usize) -> ControlFlow<(), R>,
        mut f_after: impl FnMut(&PlacedProcess, usize, R),
    ) {
        fn visit_impl<R>(
            slf: &PlacedProcess,
            offset_start: usize,
            f_before: &mut impl FnMut(&PlacedProcess, usize) -> ControlFlow<(), R>,
            f_after: &mut impl FnMut(&PlacedProcess, usize, R),
        ) {
            let offset = offset_start + slf.row_offset;
            let r = match f_before(slf, offset) {
                ControlFlow::Continue(r) => r,
                ControlFlow::Break(()) => return,
            };
            for child in &slf.children {
                visit_impl(child, offset, f_before, f_after);
            }
            f_after(slf, offset, r);
        }

        visit_impl(self, 0, &mut f_before, &mut f_after);
    }
}

fn place_process(rec: &Recording, include_threads: bool, cache: &mut TimeCache, pid: Pid) -> Option<PlacedProcess> {
    let info = rec.processes.get(&pid)?;

    // filter/flatten children
    let children = if include_threads {
        Either::Left(info.children.iter().map(|&(_, c)| c))
    } else {
        let mut children = vec![];
        rec.for_each_process_child(pid, &mut |kind, child_pid| {
            match kind {
                ProcessKind::Process => children.push(child_pid),
                ProcessKind::Thread => { /* skip threads */ }
            }
        });
        Either::Right(children.into_iter())
    };

    // collect all relevant time points and the processes that start/end that happen at those times
    let mut time_to_events: IndexMap<OrderedFloat<f32>, (Vec<Pid>, Vec<Pid>)> = IndexMap::new();
    for c in children {
        let cb = process_time_bound(rec, cache, c);
        if Some(cb.start) == cb.end {
            // TODO can we leave these in? they're tricky because they start and stop in the same cycle
            continue;
        }
        time_to_events.entry(OrderedFloat(cb.start)).or_default().0.push(c);
        if let Some(cb_end) = cb.end {
            time_to_events.entry(OrderedFloat(cb_end)).or_default().1.push(c);
        }
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

    for (children_start, children_end) in sorted_events {
        // handle child ends (first to allow immediately reusing rows)
        for child in children_end {
            if let Some(range) = children_active.swap_remove(&child) {
                free.release(range)
            }
        }

        // handle child starts
        for child in children_start {
            if let Some(mut child_placed) = place_process(rec, include_threads, cache, child) {
                assert_eq!(child_placed.row_offset, 0);

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
        time_bound: process_time_bound(rec, cache, pid),
        row_offset: 0,
        row_height: 1 + free.len(),
        children: placed_children,
    })
}

type TimeCache = IndexMap<Pid, TimeRange>;

fn process_time_bound(rec: &Recording, cache: &mut TimeCache, pid: Pid) -> TimeRange {
    if let Some(res) = cache.get(&pid) {
        return res.clone();
    }

    let mut start = f32::MAX;
    let mut end = Some(f32::MIN);

    let mut join_range = |range: TimeRange| {
        start = start.min(range.start);
        end = match (end, range.end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (_, None) | (None, _) => None,
        };
    };

    if let Some(info) = rec.processes.get(&pid) {
        join_range(info.time);
        for exec in &info.execs {
            join_range(TimeRange {
                start: exec.time,
                end: Some(exec.time),
            });
        }
        for &(_, c) in &info.children {
            join_range(process_time_bound(rec, cache, c));
        }
    }

    let res = TimeRange { start, end };
    cache.insert_first(pid, res);
    res
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
