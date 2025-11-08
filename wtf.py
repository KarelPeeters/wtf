#!/usr/bin/env python3

import argparse
import math
import os
import re
import subprocess
import time
from dataclasses import dataclass
from threading import Thread
from typing import List, Dict, Optional, Callable, Tuple

from PyQt5 import QtCore
from PyQt5.QtCore import QRectF, Qt, QPointF
from PyQt5.QtGui import QPen, QColor, QBrush, QWheelEvent, QFontMetrics
from PyQt5.QtWidgets import QApplication, QGraphicsScene, QGraphicsView


class Processes:
    # final results
    root: Optional["ProcessInfo"]
    time_min: float
    time_max: float

    # intermediate mappings
    processes: Dict[int, "ProcessInfo"]
    parents: Dict[int, int]
    unfinished: Dict[int, str]

    def __init__(self):
        self.root = None
        self.time_min = math.inf
        self.time_max = -math.inf

        self.processes = {}
        self.unfinished = {}

    def report_time(self, time: float):
        self.time_min = min(self.time_min, time)
        self.time_max = max(self.time_max, time)


@dataclass
class ProcessCommand:
    time: float
    path: str
    argv: List[str]


@dataclass
class ProcessInfo:
    # TODO is pid unique enough?
    pid: int
    parent_pid: Optional[int]

    time_start: float
    time_end: Optional[float]

    commands: List[ProcessCommand]
    children: List["ProcessInfo"]


PATTERN_STR = r"\"(?:\\x[0-9a-f]+)*\""
PATTERN_EXEC = rf"execve(?:at)?\((?P<path>{PATTERN_STR}), (?P<argv>\[(?:{PATTERN_STR}(?:, )?)*]), .*\) = .+"
REGEX_EXEC = re.compile(PATTERN_EXEC)

PATTERN_HEX = r"\\x([0-9a-f]+)"
REGEX_HEX = re.compile(PATTERN_HEX)


def unescape_hex(s: str) -> str:
    def f(m):
        return chr(int(m.group(1), 16))

    return re.subn(REGEX_HEX, f, s)[0]


def parse_hex_str(s: str) -> str:
    assert len(s) >= 2 and s[0] == "\"" and s[-1] == "\""
    s = s[1:-1]
    return unescape_hex(s)


def parse_hex_str_list(s: str) -> List[str]:
    assert len(s) >= 2 and s[0] == "[" and s[-1] == "]"
    s = s[1:-1]
    if not s:
        return []
    return [parse_hex_str(p.strip()) for p in (s.split(","))]


def handle_strace_line(processes: Processes, s: str):
    print("Handling strace line")

    # split input
    pid_str, time_str, rest = s.split(" ", 2)
    pid = int(pid_str)
    time = float(time_str)
    rest = rest.rstrip()

    processes.report_time(time)

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
    if rest.startswith("clone(") or rest.startswith("clone3(") or rest.startswith("fork(") or rest.startswith("vfork("):
        _, child_pid_str = rest.rsplit("=", 1)
        child_pid = int(child_pid_str.strip())

        info = ProcessInfo(pid=child_pid, parent_pid=pid, time_start=time, time_end=None, commands=[], children=[])
        processes.processes[child_pid] = info
        processes.processes[pid].children.append(info)

    # process starting a binary
    elif rest.startswith("execve(") or rest.startswith("execat("):
        m = REGEX_EXEC.fullmatch(rest)
        if not m:
            print("fail")
        cmd = ProcessCommand(
            time=time,
            path=parse_hex_str(m.group("path")),
            argv=parse_hex_str_list(m.group("argv"))
        )

        print(f"pid {pid} execve {cmd}")

        if pid in processes.processes:
            # exec of existing process
            info = processes.processes[pid]
        else:
            # initial exec for root process
            assert processes.root is None
            info = ProcessInfo(pid=pid, parent_pid=None, time_start=time, time_end=None, commands=[cmd], children=[])
            processes.processes[pid] = info
            processes.root = info

        info.commands.append(cmd)

    elif rest.startswith("exit(") or rest.startswith("exit_group("):
        processes.processes[pid].time_end = time

    # ignored
    elif any(rest.startswith(x) for x in ("wait3(", "wait4(", "tgkill(", "+++", "<...", "---")):
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
        f"--string-limit={2 ** 20}",
        "--strings-in-hex",
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


def place_process(processes: Processes, parent: ProcessInfo) -> PlacedProcess:
    # collect all relevant time points and map them to the processes that start/end at that time
    time_to_procs: Dict[float, Tuple[List[ProcessInfo], List[ProcessInfo]]] = {}
    for c in parent.children:
        c_time_end = c.time_end if c.time_end is not None else processes.time_max
        assert c_time_end >= c.time_start
        if c.time_start == c_time_end:
            continue

        time_to_procs.setdefault(c.time_start, ([], []))[0].append(c)
        time_to_procs.setdefault(c_time_end, ([], []))[1].append(c)
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
            proc_placed = place_process(processes, proc)

            assert proc_placed.offset == 0
            proc_placed.offset = free.allocate(proc_placed.height)

            process_running[proc.pid] = proc_placed
            placed_children.append(proc_placed)

    return PlacedProcess(info=parent, offset=0, height=1 + len(free), children=placed_children)


class ProcessTreeScene(QGraphicsScene):
    def __init__(self, processes: Processes, scale_horizontal: float, scale_vertical: float):
        super().__init__()

        # TODO add command name
        # TODO color based on command?
        # TODO get height/width of text
        H = 20 * scale_vertical
        WF = 200 * scale_horizontal

        metrics = QFontMetrics(self.font())
        font_height = metrics.height()

        def f(p: PlacedProcess, base: int, depth: int):
            start = base + p.offset

            # subtract start time from all positions to avoid 32-bit overflow, which causes issues in the scrollbars
            p_time_end = p.info.time_end if p.info.time_end is not None else processes.time_max

            p_width = WF * (p_time_end - p.info.time_start)
            rect = QRectF(
                WF * (p.info.time_start - processes.time_min),
                H * start,
                p_width,
                H * p.height
            )
            pen = QPen(QColor(0, 0, 0))
            brush = QBrush(QColor(255, 255, 255 - min(255, int(depth / 8 * 255))))
            self.addRect(rect, pen, brush)

            txt_str = "?"
            if p.info.commands:
                txt_str = p.info.commands[-1].path

            if H >= font_height and p_width >= metrics.width(txt_str):
                txt = self.addSimpleText(txt_str)
                txt.setPos(rect.topLeft())

            for c in p.children:
                f(p=c, base=start + 1, depth=depth + 1)

        if processes.root is not None:
            placed = place_process(processes, processes.root)
            f(placed, base=0, depth=0)


class ProcessTreeView(QGraphicsView):
    signal_processes_updated = QtCore.pyqtSignal()

    def __init__(self, processes: Processes):
        super().__init__()

        self.setDragMode(QGraphicsView.DragMode.ScrollHandDrag)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOn)
        self.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOn)

        self.scale_horizontal_linear = 0
        self.scale_vertical_linear = 0

        self.signal_processes_updated.connect(self.slot_processes_updated)

        self.processes = processes
        self.rebuild_scene()

    @QtCore.pyqtSlot()
    def slot_processes_updated(self):
        self.rebuild_scene()

    def rebuild_scene(self):
        # TODO benchmark if this is slow
        perf_start = time.perf_counter()
        scene = ProcessTreeScene(
            processes=self.processes,
            scale_horizontal=math.exp(self.scale_horizontal_linear),
            scale_vertical=math.exp(self.scale_vertical_linear),
        )
        print(f"Scene building took {time.perf_counter() - perf_start}s")
        self.setScene(scene)

    def enterEvent(self, event):
        # stop annoying default cursor change
        super().enterEvent(event)
        self.viewport().setCursor(Qt.CursorShape.ArrowCursor)

    def mouseReleaseEvent(self, event):
        # stop annoying default cursor change
        super().mouseReleaseEvent(event)
        self.viewport().setCursor(Qt.CursorShape.ArrowCursor)

    def keyReleaseEvent(self, event):
        if event.key() == Qt.Key_F:
            event.accept()
            self.scale_vertical_linear = 0
            self.scale_horizontal_linear = 0
            self.rebuild_scene()

    def wheelEvent(self, event):
        delta = QPointF(event.angleDelta()) / 360

        if event.modifiers() & Qt.ControlModifier:
            # horizontal zoom
            self.scale_horizontal_linear += delta.y()
            self.scale_vertical_linear += delta.x()
            self.rebuild_scene()

        elif event.modifiers() & Qt.AltModifier:
            # vertical zoom
            self.scale_vertical_linear += delta.y()
            self.scale_horizontal_linear += delta.x()
            self.rebuild_scene()

        elif event.modifiers() & Qt.ShiftModifier:
            # horizontal scroll
            # we need to edit the wheel event to remove the shift modifier, otherwise the scrollbar interprets it as
            #   "scroll an entire page"
            event_edited = QWheelEvent(
                event.pos(),
                event.globalPos(),
                event.pixelDelta(),
                event.angleDelta(),
                event.buttons(),
                event.modifiers() & ~Qt.ShiftModifier,
                event.phase(),
                event.inverted(),
                event.source(),
            )
            QApplication.sendEvent(self.horizontalScrollBar(), event_edited)
        else:
            # vertical scroll
            QApplication.sendEvent(self.verticalScrollBar(), event)

        event.accept()


def main():
    # parse args
    parser = argparse.ArgumentParser(prog="wtf")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    command: List[str] = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("Missing command")

    # create GUI
    app = QApplication([])
    processes = Processes()
    view = ProcessTreeView(processes)

    # start trace on secondary thread
    def strace_callback(s: str):
        handle_strace_line(processes, s)
        view.signal_processes_updated.emit()

    def thread_main():
        run_strace(command, strace_callback)

    strace_thread = Thread(target=thread_main)
    strace_thread.start()

    view.show()
    app.exec_()

    strace_thread.join()


if __name__ == "__main__":
    main()
