#!/usr/bin/env python3
"""kiln's own workflows on v1 and on the CI engine, side by side (docs/ci.md W24 M0 step 4, W28).

Two throwaway servers each poll a bare repository of their own: one runs `.kiln/` on v1
(`[ci] kiln_workflows` off), the other on the engine (on, with a `k8s` pool). This moves `main`
one commit at a time through the commits given, oldest first, as kiln's own `main` moved: each
push has its parent as `before`, so path filters see what production saw. Each commit goes to
v1's repository first, and once v1's runs of it have ended, to the engine's (one side at a time:
both at once don't fit one machine beside its other work, and a side waiting on the other's builds
would spend its jobs' timeouts waiting). Then it records every job's conclusion on each side and,
for the `rust` job, each test that failed (from its `test` step's cargo output). The next commit
waits for that, since ci.yaml and release.yaml cancel a run in progress when main moves. With one
`--repo` for both, both sides get each commit at once. Only push runs count: a `schedule:` fires on
each server's own clock at whatever commit its main is at then, so its runs don't line up with the
replay's commits (W28 step 3).

    python3 -I ci/conformance/engine_vs_v1.py --v1-repo <bare repository> --engine-repo <another> \\
        --v1 http://127.0.0.1:17101 --v1-token-file <a>/data/admin.token \\
        --engine http://127.0.0.1:17111 --engine-token-file <b>/data/admin.token \\
        --commits <file: one SHA a line, oldest first> --out <results directory> \\
        [--name hyper-light/kiln] [--timeout SECONDS] [--prod <runs.json>]

Each commit's record is `<out>/<sha>.json`, so a second invocation goes on where the first
stopped (a commit already recorded is moved past, not run again). `--report` writes
`<out>/report.md` from the records: per commit, each workflow's jobs side by side, whether the
conclusions match, and the tests set aside, each failing on one side only and failing at that
commit elsewhere (on the v1 side there, or in production's v1 run of it, `--prod`).
"""

import argparse
import collections
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

FINISHED_V1 = {"succeeded", "failed", "cancelled", "timed_out", "skipped"}
# v1's statuses in the engine's words.
V1_CONCLUSION = {"succeeded": "success", "failed": "failure", "cancelled": "cancelled", "timed_out": "timed_out", "skipped": "skipped"}
TEST_FAILED = re.compile(r"^test (\S+) \.\.\. FAILED")
RUNNING = re.compile(r"^\s*Running (?:unittests )?(\S+)(?: \(([^)]+)\))?")


class Server:
    def __init__(self, url, token_file):
        self.url = url.rstrip("/")
        with open(token_file) as f:
            self.token = f.read().strip()

    def post(self, path, body=None):
        data = json.dumps(body or {}).encode()
        req = urllib.request.Request(self.url + path, data=data, method="POST", headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status

    def get(self, path, raw=False):
        req = urllib.request.Request(self.url + path, headers={"Authorization": "Bearer " + self.token})
        for attempt in range(5):
            try:
                with urllib.request.urlopen(req, timeout=60) as r:
                    body = r.read()
                return body.decode("utf-8", "replace") if raw else json.loads(body or b"null")
            except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
                if attempt == 4:
                    raise
                print(f"  {self.url}{path}: {e}; again", file=sys.stderr)
                time.sleep(5 * (attempt + 1))
        return None


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], check=True, capture_output=True, text=True).stdout.strip()


def job_id(key):
    """A leg's job: its key without the matrix values (`pods (a=0, b=1)` is `pods`)."""
    return key.split(" (", 1)[0].strip()


def binary_of(line, current):
    m = RUNNING.match(line)
    if not m:
        return current
    target = m.group(1)
    return os.path.basename(target).rsplit(".", 1)[0] if "/" in target else target


def failing_tests(log):
    """The tests a cargo test log says failed, each as `<binary>::<test>`."""
    failed, binary = set(), "?"
    for line in log.splitlines():
        line = re.sub(r"\x1b\[[0-9;]*m", "", line)
        binary = binary_of(line, binary)
        m = TEST_FAILED.match(line)
        if m:
            failed.add(f"{binary}::{m.group(1)}")
    return sorted(failed)


class V1:
    side = "v1"

    def rerun(self, run):
        return self.s.post(f"/api/runs/{run['id']}/retry")

    def __init__(self, server, name):
        self.s = server
        repos = self.s.get("/api/repos") or []
        self.repo = next((r["id"] for r in repos if r["name"] == name), None)
        if self.repo is None:
            sys.exit(f"{server.url} has no repository {name}")

    def runs(self, sha):
        page = self.s.get(f"/api/runs?repo={self.repo}&limit=200") or []
        return [r for r in page if r.get("sha") == sha and not r.get("workflow_id") and r.get("event", "push") == "push"]

    def done(self, run):
        return run.get("status") in FINISHED_V1

    def jobs(self, run):
        d = self.s.get(f"/api/runs/{run['id']}") or {}
        out = []
        for j in d.get("jobs", []):
            out.append({"id": j["id"], "job": job_id(j.get("key") or j.get("display") or "?"), "key": j.get("key"), "conclusion": V1_CONCLUSION.get(j.get("status"), j.get("status")), "runner": j.get("agent"), "steps": [(s.get("idx"), s.get("name") or "") for s in j.get("steps", [])]})
        return run.get("pipeline"), out

    def log(self, job, step_name):
        for idx, name in job["steps"]:
            if name == step_name:
                return self.s.get(f"/api/jobs/{job['id']}/logs?step={idx}", raw=True) or ""
        return ""


class Engine:
    side = "engine"

    def rerun(self, run):
        return self.s.post(f"/api/ci/runs/{run['id']}/rerun")

    def __init__(self, server, name):
        self.s = server
        self.name = name

    def runs(self, sha):
        q = urllib.parse.quote(self.name, safe="")
        page = (self.s.get(f"/api/ci/runs?repo={q}&limit=100") or {}).get("runs", [])
        return [r for r in page if (r.get("sha") == sha or r.get("head_sha") == sha) and r.get("event", "push") == "push"]

    def done(self, run):
        # The engine's runs share v1's table and statuses (`failed`, …), with a conclusion besides.
        return bool(run.get("conclusion")) or run.get("status") in FINISHED_V1 or bool(run.get("finished_at"))

    def jobs(self, run):
        d = self.s.get(f"/api/ci/runs/{run['id']}") or {}
        out = []
        for leg in d.get("legs", []):
            key = leg.get("key") or leg.get("name") or "?"
            job = leg.get("job") or job_id(key)
            out.append({"id": leg["id"], "job": job.rsplit("/", 1)[-1], "key": key, "conclusion": leg.get("conclusion") or leg.get("status"), "runner": leg.get("runner"), "steps": None})
        name = (d.get("run") or d).get("name") or run.get("name") or run.get("workflow")
        return name, out

    def log(self, job, step_name):
        d = self.s.get(f"/api/ci/legs/{job['id']}") or {}
        for i, st in enumerate(d.get("steps", [])):
            if st.get("name") == step_name:
                n = st.get("number") or st.get("idx", i)
                return self.s.get(f"/api/ci/legs/{job['id']}/logs?step={n}", raw=True) or ""
        return ""


def wait_for(sides, sha, timeout, settle=120):
    """The sides' runs of `sha` once they've all ended: none new for `settle` seconds after the
    last appeared, and each finished."""
    start, seen, last_new = time.time(), {s.side: set() for s in sides}, time.time()
    while True:
        runs = {s.side: s.runs(sha) for s in sides}
        for s in sides:
            ids = {r["id"] for r in runs[s.side]}
            if ids - seen[s.side]:
                last_new = time.time()
                seen[s.side] |= ids
        all_done = all(runs[s.side] and all(s.done(r) for r in runs[s.side]) for s in sides)
        if all_done and time.time() - last_new >= settle:
            return runs, False
        if time.time() - start > timeout:
            return runs, True
        time.sleep(20)


def record(sides, sha, runs, timed_out):
    out = {"sha": sha, "timed_out": timed_out, "sides": {}}
    for s in sides:
        flows = {}
        for r in runs[s.side]:
            name, jobs = s.jobs(r)
            if name in flows:
                # Newest first: an older run of the workflow (before a rerun) doesn't count.
                continue
            flow = flows.setdefault(name, {"runs": [], "jobs": collections.defaultdict(list), "tests": {}})
            flow["runs"].append(r["id"])
            for j in jobs:
                flow["jobs"][j["job"]].append(j["conclusion"])
                if j["job"] == "rust" and name == "ci":
                    flow["tests"] = {"failed": failing_tests(s.log(j, "test")), "runner": j.get("runner")}
        out["sides"][s.side] = {n: {"runs": f["runs"], "jobs": {k: sorted(v) for k, v in f["jobs"].items()}, "tests": f["tests"]} for n, f in flows.items()}
    return out


def compare(rec, prod=None):
    """Whether the sides' job conclusions match, and what differs; a difference in the rust job
    whose one-sided failing tests all fail at that commit elsewhere is set aside, by name."""
    v1, en = rec["sides"].get("v1", {}), rec["sides"].get("engine", {})
    diffs, aside = [], []
    for flow in sorted(set(v1) | set(en)):
        a, b = v1.get(flow, {}).get("jobs", {}), en.get(flow, {}).get("jobs", {})
        for job in sorted(set(a) | set(b)):
            if a.get(job) == b.get(job):
                continue
            if flow == "ci" and job == "rust":
                # Failing on one side only: set aside when each fails at that commit elsewhere
                # (a v1-only failure on the v1 side by definition; an engine-only one in
                # production's run of the commit).
                ta = set(v1.get(flow, {}).get("tests", {}).get("failed", []))
                tb = set(en.get(flow, {}).get("tests", {}).get("failed", []))
                prod_failed = set((prod or {}).get(rec["sha"], {}).get("failed", []))
                one_sided = ta ^ tb
                if one_sided and (tb - ta) <= prod_failed:
                    aside.extend(sorted(one_sided))
                    continue
            diffs.append({"workflow": flow, "job": job, "v1": a.get(job), "engine": b.get(job)})
    return {"match": not diffs and not rec.get("timed_out"), "diffs": diffs, "set_aside": aside}


def retry_timeouts(sides, sha, rec, prod, timeout):
    """`--retry-timeouts`: a job that timed out on one side only (on a machine shared with other
    work, its timeout says as much about the load as the job) runs once more there: that side's
    run of its workflow again, then the commit recorded anew, the first record kept in it."""
    again = []
    for d in compare(rec, prod)["diffs"]:
        for s in sides:
            if "timed_out" in (d[s.side] or []):
                run = next((r for r in s.runs(sha) if (r.get("pipeline") or r.get("name")) == d["workflow"]), None)
                if run and all(run is not r for _, r, _ in again):
                    again.append((s, run, d))
    if not again:
        return rec
    for s, run, d in again:
        print(f"  {s.side} {d['workflow']}/{d['job']} timed out there only: its run again ({s.rerun(run)})", flush=True)
    time.sleep(60)
    runs, late = {}, rec.get("timed_out", False)
    for s in sides:
        got, slow = wait_for([s], sha, timeout)
        runs.update(got)
        late = late or slow
    new = record(sides, sha, runs, late)
    new.update({k: rec[k] for k in ("subject", "before", "seconds") if k in rec})
    new["retried"] = [f"{s.side}:{d['workflow']}/{d['job']}" for s, _, d in again]
    new["first_record"] = {side: {wf: f.get("jobs") for wf, f in v.items()} for side, v in rec["sides"].items()}
    return new


def report(out_dir, order, prod=None):
    rows, matched = [], 0
    for sha in order:
        p = os.path.join(out_dir, f"{sha}.json")
        if not os.path.exists(p):
            continue
        with open(p) as f:
            rec = json.load(f)
        c = compare(rec, prod)
        matched += c["match"]
        flows = sorted(set(rec["sides"].get("v1", {})) | set(rec["sides"].get("engine", {})))
        cells = []
        for flow in flows:
            a = rec["sides"].get("v1", {}).get(flow, {}).get("jobs", {})
            b = rec["sides"].get("engine", {}).get(flow, {}).get("jobs", {})
            parts = []
            for job in sorted(set(a) | set(b)):
                va, vb = ",".join(a.get(job) or ["-"]), ",".join(b.get(job) or ["-"])
                parts.append(f"{job} {va}" if va == vb else f"{job} **{va} / {vb}**")
            cells.append(f"{flow}: " + "; ".join(parts))
        why = "; ".join(f"{d['workflow']}/{d['job']} v1 {d['v1']} engine {d['engine']}" for d in c["diffs"]) or ("timed out" if rec.get("timed_out") else "")
        rows.append(f"| `{sha[:10]}` | {rec.get('subject', '')[:60]} | {'<br>'.join(cells)} | {'yes' if c['match'] else 'no'} | {', '.join(c['set_aside']) or '-'} | {why or '-'} |")
    head = [
        "# kiln's own workflows, v1 and the engine side by side",
        "",
        f"{matched} of {len(rows)} replayed commits have the same job conclusions on both sides (tests already failing at a commit set aside).",
        "",
        "| Commit | Subject | Jobs (v1 / engine where they differ) | Match | Set aside | Differences |",
        "|---|---|---|---|---|---|",
    ]
    text = "\n".join(head + rows) + "\n"
    with open(os.path.join(out_dir, "report.md"), "w") as f:
        f.write(text)
    print(text)
    return matched


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", help="one bare repository both servers poll")
    ap.add_argument("--v1-repo", help="v1's server's bare repository (with --engine-repo: one side at a time)")
    ap.add_argument("--engine-repo")
    ap.add_argument("--v1", required=True)
    ap.add_argument("--v1-token-file", required=True)
    ap.add_argument("--engine", required=True)
    ap.add_argument("--engine-token-file", required=True)
    ap.add_argument("--commits", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--name", default="hyper-light/kiln")
    ap.add_argument("--timeout", type=int, default=4 * 3600)
    ap.add_argument("--prod", help="production's failing tests by commit: {sha: {failed: [...]}}")
    ap.add_argument("--report", action="store_true")
    ap.add_argument("--rerun", help="run a recorded commit's runs again on both sides (a commit the lab disturbed) and record it anew")
    ap.add_argument("--retry-timeouts", action="store_true", help="a job that timed out on one side only runs once more there before the commit is recorded")
    a = ap.parse_args()
    with open(a.commits) as f:
        order = [l.split()[0] for l in f if l.strip() and not l.startswith("#")]
    prod = json.load(open(a.prod)) if a.prod else None
    os.makedirs(a.out, exist_ok=True)
    if a.report:
        report(a.out, order, prod)
        return
    sides = [V1(Server(a.v1, a.v1_token_file), a.name), Engine(Server(a.engine, a.engine_token_file), a.name)]
    if a.repo:
        waves = [(sides, a.repo)]
    elif a.v1_repo and a.engine_repo:
        waves = [([sides[0]], a.v1_repo), ([sides[1]], a.engine_repo)]
    else:
        sys.exit("--repo, or --v1-repo and --engine-repo")
    if a.rerun:
        sha = a.rerun
        for s in sides:
            for r in s.runs(sha):
                print(f"  {s.side} rerun {r['id']}: {s.rerun(r)}", flush=True)
        time.sleep(30)
        runs, timed_out = wait_for(sides, sha, a.timeout)
        rec = record(sides, sha, runs, timed_out)
        rec.update({"subject": git(waves[0][1], "log", "-1", "--format=%s", sha), "rerun": True})
        path = os.path.join(a.out, f"{sha}.json")
        with open(path + ".part", "w") as f:
            json.dump(rec, f, indent=1)
        os.replace(path + ".part", path)
        print(f"  rerun of {sha[:10]}: {compare(rec, prod)}", flush=True)
        return
    for sha in order:
        path = os.path.join(a.out, f"{sha}.json")
        current = git(waves[0][1], "rev-parse", "refs/heads/main")
        if os.path.exists(path):
            for _, repo in waves:
                at = git(repo, "rev-parse", "refs/heads/main")
                if at != sha:
                    git(repo, "update-ref", "refs/heads/main", sha, at)
            continue
        subject = git(waves[0][1], "log", "-1", "--format=%s", sha)
        print(f"{time.strftime('%H:%M:%S')} {sha[:10]} {subject}", flush=True)
        t0 = time.time()
        runs, timed_out = {}, False
        for side, repo in waves:
            git(repo, "update-ref", "refs/heads/main", sha, git(repo, "rev-parse", "refs/heads/main"))
            got, late = wait_for(side, sha, a.timeout)
            runs.update(got)
            timed_out = timed_out or late
        rec = record(sides, sha, runs, timed_out)
        rec.update({"subject": subject, "before": current, "seconds": round(time.time() - t0)})
        if a.retry_timeouts:
            rec = retry_timeouts(sides, sha, rec, prod, a.timeout)
        with open(path + ".part", "w") as f:
            json.dump(rec, f, indent=1)
        os.replace(path + ".part", path)
        c = compare(rec, prod)
        print(f"  {'match' if c['match'] else 'DIFFERS'} in {rec['seconds']} s: {c['diffs'] or ''} {c['set_aside'] or ''}", flush=True)
    report(a.out, order, prod)


if __name__ == "__main__":
    main()
