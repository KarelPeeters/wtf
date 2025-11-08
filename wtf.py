import argparse
import math
import os
import subprocess
import sys
from dataclasses import dataclass
from typing import List, Dict, Optional, Callable

from PyQt5.QtCore import QRectF
from PyQt5.QtWidgets import QApplication, QGraphicsScene, QGraphicsView


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


class ProcessTreeScene(QGraphicsScene):
    def __init__(self, processes: Processes):
        super().__init__()

        H = 20
        WF = 200

        def f(info: ProcessInfo, start_y: float) -> int:
            # TODO make rect actually contain the children?
            # TODO color by command
            r = QRectF(
                WF * info.time_start,
                start_y,
                WF * (info.time_end - info.time_start),
                H
            )
            self.addRect(r)

            # TODO properly "schedule" children, don't just stack them
            curr_steps = 1
            for c in info.children:
                curr_steps += f(c, start_y=start_y + curr_steps * H)
            return curr_steps

        f(processes.root, start_y=0)


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
