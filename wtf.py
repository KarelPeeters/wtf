import argparse
import os
import subprocess
import sys
from dataclasses import dataclass
from typing import List, Dict, Optional


@dataclass
class ProcessInfo:
    # TODO is pid unique enough?
    # TODO args
    pid: int
    command: str
    time_start: float
    time_end: Optional[float]
    children: List[int]


def handle_strace_line(processes: Dict[int, ProcessInfo], s: str):
    # split input
    pid_str, time_str, rest = s.split(" ", 2)
    pid = int(pid_str)
    time = float(time_str)
    rest = rest.rstrip()

    # first syscall in new process: execve(at)
    if rest.startswith("execve"):
        if pid in processes:
            print("Error: exec for existing pid", pid)
            return

        info = ProcessInfo(pid=pid, command=rest, time_start=time, time_end=None, children=[])
        processes[pid] = info

    # parent spawning child process: clone/fork/vfork
    elif rest.startswith("clone") or rest.startswith("fork") or rest.startswith("vfork"):
        _, child_pid_str = rest.rsplit("=", 1)
        child_pid = int(child_pid_str.strip())

        if pid not in processes:
            print("Error: parent pid not found for fork", pid)
            return
        processes[pid].children.append(child_pid)

    # process ending
    elif rest.startswith("exit") or rest.startswith("exit_group"):
        if pid not in processes:
            print("Error: exiting pid not found", pid)
            return
        if processes[pid].time_end is not None:
            print("Error: pid already exited", pid)
            return

        processes[pid].time_end = time

    # ignored
    elif rest.startswith("wait") or rest.startswith("+++") or rest.startswith("<...") or rest.startswith("---"):
        pass

    else:
        print("Warning: unhandled strace line:", rest)


def print_processes(processes: Dict[int, ProcessInfo]):
    seen = set()

    def f(curr_pid: int, curr_indent: int):
        seen.add(curr_pid)
        print("  " * curr_indent + str(processes[curr_pid]))
        for c in processes[curr_pid].children:
            f(c, curr_indent + 1)

    for p in processes:
        if p in seen:
            continue
        f(p, 0)


def run(command: List[str]) -> int:
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

    # parse strace output, collecting info
    processes: Dict[int, ProcessInfo] = {}
    with os.fdopen(rx) as frx, open("log.txt", "w") as log_file:
        for line in frx:
            print(line, end='', file=log_file)
            handle_strace_line(processes, line)

    print_processes(processes)

    # the rx pipe has been closed because strace has exited,
    #   now just get the final exit code
    return strace_process.wait()


def main():
    parser = argparse.ArgumentParser(prog="wtf")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    command: List[str] = args.command
    if command and command[0] == "--":
        command = command[1:]

    if not command:
        parser.error("Missing command")

    sys.exit(run(command))


if __name__ == "__main__":
    main()
