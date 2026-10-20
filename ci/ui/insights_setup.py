"""W16's repository tests and metric trends against a real server: eight runs of a workflow that
reports tests (one always failing, one flipping, two slow) and three metrics, on the server's own
runner; then a second kind of runner's values of the same metrics, spread over ten days, put in
the store. Prints what the browser check needs as JSON.

    python3 ci/ui/insights_setup.py target/debug/kiln > /tmp/insights.json
    docker run --rm --network host -e INSIGHTS="$(cat /tmp/insights.json)" \
      -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/insights-shots:/shots \
      mcr.microsoft.com/playwright:v1.64.1-noble node /check/insights_check.mjs

The kiln binary should carry the UI server (built after `npm run build` in ui/). The server runs
from `ci/ui/run/insights/` (its own copy of the binary); stop it with
`kill $(cat ci/ui/run/insights/pid)`."""
import glob, json, os, shutil, socket, sqlite3, subprocess, sys, time, urllib.request

here = os.path.dirname(os.path.abspath(__file__))
kiln_src = sys.argv[1]
work = os.environ.get("KILN_UI_RUN") or os.path.join(here, "run", "insights")
shutil.rmtree(work, ignore_errors=True)
os.makedirs(work)


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


port, quic, issuer_port = free(), free(), free()
data = os.path.join(work, "data")
runs_dir = os.path.join(work, "work")
cfg = os.path.join(work, "kiln.toml")
open(cfg, "w").write(f"""[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = false

[gitops]
auto_pipelines = false

[local_agent]
enabled = true
labels = ["local"]
slots = 2
work_dir = "{runs_dir}"

[kubernetes]
enabled = false

[terminal]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = true
kiln_workflows = true

[ci.oidc]
listen = ["127.0.0.1:{issuer_port}"]
publish_to = "{os.path.join(work, 'oidc-published')}"
""")
# Its own binary name, so nothing that looks for kiln's processes takes it for another's.
binary = os.path.join(work, "kiln-insights-ui")
shutil.copy(kiln_src, binary)
home = os.path.join(work, "home")
os.makedirs(home)
env = {k: v for k, v in os.environ.items() if not k.startswith("KILN_")}
env.update(HOME=home, NO_COLOR="1")
log = open(os.path.join(work, "server.log"), "w")
proc = subprocess.Popen([binary, "--config", cfg, "server"], env=env, stdout=log, stderr=log, cwd=work, start_new_session=True)
open(os.path.join(work, "pid"), "w").write(str(proc.pid))
base = f"http://127.0.0.1:{port}"


def call(method, path, token, body=None):
    req = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        t = r.read()
        return json.loads(t) if t else None


deadline = time.time() + 120
admin = None
while time.time() < deadline:
    try:
        admin = open(os.path.join(data, "admin.token")).read().strip()
        urllib.request.urlopen(base + "/healthz", timeout=5)
        break
    except Exception:
        time.sleep(0.5)
assert admin, "the server didn't start"
call("POST", "/api/repos", admin, {"name": "acme/app", "mode": "native"})
call("POST", "/api/users", admin, {"name": "lead", "grants": ["owner"]})
call("POST", "/api/users", admin, {"name": "dev", "grants": ["developer@repo:acme/app"]})


def git(cwd, *args):
    out = subprocess.run(["git", "-c", "user.name=Dev", "-c", "user.email=dev@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", *args], cwd=cwd, capture_output=True, text=True, env={**env, "GIT_TERMINAL_PROMPT": "0"})
    assert out.returncode == 0, (args, out.stdout, out.stderr)
    return out.stdout


url = f"http://x:{admin}@127.0.0.1:{port}/git/acme/app.git"
repo = os.path.join(work, "repo")
os.makedirs(repo)
git(repo, "init", "-q", "-b", "main")
w = lambda p, t: (os.makedirs(os.path.dirname(os.path.join(repo, p)) or repo, exist_ok=True), open(os.path.join(repo, p), "w").write(t))
w("README.md", "app\n")
git(repo, "add", ".")
git(repo, "commit", "-qm", "first")
git(repo, "push", "-q", url, "main")
deadline = time.time() + 60
while time.time() < deadline:
    rs = call("GET", "/api/repos", admin)
    rs = rs.get("repos", rs) if isinstance(rs, dict) else rs
    if any(r.get("name") == "acme/app" and r.get("last_polled_at") for r in rs):
        break
    time.sleep(0.5)

# The repository's own action, which each run pins: a rerun may look it up again (W07-R02).
w(".kiln/actions/note/action.yml", """name: note
description: A line in the log
runs:
  using: composite
  steps:
    - shell: bash
      run: echo "a note from the repository's own action"
""")
# Round n's report: `rounds` always fails, `divides` fails on odd rounds, two tests are slow.
w(".kiln/bench.yaml", r"""on: [push, diff]
jobs:
  bench:
    runs-on: local
    bench-gate: { metric: "parse*", max-regression: 5% }
    steps:
      - uses: $/.kiln/actions/note
      - test-results: { path: report.xml }
        run: |
          n=$(cat round.txt)
          if [ $((n % 2)) -eq 1 ]; then div='<failure message="off by one">expected 2, got 3</failure>'; fails=2; else div=''; fails=1; fi
          printf '%s' "<?xml version=\"1.0\"?><testsuite name=\"calc\" tests=\"5\" failures=\"$fails\"><testcase classname=\"calc\" name=\"adds\" file=\"src/calc.rs\" time=\"0.012\"/><testcase classname=\"calc\" name=\"divides\" file=\"src/calc.rs\" time=\"0.034\">$div</testcase><testcase classname=\"calc\" name=\"rounds\" file=\"src/calc.rs\" time=\"0.008\"><failure message=\"rounding\">expected 1.5, got 1.4</failure></testcase><testcase classname=\"parse\" name=\"parses a large input\" file=\"src/parse.rs\" time=\"2.41\"/><testcase classname=\"net\" name=\"retries a slow network\" file=\"src/net.rs\" time=\"7.5\"/></testsuite>" > report.xml
          printf '{"name":"parse/large_file","value":%d,"unit":"ns","better":"lower"}\n' $((1250000 + n * 5000 + (n % 3) * 3000)) >> "$KILN_METRICS"
          printf '{"name":"throughput","value":%d,"unit":"ops/s","better":"higher"}\n' $((5400 - n * 30)) >> "$KILN_METRICS"
          printf '{"name":"startup","value":%d,"unit":"ms","better":"lower"}\n' $((420 + (n % 4) * 6)) >> "$KILN_METRICS"
""")
rounds = 8
runs = []
for n in range(1, rounds + 1):
    w("round.txt", f"{n}\n")
    git(repo, "add", ".")
    git(repo, "commit", "-qm", f"round {n}")
    git(repo, "push", "-q", url, "main")
    sha = git(repo, "rev-parse", "HEAD").strip()
    deadline = time.time() + 240
    while True:
        rl = call("GET", "/api/ci/runs?repo=acme/app&limit=20", admin).get("runs", [])
        done = [r for r in rl if r.get("sha") == sha and r.get("workflow", "").endswith("bench.yaml") and r.get("finished_at")]
        if done:
            runs.append(done[0]["id"])
            break
        assert time.time() < deadline, f"round {n} didn't finish: {rl}"
        time.sleep(0.5)

# A diff 20 % slower: its run fails the gate, which its review page shows.
git(repo, "checkout", "-qb", "slower")
w("round.txt", "30\n")
git(repo, "add", ".")
git(repo, "commit", "-qm", "Parse more carefully\n\nChange-Id: I" + "8" * 40)
git(repo, "push", "-q", url, "HEAD:refs/for/main")
slow = git(repo, "rev-parse", "HEAD").strip()
deadline = time.time() + 240
while True:
    rl = call("GET", "/api/ci/runs?repo=acme/app&limit=30", admin).get("runs", [])
    if any(r.get("sha") == slow and r.get("finished_at") for r in rl):
        break
    assert time.time() < deadline, f"the diff's run didn't finish: {rl}"
    time.sleep(0.5)

# A second kind of runner: the same metrics, a little faster, one a day over the last ten days.
dbs = [p for p in glob.glob(os.path.join(data, "**", "*.db"), recursive=True)]
db = None
for p in dbs:
    c = sqlite3.connect(p, timeout=30)
    try:
        if c.execute("SELECT COUNT(*) FROM metrics").fetchone()[0] > 0:
            db = c
            break
    except sqlite3.Error:
        pass
    c.close()
assert db, f"no store with metrics among {dbs}"
other = "linux/aarch64 · Neoverse-N1 · 6.8.0-1012-aws · host"
rows = db.execute("SELECT repo_id, run_id, leg, leg_key, attempt, branch, sha, step_idx, name, value, unit, better, tags, ts FROM metrics ORDER BY ts").fetchall()
now = int(time.time())
by_name = {}
for r in rows:
    by_name.setdefault(r[8], []).append(r)
for name, rs in by_name.items():
    for i, r in enumerate(rs):
        ts = now - (len(rs) - i) * 86400 - 3600
        db.execute(
            "INSERT INTO metrics (repo_id, run_id, leg, leg_key, attempt, branch, sha, step_idx, name, value, unit, better, tags, samples, fingerprint, comparison, ts) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,?,NULL,?)",
            (*r[:9], r[9] * 0.82, r[10], r[11], r[12], other, ts),
        )
db.commit()
fp = db.execute("SELECT DISTINCT fingerprint FROM metrics").fetchall()
db.close()
print(json.dumps({"base": base, "admin": admin, "pid": proc.pid, "runs": runs, "fingerprints": [f[0] for f in fp]}))
