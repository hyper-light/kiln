#!/usr/bin/env python3
"""docs/ci.md W22-AC01, the rest of F-03: the workflows of the popular repositories the corpus
took them from (crates/kiln-ci/tests/fixtures/gha/SOURCES.md), each run on a kiln server against
its own repository at the commit the corpus records.

For each repository, one at a time and the lightest first: the tree at that commit becomes a
repository of its own (one commit, no history) under the repository's own name, since workflows
check `github.repository`; the server polls it; a second commit keeps only the workflows F-03
took, and its push runs them. Each workflow's result goes to `<out>/<owner>_<name>.json`, so a
second invocation goes on where the first stopped; then `docs/ci-conformance-corpus.md` is
written from all of them, with the reason for each run that didn't pass.

    python3 -I ci/conformance/corpus.py --server http://127.0.0.1:17070 \\
        --token-file <data_dir>/admin.token --work <a directory for the repositories> \\
        --out <a directory for the results> [--only owner/name ...] [--timeout SECONDS] \\
        [--pass NAME]

The server's runners must route `ubuntu-latest` (`[ci] github_labels = true`) and give legs
Docker, as for the starter workflows (ci/conformance/starters.py). The labels some of the
repositories use for other hosted runners (Depot's, Namespace's, their own names for GitHub's
larger runners) this script routes as GitHub's equivalents, with the server's routing rules
(`PUT /api/ci/routing/rules`, which it replaces), as a user moving to kiln would.
"""

import argparse
import hashlib
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
CORPUS = os.path.join(ROOT, "crates", "kiln-ci", "tests", "fixtures", "gha")
OUT = os.path.join(ROOT, "docs", "ci-conformance-corpus.md")

# The order they run in: the quickest to build first, so a short session still covers many.
ORDER = [
    "BurntSushi/ripgrep", "sharkdp/bat", "rust-lang/cargo", "tokio-rs/tokio", "elixir-lang/elixir",
    "prometheus/prometheus", "docker/buildx", "microsoft/TypeScript", "facebook/react", "neovim/neovim",
    "astral-sh/uv", "astral-sh/ruff", "rust-lang/rust-analyzer", "tauri-apps/tauri", "bevyengine/bevy",
    "denoland/deno", "python/cpython", "rails/rails", "laravel/framework", "home-assistant/core",
    "vercel/next.js", "apache/arrow", "nodejs/node", "pytorch/pytorch",
]
FINISHED = {"succeeded", "failed", "cancelled", "timed_out", "skipped"}

# Other hosted runners' labels in the corpus, by the platform and image of GitHub's equivalent
# (kiln's own rules for GitHub's labels, `github_label_rules`).
HOSTED = {
    ("linux", "x86_64", "kiln/runner-ubuntu-24.04"): [
        "depot-ubuntu-24.04", "depot-ubuntu-24.04-4", "depot-ubuntu-24.04-8", "depot-ubuntu-24.04-16", "depot-ubuntu-latest-4",
        "github-ubuntu-24.04-x86_64-2", "github-ubuntu-24.04-x86_64-4", "github-ubuntu-24.04-x86_64-8", "github-ubuntu-24.04-x86_64-32",
        "ubuntu-24.04-xl", "ubuntu-latest-4-core-oss", "ubuntu-latest-16-core-oss",
    ],
    ("linux", "x86_64", "kiln/runner-ubuntu-22.04"): ["depot-ubuntu-22.04-8", "depot-ubuntu-22.04-16"],
    ("linux", "aarch64", "kiln/runner-ubuntu-24.04"): ["depot-ubuntu-24.04-arm-8", "depot-ubuntu-24.04-arm-16", "github-ubuntu-24.04-aarch64-4", "ubuntu-24.04-arm64-xl"],
    ("linux", "aarch64", "kiln/runner-ubuntu-22.04"): ["depot-ubuntu-22.04-arm-4"],
    ("macos", "aarch64", None): ["namespace-profile-macos-15"],
    ("windows", "x86_64", None): ["namespace-profile-windows-2022-x86-64-4", "namespace-profile-windows-2022-x86-64-16x32", "windows-2022-xl", "windows-latest-8-core-oss"],
    ("windows", "aarch64", None): ["github-windows-11-aarch64-8"],
}

# Failures that aren't kiln's, by what their notes say: the reason the table gives.
KNOWN = [
    (r"deploy-pages|Failed to create deployment \(status: 501\)", "GitHub Pages is outside kiln (docs/ci.md §1.3)"),
    # The corpus submits the whole tree against an empty trunk: a check that compares a pull
    # request with its base finds nothing there to compare.
    (r"xtask-bump-check", "it compares the pull request with its base, an empty trunk in the corpus"),
    # What publishes with the repository's own secrets, which the corpus's copy hasn't.
    (r"you are not authenticated for buf\.build", "it publishes to the Buf registry with the repository's secret (BUF_TOKEN), which the corpus's copy hasn't"),
    (r"Docker config file not found|could not find api key for server quay\.io", "it pushes to Docker Hub and quay.io with the repository's secrets, which the corpus's copy hasn't"),
    # prometheus's: the pull request's description, which the corpus's pull request (the whole
    # tree) hasn't.
    (r"release-notes block empty or not found", "it reads the pull request's description, which the corpus's pull request (the whole tree) hasn't"),
    # bat's: the pull request's own changelog line, by its number and its author on GitHub.
    (r"Search for added line in changelog", "it looks for the pull request's own changelog line (its number, its author from GitHub's API); the corpus's pull request is the whole tree"),
    # A test that waits a few seconds for a message, or a minute for a release to boot, run while
    # the lab's pacer froze its work (the machine's disks busy): to be run again quietly.
    (r"no matching message after \d+ms|ExUnit\.TimeoutError|test timed out after \d+ms", "a test's timed wait, in the lab while its pacer freezes the work when the machine's disks are busy: timing, to be run again when quiet"),
    # buildx's debug adapter tests wait 5 s for a build to end (`require.Fail(t, "timeout reached")`).
    (r"Error:\s+timeout reached", "a test's timed wait (buildx's debug adapter waits 5 s for a build to end), in the lab, which runs at nice 19 and freezes its work while the machine's disks are busy: timing, to be run again when quiet"),
    # CodeMirror parses to a time budget; with no editor view, a test's tree stays as far as it got.
    (r"computeStartCompletePosition test", "CodeMirror's parser works to a time budget, which prometheus's completion test ran out of in the slow lab (its suite took 19 s, 2 s quiet): timing, to be run again when quiet"),
    (r"Condition never satisfied", "a test's timed wait (testify's `Eventually`: prometheus's waits seconds for its server), in the slow lab: timing, to be run again when quiet"),
    (r"panic: test timed out after 10m0s", "a package's tests ran past `go test`'s ten minutes in the lab, which runs at nice 19 with idle I/O and freezes its work while the machine's disks are busy: timing, to be run again when quiet"),
    # The lab's firewall keeps containers on networks of their own from its server, so its legs
    # share the host's network (`KILN_CONTAINER_NETWORK=host`), where what listens on the host
    # answers. Prometheus's Triton test expects nothing at 127.0.0.1:443.
    (r"an error occurred unmarshaling the discovery response json: invalid character '<'", "the lab's legs share the host's network, where a server listens on 443; a hosted runner's own machine has nothing there"),
    # Two legs at once on that network share one EPMD (Erlang's port mapper), and name their
    # nodes alike: elixir's `primary@<host>`.
    (r"seems to be in use by another Erlang node", "two of its legs at once share the lab's host network, so their Erlang nodes share one port mapper and one name; a hosted runner's own machine has one each"),
]


def triggers(text):
    """The events a workflow runs on: `ci/conformance/starters.py`'s reading of its `on`."""
    import importlib.util

    spec = importlib.util.spec_from_file_location("starters", os.path.join(HERE, "starters.py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m.triggers(text)


def no_run(repo, name, event="push"):
    """Why `event` (a push to the default branch, a pull request to it) started no run of a
    workflow."""
    with open(os.path.join(CORPUS, repo.replace("/", "_"), name)) as f:
        on = sorted(triggers(f.read()))
    if event in on:
        return f"its `{event}` filters (branches, tags, paths or types) leave this one out"
    return "it runs on " + ", ".join(f"`{e}`" for e in on)


def sources():
    """`owner/name` → the commit F-03 took its workflows at."""
    out = {}
    with open(os.path.join(CORPUS, "SOURCES.md")) as f:
        for line in f:
            m = re.match(r"^\| `([^`]+)` \| `([0-9a-f]{40})` \|", line)
            if m:
                out[m.group(1)] = m.group(2)
    return out


class Api:
    def __init__(self, server, token):
        self.server = server.rstrip("/")
        self.token = token

    def call(self, method, path, body=None):
        """The server's answer. A server that doesn't answer (the lab frozen by its pacer while the
        machine's disks are busy) is asked again for up to 15 minutes. A retried change the first
        try made already (404 for a DELETE, 409 for a POST) is done."""
        data = json.dumps(body).encode() if body is not None else None
        for attempt in range(31):
            req = urllib.request.Request(self.server + path, data=data, method=method, headers={"Authorization": "Bearer " + self.token})
            if data is not None:
                req.add_header("Content-Type", "application/json")
            try:
                with urllib.request.urlopen(req, timeout=60) as r:
                    raw = r.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as e:
                if attempt and method != "GET" and e.code in (404, 409):
                    return None
                raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')[:300]}") from None
            except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
                if attempt == 30:
                    raise RuntimeError(f"{method} {path}: no answer in 15 minutes ({e})") from None
                print(f"  {method} {path}: no answer ({e}); again in 30 s", flush=True)
                time.sleep(30)
        return None


def route_hosted(api):
    """The server's routing rules: [`HOSTED`]'s labels each routed as its GitHub equivalent."""
    rules = []
    for (os_, arch, image), labels in HOSTED.items():
        for label in labels:
            rule = {"match": {"labels": [label]}, "require": [f"os == {os_}", f"arch == {arch}"]}
            if image:
                rule.update(executor=["docker", "host"], image=image)
            rules.append(rule)
    api.call("PUT", "/api/ci/routing/rules?scope=server", {"rules": rules})
    print(f"routed {len(rules)} other hosted runners' labels as GitHub's", flush=True)


def git(cwd, *args, check=True, env=None):
    env = dict(os.environ, GIT_AUTHOR_NAME="kiln", GIT_AUTHOR_EMAIL="kiln@example.com", GIT_COMMITTER_NAME="kiln", GIT_COMMITTER_EMAIL="kiln@example.com", GIT_TERMINAL_PROMPT="0", **(env or {}))
    r = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr.strip()[:400]}")
    return r.stdout.strip()


def snapshot(work, repo, commit):
    """The tree at `commit` as a repository of one commit, on the upstream's default branch, its
    submodules as the upstream has them (their commits; a checkout fetches them from their URLs,
    as on GitHub). One made before submodules were kept is made again."""
    key = repo.replace("/", "_")
    fetched, dest = os.path.join(work, "fetch", key), os.path.join(work, "repos", key)
    mark = os.path.join(dest, ".git", "kiln-snapshot-submodules")
    if os.path.isdir(dest) and os.path.exists(mark) and git(dest, "rev-parse", "--verify", "-q", "HEAD", check=False):
        return dest, git(dest, "symbolic-ref", "--short", "HEAD")
    shutil.rmtree(fetched, ignore_errors=True)
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(fetched)
    git(fetched, "init", "-q")
    git(fetched, "remote", "add", "origin", f"https://github.com/{repo}.git")
    branch = "main"
    for line in git(fetched, "ls-remote", "--symref", "origin", "HEAD").splitlines():
        m = re.match(r"^ref: refs/heads/(\S+)\s+HEAD$", line)
        if m:
            branch = m.group(1)
    git(fetched, "fetch", "-q", "--depth", "1", "origin", commit)
    git(fetched, "checkout", "-q", "FETCH_HEAD")
    os.makedirs(dest)
    git(dest, "init", "-q", "-b", branch)
    for entry in os.listdir(fetched):
        if entry == ".git":
            continue
        src, dst = os.path.join(fetched, entry), os.path.join(dest, entry)
        if os.path.isdir(src) and not os.path.islink(src):
            shutil.copytree(src, dst, symlinks=True)
        else:
            shutil.copy2(src, dst, follow_symlinks=False)
    git(dest, "add", "-A", "-f")
    # A submodule is a commit in the tree, not files a copy carries.
    for line in git(fetched, "ls-tree", "-r", "FETCH_HEAD").splitlines():
        meta, _, path = line.partition("\t")
        mode, kind, sha = (meta.split() + ["", "", ""])[:3]
        if mode == "160000" and kind == "commit":
            git(dest, "update-index", "--add", "--cacheinfo", f"160000,{sha},{path}")
    git(dest, "commit", "-q", "-m", f"F-03: {repo} at {commit}")
    with open(mark, "w") as f:
        f.write("submodules kept\n")
    shutil.rmtree(fetched, ignore_errors=True)
    return dest, branch


# A version's tag (`v1.2.3`, `1.2`, `release-1.2.3` aside): those a checkout of the whole history
# fetches that a build reads (elixir's docs list them for their version chooser).
VERSION_TAG = re.compile(r"^v?\d+(\.\d+)+([-.+][0-9A-Za-z.-]+)?$")


def upstream_tags(work, repo, commit):
    """The upstream's tag nearest `commit`, as `git describe --tags --abbrev=0` finds it in a
    checkout of the whole history there (`fetch-depth: 0`), and its version tags (a thousand at
    most, the newest first): `(None, [])` without them. Only the commits are fetched, none of
    their files."""
    d = os.path.join(work, "describe", repo.replace("/", "_") + ".git")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.dirname(d), exist_ok=True)
    try:
        git(work, "clone", "-q", "--bare", "--filter=tree:0", f"https://github.com/{repo}.git", d)
        if not git(d, "rev-parse", "-q", "--verify", commit + "^{commit}", check=False):
            git(d, "fetch", "-q", "--filter=tree:0", "origin", commit)
        nearest = git(d, "describe", "--tags", "--abbrev=0", commit, check=False) or None
        tags = [t for t in git(d, "tag", "--sort=-v:refname").splitlines() if VERSION_TAG.match(t)][:1000]
        return nearest, tags
    except RuntimeError as e:
        print(f"{repo}: no tags of the upstream's found ({e})", flush=True)
        return None, []
    finally:
        shutil.rmtree(d, ignore_errors=True)


def version_tags(work, repo, commit, dest):
    """`upstream_tags`, kept with the snapshot at `dest`."""
    mark = os.path.join(dest, ".git", "kiln-snapshot-tags.json")
    if os.path.exists(mark):
        with open(mark) as f:
            kept = json.load(f)
        return kept.get("nearest"), kept.get("tags", [])
    nearest, tags = upstream_tags(work, repo, commit)
    with open(mark, "w") as f:
        json.dump({"nearest": nearest, "tags": tags}, f)
    return nearest, tags


def only_corpus(dest, repo):
    """A commit keeping only the workflows F-03 took of `repo` (the one made before, when there
    is one: its runs are the ones to read); its sha."""
    if git(dest, "log", "-1", "--format=%s") == "F-03: its corpus workflows alone":
        return git(dest, "rev-parse", "HEAD")
    keep = set(os.listdir(os.path.join(CORPUS, repo.replace("/", "_"))))
    wf = os.path.join(dest, ".github", "workflows")
    for f in os.listdir(wf) if os.path.isdir(wf) else []:
        if f not in keep and re.search(r"\.ya?ml$", f):
            git(dest, "rm", "-q", "--cached", os.path.join(".github", "workflows", f))
            os.remove(os.path.join(wf, f))
    git(dest, "commit", "-q", "--allow-empty", "-m", "F-03: its corpus workflows alone")
    return git(dest, "rev-parse", "HEAD")


def stuck(api, run):
    """Whether every leg a run has left waits for a runner no one has (macOS, Windows here)."""
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


# A line that says what went wrong (not a shell's `set -x` trace of what ran after).
ERRORISH = re.compile(r"(?i)\b(error|fatal|not found|failed|panicked|no such|cannot|denied|refused|exception)\b")


# A test runner's own words for a failed test, before the generic last line (make's, an exit code):
# ExUnit's numbered failures (and the line after, what failed), libtest's, pytest's, Go's.
FAILED_TEST = re.compile(r"^\d+\) test .+ \(.+\)$|^test \S+ \.\.\. FAILED$|^FAILED \S+::\S+|^--- FAIL: \S+|^---- \S+ stdout ----$")
GENERIC = re.compile(r"^make(\[\d+\])?: \*\*\*|exit(ed with)? code|exit status \d+|^error: test failed, to rerun pass")


def leg_detail(api, leg):
    """The failing step's name and its last useful line: the last that reads as an error, else
    the last. A generic one (make's, an exit code) is preceded by the first test the log says
    failed, and what failed in it."""
    try:
        f = api.call("GET", f"/api/ci/legs/{leg['id']}/logs/failure")
    except RuntimeError:
        return "", ""
    lines = [re.sub(r"\x1b\[[0-9;]*m", "", x.get("text", "")).strip() for x in (f or {}).get("lines") or []]
    lines = [x for x in lines if x and not x.startswith("[command]")]
    said = next((x for x in reversed(lines) if ERRORISH.search(x) and not x.startswith("+ ")), lines[-1] if lines else "")
    if GENERIC.search(said):
        at = next((i for i, x in enumerate(lines) if FAILED_TEST.search(x)), None)
        if at is not None:
            test = lines[at]
            # ExUnit says what failed on the next line.
            if re.match(r"^\d+\) test ", test) and at + 2 < len(lines):
                test = f"{test}: {lines[at + 2]}" if lines[at + 1].startswith(("test/", "lib/", "/")) else f"{test}: {lines[at + 1]}"
            # libtest, after where the test panicked.
            if m := re.match(r"^---- (\S+) stdout ----$", test):
                panicked = next((i for i in range(at + 1, min(at + 8, len(lines))) if "panicked at" in lines[i]), None)
                test = f"{m.group(1)}: {lines[panicked + 1]}" if panicked is not None and panicked + 1 < len(lines) else m.group(1)
            said = f"{test} ({said})"
    # A line the known failures name, wherever it is in the step's log, goes with it.
    known = next((x for x in lines if any(re.search(pat, x) for pat, _ in KNOWN) and x not in said), None)
    if known and not any(re.search(pat, said) for pat, _ in KNOWN):
        said = f"{said[:180]} — {known[:110]}"
    return (f or {}).get("step_name") or "", said[:300]


# The empty tree: git has it without its being stored.
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
# What the corpus raises, in order: a pull request of the whole tree, then a push of it.
EVENTS = ("pull_request", "push")


def commits(dest, repo, name=""):
    """An empty trunk and, on it, the tree with only F-03's workflows (kept as tags, so a second
    invocation reads the runs of the first): their shas. A pass with a `name` has commits of its
    own (the name in their messages), so its runs are its own, not an earlier pass's."""
    key = f"kiln-corpus-{name}-" if name else "kiln-corpus-"
    have = [git(dest, "rev-parse", "--verify", "-q", f"refs/tags/{key}{t}", check=False) for t in ("trunk", "tree")]
    if all(have):
        return have[0], have[1]
    tree = git(dest, "rev-parse", only_corpus(dest, repo) + "^{tree}")
    stamp = f" (pass {name})" if name else ""
    trunk = git(dest, "commit-tree", EMPTY_TREE, "-m", f"F-03: an empty trunk{stamp}")
    # A diff's commit names its change (W14-R03).
    change = "I" + hashlib.sha1(f"F-03 {repo}".encode()).hexdigest()
    top = git(dest, "commit-tree", tree, "-p", trunk, "-m", f"F-03: {repo}, its corpus workflows alone{stamp}\n\nChange-Id: {change}")
    git(dest, "tag", "-f", f"{key}trunk", trunk)
    git(dest, "tag", "-f", f"{key}tree", top)
    return trunk, top


def native(api, repo, branch):
    """kiln's own repository `repo`, its default branch `branch`, public as the repository is on
    GitHub (one of that name polled from elsewhere, from an earlier pass, on another branch, or
    private, goes)."""
    found = next((r for r in api.call("GET", "/api/repos") or [] if r["name"] == repo), None)
    if found and (found.get("mode") != "native" or found.get("default_branch") != branch or not found.get("public")):
        api.call("DELETE", "/api/repos/" + found["id"])
        found = None
    if not found:
        api.call("POST", "/api/repos", {"name": repo, "mode": "native", "default_branch": branch, "public": True})


def push(api, dest, repo, *refspecs):
    """Pushes to kiln's repository `repo`, the token in a header (not in the URL)."""
    import base64

    basic = base64.b64encode(f"x:{api.token}".encode()).decode()
    env = {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "http.extraHeader", "GIT_CONFIG_VALUE_0": "Authorization: Basic " + basic}
    git(dest, "push", "-q", "-f", f"{api.server}/git/{repo}.git", *refspecs, env=env)


def settle(api, repo, event, sha, timeout):
    """`repo`'s runs of `event` at `sha` (a pull request's commit is the diff's, the one pushed),
    by workflow file, once every one has settled; and those blocked. Runs an earlier invocation
    started count: a second one goes on where the first stopped."""
    runs, quiet_since, blocked = {}, time.time(), set()
    deadline = time.time() + timeout
    while time.time() < deadline:
        listing = api.call("GET", "/api/ci/runs?" + urllib.parse.urlencode({"repo": repo, "limit": 200}))["runs"]
        before = len(runs)
        for r in listing:
            if r.get("event") == event and r.get("sha") == sha:
                runs[os.path.basename(r.get("workflow", ""))] = r
        if len(runs) != before:
            quiet_since = time.time()
        for n, r in runs.items():
            if r.get("status") not in FINISHED and n not in blocked and stuck(api, r):
                blocked.add(n)
        settled = [n for n, r in runs.items() if r.get("status") in FINISHED or n in blocked]
        print(f"  {event}: {len(runs)} runs, {len(settled)} settled", flush=True)
        # Every run settled, and no new one for two minutes: done.
        if runs and len(settled) == len(runs) and time.time() - quiet_since > 120:
            break
        # Nothing started at all within ten minutes: nothing will.
        if not runs and time.time() - quiet_since > 600:
            break
        time.sleep(30)
    return runs, blocked


def results(api, repo, event, corpus, runs, blocked):
    """Each corpus workflow's result for `event`."""
    out = {}
    for n in corpus:
        r = runs.get(n)
        if not r:
            out[n] = {"result": "no run", "notes": [no_run(repo, n, event)]}
            continue
        legs = (api.call("GET", "/api/ci/runs/" + r["id"]).get("legs")) or []
        notes = []
        for leg in legs:
            if leg.get("conclusion") in ("failure", "timed_out", "startup_failure"):
                step, msg = leg_detail(api, leg)
                notes.append(f"`{leg.get('key')}`: {step}" + (f": {msg}" if msg else ""))
            elif leg.get("status") not in FINISHED:
                notes.append(f"`{leg.get('key')}` waits: {wait_reason(leg.get('wait_reason'))}")
        passed = sum(1 for leg in legs if leg.get("conclusion") == "success")
        if r.get("status") not in FINISHED:
            api.call("POST", f"/api/ci/runs/{r['id']}/cancel", {})
            out[n] = {"result": "blocked" if n in blocked else "unfinished", "legs": f"{passed} of {len(legs)} legs passed", "notes": notes}
            continue
        if not notes and r.get("error"):
            notes.append(r["error"])
        out[n] = {"result": "pass" if r.get("conclusion") == "success" else (r.get("conclusion") or r.get("status")), "legs": f"{passed} of {len(legs)} legs passed", "notes": notes, "run": r["id"]}
    return out


def run_repo(api, args, repo, commit):
    """Runs `repo`'s corpus workflows, as kiln's own repository: the whole tree submitted for
    review against an empty trunk (`pull_request`, every file changed), then pushed to the trunk
    (`push`). Their results, by event."""
    print(f"{repo}: preparing", flush=True)
    dest, branch = snapshot(args.work, repo, commit)
    trunk, top = commits(dest, repo, args.pass_name or "")
    native(api, repo, branch)
    push(api, dest, repo, f"{trunk}:refs/heads/{branch}")
    # The upstream's tags, on the trunk, as a checkout of the whole history (`fetch-depth: 0`) has
    # them on GitHub: a build that says its version by `git describe` (buildx's, which compose
    # checks) says `<nearest>-1-g<sha>` there, not a bare sha, and one that lists the versions
    # (elixir's docs) finds them. The nearest is annotated, which `describe` takes over the others
    # on the same commit (and without `--tags`, alone). The trunk has no workflows: these pushes
    # run nothing.
    nearest, tags = version_tags(args.work, repo, commit, dest)
    for t in tags:
        if t != nearest:
            git(dest, "tag", "-f", t, trunk)
    if nearest:
        git(dest, "tag", "-f", "-a", "-m", f"F-03: {repo}'s nearest tag upstream", nearest, trunk)
    names = [t for t in tags if t != nearest] + ([nearest] if nearest else [])
    for i in range(0, len(names), 200):
        push(api, dest, repo, *[f"refs/tags/{t}:refs/tags/{t}" for t in names[i : i + 200]])
    corpus = sorted(f for f in os.listdir(os.path.join(CORPUS, repo.replace("/", "_"))) if re.search(r"\.ya?ml$", f))
    out = {"repo": repo, "commit": commit, "branch": branch, "sha": top, "events": {}}
    for event in EVENTS:
        push(api, dest, repo, f"{top}:refs/{'for' if event == 'pull_request' else 'heads'}/{branch}")
        print(f"{repo}: {event} of {top[:8]} on {branch}; waiting for its runs", flush=True)
        runs, blocked = settle(api, repo, event, top, args.timeout)
        out["events"][event] = results(api, repo, event, corpus, runs, blocked)
    return out


# A leg that waits for a machine the lab hasn't: macOS's or Windows's.
ELSEWHERE = re.compile(r"` waits: no runner meets its requirements \(.*\b(os == (macos|windows)|runs-on \[[^\]]*(macos|windows))")


def not_kilns(note):
    """Why a leg's miss (its note) isn't kiln's to fix, or None when it is."""
    if ELSEWHERE.search(note):
        return "it waits for macOS or Windows, which the lab hasn't (its runner is Linux's)"
    return next((r for pat, r in KNOWN if re.search(pat, note)), None)


def miss(w):
    """A workflow's result for an event, a miss: the reasons it's not kiln's, every leg's that
    missed, or None when one of them is kiln's (or none says why)."""
    reasons = [not_kilns(x) for x in w.get("notes", [])]
    return sorted(set(reasons)) if reasons and all(reasons) else None


def write(out_dir):
    done = []
    for repo in ORDER:
        p = os.path.join(out_dir, repo.replace("/", "_") + ".json")
        if os.path.exists(p):
            with open(p) as f:
                d = json.load(f)
            # An earlier pass ran pushes only (from a polled repository).
            d.setdefault("events", {"push": d.get("workflows", {})})
            done.append(d)
    rows = [(d, n) for d in done for n in sorted({n for e in d["events"].values() for n in e})]
    ran = [(d, n) for d, n in rows if any(e.get(n, {}).get("result") != "no run" for e in d["events"].values())]
    passed = [(d, n) for d, n in ran if all(e.get(n, {}).get("result") in ("pass", "no run", None) for e in d["events"].values())]
    missed = [(d, n) for d, n in ran if (d, n) not in passed]
    explained = [(d, n) for d, n in missed if all(e.get(n, {}).get("result") in ("pass", "no run", None) or miss(e[n]) for e in d["events"].values())]
    lines = [
        "# CI conformance: F-03's popular repositories",
        "",
        "Generated by `ci/conformance/corpus.py` (docs/ci.md W22-AC01): the workflows F-03 took of",
        "each repository, run unmodified on kiln against that repository at the commit F-03 took them",
        "at, as kiln's own repository: the whole tree submitted for review against an empty trunk",
        "(`pull_request`, every file changed), then pushed to the trunk (`push`). The starter workflows",
        "are in `docs/ci-conformance.md`.",
        "",
        f"**{len(passed)} of {len(ran)} workflows that ran passed.** {len(explained)} more missed only for reasons not kiln's, each leg's named below,"
        f" and {len(missed) - len(explained)} for one that is kiln's to fix ({len(rows)} in all) in {len(done)} of {len(ORDER)} repositories run so far.",
        "",
        "A cell says why each of its legs that missed did, when none is kiln's; `kiln's` when one is, or says nothing.",
        "",
    ]
    for d in done:
        events = [e for e in EVENTS if e in d["events"]]
        lines += [f"## `{d['repo']}` at `{d['commit'][:12]}`", "", "| Workflow | " + " | ".join(f"`{e}`" for e in events) + " | Notes |", "| --- | " + " | ".join("---" for _ in events) + " | --- |"]
        for n in sorted({n for e in d["events"].values() for n in e}):
            cells, notes = [], []
            for e in events:
                w = d["events"][e].get(n, {"result": "no run", "notes": []})
                said = w.get("notes", [])
                if w["result"] == "no run":
                    said = [no_run(d["repo"], n, e)]
                result = w["result"]
                if result not in ("pass", "no run"):
                    why = miss(w)
                    result += f": {'; '.join(why)}" if why else ": kiln's"
                cells.append(result)
                said = ([w["legs"]] if w.get("legs") else []) + said
                if said:
                    notes.append(f"{e}: " + "; ".join(said) if len(events) > 1 else "; ".join(said))
            text = " / ".join(notes).replace("|", "\\|").replace("\n", " ")
            lines.append(f"| `{n}` | " + " | ".join(cells) + f" | {text} |")
        lines.append("")
    with open(OUT, "w") as f:
        f.write("\n".join(lines))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", required=True)
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--work", required=True, help="where the repositories are made")
    ap.add_argument("--out", required=True, help="where each repository's results go")
    ap.add_argument("--only", nargs="*", help="these repositories (owner/name)")
    ap.add_argument("--timeout", type=int, default=7200, help="seconds to wait for one repository's runs")
    ap.add_argument("--pass", dest="pass_name", help="a pass of its own (new commits, so new runs, as after a change to kiln); the same name and --out go on with it")
    args = ap.parse_args()
    api = Api(args.server, open(args.token_file).read().strip())
    os.makedirs(args.work, exist_ok=True)
    os.makedirs(args.out, exist_ok=True)
    srcs = sources()
    route_hosted(api)
    for repo in ORDER:
        if args.only and repo not in args.only:
            continue
        p = os.path.join(args.out, repo.replace("/", "_") + ".json")
        if os.path.exists(p):
            continue
        try:
            res = run_repo(api, args, repo, srcs[repo])
        except Exception as e:  # noqa: BLE001 — one repository's failure is its result
            res = {"repo": repo, "commit": srcs[repo], "workflows": {}, "error": str(e)}
            print(f"{repo}: {e}", file=sys.stderr, flush=True)
            continue
        with open(p, "w") as f:
            json.dump(res, f, indent=1)
        write(args.out)
    write(args.out)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
