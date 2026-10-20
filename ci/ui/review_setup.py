"""W14-AC06 against a real server: a native repository and a diff with a 10,000-line change, a
moved block, a word-level edit, a generated file and an image, and a reviewer's thread waiting for
the author. Prints what the browser check (review_ac06.mjs) needs as JSON.

    python3 ci/ui/review_setup.py target/debug/kiln > /tmp/ac06.json
    docker run --rm --network host --ipc host -e AC06="$(cat /tmp/ac06.json)" \
      -v $PWD/ci/ui/review_ac06.mjs:/work/ac06.mjs:ro -v <dir with node_modules/playwright-core>:/pw:ro \
      mcr.microsoft.com/playwright:v1.64.1-noble node /work/ac06.mjs

The kiln binary should carry the UI server (built after `npm run build` in ui/), so pages are
rendered as in production. The server runs from `ci/ui/run/` (its own copy of the binary); stop
it with `kill $(cat ci/ui/run/pid)`."""
import json, os, shutil, socket, struct, subprocess, sys, time, urllib.request, zlib

here = os.path.dirname(os.path.abspath(__file__))
kiln_src = sys.argv[1]
work = os.environ.get("AC06_DIR") or os.path.join(here, "run")
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
""")
# Its own binary name, so nothing that looks for kiln's processes takes it for another's.
binary = os.path.join(work, "kiln-ac06")
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


def person(name, grants):
    call("POST", "/api/users", admin, {"name": name, "grants": grants})
    t = call("POST", "/api/tokens", admin, {"kind": "pat", "user": name, "grants": ["owner"]})
    return t.get("token") or t.get("secret")


dev = person("dev", ["developer@repo:acme/app"])
rev = person("rev", ["developer@repo:acme/app"])


def git(cwd, *args):
    out = subprocess.run(["git", "-c", "user.name=Dev", "-c", "user.email=dev@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", *args], cwd=cwd, capture_output=True, text=True, env={**env, "GIT_TERMINAL_PROMPT": "0"})
    assert out.returncode == 0, (args, out.stdout, out.stderr)
    return out.stdout


def png(r, g, b):
    raw = b"\x00" + bytes([r, g, b]) * 16
    rows = raw * 16
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


url = f"http://x:{dev}@127.0.0.1:{port}/git/acme/app.git"
git(work, "clone", "-q", url, "repo")
repo = os.path.join(work, "repo")
w = lambda p, t: (os.makedirs(os.path.dirname(os.path.join(repo, p)) or ".", exist_ok=True), open(os.path.join(repo, p), "wb" if isinstance(t, bytes) else "w").write(t))
w(".gitattributes", "dist/** linguist-generated\n")
w("big.txt", "".join(f"line {i}: the quick brown fox jumps over the lazy dog\n" for i in range(10_000)))
blocks = {n: "".join(f"    let {n.lower()}{i} = {i};\n" for i in range(8)) for n in "ABC"}
w("moved.rs", "fn a() {\n" + blocks["A"] + "}\n\nfn b() {\n" + blocks["B"] + "}\n\nfn c() {\n" + blocks["C"] + "}\n")
w("words.txt", "the colour of the sky is blue\nand the grass is green\n")
w("logo.png", png(200, 30, 30))
w("dist/gen.js", "built(1);\n")
lock = lambda pkgs: "version = 3\n\n" + "".join(f'[[package]]\nname = "{n}"\nversion = "{v}"\n\n' for n, v in pkgs)
w("Cargo.lock", lock([("app", "0.1.0"), ("rand", "0.8.5"), ("serde", "1.0.200")]))
git(repo, "add", ".")
git(repo, "commit", "-qm", "first")
git(repo, "push", "-q", "origin", "main")
git(repo, "checkout", "-qb", "change")
w("big.txt", "".join(f"line {i}: the quick red fox jumps over the lazy dog\n" for i in range(10_000)))
w("moved.rs", "fn a() {\n" + blocks["A"] + "}\n\nfn c() {\n" + blocks["C"] + "}\n\nfn b() {\n" + blocks["B"] + "}\n")
w("words.txt", "the color of the sky is blue\nand the grass is green\n")
w("logo.png", png(30, 30, 200))
w("dist/gen.js", "built(2);\n")
w("Cargo.lock", lock([("app", "0.1.0"), ("serde", "1.0.210"), ("tokio", "1.40.0")]))
git(repo, "add", ".")
git(repo, "commit", "-qm", "colors and moves\n\nChange-Id: I5555555555555555555555555555555555555555")
out = subprocess.run(["git", "-c", "credential.helper=", "push", "origin", "HEAD:refs/for/main"], cwd=repo, capture_output=True, text=True, env=env)
assert out.returncode == 0, out.stderr
# The reviewer asks something on the word that changed: a thread waiting for the author's reply.
call("POST", "/api/diffs/D1/comments", rev, {"file": "words.txt", "side": "new", "start_line": 1, "end_line": 1, "body": "Why the American spelling?", "publish": True})
print(json.dumps({"base": base, "dev": dev, "rev": rev, "admin": admin, "pid": proc.pid}))
