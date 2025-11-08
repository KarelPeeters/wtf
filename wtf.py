import argparse
import math
import os
import subprocess
import sys
from dataclasses import dataclass
from typing import List, Dict, Optional, Callable, Tuple

from PyQt5.QtCore import QRectF, Qt
from PyQt5.QtGui import QPen, QColor, QBrush
from PyQt5.QtWidgets import QApplication, QGraphicsScene, QGraphicsView, QGraphicsItem


class Processes:
    # final results
    root: Optional["ProcessInfo"]
    time_start_min: float
    time_end_max: float

    # intermediate mappings
    processes: Dict[int, "ProcessInfo"]
    parents: Dict[int, int]
    unfinished: Dict[int, str]

    def __init__(self):
        self.root = None
        self.time_start_min = math.inf
        self.time_end_max = -math.inf

        self.processes = {}
        self.parents = {}
        self.unfinished = {}


@dataclass
class ProcessInfo:
    # TODO is pid unique enough?
    # TODO args
    pid: int
    command: str
    time_start: float
    time_end: Optional[float]
    children: List["ProcessInfo"]


def handle_strace_line(processes: Processes, s: str):
    # split input
    pid_str, time_str, rest = s.split(" ", 2)
    pid = int(pid_str)
    time = float(time_str)
    rest = rest.rstrip()

    # re-join unfinished/resumed syscalls
    UNFINISHED_SUFFIX = " <unfinished ...>"
    RESUMED_PREFIX_START = "<... "
    RESUMED_PREFIX_END = " resumed>"

    if rest.endswith(UNFINISHED_SUFFIX):
        assert pid not in processes.unfinished
        processes.unfinished[pid] = rest[:-len(UNFINISHED_SUFFIX)]
        return

    if rest.startswith(RESUMED_PREFIX_START):
        pos = rest.index(RESUMED_PREFIX_END)
        prev = processes.unfinished.pop(pid)
        rest = prev + rest[pos + len(RESUMED_PREFIX_END):]

    # parent spawning child process
    if rest.startswith("clone(") or rest.startswith("fork(") or rest.startswith("vfork("):
        _, child_pid_str = rest.rsplit("=", 1)
        child_pid = int(child_pid_str.strip())
        processes.parents[child_pid] = pid

    # first syscall in new process
    elif rest.startswith("execve(") or rest.startswith("execat("):
        info = ProcessInfo(pid=pid, command=rest, time_start=time, time_end=None, children=[])
        processes.processes[pid] = info
        processes.time_start_min = min(processes.time_start_min, time)

        if processes.root is None:
            processes.root = info
        else:
            parent_pid = processes.parents[pid]
            processes.processes[parent_pid].children.append(info)

    # process ending
    elif rest.startswith("exit(") or rest.startswith("exit_group("):
        processes.processes[pid].time_end = time
        processes.time_end_max = max(processes.time_end_max, time)

    # ignored
    elif any(rest.startswith(x) for x in ("wait3(", "wait4(", "+++", "<...", "---")):
        pass
    else:
        print("Warning: unhandled strace line:", rest)


def print_processes(root: ProcessInfo):
    def f(proc: ProcessInfo, curr_indent: int):
        print("  " * curr_indent + str(proc))
        for c in proc.children:
            f(c, curr_indent + 1)

    f(root, 0)


# TODO add callback?
def run_strace(command: List[str], callback: Callable[[str], None]) -> int:
    # start strace command
    # TODO create large buffer to avoid latency?
    rx, tx = os.pipe()
    strace_command = [
        "strace",
        "--follow-forks",
        "-e",
        "trace=process",
        "--always-show-pid",
        "--timestamps=unix,us",
        f"--output=/dev/fd/{tx}",
        "--",
        *command
    ]
    strace_process = subprocess.Popen(strace_command, pass_fds=[tx])
    os.close(tx)

    # handle strace output
    with os.fdopen(rx) as frx, open("log.txt", "w") as log_file:
        for line in frx:
            # TODO stop logging
            print(line, end='', file=log_file)
            callback(line)

    # the rx pipe has been closed because strace has exited,
    #   now just get the final exit code
    return strace_process.wait()


class ProcessTreeView(QGraphicsView):
    def __init__(self, processes: Processes):
        scene = ProcessTreeScene(processes)
        super().__init__(scene)

        self.setDragMode(QGraphicsView.DragMode.ScrollHandDrag)

    def enterEvent(self, event):
        # stop annoying default cursor change
        super().enterEvent(event)
        self.viewport().setCursor(Qt.CursorShape.ArrowCursor)

    def mouseReleaseEvent(self, event):
        # stop annoying default cursor change
        super().mouseReleaseEvent(event)
        self.viewport().setCursor(Qt.CursorShape.ArrowCursor)

    def wheelEvent(self, event):
        self.scale(math.exp(event.angleDelta().y() / 360), 1)


@dataclass
class PlacedProcess:
    info: ProcessInfo

    offset: int
    height: int
    children: List["PlacedProcess"]


class FreeList:
    def __init__(self):
        self.free_mask: List[bool] = []

    def __len__(self):
        return len(self.free_mask)

    def _allocate_find_start(self, length: int) -> int:
        # try to find an existing empty spot
        for s in range(len(self.free_mask)):
            if all(s + i < len(self.free_mask) and self.free_mask[s + i] for i in range(length)):
                return s

        # try to find free spaces at the end
        for i in reversed(range(len(self.free_mask))):
            if self.free_mask[i]:
                return i + 1

        # start at the end
        return len(self.free_mask)

    def allocate(self, length: int) -> int:
        s = self._allocate_find_start(length)

        while len(self.free_mask) < s + length:
            self.free_mask.append(True)

        for i in range(s, s + length):
            assert self.free_mask[i]
            self.free_mask[i] = False

        return s

    def release(self, start: int, length: int):
        for i in range(start, start + length):
            assert not self.free_mask[i]
            self.free_mask[i] = True


def place_process(parent: ProcessInfo) -> PlacedProcess:
    # collect all relevant time points and map them to the processes that start/end at that time
    time_to_procs: Dict[float, Tuple[List[ProcessInfo], List[ProcessInfo]]] = {}
    for c in parent.children:
        time_to_procs.setdefault(c.time_start, ([], []))[0].append(c)
        time_to_procs.setdefault(c.time_end, ([], []))[1].append(c)
    times_sorted = sorted(time_to_procs.keys())

    # simulate time left to right
    free = FreeList()
    process_running: Dict[int, PlacedProcess] = {}
    placed_children = []

    for time in times_sorted:
        procs_start, procs_end = time_to_procs[time]

        # handle process ends (do this first to allow immediate reuse of space)
        for proc in procs_end:
            placed = process_running.pop(proc.pid)
            placed_children.append(placed)
            free.release(placed.offset, placed.height)

        # handle process starts
        for proc in procs_start:
            proc_placed = place_process(proc)

            assert proc_placed.offset == 0
            proc_placed.offset = free.allocate(proc_placed.height)

            process_running[proc.pid] = proc_placed
            placed_children.append(proc_placed)

    return PlacedProcess(info=parent, offset=0, height=1 + len(free), children=placed_children)


class ProcessTreeScene(QGraphicsScene):
    def __init__(self, processes: Processes):
        super().__init__()

        # TODO add command name
        # TODO color based on command?
        H = 20
        WF = 200

        def f(p: PlacedProcess, base: int, depth: int):
            start = base + p.offset

            rect = QRectF(
                WF * p.info.time_start,
                H * start,
                WF * (p.info.time_end - p.info.time_start),
                H * p.height
            )
            pen = QPen(QColor(0, 0, 0))
            pen.setCosmetic(True)
            brush = QBrush(QColor(255, 255, 255 - min(255, int(depth / 8 * 255))))

            self.addRect(rect, pen, brush)

            for c in p.children:
                f(p=c, base=start + 1, depth=depth + 1)

        placed = place_process(processes.root)
        f(placed, base=0, depth=0)


def main():
    parser = argparse.ArgumentParser(prog="wtf")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    command: List[str] = args.command
    if command and command[0] == "--":
        command = command[1:]

    if not command:
        parser.error("Missing command")

    processes = Processes()
    exit_code = run_strace(command, lambda s: handle_strace_line(processes, s))
    assert processes.root is not None
    print_processes(processes.root)

    # TODO start GUI thread
    app = QApplication([])
    # w = MainWindow(processes)
    # w.show()

    # view = QGraphicsView(scene)
    view = ProcessTreeView(processes)
    view.show()

    app.exec_()

    sys.exit(exit_code)


if __name__ == "__main__":
    main()
