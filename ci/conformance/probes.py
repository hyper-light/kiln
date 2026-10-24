#!/usr/bin/env python3
"""docs/ci.md W22-AC03: F-22's probe workflows (crates/kiln-ci/tests/fixtures/probes), run on a
provider, what they say kept as JSON, and kiln compared with GitHub.

For each probe workflow: whether its run started and how it concluded; for each job, its
conclusion, the `PROBE key=value` lines its steps printed (a line that starts with `PROBE `), and
its annotations (level, title, message). On kiln they're read from kiln's API, on GitHub from its
REST API, into the same shape.

    # On kiln: the probes as a repository of the server's own (made if it isn't there):
    python3 -I ci/conformance/probes.py kiln --server http://127.0.0.1:17070 \\
        --token-file <data_dir>/admin.token --work <empty dir> --out kiln.json
    # On GitHub, once (user-assisted): the probes pushed to the default branch of a public
    # repository you own, then their runs read; the token needs `repo` and `workflow`:
    python3 -I ci/conformance/probes.py github --repo you/kiln-probes --token-file <file> \\
        --work <empty dir> --out crates/kiln-ci/tests/fixtures/probes/golden.json
    # kiln against GitHub's recording (exit 1 when they differ):
    python3 -I ci/conformance/probes.py compare kiln.json crates/kiln-ci/tests/fixtures/probes/golden.json

The kiln server's runners must route `ubuntu-latest` (`[ci] github_labels = true`).
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
PROBES = os.path.join(ROOT, "crates", "kiln-ci", "tests", "fixtures", "probes")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
STAMP = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z ")
LEVELS = {"failure": "error", "error": "error", "warning": "warning", "notice": "notice"}


def call(url, token, method="GET", body=None, auth="Bearer", accept="application/json"):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"Authorization": f"{auth} {token}", "Accept": accept, "Content-Type": "application/json", "User-Agent": "kiln-probes"})
    with urllib.request.urlopen(req, timeout=60) as r:
        raw = r.read()
    return raw if accept != "application/json" else (json.loads(raw) if raw else None)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


def log_text(url, token):
    """A GitHub job's log: its API answers with a redirect to storage elsewhere, which is fetched
    without the token (it's GitHub's, never another host's)."""
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}", "User-Agent": "kiln-probes"})
    try:
        with urllib.request.build_opener(NoRedirect).open(req, timeout=60) as r:
            return r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        if e.code not in (301, 302, 303, 307, 308) or not e.headers.get("Location"):
            raise
        with urllib.request.urlopen(urllib.request.Request(e.headers["Location"], headers={"User-Agent": "kiln-probes"}), timeout=60) as r:
            return r.read().decode(errors="replace")


def annotations(url, token):
    """A GitHub job's annotations, its check run's (a GitHub App's token needs `checks: read`: a
    public repository's check runs aren't anyone's to read)."""
    try:
        return call(url, token) or []
    except urllib.error.HTTPError as e:
        if e.code in (403, 404):
            sys.exit(f"{url}: {e.code}: the token can't read check runs (a GitHub App needs Checks: Read-only)")
        raise


def git(cwd, *args):
    env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0", GIT_AUTHOR_NAME="probes", GIT_AUTHOR_EMAIL="probes@example.invalid", GIT_COMMITTER_NAME="probes", GIT_COMMITTER_EMAIL="probes@example.invalid")
    out = subprocess.run(["git", "-C", cwd, *args], env=env, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout.strip()


def probes_of(text):
    """The `PROBE key=value` lines of a job's log, by key."""
    found = {}
    for line in text.splitlines():
        line = STAMP.sub("", ANSI.sub("", line)).rstrip("\r")
        if line.startswith("PROBE ") and "=" in line:
            k, v = line[len("PROBE "):].split("=", 1)
            found[k.strip()] = v.strip()
    return found


def workflows():
    """The probe workflows that run on push (the reusable ones only run when called)."""
    d = os.path.join(PROBES, ".github", "workflows")
    out = []
    for name in sorted(os.listdir(d)):
        with open(os.path.join(d, name)) as f:
            text = f.read()
        if re.search(r"^on:\s*\[?.*push", text, re.M):
            out.append(f".github/workflows/{name}")
    return out


def tree(work):
    """The probes as a repository's tree, in `work` (its `.git` kept)."""
    for name in os.listdir(work):
        if name != ".git":
            p = os.path.join(work, name)
            shutil.rmtree(p) if os.path.isdir(p) else os.remove(p)
    shutil.copytree(os.path.join(PROBES, ".github"), os.path.join(work, ".github"))
    with open(os.path.join(work, "README.md"), "w") as f:
        f.write("F-22: kiln's GitHub probe workflows (docs/ci.md W22-AC03).\n")


def wait(what, f, seconds):
    deadline = time.time() + seconds
    while True:
        got = f()
        if got is not None:
            return got
        if time.time() > deadline:
            sys.exit(f"{what}: not within {seconds} s")
        time.sleep(5)


# ------------------------------------------------------------------ kiln


def on_kiln(a):
    token = open(a.token_file).read().strip()
    base = a.server.rstrip("/")
    api = lambda path, method="GET", body=None: call(f"{base}{path}", token, method, body)
    repo = a.repo
    if not any(r["name"] == repo for r in api("/api/repos")):
        api("/api/repos", "POST", {"name": repo, "mode": "native"})
    host = base.split("://", 1)[1]
    url = f"{base.split('://', 1)[0]}://x:{token}@{host}/git/{repo}.git"
    work = a.work
    os.makedirs(work, exist_ok=True)
    if not os.path.isdir(os.path.join(work, ".git")):
        git(work, "init", "-q", "-b", "main")
        with open(os.path.join(work, "README.md"), "w") as f:
            f.write("probes\n")
        git(work, "add", ".")
        git(work, "commit", "-qm", "first")
        git(work, "push", "-q", "--force", url, "HEAD:main")
        wait("the first poll", lambda: True if any(r["name"] == repo and r.get("last_polled_at") for r in api("/api/repos")) else None, 120)
    tree(work)
    git(work, "add", "-A")
    git(work, "commit", "-qm", "the probes", "--allow-empty")
    sha = git(work, "rev-parse", "HEAD")
    git(work, "push", "-q", url, "HEAD:main")
    want = set(workflows())

    def done():
        runs = [r for r in (api(f"/api/ci/runs?repo={urllib.parse.quote(repo)}&limit=200").get("runs") or []) if r.get("sha") == sha]
        seen = {r.get("workflow") for r in runs}
        if not want <= seen or any(r.get("finished_at") is None for r in runs if r.get("workflow") in want):
            return None
        return runs

    runs = wait("the probes' runs", done, a.timeout)
    out = {}
    for r in runs:
        path = r.get("workflow")
        if path not in want:
            continue
        detail = api(f"/api/ci/runs/{r['id']}")
        jobs = {}
        for leg in detail.get("legs") or []:
            about = api(f"/api/ci/legs/{leg['id']}")
            text = ""
            for step in about.get("steps") or []:
                text += call(f"{base}/api/ci/legs/{leg['id']}/logs?step={step['idx']}", token, accept="text/plain").decode(errors="replace")
            name = about.get("leg", {}).get("name") or about.get("leg", {}).get("key") or leg["id"]
            notes = [{"level": LEVELS.get(n.get("level", ""), n.get("level")), "title": n.get("title"), "message": n.get("message"), "step": n.get("step")} for n in about.get("annotations") or []]
            jobs[name] = {"conclusion": about.get("leg", {}).get("conclusion"), "probes": probes_of(text), "annotations": notes}
        started = bool(detail.get("legs"))
        out[path] = {"started": started, "conclusion": detail.get("run", {}).get("conclusion") if started else "startup_failure", "jobs": jobs}
    return {"provider": "kiln", "server": base, "sha": sha, "probes": out}


# ------------------------------------------------------------------ GitHub


def on_github(a):
    token = open(a.token_file).read().strip()
    gh = "https://api.github.com"
    api = lambda path, method="GET", body=None: call(f"{gh}{path}", token, method, body)
    owner, name = a.repo.split("/", 1)
    info = api(f"/repos/{owner}/{name}")
    branch = info["default_branch"]
    if a.sha:
        # Runs of the probes pushed before: read again, not run again.
        sha = a.sha
    else:
        work = a.work
        if not os.path.isdir(os.path.join(work, ".git")):
            subprocess.run(["git", "clone", "-q", f"https://x-access-token:{token}@github.com/{owner}/{name}.git", work], check=True, env=dict(os.environ, GIT_TERMINAL_PROMPT="0"))
        tree(work)
        git(work, "add", "-A")
        git(work, "commit", "-qm", "the probes", "--allow-empty")
        sha = git(work, "rev-parse", "HEAD")
        git(work, "push", "-q", "origin", f"HEAD:{branch}")
        print(f"pushed the probes to {a.repo} at {sha}", flush=True)
    want = set(workflows())

    def done():
        runs = api(f"/repos/{owner}/{name}/actions/runs?head_sha={sha}&per_page=100").get("workflow_runs") or []
        seen = {r.get("path") for r in runs}
        if not want <= seen or any(r.get("status") != "completed" for r in runs if r.get("path") in want):
            return None
        return runs

    runs = wait("the probes' runs", done, a.timeout)
    out = {}
    for r in runs:
        path = r.get("path")
        if path not in want:
            continue
        jobs, page = {}, 1
        while True:
            got = api(f"/repos/{owner}/{name}/actions/runs/{r['id']}/jobs?per_page=100&page={page}&filter=latest").get("jobs") or []
            for j in got:
                try:
                    text = log_text(f"{gh}/repos/{owner}/{name}/actions/jobs/{j['id']}/logs", token)
                except urllib.error.HTTPError:
                    text = ""
                notes = annotations(f"{gh}/repos/{owner}/{name}/check-runs/{j['id']}/annotations?per_page=100", token)
                jobs[j["name"]] = {
                    "conclusion": {"success": "success", "failure": "failure", "cancelled": "cancelled", "skipped": "skipped"}.get(j.get("conclusion"), j.get("conclusion")),
                    "probes": probes_of(text),
                    "annotations": [{"level": LEVELS.get(n.get("annotation_level", ""), n.get("annotation_level")), "title": n.get("title"), "message": n.get("message")} for n in notes],
                }
            if len(got) < 100:
                break
            page += 1
        started = r.get("conclusion") != "startup_failure"
        out[path] = {"started": started, "conclusion": r.get("conclusion"), "jobs": jobs}
    return {"provider": "github", "repo": a.repo, "sha": sha, "probes": out}


# ------------------------------------------------------------------ compare


def shown(notes):
    """kiln's annotations as GitHub shows a job's (docs/ci.md W06-R16): 10 errors and 10 warnings
    a step, 50 in all. kiln keeps the rest (A.9), which GitHub drops."""
    per, out = {}, []
    for n in notes:
        if n["level"] in ("error", "warning"):
            key = (n.get("step"), n["level"])
            per[key] = per.get(key, 0) + 1
            if per[key] > 10:
                continue
        out.append(n)
    return out[:50]


def compare(kiln, golden):
    """What kiln said that GitHub didn't: one line each."""
    diffs = []
    for path, g in sorted(golden["probes"].items()):
        k = kiln["probes"].get(path)
        if k is None:
            diffs.append(f"{path}: kiln has no run of it")
            continue
        if k["started"] != g["started"]:
            diffs.append(f"{path}: started {k['started']} on kiln, {g['started']} on GitHub")
        if g["started"] and k["conclusion"] != g["conclusion"]:
            diffs.append(f"{path}: concluded {k['conclusion']} on kiln, {g['conclusion']} on GitHub")
        merged = lambda r: {key: v for j in r["jobs"].values() for key, v in j["probes"].items()}
        kp, gp = merged(k), merged(g)
        for key in sorted(set(kp) | set(gp)):
            if kp.get(key) != gp.get(key):
                diffs.append(f"{path}: PROBE {key} is {kp.get(key)!r} on kiln, {gp.get(key)!r} on GitHub")
        for job, gj in sorted(g["jobs"].items()):
            kj = k["jobs"].get(job)
            if kj is None:
                diffs.append(f"{path}: kiln has no job named {job!r}")
                continue
            count = lambda notes: {lvl: sum(1 for n in notes if n["level"] == lvl) for lvl in ("error", "warning", "notice")}
            mine = shown(kj["annotations"])
            if count(mine) != count(gj["annotations"]):
                diffs.append(f"{path} / {job}: annotations {count(mine)} on kiln (as shown), {count(gj['annotations'])} on GitHub")
    return diffs


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    k = sub.add_parser("kiln")
    k.add_argument("--server", required=True)
    k.add_argument("--token-file", required=True)
    k.add_argument("--repo", default="acme/probes")
    k.add_argument("--work", required=True)
    k.add_argument("--out", required=True)
    k.add_argument("--timeout", type=int, default=1800)
    g = sub.add_parser("github")
    g.add_argument("--repo", required=True)
    g.add_argument("--token-file", required=True)
    g.add_argument("--work", required=True)
    g.add_argument("--out", required=True)
    g.add_argument("--timeout", type=int, default=1800)
    g.add_argument("--sha", help="read the runs of the probes pushed at this commit, pushing nothing")
    c = sub.add_parser("compare")
    c.add_argument("kiln")
    c.add_argument("golden")
    a = p.parse_args()
    if a.cmd == "compare":
        diffs = compare(json.load(open(a.kiln)), json.load(open(a.golden)))
        for d in diffs:
            print(d)
        print(f"{len(diffs)} differences")
        sys.exit(1 if diffs else 0)
    result = on_kiln(a) if a.cmd == "kiln" else on_github(a)
    with open(a.out, "w") as f:
        json.dump(result, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"{len(result['probes'])} probe runs written to {a.out}")


if __name__ == "__main__":
    main()
