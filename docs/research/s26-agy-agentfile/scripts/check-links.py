#!/usr/bin/env python3
"""Checks the step's docs name only paths the repository holds.

Every relative Markdown link in each FILE must name a file or directory that git tracks at
REV, and so must every PATH given after --path (paths as the Antigravity note in
docs/agents.md writes them, from the repository's root).

    check-links.py REPO REV FILE... [--path PATH...]
"""
import os
import re
import subprocess
import sys


def main(argv):
    repo, rev, rest = argv[1], argv[2], argv[3:]
    files, paths = rest, []
    if "--path" in rest:
        i = rest.index("--path")
        files, paths = rest[:i], rest[i + 1:]
    listing = subprocess.run(["git", "-C", repo, "ls-tree", "-r", "--name-only", rev],
                             capture_output=True, text=True, check=True).stdout.splitlines()
    tracked = set(listing)
    dirs = {"/".join(t.split("/")[:i]) for t in listing for i in range(1, t.count("/") + 1)}

    def held(path):
        path = os.path.normpath(path)
        return path in tracked or path in dirs

    checked = missing = 0
    for f in files:
        text = subprocess.run(["git", "-C", repo, "show", f"{rev}:{f}"],
                              capture_output=True, text=True, check=True).stdout
        for m in re.finditer(r"\]\(([^)\s]+)\)", text):
            target = m.group(1)
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("#"):
                continue
            path = os.path.join(os.path.dirname(f), target.split("#")[0])
            ok = held(path)
            checked += 1
            missing += not ok
            print(f"{'ok' if ok else 'MISSING'}  {f}: ({target}) -> {os.path.normpath(path)}")
    for p in paths:
        ok = held(p)
        checked += 1
        missing += not ok
        print(f"{'ok' if ok else 'MISSING'}  named: {p}")
    print(f"{checked} paths checked at {rev}, {missing} missing")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
