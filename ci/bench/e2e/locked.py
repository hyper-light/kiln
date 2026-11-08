#!/usr/bin/env python3
"""Runs one command holding the bench machine's measure lock (LB14, ci/bench/e2e).

    locked.py --exclusive|--shared LOCKFILE -- COMMAND [ARG...]

Measurements hold the lock exclusively, so no other bench measures beside them (the kernel bench in
ci/bench/run.py takes the same file); builds hold it shared, so no measurement, theirs or ours, runs
while one of ours builds. What it waited and the load average before and after go to stderr as one
JSON line each. Standard library only.
"""

import fcntl
import json
import os
import subprocess
import sys
import time


def main(argv):
    if len(argv) < 4 or argv[0] not in ("--exclusive", "--shared") or argv[2] != "--":
        print(__doc__.strip(), file=sys.stderr)
        return 2
    mode, path, command = argv[0], os.path.expanduser(argv[1]), argv[3:]
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    t0 = time.monotonic()
    fcntl.flock(fd, fcntl.LOCK_EX if mode == "--exclusive" else fcntl.LOCK_SH)
    held = {"lock": path, "mode": mode[2:], "waited_s": round(time.monotonic() - t0, 3), "load_before": os.getloadavg(), "at": time.time()}
    print(json.dumps(held), file=sys.stderr, flush=True)
    try:
        rc = subprocess.call(command)
    finally:
        print(json.dumps({"lock": path, "load_after": os.getloadavg(), "at": time.time()}), file=sys.stderr, flush=True)
        os.close(fd)
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
