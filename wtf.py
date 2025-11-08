import argparse
import math
import os
import subprocess
import sys
from dataclasses import dataclass
from typing import List, Dict, Optional, Callable

from PyQt5.QtCore import QRectF, Qt, QPoint
from PyQt5.QtWidgets import QApplication, QGraphicsScene, QGraphicsView


class Processes:
    # final results
    root: Optional["ProcessInfo"]
    time_start_min: float
    time_end_max: float

    # intermediate mappings
    processes: Dict[int, "ProcessInfo"]
    parents: Dict[int, int]

    def __init__(self):
        self.root = None
        self.time_start_min = math.inf
        self.time_end_max = -math.inf

        self.processes = {}
        self.parents = {}


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

        self.prev_mouse_hold_pos: Optional[QPoint] = None

    def mousePressEvent(self, event):
        if event.button() == Qt.LeftButton:
            self.prev_mouse_hold_pos = event.pos()
        else:
            self.prev_mouse_hold_pos = None

        super().mousePressEvent(event)

    def mouseReleaseEvent(self, event):
        self.prev_mouse_hold_pos = None

        super().mouseReleaseEvent(event)

    def mouseMoveEvent(self, event):
        if self.prev_mouse_hold_pos is not None:
            delta = self.mapToScene(event.pos()) - self.mapToScene(self.prev_mouse_hold_pos)
            print(event.pos(), self.prev_mouse_hold_pos, delta)
            self.translate(delta.x(), delta.y())
            self.prev_mouse_hold_pos = event.pos()

        super().mouseMoveEvent(event)


class ProcessTreeScene(QGraphicsScene):
    def __init__(self, processes: Processes):
        super().__init__()

        y = 0
        H = 20
        W = 100

        def f(info: ProcessInfo):
            nonlocal y
            r = QRectF(W * info.time_start, y, W * (info.time_end - info.time_start), H)
            self.addRect(r)
            y += H

            for c in info.children:
                f(c)

        f(processes.root)


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

    scene = ProcessTreeScene(processes)
    # view = QGraphicsView(scene)
    view = ProcessTreeView(processes)
    view.show()

    app.exec_()

    sys.exit(exit_code)


if __name__ == "__main__":
    main()
