#!/usr/bin/env python3
"""Runs `run.py ARGS...` as a measurement (LB14): under the machine's exclusive measure lock, and on
Linux in a user service of its own.

An agent's command runs in a cgroup at the least CPU weight (docs/priority.md), below whatever else
the machine is doing: a timed run should have the default weight instead, with builds yielding to it
(theirs run at the least weight, nice 19). The service's memory is capped at what is available when
it starts, so a fleet can never take memory that wasn't free. Standard library only.

    measure.py ARGS...    what run.py takes (smoke, pilot, knee, sweep)
"""

import os
import platform
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
LOCK = os.path.join(os.path.expanduser("~"), ".cache", "kiln-lb", "bench-measure.lock")


def available_kb():
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable:"):
                return int(line.split()[1])
    raise SystemExit("no MemAvailable in /proc/meminfo")


def main(argv):
    locked = [sys.executable, os.path.join(HERE, "locked.py"), "--exclusive", LOCK, "--", sys.executable, os.path.join(HERE, "run.py")] + argv
    if platform.system() == "Linux" and shutil.which("systemd-run"):
        cmd = ["systemd-run", "--user", "--collect", "--wait", "--pipe", "--quiet", "-p", "MemoryMax=%dK" % available_kb(), "--working-directory=" + os.getcwd(), "-E", "PATH=" + os.environ["PATH"], "-E", "HOME=" + os.environ["HOME"]] + locked
    else:
        cmd = locked
    os.execvp(cmd[0], cmd)


if __name__ == "__main__":
    main(sys.argv[1:])
