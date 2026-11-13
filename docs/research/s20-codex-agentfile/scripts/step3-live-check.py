# Step 3's live check (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): a context `kiln agentfiles context
# codex CTX --platform linux/amd64,linux/arm64` wrote, held to the pin it was made from.
#
#   python3 -I step3-live-check.py CTX PIN_JSON HARNESS_JSON AGENTFILE WORKDIR
#
# For each build in the pin, it downloads every pinned file again into WORKDIR, holds it to its pinned
# SHA-256 and size, and computes what its unpack lays out:
# - a tree: each regular file's bytes at its path once `strip` names are dropped, `skip` paths left out;
# - a member: that member's bytes at the file's path;
# - a plain file: its bytes at its path.
# Then CTX/codex-<arch> must hold exactly those files plus harness.json (the built-in's, byte for byte),
# with the same SHA-256 each. There must be no symlink or other kind anywhere, every directory 0755, and
# every file 0755 or 0644. CTX itself holds the built-in's Agentfile, byte for byte, beside the two
# directories, and nothing else (no temporary directory left). It prints one line per platform and a
# verdict, exiting 0 only if both platforms match.
import hashlib, json, os, stat, sys, tarfile, urllib.request

ctx, pin_path, harness_path, agentfile_path, work = sys.argv[1:6]
pin = json.load(open(pin_path))
harness = open(harness_path, "rb").read()
top = sorted(os.listdir(ctx))
top_ok = top == ["Agentfile", "codex-amd64", "codex-arm64"] and open(os.path.join(ctx, "Agentfile"), "rb").read() == open(agentfile_path, "rb").read()
print(f"context: {top} -> {'MATCH' if top_ok else 'MISMATCH'} (the Agentfile byte for byte, nothing else beside)")
ARCH = {"linux/amd64": "amd64", "linux/arm64": "arm64"}

def fetch(url, sha256, size, to):
    h, n = hashlib.sha256(), 0
    with urllib.request.urlopen(url, timeout=600) as r, open(to, "wb") as out:
        while True:
            b = r.read(1 << 20)
            if not b:
                break
            h.update(b); n += len(b); out.write(b)
    if (h.hexdigest(), n) != (sha256, size):
        raise SystemExit(f"{url}: sha256 {h.hexdigest()} size {n}, pinned {sha256} {size}")

def names(p):
    return [x for x in p.split("/") if x not in ("", ".")]

def laid_out(f, path):
    """What one pinned file lays out: {relative path: sha256}."""
    u = f.get("unpack")
    if u is None:
        return {"/".join(names(f["path"])): f["sha256"]}
    out = {}
    with tarfile.open(path, "r|gz") as t:
        for m in t:
            if not m.isreg():
                continue
            parts = names(m.name)
            if "member" in u:
                if names(u["member"]) == parts:
                    out["/".join(names(f["path"]))] = hashlib.sha256(t.extractfile(m).read()).hexdigest()
                continue
            rel = parts[u.get("strip", 0):]
            key = "/".join(names(f["path"]) + rel)
            if not rel or "/".join(rel) in {"/".join(names(s)) for s in u.get("skip", [])}:
                continue
            out[key] = hashlib.sha256(t.extractfile(m).read()).hexdigest()
    return out

def walk(root):
    files, bad = {}, []
    for d, dirs, fs in os.walk(root):
        for x in dirs + fs:
            p = os.path.join(d, x)
            st = os.lstat(p)
            rel = os.path.relpath(p, root)
            mode = stat.S_IMODE(st.st_mode)
            if stat.S_ISLNK(st.st_mode):
                bad.append(f"symlink {rel}")
            elif stat.S_ISDIR(st.st_mode):
                if mode != 0o755:
                    bad.append(f"directory {rel} {mode:o}")
            elif stat.S_ISREG(st.st_mode):
                if mode not in (0o755, 0o644):
                    bad.append(f"file {rel} {mode:o}")
                files[rel] = hashlib.sha256(open(p, "rb").read()).hexdigest()
            else:
                bad.append(f"other {rel}")
    root_mode = stat.S_IMODE(os.lstat(root).st_mode)
    if root_mode != 0o755:
        bad.append(f"directory . {root_mode:o}")
    return files, bad

good = 0
for b in pin["builds"]:
    arch = ARCH[b["platform"]]
    want = {}
    for i, f in enumerate(b["files"]):
        path = os.path.join(work, f"{arch}-{i}")
        fetch(f["url"], f["sha256"], f["size"], path)
        for k, v in laid_out(f, path).items():
            if k in want:
                raise SystemExit(f"{b['platform']}: {k} laid out twice by the pin")
            want[k] = v
        os.remove(path)
    want["harness.json"] = hashlib.sha256(harness).hexdigest()
    got, bad = walk(os.path.join(ctx, f"codex-{arch}"))
    missing = sorted(set(want) - set(got))
    extra = sorted(set(got) - set(want))
    differ = sorted(k for k in set(want) & set(got) if want[k] != got[k])
    ok = not (missing or extra or differ or bad)
    good += ok
    print(f"{b['platform']}: files {len(got)}, pinned {len(want)}, matching {len(want) - len(missing) - len(differ)}; "
          f"missing {missing[:5]}, extra {extra[:5]}, differing {differ[:5]}, bad {bad[:5]} -> {'MATCH' if ok else 'MISMATCH'}")
print(f"PLATFORMS_MATCHING {good} of {len(pin['builds'])}")
sys.exit(0 if top_ok and good == len(pin["builds"]) == 2 else 1)
