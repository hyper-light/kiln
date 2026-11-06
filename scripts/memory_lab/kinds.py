#!/usr/bin/env python3
"""A process's resident memory by kind (goal 11, plan 01a1a87b2e56 step 5): what each of its mappings holds, from
/proc/<pid>/smaps, so a lab says how much of the server's anonymous memory the allocator holds, how much the threads'
stacks, turso's arenas and the C libraries' malloc, beside jemalloc's own word for its kinds
(`kiln_memory_allocator_bytes`, whose `resident` counts every page of its extents and metadata, written or not: in
g9p16, main's tree, 169 MB of it beside 138 MB of the process's anonymous memory in all).

    python3 -I kinds.py <pid>

One line a kind, in this order, then `total` (every mapping's, the process's Rss as smaps_rollup says it):
    <kind> rss_kb=<resident> anon_kb=<of it, the process's own pages> huge_kb=<of it, in transparent huge pages>
    maps=<mappings>

The kinds:
    binary-code   the executable's mappings with execute permission: its code (the file's own pages)
    binary-ro     the executable's read-only mappings: read-only data, unwind tables
    binary-rw     the executable's writable mappings: relocated read-only data and .data (a written page of a private
                  mapping is the process's own)
    files         every other file's mappings (a glibc build's libraries)
    stacks        threads' stacks: an anonymous mapping with guard pages in it (VmFlags `gu`: glibc's stacks on a kernel
                  with MADV_GUARD_INSTALL, adjacent ones merged into one mapping); an anonymous mapping of at least
                  STACK_LEAST just above an inaccessible one of at most GUARD_MOST (the guard musl's pthread_create
                  maps below a stack), and where kiln advised that stack `MADV_NOHUGEPAGE` (`nh`), the unadvised
                  mapping of at most TOP_MOST just above it that continues it (the thread's control block and TLS,
                  split from it by the advice); and the main thread's [stack]
    thp-advised   anonymous mappings advised `MADV_HUGEPAGE` (VmFlags `hg`): turso's buffer pools' arenas
    allocator     anonymous mappings advised `MADV_NOHUGEPAGE` (VmFlags `nh`) that aren't stacks: jemalloc's, on a
                  kernel that gives huge pages (`always` or `madvise`; `thp:never` has jemalloc mark each mapping it
                  makes). On a kernel set to `never` they count as other-anon
    heap          [heap]: the C library's brk heap (glibc's main arena; musl's first groups)
    other-anon    every other anonymous mapping: the C libraries' malloc (glibc's other arenas, musl's groups), and
                  mappings made directly (a thread's signal stack among them)
    kernel        [vdso], [vvar] and the like

Reading another process's smaps takes ptrace's read access to it: the same user.
"""
from __future__ import annotations

import os
import sys

# A thread's guard: glibc's and musl's default is a page, kiln's threads take the default; anything inaccessible up
# to this is taken for a guard.
GUARD_MOST = 1 << 20
# The least a thread's stack is: musl's default (128 KiB) past a signal stack's (SIGSTKSZ's 8 KiB and Rust's own of
# a few pages), which lies above a guard page too and isn't a thread's stack.
STACK_LEAST = 128 << 10
# The most of a stack's mapping that lies above the part its C library reports as the stack: its thread control block
# and static TLS, a few pages.
TOP_MOST = 1 << 20

KINDS = ["binary-code", "binary-ro", "binary-rw", "files", "stacks", "thp-advised", "allocator", "heap", "other-anon", "kernel"]


def mappings(pid: int) -> list[dict]:
    """Each of the process's mappings as smaps has it: its range, permissions, path, Rss, Anonymous and
    AnonHugePages (KiB) and VmFlags."""
    out: list[dict] = []
    with open(f"/proc/{pid}/smaps") as f:
        for line in f:
            head = line.split(None, 5)
            if len(head) >= 5 and "-" in head[0] and ":" not in head[0]:
                lo, hi = (int(x, 16) for x in head[0].split("-"))
                path = head[5].rstrip("\n") if len(head) > 5 else ""
                out.append(dict(lo=lo, hi=hi, perms=head[1], path=path, rss=0, anon=0, huge=0, flags=set()))
                continue
            if not out:
                continue
            key, _, value = line.partition(":")
            if key == "Rss":
                out[-1]["rss"] = int(value.split()[0])
            elif key == "Anonymous":
                out[-1]["anon"] = int(value.split()[0])
            elif key == "AnonHugePages":
                out[-1]["huge"] = int(value.split()[0])
            elif key == "VmFlags":
                out[-1]["flags"] = set(value.split())
    return out


def anonymous(m: dict) -> bool:
    return m["path"] == ""


def classify(maps: list[dict], exe: str) -> list[str]:
    """Each mapping's kind, in the order of `maps` (address order, as smaps lists them)."""
    kinds = ["" for _ in maps]
    for i, m in enumerate(maps):
        if kinds[i]:
            continue
        below = maps[i - 1] if i > 0 else None
        if m["path"] == exe:
            kinds[i] = "binary-code" if "x" in m["perms"] else ("binary-rw" if "w" in m["perms"] else "binary-ro")
        elif m["path"] == "[stack]":
            kinds[i] = "stacks"
        elif m["path"] == "[heap]":
            kinds[i] = "heap"
        elif m["path"].startswith("["):
            kinds[i] = "kernel"
        elif not anonymous(m):
            kinds[i] = "files"
        elif "gu" in m["flags"]:
            # Guard pages installed in the mapping itself (MADV_GUARD_INSTALL, Linux 6.13): glibc's thread stacks
            # where the kernel has them (VmFlags `gu`), adjacent ones merged into one mapping.
            kinds[i] = "stacks"
        elif (
            below is not None
            and anonymous(below)
            and below["perms"].startswith("---")
            and below["hi"] == m["lo"]
            and below["hi"] - below["lo"] <= GUARD_MOST
            and m["hi"] - m["lo"] >= STACK_LEAST
            and m["perms"].startswith("rw")
        ):
            kinds[i] = "stacks"
            # A stack kiln advised (`nh`) was split from the rest of its thread's mapping: its control block and TLS
            # just above, unadvised. An unadvised stack's mapping holds them already.
            above = maps[i + 1] if i + 1 < len(maps) else None
            if (
                "nh" in m["flags"]
                and above is not None
                and anonymous(above)
                and above["lo"] == m["hi"]
                and above["hi"] - above["lo"] <= TOP_MOST
                and above["perms"].startswith("rw")
                and "nh" not in above["flags"]
            ):
                kinds[i + 1] = "stacks"
        elif "hg" in m["flags"]:
            kinds[i] = "thp-advised"
        elif "nh" in m["flags"]:
            kinds[i] = "allocator"
        else:
            kinds[i] = "other-anon"
    return kinds


def tally(maps: list[dict], kinds: list[str]) -> dict[str, dict]:
    rows = {k: dict(rss=0, anon=0, huge=0, maps=0) for k in KINDS}
    for m, k in zip(maps, kinds):
        r = rows[k]
        r["rss"] += m["rss"]
        r["anon"] += m["anon"]
        r["huge"] += m["huge"]
        r["maps"] += 1
    return rows


def line(name: str, r: dict) -> str:
    return f"{name} rss_kb={r['rss']} anon_kb={r['anon']} huge_kb={r['huge']} maps={r['maps']}"


def main() -> int:
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        print("usage: kinds.py <pid>", file=sys.stderr)
        return 2
    pid = int(sys.argv[1])
    try:
        exe = os.readlink(f"/proc/{pid}/exe")
        maps = mappings(pid)
    except OSError as e:
        print(f"kinds.py: process {pid}: {e}", file=sys.stderr)
        return 1
    rows = tally(maps, classify(maps, exe))
    for k in KINDS:
        print(line(k, rows[k]))
    total = {f: sum(r[f] for r in rows.values()) for f in ("rss", "anon", "huge", "maps")}
    print(line("total", total))
    return 0


if __name__ == "__main__":
    sys.exit(main())
