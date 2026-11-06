#!/usr/bin/env python3
"""What the BOLT A/B and its smoke say (goal 11, plan 01a1a87b2e56 step 5; scripts/memory_lab/bolt-lab.sh).

    python3 -I bolt-read.py ab <label-a> <label-b> <run dir>...
    python3 -I bolt-read.py smoke <run dir> <label-a> <label-b>

A run dir is kiln-lab's --out: run.json (every round's numbers, by build), and each round's hook folder,
rounds/<round>-<label>.hook, where bolt-hook.sh wrote pages.txt (the binary's resident KiB by mapping) and sections.txt
(sections.py: each section's resident KiB and 64 KiB windows touched) as the window ended.

`ab` writes, for each run dir, in Markdown:
- the numbers the A/B is read by, each the median of a build's rounds: the server's CPU per request, the shards' busy
  share, its resident memory and the binary's file pages (rss_file_mb), and each class's latency; kiln-lab's own
  report.md says each with its noise, B against A, and is the one to judge by;
- the binary's pages by section, each the median of a build's rounds: its code (every .text section, BOLT's hot .text,
  its split-out .text.cold and the .bolt.org.text it left where the linker put it, together and each), the read-only
  data, the unwinding tables, the data (each with what BOLT made of it: the original it renamed .bolt.org.<name>, and
  <name>.cold, as BOLT renames .rodata, .eh_frame, .gcc_except_table and .eh_frame_hdr and splits .rodata's cold
  part out), and all of it: resident KiB, 4 KiB pages, and 64 KiB windows of how many.

`smoke` says whether the BOLTed build ran its rounds as the release did: each build's every round done, none failed,
and B's errors (a status of 400 or more, or no answer, summed over the classes) no more than A's. Exit 1 otherwise.
"""
from __future__ import annotations

import json
import re
import statistics
import sys
from pathlib import Path

LINE = re.compile(r"^(\S+) size_kb=(\d+) resident_kb=(\d+) file_kb=(\d+) anon_kb=(\d+) windows=(\d+) of=(\d+)$")
# The sections that hold code: the linker's .text, and BOLT's .text (the hot code), .text.cold (what splitting moved out)
# and .bolt.org.text (the original .text, holding what the profile didn't see, in lite mode).
CODE = re.compile(r"^\.(text|bolt\.org\.text)")
DATA = [".rodata", ".data.rel.ro", ".data", ".eh_frame", ".eh_frame_hdr", ".gcc_except_table"]
NUMBERS = [
    "server.cpu_ms_per_request",
    "shards.busy_share",
    "shards.worst_busy_share",
    "server.rss_mb",
    "server.rss_file_mb",
    "server.rss_anon_mb",
]
CLASSES = ["page", "api_read", "api_write", "websocket", "attach"]
SHARDS = ["shards.delay_p50_us", "shards.delay_p99_us", "shards.delay_p999_us", "shards.worst_delay_p999_us"]


def rounds_of(run: Path) -> list[dict]:
    data = json.loads((run / "run.json").read_text())
    return data.get("rounds", [])


def sections_of(path: Path) -> dict[str, tuple[int, int, int]]:
    """A sections.txt: each section's (resident KiB, windows touched, windows)."""
    out = {}
    for line in path.read_text().splitlines():
        m = LINE.match(line.strip())
        if m:
            out[m.group(1)] = (int(m.group(3)), int(m.group(6)), int(m.group(7)))
    return out


def med(xs: list[float]) -> float | None:
    return statistics.median(xs) if xs else None


def fmt(x: float | None, digits: int = 3) -> str:
    return "-" if x is None else f"{x:,.{digits}f}"


def ratio(a: float | None, b: float | None) -> str:
    return "-" if a in (None, 0) or b is None else f"{b / a:.3f}"


def numbers_table(rounds: list[dict], a: str, b: str) -> list[str]:
    keys = list(NUMBERS) + [f"{c}.{q}" for c in CLASSES for q in ("p50_ms", "p99_ms", "p999_ms")] + SHARDS
    out = [f"| number (median of rounds) | {a} | {b} | {b}/{a} |", "| --- | --- | --- | --- |"]
    for k in keys:
        va = med([r["numbers"][k] for r in rounds if r.get("build") == a and k in r.get("numbers", {})])
        vb = med([r["numbers"][k] for r in rounds if r.get("build") == b and k in r.get("numbers", {})])
        if va is None and vb is None:
            continue
        out.append(f"| {k} | {fmt(va)} | {fmt(vb)} | {ratio(va, vb)} |")
    return out


def sections_table(run: Path, rounds: list[dict], a: str, b: str) -> list[str]:
    by: dict[str, list[dict[str, tuple[int, int, int]]]] = {a: [], b: []}
    for r in rounds:
        label = r.get("build")
        if label not in by:
            continue
        f = run / "rounds" / f"{int(r.get('index', 0)):03d}-{label}.hook" / "sections.txt"
        if f.is_file():
            by[label].append(sections_of(f))
    names: list[str] = []
    for each in by.values():
        for s in each:
            names += [n for n in s if CODE.match(n) and n not in names]
    # A data section with what BOLT made of it: its original renamed .bolt.org<name>, and <name>.cold.
    data = [(f"{d} (and .bolt.org{d}, {d}.cold)", [d, f".bolt.org{d}", f"{d}.cold"]) for d in DATA]
    rows = [("code (every .text section)", names)] + [(n, [n]) for n in sorted(names)] + data + [("all of the binary", ["total"])]
    out = [
        f"rounds with sections.txt: {a} {len(by[a])}, {b} {len(by[b])}",
        "",
        f"| section (median of rounds) | {a} resident KiB | {a} 4 KiB pages | {a} 64 KiB windows | {b} resident KiB | {b} 4 KiB pages | {b} 64 KiB windows |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for title, parts in rows:
        cells = []
        for label in (a, b):
            res = [sum(s[p][0] for p in parts if p in s) for s in by[label] if any(p in s for p in parts)]
            win = [sum(s[p][1] for p in parts if p in s) for s in by[label] if any(p in s for p in parts)]
            of = [sum(s[p][2] for p in parts if p in s) for s in by[label] if any(p in s for p in parts)]
            r, w, o = med(res), med(win), med(of)
            cells += [fmt(r, 0), fmt(None if r is None else r / 4, 0), "-" if w is None else f"{w:,.0f} of {o:,.0f}"]
        if all(c == "-" for c in cells[0::3]):
            continue
        out.append(f"| {title} | " + " | ".join(cells) + " |")
    return out


def ab(a: str, b: str, runs: list[str]) -> int:
    worst = 0
    for name in runs:
        run = Path(name)
        print(f"## {run}\n")
        try:
            rounds = rounds_of(run)
        except (OSError, ValueError) as e:
            print(f"no run.json: {e}\n")
            worst = 1
            continue
        print(f"rounds: {a} {sum(1 for r in rounds if r.get('build') == a)}, {b} {sum(1 for r in rounds if r.get('build') == b)}")
        print("(medians; each number's noise, B against A: kiln-lab's report.md beside run.json)\n")
        print("\n".join(numbers_table(rounds, a, b)))
        print()
        print("\n".join(sections_table(run, rounds, a, b)))
        print()
    return worst


def smoke(run: Path, a: str, b: str) -> int:
    try:
        data = json.loads((run / "run.json").read_text())
    except (OSError, ValueError) as e:
        print(f"smoke: {run}: no run.json: {e}")
        return 1
    rounds = data.get("rounds", [])
    failures = data.get("failures", [])
    errs = {}
    for label in (a, b):
        mine = [r for r in rounds if r.get("build") == label]
        if not mine:
            print(f"smoke: no round of {label} done ({'; '.join(failures) or 'no failure said'})")
            return 1
        errs[label] = sum(v for r in mine for k, v in r.get("numbers", {}).items() if k.endswith(".errors"))
    if failures:
        print(f"smoke: rounds failed: {'; '.join(failures)}")
        return 1
    if errs[b] > errs[a]:
        print(f"smoke: {b} answered {errs[b]:.0f} requests with errors, {a} {errs[a]:.0f}")
        return 1
    print(f"smoke: {a} and {b} each ran their rounds, errors {errs[a]:.0f} and {errs[b]:.0f}")
    return 0


def main() -> int:
    args = sys.argv[1:]
    if len(args) >= 4 and args[0] == "ab":
        return ab(args[1], args[2], args[3:])
    if len(args) == 4 and args[0] == "smoke":
        return smoke(Path(args[1]), args[2], args[3])
    print(__doc__.split("\n\n")[1], file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
