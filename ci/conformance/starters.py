#!/usr/bin/env python3
"""docs/ci.md W22-AC01: GitHub's starter workflows, run on a kiln server.

Puts the fixture project (crates/kiln/tests/fixtures/starters) and every starter workflow
(crates/kiln-ci/tests/fixtures/gha/starter-workflows) that can run without an account or a
macOS or Windows machine into a repository the server polls, commits it, dispatches the ones
that only run by hand, waits for every run, and writes docs/ci-conformance.md: each workflow's
result, with the reason for every one that didn't pass or didn't run.

    python3 -I ci/conformance/starters.py --server http://127.0.0.1:17070 \\
        --token-file <data_dir>/admin.token --repo-dir <the repository's work tree> --repo acme/starters

The repository must be one the server polls (`[[repos]]`) with GitHub workflows on, and the
server's runners must route `ubuntu-latest` (`[ci] github_labels = true`).
"""

import argparse
import datetime
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
FIXTURE = os.path.join(ROOT, "crates", "kiln", "tests", "fixtures", "starters")
STARTERS = os.path.join(ROOT, "crates", "kiln-ci", "tests", "fixtures", "gha", "starter-workflows")
OUT = os.path.join(ROOT, "docs", "ci-conformance.md")

# GitHub fills these in when a starter is added to a repository.
PLACEHOLDERS = {
    "$default-branch": "main",
    "$protected-branches": "[ main ]",
    "$cron-daily": "0 0 * * *",
    "$cron-weekly": "0 0 * * 0",
}

# Starters this run leaves out, and why. Everything not here and not caught by a family's rule
# below runs.
NOT_RUN = {
    "ci_android.yml": "builds an Android app: the fixture has none, and kiln's runner image has no Android SDK",
    "ci_datadog-synthetics.yml": "runs Datadog Synthetic tests: needs a Datadog account",
    "ci_docker-publish.yml": "pushes to ghcr.io and signs with Sigstore: needs a registry account (W12 will host one)",
    "ci_gem-push.yml": "publishes to RubyGems and GitHub Packages on a release: needs accounts",
    "ci_generator-generic-ossf-slsa3-publish.yml": "SLSA provenance through slsa-github-generator's reusable workflows: needs Sigstore and attestations (W18)",
    "ci_go-ossf-slsa3-publish.yml": "SLSA provenance through slsa-github-generator's reusable workflows: needs Sigstore and attestations (W18)",
    "ci_gradle-publish.yml": "publishes to GitHub Packages on a release: needs a registry (W12)",
    "ci_laravel.yml": "tests a Laravel application: the fixture isn't one",
    "ci_maven-publish.yml": "publishes to GitHub Packages on a release: needs a registry (W12)",
    "ci_npm-publish.yml": "publishes to npm on a release: needs an npm account",
    "ci_npm-publish-github-packages.yml": "publishes to GitHub Packages on a release: needs a registry (W12)",
    "ci_python-publish.yml": "publishes to PyPI on a release: needs a PyPI account",
    "ci_rubyonrails.yml": "tests a Rails application: the fixture isn't one",
    "ci_super-linter.yml": "every linter over the whole code base, in a multi-gigabyte image: not run",
    "ci_symfony.yml": "tests a Symfony application: the fixture isn't one",
    "automation_greetings.yml": "comments on new issues and pull requests through GitHub's REST API: not in kiln's facade yet (W14)",
    "automation_label.yml": "labels pull requests through GitHub's REST API: not in kiln's facade yet (W14)",
    "automation_stale.yml": "closes stale issues through GitHub's REST API: not in kiln's facade yet (W14)",
    "automation_summary.yml": "summarizes issues with GitHub Models: needs an account",
}

# Why a run that doesn't pass doesn't, when kiln isn't the reason: each fails the same way on
# GitHub (or would there, as written).
KNOWN = {
    "ci_cmake-multi-platform.yml": "its Windows leg needs a Windows runner",
    "ci_django.yml": "as on GitHub's ubuntu-latest: setup-python has no Python 3.7 for Ubuntu 24.04",
    "ci_elixir.yml": "as on GitHub's ubuntu-latest: the pinned setup-beam v1.16.0 predates Ubuntu 24.04",
    "ci_erlang.yml": "as on GitHub: the erlang:22.0.7 container's glibc is too old for the Node 20 that actions/checkout@v4 runs on",
    "ci_haskell.yml": "as on GitHub: the deprecated actions/setup-haskell installs cabal 3.2, which can't verify Hackage's current root keys",
    "ci_ruby.yml": "as on GitHub's ubuntu-latest: the pinned setup-ruby v1.146.0 predates Ubuntu 24.04, and Rubies 2.6 and 2.7 aren't built for it",
    "ci_maven.yml": "the pinned maven-dependency-submission-action posts to api.github.com whatever GITHUB_API_URL says, as it does on GitHub Enterprise Server; on GitHub its read-only default token is refused too",
    "ci_scala.yml": "as on GitHub: the workflow's `permissions: contents: read` is too little to submit the dependency graph (403)",
    "ci_python-package-conda.yml": "kiln's runner image has no conda (`$CONDA`), which GitHub's has",
}

# Dispatched by hand, with these inputs.
DISPATCH = {"automation_manual.yml": {"name": "kiln"}}


def family_reason(name, text):
    """Why a starter of a family that needs an account or GitHub's own services doesn't run."""
    if name.startswith("deployments_"):
        cloud = name[len("deployments_"):-len(".yml")].split("-")[0]
        return f"deploys to {cloud}: needs a cloud account (W11-AC03)"
    if name.startswith("pages_"):
        return "publishes to GitHub Pages (configure-pages, deploy-pages): kiln has no Pages (W14)"
    if name.startswith("code-scanning_"):
        secrets = sorted(set(re.findall(r"secrets\.([A-Za-z0-9_]+)", text)) - {"GITHUB_TOKEN"})
        if secrets:
            return "needs an account (" + ", ".join(secrets) + ")"
        # Their SARIF goes to kiln's facade, as to GitHub's code scanning (W14).
        if "upload-sarif" in text or "codeql-action" in text:
            return None
        return "needs GitHub's dependency graph or security APIs: not in kiln's facade yet (W14)"
    runs_on = " ".join(re.findall(r"runs-on:\s*(.+)", text))
    if "ubuntu" not in runs_on and "${{" not in runs_on:
        if "macos" in runs_on:
            return "runs on macOS: no macOS runner in this run"
        if "windows" in runs_on:
            return "runs on Windows: no Windows runner in this run"
    return None


class Api:
    def __init__(self, server, token):
        self.server = server.rstrip("/")
        self.token = token

    def call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.server + path, data=data, method=method, headers={"Authorization": "Bearer " + self.token})
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                raw = r.read()
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')[:300]}") from None


def git(repo_dir, *args):
    return subprocess.run(["git", "-C", repo_dir, *args], check=True, capture_output=True, text=True).stdout.strip()


def prepare(repo_dir, runnable):
    """The repository's tree becomes the fixture and the runnable starters; returns the commit."""
    for entry in os.listdir(repo_dir):
        if entry == ".git":
            continue
        p = os.path.join(repo_dir, entry)
        if os.path.isdir(p) and not os.path.islink(p):
            shutil.rmtree(p)
        else:
            os.remove(p)
    shutil.copytree(FIXTURE, repo_dir, dirs_exist_ok=True, symlinks=True)
    wf_dir = os.path.join(repo_dir, ".github", "workflows")
    os.makedirs(wf_dir, exist_ok=True)
    for name in runnable:
        with open(os.path.join(STARTERS, name)) as f:
            text = f.read()
        for k, v in PLACEHOLDERS.items():
            text = text.replace(k, v)
        with open(os.path.join(wf_dir, name), "w") as f:
            f.write(text)
    git(repo_dir, "add", "-A")
    git(repo_dir, "commit", "-q", "--allow-empty", "-m", f"starters: {len(runnable)} workflows on the fixture")
    return git(repo_dir, "rev-parse", "HEAD")


FINISHED = ("succeeded", "failed", "cancelled", "skipped", "timed_out")


def stuck(api, run):
    """Whether every leg a run has left waits for a runner no one has (a Windows leg here)."""
    legs = api.call("GET", "/api/ci/runs/" + run["id"]).get("legs") or []
    pending = [leg for leg in legs if leg.get("status") not in FINISHED]
    return bool(pending) and all(isinstance(leg.get("wait_reason"), dict) and leg["wait_reason"].get("reason") in ("no-runner", "unroutable") for leg in pending)


def wait_reason(w):
    if not isinstance(w, dict):
        return str(w) if w else "waiting"
    r = w.get("reason", "waiting")
    if r == "unroutable":
        return "no runner meets its requirements (" + ", ".join(w.get("failing") or []) + ")"
    if r == "no-runner":
        return "no runner that can take it is connected"
    return r.replace("-", " ")


def leg_detail(api, leg):
    """The first failing step of a leg and what it said."""
    d = api.call("GET", "/api/ci/legs/" + leg["id"])
    steps = d.get("steps") or []
    failed = next((s for s in steps if s.get("conclusion") == "failure"), None)
    errors = [a.get("message", "") for a in (d.get("annotations") or []) if a.get("level") == "error"]
    msg = (errors[0] if errors else "").strip().splitlines()
    msg = msg[0] if msg else ""
    if failed and not msg:
        try:
            q = urllib.parse.urlencode({"step": failed.get("idx", 0)})
            raw = urllib.request.urlopen(urllib.request.Request(api.server + f"/api/ci/legs/{leg['id']}/logs?{q}", headers={"Authorization": "Bearer " + api.token}), timeout=60).read().decode(errors="replace")
            try:
                j = json.loads(raw)
                raw = j.get("text") or j.get("log") or ""
            except ValueError:
                pass
            lines = [re.sub(r"\x1b\[[0-9;]*m", "", ln).strip() for ln in raw.replace("\r", "").split("\n")]
            lines = [ln for ln in lines if ln]
            msg = lines[-1] if lines else ""
        except (urllib.error.URLError, OSError):
            pass
    step = failed.get("name") if failed else (leg.get("error") or "")
    return step, msg[:240]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", required=True)
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--repo-dir", required=True)
    ap.add_argument("--repo", required=True, help="the repository's name on the server, owner/name")
    ap.add_argument("--timeout", type=int, default=3600, help="seconds to wait for the runs")
    ap.add_argument("--only", nargs="*", help="run only these starters (file names)")
    args = ap.parse_args()
    api = Api(args.server, open(args.token_file).read().strip())

    names = sorted(n for n in os.listdir(STARTERS) if n.endswith(".yml"))
    results = {}
    runnable = []
    for n in names:
        with open(os.path.join(STARTERS, n)) as f:
            text = f.read()
        reason = NOT_RUN.get(n) or family_reason(n, text)
        if args.only and n not in args.only:
            reason = reason or "not in this run (--only)"
        if reason:
            results[n] = ("not run", reason, [])
        else:
            runnable.append(n)
    sha = prepare(args.repo_dir, runnable)
    print(f"committed {sha[:8]} with {len(runnable)} starters; waiting for their runs", flush=True)

    dispatched = set()
    deadline = time.time() + args.timeout
    runs, blocked = {}, set()
    while time.time() < deadline:
        listing = api.call("GET", "/api/ci/runs?" + urllib.parse.urlencode({"repo": args.repo, "limit": 200}))["runs"]
        for r in listing:
            wf = os.path.basename(r.get("workflow", ""))
            if r.get("sha") == sha and wf in runnable:
                runs[wf] = r
        # The ones that run by hand, once the server has the commit (a run of it started).
        if runs:
            for n in DISPATCH:
                if n in runnable and n not in dispatched:
                    api.call("POST", f"/api/ci/repos/{urllib.parse.quote(args.repo, safe='')}/workflows/.github/workflows/{n}/dispatches", {"ref": "main", "inputs": DISPATCH[n]})
                    dispatched.add(n)
        for n, r in runs.items():
            if r.get("status") not in FINISHED and n not in blocked and stuck(api, r):
                blocked.add(n)
        done = [n for n in runnable if n in runs and (runs[n].get("status") in FINISHED or n in blocked)]
        print(f"  {len(runs)} of {len(runnable)} runs started, {len(done)} settled", flush=True)
        if len(done) == len(runnable):
            break
        time.sleep(20)

    for n in runnable:
        r = runs.get(n)
        if not r:
            results[n] = ("no run", "the push started no run of it", [])
            continue
        legs = (api.call("GET", "/api/ci/runs/" + r["id"]).get("legs")) or []
        notes = []
        for leg in legs:
            if leg.get("conclusion") in ("failure", "timed_out", "startup_failure"):
                step, msg = leg_detail(api, leg)
                notes.append(f"`{leg.get('key')}`: {step}" + (f": {msg}" if msg else ""))
        passed = sum(1 for leg in legs if leg.get("conclusion") == "success")
        if r.get("status") not in FINISHED:
            waiting = [leg for leg in legs if leg.get("status") not in FINISHED]
            notes += [f"`{leg.get('key')}` waits: {wait_reason(leg.get('wait_reason'))}" for leg in waiting]
            api.call("POST", f"/api/ci/runs/{r['id']}/cancel", {})
            results[n] = ("blocked" if n in blocked else "waiting", f"{passed} of {len(legs)} legs passed", notes)
            continue
        if r.get("conclusion") == "success":
            results[n] = ("pass", f"{len(legs)} leg{'s' if len(legs) != 1 else ''}", [])
            continue
        if not notes and r.get("error"):
            notes.append(r["error"])
        results[n] = (r.get("conclusion") or r.get("status"), f"{passed} of {len(legs)} legs passed", notes)

    write(results, sha)
    passed = sum(1 for v in results.values() if v[0] == "pass")
    ran = len(runnable)
    print(f"{passed} of {ran} run passed; {len(results) - ran} not run. Wrote {os.path.relpath(OUT, ROOT)}")


def write(results, sha):
    counts = {}
    for v in results.values():
        counts[v[0]] = counts.get(v[0], 0) + 1
    ran = {k: v for k, v in results.items() if v[0] != "not run"}
    lines = [
        "# CI conformance: GitHub's starter workflows",
        "",
        "Generated by `ci/conformance/starters.py` (docs/ci.md W22-AC01) on "
        + datetime.date.today().isoformat()
        + f": each of [actions/starter-workflows](https://github.com/actions/starter-workflows)' workflows (F-03) run on kiln, unmodified,"
        " against the fixture project in `crates/kiln/tests/fixtures/starters`, on `ubuntu-latest` in kiln's runner image."
        f" Commit `{sha[:12]}` of the test repository.",
        "",
        f"**{counts.get('pass', 0)} of {len(ran)} run passed**; " + ", ".join(f"{n} {k}" for k, n in sorted(counts.items()) if k != "pass") + ".",
        "",
        "## Run",
        "",
        "| Workflow | Result | Notes |",
        "| --- | --- | --- |",
    ]
    for n in sorted(ran):
        res, why, notes = ran[n]
        if res != "pass" and n in KNOWN:
            notes = [f"**{KNOWN[n]}**"] + notes
        if res != "pass" and any("API rate limit exceeded" in x for x in notes):
            notes = ["**github.com's limit on anonymous API reads: setup actions send a token only to github.com itself**"] + notes
        text = "; ".join([why] + notes if why else notes).replace("|", "\\|").replace("\n", " ")
        lines.append(f"| `{n}` | {res} | {text} |")
    lines += ["", "## Not run", "", "| Workflow | Why |", "| --- | --- |"]
    for n in sorted(k for k, v in results.items() if v[0] == "not run"):
        lines.append(f"| `{n}` | {results[n][1]} |")
    with open(OUT, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    sys.exit(main())
