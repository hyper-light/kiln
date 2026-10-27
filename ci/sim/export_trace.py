#!/usr/bin/env python3
"""The v1 trace for kiln_ci::sched::sim::replay (W25 step 1), trimmed and anonymous.

Reads the runs runs_fetch.py fetched and the repository's git history, and writes trace.json:
every run of the window with its jobs (labels, needs, queued/started/finished offsets in seconds
from the window's start, status, why it was cancelled, the steps P5 could share), each run's
concurrency setting as the workflow file at its commit had it, each main commit's input keys
(hashes of the git tree ids of each input class), and the runners' capacity by label.

Run and job ids, authors, messages, shas, repository ids and machine names are left out.
The same runs.json and git history give the same bytes.

    python3 ci/sim/export_trace.py /tmp/cires/runs.json ci/sim/trace.json
"""
import hashlib
import json
import os
import re
import subprocess
import sys

RUNS = sys.argv[1] if len(sys.argv) > 1 else "/tmp/cires/runs.json"
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "trace.json")
REPO = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True, cwd=os.path.dirname(os.path.abspath(__file__))).stdout.strip()

# A job's inputs, as path sets of the repository (an approximation of W24's keys, which come from
# observed reads): the key of a job at a commit is a hash of the tree ids of its class's paths.
CLASSES = {
    "rust": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo", "docs", "AGENTS.md"],
    "ui": ["ui"],
    "vendor": ["vendor", "scripts"],
    "binary": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo", "ui"],
    "uichecks": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo", "ui", "ci/ui"],
    "release": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo", "ui", "ci/release"],
    "image": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo", "ui", "Dockerfile", "deploy"],
    "bench": ["crates", "Cargo.toml", "Cargo.lock", "vendor", ".cargo"],
}
CLASS_OF = {
    ("ci", "rust"): "rust",
    ("ci", "ui"): "ui",
    ("ci", "vendor"): "vendor",
    ("ci", "ui-checks"): "uichecks",
    ("ci", "release-binary"): "binary",
    ("image", "image"): "image",
}

# Runner capacity by label (ci/sim/README.md says where each number comes from). No hostnames:
# what a pool can do, as the workflows ask for it.
POOLS = [
    {"name": "server", "labels": ["baremetal", "local", "bench", "linux"], "slots": 8},
    {"name": "linux", "labels": ["baremetal", "linux"], "slots": 8},
    {"name": "mac-1", "labels": ["baremetal", "macos"], "slots": 2},
    {"name": "mac-2", "labels": ["baremetal", "macos"], "slots": 2},
    {"name": "k8s", "labels": ["k8s"], "cpu_milli": 18000, "mem_mib": 36864, "pods": 64},
]
# A k8s job's requests when its workflow gives none (the server's default_cpu and default_memory).
DEFAULT_REQ = [1000, 2048]
DONE = ("succeeded", "failed", "timed_out")


def git_batch(args, lines):
    p = subprocess.run(["git", *args], input="".join(f"{x}\n" for x in lines).encode(), capture_output=True, check=True, cwd=REPO)
    return p.stdout


def tree_ids(shas):
    """The tree (or blob) id of every class path at every commit: {(sha, path): oid or ""}."""
    paths = sorted({p for ps in CLASSES.values() for p in ps})
    asks = [f"{s}:{p}" for s in shas for p in paths]
    out = git_batch(["cat-file", "--batch-check"], asks).decode().splitlines()
    ids = {}
    for ask, line in zip(asks, out):
        sha, path = ask.split(":", 1)
        ids[(sha, path)] = "" if line.endswith("missing") else line.split(" ", 1)[0]
    return ids


def files_at(asks):
    """The contents of `sha:path` for each ask ("" when missing)."""
    raw = git_batch(["cat-file", "--batch"], asks)
    out, i = {}, 0
    for ask in asks:
        nl = raw.index(b"\n", i)
        head = raw[i:nl].decode()
        i = nl + 1
        if head.endswith("missing"):
            out[ask] = ""
            continue
        size = int(head.rsplit(" ", 1)[1])
        out[ask] = raw[i:i + size].decode()
        i += size + 1
    return out


def workflow(text):
    """What the replay needs of a workflow file: its concurrency setting, and per job its requests
    and whether it's exclusive. Read with patterns: kiln's own workflow files keep these on lines
    of their own, in the forms below."""
    head, _, body = text.partition("\njobs:")
    cip = re.search(r"cancel-in-progress:\s*(true|false)", head)
    jobs, name = {}, None
    for line in body.splitlines():
        m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if m:
            name = m.group(1)
            jobs[name] = {"req": None, "excl": False}
            continue
        if name is None:
            continue
        r = re.match(r'^\s+resources:\s*\{\s*cpu:\s*"?([0-9.]+)"?,\s*memory:\s*([0-9]+)Gi\s*\}', line)
        if r:
            jobs[name]["req"] = [int(float(r.group(1)) * 1000), int(r.group(2)) * 1024]
        if re.match(r"^\s+exclusive:\s*true\s*$", line):
            jobs[name]["excl"] = True
    return {"cancel_in_progress": None if cip is None else cip.group(1) == "true", "jobs": jobs}


def why(job, run):
    """Why a job ended as it did, as runs_analyze.py tells superseded runner time."""
    err = job.get("error") or ""
    st = job["status"]
    if st == "cancelled":
        if "supersed" in err or "supersed" in (run.get("error") or ""):
            return "superseded"
        return "user" if "user" in err else "cancelled"
    if st == "skipped":
        return "dependency"
    if st == "timed_out":
        return "timeout"
    if st == "failed":
        return "upload" if "artifact upload" in err else "step"
    return None


def shared_steps(pipeline, key, steps):
    """The steps P5 could take from another pipeline's work, with their durations: the UI build
    (`ui`) and the glibc release compile (`glibc`)."""
    out = {}
    for s in steps:
        if s.get("status") != "succeeded" or not s.get("started_at") or not s.get("finished_at"):
            continue
        d = s["finished_at"] - s["started_at"]
        name = s["name"]
        if name == "ui" or ("npm ci --prefix ui" in name and "ui build" in name):
            out["ui"] = out.get("ui", 0) + d
        elif name == "cargo build --release --locked -p kiln" or (pipeline, key, name) == ("bench", "ci-bench", "build (release)"):
            out["glibc"] = out.get("glibc", 0) + d
    return out


def trim(obj, keep):
    """Leaves out fields at their defaults (null, false, empty, an attempt of 0), except those in
    `keep`. Times of 0 are kept: they are the window's first second."""
    return {k: v for k, v in obj.items() if k in keep or not (v is None or v is False or v == [] or v == {} or (k == "attempt" and v == 0))}


def main():
    d = json.load(open(RUNS))
    runs = sorted(d["runs"], key=lambda x: (x["run"]["created_at"], x["run"]["number"]))
    t0 = min(x["run"]["created_at"] for x in runs)
    end = int(d["fetched_at"]) - t0

    shas = []
    for x in runs:
        if x["run"]["sha"] not in shas:
            shas.append(x["run"]["sha"])
    index = {s: i for i, s in enumerate(shas)}
    ids = tree_ids(shas)
    commits = []
    for s in shas:
        keys = {}
        for cls, paths in CLASSES.items():
            h = hashlib.sha256("\n".join(f"{p}={ids[(s, p)]}" for p in paths).encode()).hexdigest()
            keys[cls] = h[:8]
        commits.append({"keys": keys})

    asks = sorted({f"{x['run']['sha']}:.kiln/{x['run']['pipeline']}.yaml" for x in runs})
    texts = files_at(asks)
    wfs = {a: workflow(t) for a, t in texts.items()}

    out_runs = []
    for x in runs:
        r = x["run"]
        wf = wfs[f"{r['sha']}:.kiln/{r['pipeline']}.yaml"]
        group = r.get("concurrency_group")
        if group:
            group = group.split(":", 1)[1] if ":" in group else group
        jobs = []
        for j in x["jobs"]:
            spec = wf["jobs"].get(j["key"], {"req": None, "excl": False})
            labels = list(j.get("labels") or [])
            off = lambda v: None if v is None else int(v) - t0  # noqa: E731
            failed = next((s["name"] for s in j.get("steps") or [] if s.get("status") == "failed"), None)
            job = {
                "key": j["key"],
                "labels": labels,
                "needs": list(j.get("needs") or []),
                "class": CLASS_OF.get((r["pipeline"], j["key"]), r["pipeline"]),
                "exclusive": bool(spec["excl"]),
                "req": (spec["req"] or DEFAULT_REQ) if "k8s" in labels else None,
                "queued": off(j.get("queued_at")),
                "started": off(j.get("started_at")),
                "finished": off(j.get("finished_at")),
                "status": j["status"],
                "why": why(j, r),
                "attempt": int(j.get("attempt") or 0),
                "failed_step": failed,
                "shared": shared_steps(r["pipeline"], j["key"], j.get("steps") or []),
            }
            jobs.append(trim(job, keep=("key", "labels", "status")))
        out_runs.append(trim({
            "workflow": r["pipeline"],
            "event": r["event"],
            "branch": r["branch"],
            "group": group,
            "cancel_in_progress": wf["cancel_in_progress"] if group else None,
            "at": r["created_at"] - t0,
            "commit": index[r["sha"]],
            "status": r["status"],
            "finished": None if r.get("finished_at") is None else int(r["finished_at"]) - t0,
            "jobs": jobs,
        }, keep=("workflow", "event", "branch", "at", "commit", "status", "jobs")))

    head = {
        "format": 1,
        "about": "kiln's own v1 CI over 7 days (Oct 18-25, 2026), for kiln_ci::sched::sim::replay (W25 step 1). Times are seconds from the window's start; ci/sim/README.md describes every field.",
        "end": end,
        "measured": {"superseded_share": 0.250, "ci_concluded": 137, "ci_pushes": 270},
        "pools": POOLS,
        "commits": commits,
    }
    with open(OUT, "w") as f:
        body = json.dumps(head, separators=(",", ":"), ensure_ascii=False)
        f.write(body[:-1] + ',"runs":[\n')
        f.write(",\n".join(json.dumps(r, separators=(",", ":"), ensure_ascii=False) for r in out_runs))
        f.write("\n]}\n")
    print(f"{len(out_runs)} runs, {sum(len(r['jobs']) for r in out_runs)} jobs, {len(commits)} commits -> {OUT}")


if __name__ == "__main__":
    main()
