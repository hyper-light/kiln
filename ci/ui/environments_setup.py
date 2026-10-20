# W21-R10's environments (docs/ci.md W15-R08, R09), on signin_setup.py's server: a repository kiln
# hosts with the starter set (dev, staging, production), a host environment made in kiln (web, on
# runners labelled deploy-web), one its trunk's .kiln/environments.yaml defines (cluster: kiln
# CD's in-cluster, namespace web), and a run whose job deploying to production waits for its
# review (the server has no runner: a deployment waits for its environment first). And a fresh
# repository each for the check's four browsers and sizes to make a starter set in.
# Prints {"base", "admin", "repo", "run", "fresh": {"chromium-wide": name, …}}.
#
#     SIGNIN="$(cat <signin_setup.py's output>)" python3 -I ci/ui/environments_setup.py
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

cfg = json.loads(os.environ["SIGNIN"])
base, admin = cfg["base"], cfg["admin"]
REPO = "acme/stages"


def call(method, path, body=None):
    req = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"Authorization": f"Bearer {admin}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read() or b"null")


def git(src, *args):
    genv = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="lead", GIT_AUTHOR_EMAIL="lead@example.invalid", GIT_COMMITTER_NAME="lead", GIT_COMMITTER_EMAIL="lead@example.invalid")
    subprocess.run(["git", "-C", src, *args], env=genv, check=True, capture_output=True)


def repo(name):
    if not any(r["name"] == name for r in call("GET", "/api/repos")):
        call("POST", "/api/repos", {"name": name, "mode": "native"})


fresh = {f"{b}-{t}": f"acme/fresh-{b}-{t}" for b in ["chromium", "webkit"] for t in ["wide", "phone"]}
for name in [REPO, *fresh.values()]:
    repo(name)

src = tempfile.mkdtemp(prefix="stages-")
url = f"http://x:{admin}@{base.split('://', 1)[1]}/git/{REPO}.git"
# kiln takes what a repository has on its first look, and runs what comes after: a first commit,
# looked at, then the environments and the workflow.
git(src, "init", "-q", "-b", "main")
open(os.path.join(src, "README.md"), "w").write("stages\n")
git(src, "add", ".")
git(src, "commit", "-qm", "first")
git(src, "push", "-q", "--force", url, "main")
seen = time.time() + 60
while not any(r["name"] == REPO and r.get("last_polled_at") for r in call("GET", "/api/repos")):
    if time.time() > seen:
        sys.exit(f"{REPO} was never looked at")
    time.sleep(0.5)
time.sleep(2)

call("POST", f"/api/ci/environments/starter?repo={REPO}", {})
call("PUT", f"/api/ci/environments/web?repo={REPO}", {"kind": "host", "targets": {"runners": "label deploy-web"}, "protection": {}})
os.makedirs(os.path.join(src, ".kiln"))
os.makedirs(os.path.join(src, ".github", "workflows"))
open(os.path.join(src, ".kiln", "environments.yaml"), "w").write("cluster:\n  clusters: [in-cluster]\n  namespace: web\n")
open(os.path.join(src, ".github", "workflows", "deploys.yml"), "w").write("on: push\njobs:\n  toprod:\n    runs-on: local\n    environment: production\n    steps:\n      - run: echo shipping\n")
git(src, "add", ".")
git(src, "commit", "-qm", "deploys")
git(src, "push", "-q", url, "main")

deadline = time.time() + 180
run = None
while run is None:
    for r in call("GET", f"/api/ci/runs?repo={REPO}&limit=5").get("runs") or []:
        if r.get("workflow", "").endswith("deploys.yml"):
            legs = call("GET", f"/api/ci/runs/{r['id']}").get("legs") or []
            if any((l.get("wait_reason") or {}).get("reason") == "approval" for l in legs):
                run = r["id"]
    if run is None:
        if time.time() > deadline:
            sys.exit(f"{REPO}: no deployment waiting for its review")
        time.sleep(1)
print(json.dumps({"base": base, "admin": admin, "repo": REPO, "run": run, "fresh": fresh}))
