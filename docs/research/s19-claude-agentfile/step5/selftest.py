#!/usr/bin/env python3
"""Step 5's checks of scan.py itself (plan 01a1cf48e8157494ac9a51f7a564f526).

Each scan.py verdict is held to small inputs made here, clean and mutated. That includes the mutations the real
runs can't make: a key-shaped value in a file or in the pinned program, a sign-in file, a harness started with
other arguments, and a HOMEDIR missing what the probe writes. Key-shaped strings are put together as it runs, so
this file holds none whole, and each finding's output is checked not to hold one either.

    python3 -I selftest.py SCAN_PY WORK
"""
import gzip
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile

scan, work = sys.argv[1:]
os.makedirs(work, exist_ok=True)
results = []


def run(*args):
    p = subprocess.run([sys.executable, "-I", scan, *args], capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def check(name, ok, out=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'} {name}")
    if not ok:
        print(out)


def put(path, data, mode=0o644):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(path, mode)


def tar_bytes(files):
    """A tar of files, each bytes, or ("symlink", target) for a link."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as t:
        for p, data in files.items():
            ti = tarfile.TarInfo(p)
            if isinstance(data, tuple):
                ti.type, ti.linkname = tarfile.SYMTYPE, data[1]
                t.addfile(ti)
                continue
            ti.size, ti.mode = len(data), 0o644
            t.addfile(ti, io.BytesIO(data))
    return raw.getvalue()


def layout(path, platforms):
    """An OCI layout tar, as shards save writes one: index.json naming one index of the platforms given, each
    (arch, env, files) a manifest with its config and a gzip layer of files, or one for each of a list of them."""
    store = {}

    def blob(data, mt):
        d = hashlib.sha256(data).hexdigest()
        store[f"blobs/sha256/{d}"] = data
        return {"mediaType": mt, "digest": f"sha256:{d}", "size": len(data)}

    mans = []
    for arch, env, files in platforms:
        cfg = blob(json.dumps({"architecture": arch, "os": "linux", "config": {"Env": env}}).encode(), "application/vnd.oci.image.config.v1+json")
        lays = [blob(gzip.compress(tar_bytes(f)), "application/vnd.oci.image.layer.v1.tar+gzip") for f in (files if isinstance(files, list) else [files])]
        man = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json", "config": cfg, "layers": lays}
        md = blob(json.dumps(man).encode(), "application/vnd.oci.image.manifest.v1+json")
        md["platform"] = {"os": "linux", "architecture": arch}
        mans.append(md)
    idx = blob(json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": mans}).encode(), "application/vnd.oci.image.index.v1+json")
    store["index.json"] = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [idx]}).encode()
    store["oci-layout"] = b'{"imageLayoutVersion":"1.0.0"}'
    with tarfile.open(path, "w") as t:
        for name in sorted(store):
            ti = tarfile.TarInfo(name)
            ti.size = len(store[name])
            t.addfile(ti, io.BytesIO(store[name]))
    return path


def pin_of(*programs):
    path = os.path.join(work, f"pin-{len(programs)}-{hashlib.sha256(b''.join(programs)).hexdigest()[:8]}.json")
    json.dump({"builds": [{"platform": "linux/amd64", "variant": "gnu", "files": [{"sha256": hashlib.sha256(p).hexdigest()}]} for p in programs]}, open(path, "w"))
    return path


key = ("sk-" + "ant-" + "test0-").encode() + b"x" * 24  # key-shaped, made here
prog = b"\x7fELF a program that reads ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN, apiKeyHelper and .credentials.json"
files = {"etc/os-release": b"ID=test\n", "harness/claude/bin/claude": prog, "harness/claude/harness.json": b'{"run":{"env":["HOME=/home/claude"]}}'}
both = [("amd64", ["PATH=/usr/bin"], files), ("arm64", ["PATH=/usr/bin"], files)]
pin = pin_of(prog)

# index
rc, out = run("index", layout(os.path.join(work, "both.tar"), both))
check("index: one index holding linux/amd64 and linux/arm64", rc == 0 and "ONE_OCI_INDEX_HOLDS_LINUX_AMD64_AND_ARM64 True" in out, out)
rc, out = run("index", layout(os.path.join(work, "one.tar"), both[:1]))
check("index mutation caught: linux/amd64 alone", rc == 1 and "ONE_OCI_INDEX_HOLDS_LINUX_AMD64_AND_ARM64 False" in out, out)

# secrets
rc, out = run("secrets", os.path.join(work, "both.tar"), pin)
check("secrets: a clean image; the pinned program's names are its own", rc == 0 and "SCAN clean" in out and "pinned binary /harness/claude/bin/claude" in out, out)
prog_key = prog + b" " + key
mutations = {
    "config Env ANTHROPIC_API_KEY": ([("amd64", ["PATH=/usr/bin", "ANTHROPIC_API_KEY=placeholder"], files)], pin),
    "a key-shaped value in a file": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"etc/profile.d/k.sh": b"export X=" + key + b"\n"}))], pin),
    "a key-shaped value in the pinned program": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"harness/claude/bin/claude": prog_key}))], pin_of(prog_key)),
    "an unpinned file naming apiKeyHelper": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"etc/claude-code/managed-settings.json": b'{"apiKeyHelper":"cat /k"}'}))], pin),
    "the program changed (not the pinned one)": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"harness/claude/bin/claude": prog + b"!"}))], pin),
    "a sign-in file .credentials.json": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"root/.claude/.credentials.json": b"{}"}))], pin),
    "a sign-in file .claude.json": ([("amd64", ["PATH=/usr/bin"], dict(files, **{"home/claude/.claude.json": b"{}"}))], pin),
}
for name, (plats, p) in mutations.items():
    rc, out = run("secrets", layout(os.path.join(work, "mutant.tar"), plats), p)
    check(f"secrets mutation caught: {name}", rc == 1 and "SCAN FOUND" in out and key.decode() not in out, out)

# paths
loader = {"lib64/ld-linux-x86-64.so.2": b"x", "bin/sh": b"y"}
rc, out = run("paths", layout(os.path.join(work, "paths.tar"), [("amd64", [], [loader, {"lib/ld-musl-x86_64.so.1": b"z"}])]), "/lib64/ld-linux-x86-64.so.2", "/lib/ld-musl-x86_64.so.1", "/etc/none")
check("paths: a file in a lower layer, one in an upper, one in neither", rc == 0 and "/lib64/ld-linux-x86-64.so.2: file" in out and "/lib/ld-musl-x86_64.so.1: file" in out and "/etc/none: absent" in out, out)
rc, out = run("paths", layout(os.path.join(work, "paths.tar"), [("amd64", [], [loader, {"lib64/.wh.ld-linux-x86-64.so.2": b""}])]), "/lib64/ld-linux-x86-64.so.2", "/bin/sh")
check("paths: a whiteout takes a lower layer's file away", rc == 0 and "/lib64/ld-linux-x86-64.so.2: absent" in out and "/bin/sh: file" in out, out)
merged = {"lib64": ("symlink", "usr/lib64"), "usr/lib64/ld-linux-x86-64.so.2": ("symlink", "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"), "lib": ("symlink", "usr/lib"), "usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2": b"l"}
rc, out = run("paths", layout(os.path.join(work, "paths.tar"), [("amd64", [], [merged])]), "/lib64/ld-linux-x86-64.so.2")
check("paths: a merged-/usr loader, through its symlinks", rc == 0 and "/lib64/ld-linux-x86-64.so.2: file at /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" in out, out)
rc, out = run("paths", layout(os.path.join(work, "paths.tar"), [("amd64", [], [{"a": ("symlink", "b"), "b": ("symlink", "a")}])]), "/a/x")
check("paths: a symlink loop is told, not followed for ever", rc == 0 and "/a/x: a symlink loop" in out, out)

# exec
hj = os.path.join(work, "harness.json")
json.dump({"name": "claude", "run": {"command": ["claude", "-p", "--verbose"], "env": []}}, open(hj, "w"))
head = "TIME                            MEMBER          PID  EVENT  WHAT\n"
good = (head + "2026-11-12T00:00:00.000000001Z  command         900  exec   sleep 120\n"
        "2026-11-12T00:00:00.000000002Z  harness claude  910  exec   /harness/claude/bin/claude -p --verbose\n"
        "2026-11-12T00:00:00.000000003Z  harness claude  910  exit   code 1\n")
records = {
    "as declared": (good, 0),
    "an argument missing": (good.replace(" -p --verbose\n", " -p\n"), 1),
    "another program": (good.replace("/harness/claude/bin/claude", "/usr/bin/claude"), 1),
    "another member": (good.replace("harness claude  910", "agent claude    910"), 1),
}
for name, (text, want) in records.items():
    rp = os.path.join(work, "record.txt")
    open(rp, "w").write(text)
    rc, out = run("exec", rp, hj)
    check(f"exec {'holds' if want == 0 else 'mutation caught'}: {name}", rc == want and f"HARNESS_EXEC_AS_DECLARED {want == 0}" in out and (want or "its end: code 1" in out), out)


# homedir
def home(name, x=True, git=b"[user]\n\tname = kiln-probe\n", cj=b'{"mcpServers":{"kiln-probe":{"type":"stdio","command":"true","args":[]}}}'):
    d = os.path.join(work, "home-" + name)
    os.makedirs(d, exist_ok=True)
    if x:
        put(os.path.join(d, "x"), b"")
    if git is not None:
        put(os.path.join(d, ".gitconfig"), git)
    if cj is not None:
        put(os.path.join(d, ".claude.json"), cj)
    return d


rc, out = run("homedir", home("good"))
check("homedir: x, .gitconfig and .claude.json as the probe writes them", rc == 0 and "HOMEDIR_HOLDS x=True gitconfig_names_kiln_probe=True claude_json_holds_kiln_probe=True" in out, out)
for name, kw in {"no x": {"x": False}, "another git name": {"git": b"[user]\n\tname = someone\n"}, "no .claude.json": {"cj": None}, "no kiln-probe server": {"cj": b'{"mcpServers":{}}'}}.items():
    rc, out = run("homedir", home(name.replace(" ", "-"), **kw))
    check(f"homedir mutation caught: {name}", rc == 1, out)

# interp
if sys.platform.startswith("linux"):
    rc, out = run("interp", "/bin/sh")
    check("interp: /bin/sh names its loader", rc == 0 and out.strip().startswith("/lib"), out)
rc, out = run("interp", hj)
check("interp: not an ELF is refused", rc == 2, out)

# context
ksrc = os.path.join(work, "ksrc")
images = os.path.join(ksrc, ".kiln/images/harness-claude")
binary = b"\x7fELF the pinned build"
put(os.path.join(images, "Agentfile"), b"FROM scratch\n")
put(os.path.join(images, "harness.json"), b'{"name":"claude"}\n')
put(os.path.join(images, "harness.musl.json"), b'{"name":"claude","musl":true}\n')
json.dump({"builds": [{"platform": "linux/amd64", "variant": "gnu", "files": [{"sha256": hashlib.sha256(binary).hexdigest(), "size": len(binary), "path": "bin/claude", "mode": "0755"}]}]}, open(os.path.join(images, "pin.json"), "w"))


def ctx(name, data=binary, mode=0o755, hj=b'{"name":"claude"}\n', af=b"FROM scratch\n"):
    c = os.path.join(work, "ctx-" + name)
    put(os.path.join(c, "Agentfile"), af)
    put(os.path.join(c, "claude-amd64-gnu/bin/claude"), data, mode)
    put(os.path.join(c, "claude-amd64-gnu/harness.json"), hj)
    return c


rc, out = run("context", ctx("good"), ksrc, "gnu", "linux/amd64")
check("context: the pin's binary, mode and harness.json", rc == 0 and "CONTEXT_MATCHES_PIN 1 of 1" in out, out)
for name, kw in {"a byte changed": {"data": binary[:-1] + b"?"}, "mode 0644": {"mode": 0o644}, "the musl harness file": {"hj": b'{"name":"claude","musl":true}\n'}, "another Agentfile": {"af": b"FROM alpine\n"}}.items():
    rc, out = run("context", ctx(name.replace(" ", "-"), **kw), ksrc, "gnu", "linux/amd64")
    check(f"context mutation caught: {name}", rc == 1, out)

print(f"SELFTEST {sum(results)} of {len(results)} passed")
sys.exit(0 if all(results) else 1)
