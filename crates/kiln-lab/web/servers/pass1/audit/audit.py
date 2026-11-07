#!/usr/bin/env python3
# Goal 1 step 10's audit: every number in docs/browser-lab.md's prose (outside its tables and code
# blocks) looked for in its section's sources (run files and what kiln-lab prints from them). Prints,
# by section, the numbers no source holds, with their line, for review by hand.
#   sources.sh KILN_LAB AUDIT_DIR && audit.py REPO AUDIT_DIR
import json, pathlib, re, sys

repo, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
B = repo / "docs/browser-lab"


def text(*paths):
    s = ""
    for p in paths:
        p = pathlib.Path(p)
        if p.exists():
            s += p.read_text(errors="replace") + "\n"
    return s


def numbers_of(s):
    """Every number in s as it could be written: 1,280 / 1280 / 1.28e3 written plainly."""
    got = set()
    for m in re.finditer(r"-?\d[\d,]*(?:\.\d+)?", s):
        v = m.group(0).replace(",", "")
        got.add(v.lstrip("-"))
        try:
            f = float(v)
            for d in (0, 1, 2, 3):
                got.add(f"{abs(f):.{d}f}")
            got.add(str(int(round(abs(f)))))
        except ValueError:
            pass
    return got


runs = {p.name: p for p in B.rglob("*.json")}
SRC = {
    "## The machine": text(out / "records.txt"),
    "### The profiles' checks": text(B / "2026-11-04/profile-checks.md"),
    "## The profiles": text(out / "profiles.txt"),
    "## The operations": text(B / "catalogue.json", out / "records.txt"),
    "## The probe and the three measures": text(*out.glob("*.check.txt"), *out.glob("check*.report.txt"), out / "grain.report.txt"),
    "### The probe's checks": text(out / "records.txt", *out.glob("*.check.txt"), *out.glob("check*.report.txt"), out / "grain.report.txt", out / "more.report.txt", *[runs[n] for n in ("check4.json", "grain.json", "check4c.json", "check5c.json") if n in runs]),
    "## The baselines": text(out / "records.txt", out / "pilot.extra.txt", *[runs[n] for n in ("check5c.json", "check4c.json") if n in runs], out / "check5c.check.txt"),
    "## The statistics, the run plan and the decision rule": text(*out.glob("plan-*.txt")),
    "### What the numbers say": text(out / "say.txt"),
    "### Step 7: the pilot's checks": text(out / "step7.compare.txt", out / "pilot.report.txt", out / "kcheck.report.txt", out / "pilot.extra.txt"),
    "### Step 8: the matrix": text(out / "short.md", out / "machine.md", out / "coverage.md", out / "step8.extra.txt", out / "more.report.txt", out / "pilot.extra.txt"),
    "### Where the run stands": text(out / "pilot.extra.txt", out / "more.report.txt"),
}
# The sections that define the lab (its method, parameters and configs) trace to the code and to
# goal 1's graded research; the measured sections only to their records.
DEF = text(out / "code.txt", out / "profiles.txt", *out.glob("plan-*.txt"))
for k in ("# The browser lab", "## The machine", "## The profiles", "### The profiles' checks", "## The operations",
          "## The probe and the three measures", "## The baselines", "## The statistics, the run plan and the decision rule",
          "## Running it", "## The numbers", "## What the lab leaves out"):
    SRC[k] = SRC.get(k, "") + DEF
SRC["## Running it"] += text(out / "running.txt", out / "pilot.extra.txt")
SRC["### Where the run stands"] = SRC.get("### Where the run stands", "") + text(out / "genparts.txt")
for k in ("### The probe's checks", "## The baselines", "### Where the run stands", "### Step 8: the matrix", "## The probe and the three measures", "## The operations"):
    SRC[k] = SRC.get(k, "") + text(out / "records.txt")
doc = (repo / "docs/browser-lab.md").read_text()
sec, code, tables, missing, total = None, False, False, {}, 0
for i, line in enumerate(doc.splitlines(), 1):
    if line.startswith("```"):
        code = not code
        continue
    if "<!-- browser-lab tables: begin" in line:
        tables = True
    if "<!-- browser-lab tables: end" in line:
        tables = False
        continue
    if line.startswith("#"):
        sec = line.strip()
        continue
    if code or tables:
        continue
    # Not numbers of measure: findings (f12), steps, goals, plans, dates, times, code spans, versions.
    plain = re.sub(r"\bf\d+\b|\b(?:step|steps|goal|goals|plan|v|pass|round|rounds|K =|K=)\s*\d+|20\d\d-\d\d-\d\d|\d\d:\d\dZ|`[^`]*`|\b[0-9a-f]{7,}\b", "", line)
    nums = numbers_of(plain)
    if not nums:
        continue
    src = SRC.get(sec)
    have = numbers_of(src) if src else set()
    for m in re.finditer(r"-?\d[\d,]*(?:\.\d+)?", plain):
        total += 1
        v = m.group(0).replace(",", "").lstrip("-")
        if v not in have:
            missing.setdefault(sec, []).append((i, m.group(0)))
# Numbers no record holds as written, each derived from one that does: (section, number) -> how.
derived = {}
for line in (pathlib.Path(__file__).parent / "derived.tsv").read_text().splitlines():
    if line.strip():
        sec_, num, how = line.split("\t")
        derived[(sec_, num)] = how
left, used = [], set()
for s, ms in missing.items():
    for i, n in ms:
        k = (s, n.rstrip(","))
        if k in derived:
            used.add(k)
        else:
            left.append((s, i, n))
print(f"numbers in the doc's prose a record holds as written: {total - sum(len(m) for m in missing.values())} of {total}")
print(f"derived from a record (derived.tsv, each with how): {len(used)} distinct")
print(f"neither: {len(left)}")
for s, i, n in left:
    print(f"  {s}: {n} at line {i}")
for k in sorted(set(derived) - used):
    print(f"  derived.tsv's {k} no longer in the doc")
