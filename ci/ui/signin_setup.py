"""W10's sign-in against a real server whose address is http://localhost:PORT (WebAuthn needs a
secure page: localhost is one, an IP address isn't), with GitHub SSO configured (its button shows)
and a person, lead, who holds the owner role. Prints what the browser check needs as JSON.

    python3 ci/ui/signin_setup.py target/debug/kiln > /tmp/signin.json
    docker run --rm --network host -e SIGNIN="$(cat /tmp/signin.json)" \
      -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/signin-shots:/shots \
      mcr.microsoft.com/playwright:v1.64.1-noble node /check/signin_check.mjs

The kiln binary should carry the UI server (built after `npm run build` in ui/). The server runs
from `ci/ui/run/signin/` (its own copy of the binary); stop it with
`kill $(cat ci/ui/run/signin/pid)`."""
import json, os, shutil, socket, subprocess, sys, time, urllib.request

here = os.path.dirname(os.path.abspath(__file__))
kiln_src = sys.argv[1]
work = os.environ.get("KILN_UI_RUN") or os.path.join(here, "run", "signin")
shutil.rmtree(work, ignore_errors=True)
os.makedirs(work)


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


port, quic = free(), free()
data = os.path.join(work, "data")
secret = os.path.join(work, "github-secret")
open(secret, "w").write("not-a-real-secret\n")
cfg = os.path.join(work, "kiln.toml")
open(cfg, "w").write(f"""[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://localhost:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = false

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

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

[sso.github]
client_id = "kiln-check"
client_secret_file = "{secret}"
""")
# Its own binary name, so nothing that looks for kiln's processes takes it for another's.
binary = os.path.join(work, "kiln-signin-ui")
shutil.copy(kiln_src, binary)
home = os.path.join(work, "home")
os.makedirs(home)
env = {k: v for k, v in os.environ.items() if not k.startswith("KILN_")}
env.update(HOME=home, NO_COLOR="1")
log = open(os.path.join(work, "server.log"), "w")
proc = subprocess.Popen([binary, "--config", cfg, "server"], env=env, stdout=log, stderr=log, cwd=work, start_new_session=True)
open(os.path.join(work, "pid"), "w").write(str(proc.pid))
base = f"http://localhost:{port}"


def call(method, path, token, body=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}" + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        t = r.read()
        return json.loads(t) if t else None


deadline = time.time() + 120
admin = None
while time.time() < deadline:
    try:
        admin = open(os.path.join(data, "admin.token")).read().strip()
        urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=5)
        break
    except Exception:
        time.sleep(0.5)
assert admin, "the server didn't start"
call("POST", "/api/users", admin, {"name": "lead", "grants": ["owner"]})
call("POST", "/api/users", admin, {"name": "dev", "grants": ["viewer"]})
# A repository kiln hosts, with a workflow on its trunk: the people check's secrets on its page,
# and its editor's diffs (W09-R11, W21-R06).
call("POST", "/api/repos", admin, {"name": "acme/people", "mode": "native"})
src = os.path.join(work, "people-src")
os.makedirs(os.path.join(src, ".kiln"))
open(os.path.join(src, ".kiln", "ci.yaml"), "w").write("on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo hi\n")
genv = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="lead", GIT_AUTHOR_EMAIL="lead@example.invalid", GIT_COMMITTER_NAME="lead", GIT_COMMITTER_EMAIL="lead@example.invalid")
for args in (["init", "-q", "-b", "main"], ["add", "."], ["commit", "-qm", "A workflow"], ["push", "-q", f"http://x:{admin}@127.0.0.1:{port}/git/acme/people.git", "main"]):
    subprocess.run(["git", "-C", src, *args], env=genv, check=True, capture_output=True)
print(json.dumps({"base": base, "admin": admin, "pid": proc.pid}))
