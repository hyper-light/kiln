#!/usr/bin/env python3
"""Step 5's checks (plan 01a1cf48e8157494ac9a51f7a564f526; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19).

    scan.py context CTX KSRC VARIANT PLATFORM...  the fetcher's context against the pin and the built-in's files
    scan.py index TAR                             the OCI index `shards save` exported, and its platforms
    scan.py secrets TAR PIN                       every file of every layer, and each config, for credentials
    scan.py paths TAR PATH...                     whether each image's filesystem holds each path
    scan.py exec RECORD HARNESS_JSON              the harness's exec in `shards read processes`, against harness.json
    scan.py homedir DIR                           what the probe wrote into the host's HOMEDIR
    scan.py tree DIR                              a host folder's entries
    scan.py interp ELF                            an ELF's program interpreter (PT_INTERP)

Run with python3 -I. Each prints KEY value verdicts and exits 1 where its check fails. Nothing here prints a
credential-shaped value: a match is told by where it is, its length and its prefix alone.
"""
import hashlib
import json
import os
import re
import stat
import struct
import sys
import tarfile

INDEX_TYPES = {"application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json"}
MANIFEST_TYPES = {"application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"}
# Step 2's patterns (crates/kiln/src/agentfiles/builtin/claude_tests.rs, credentials_in): Claude Code's own
# credentials by name, an ENV or ARG named like one, and a provider's key.
NAMES = [b"ANTHROPIC_API_KEY", b"CLAUDE_CODE_OAUTH_TOKEN", b"apiKeyHelper", b".credentials.json", b"sk-ant-"]
WORDS = ("KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL")
# A key or token itself, as Anthropic's are shaped: sk-ant-<kind>-<body>, the body 20 characters or more.
KEYVAL = re.compile(rb"sk-ant-[A-Za-z0-9]+-[A-Za-z0-9_-]{20,}")
# Files that are a sign-in: they belong on their owner's HOME volume alone, never in an image.
SIGNIN = {".credentials.json", ".claude.json"}
CHUNK = 1 << 20
OVERLAP = 512


def fail(msg):
    print(msg)
    sys.exit(2)


def sha256_file(path):
    h, n = hashlib.sha256(), 0
    with open(path, "rb") as f:
        for c in iter(lambda: f.read(CHUNK), b""):
            h.update(c)
            n += len(c)
    return h.hexdigest(), n


def plat_name(p):
    return "/".join(x for x in (p.get("os"), p.get("architecture"), p.get("variant")) if x)


# --- the fetcher's context ---------------------------------------------------------------------------------------
def cmd_context(ctx, ksrc, variant, *platforms):
    images = os.path.join(ksrc, ".kiln/images/harness-claude")
    pin = json.load(open(os.path.join(images, "pin.json")))
    hj_name = "harness.json" if variant == "gnu" else f"harness.{variant}.json"
    want_hj = open(os.path.join(images, hj_name), "rb").read()
    af_ok = open(os.path.join(ctx, "Agentfile"), "rb").read() == open(os.path.join(images, "Agentfile"), "rb").read()
    print(f"Agentfile equals the built-in's: {af_ok}")
    good = 0
    for plat in platforms:
        arch = plat.split("/", 1)[1]
        builds = [b for b in pin["builds"] if b["platform"] == plat and b.get("variant") == variant]
        if len(builds) != 1:
            fail(f"{plat} {variant}: {len(builds)} builds pinned")
        d = os.path.join(ctx, f"claude-{arch}-{variant}")
        ok = True
        for f in builds[0]["files"]:
            path = os.path.join(d, f["path"])
            sha, size = sha256_file(path)
            mode = os.stat(path).st_mode & 0o777
            match = (sha, size, mode) == (f["sha256"], f["size"], int(f["mode"], 8))
            ok = ok and match
            print(f"{plat} {variant}: {os.path.relpath(path, ctx)} sha256 {sha} size {size} mode {mode:04o}; pinned {f['sha256']} {f['size']} {f['mode']}: {match}")
        hj_ok = open(os.path.join(d, "harness.json"), "rb").read() == want_hj
        print(f"{plat} {variant}: harness.json is the built-in's {hj_name}: {hj_ok}")
        entries = sorted(os.path.relpath(os.path.join(r, n), d) for r, _, fs in os.walk(d) for n in fs)
        print(f"{plat} {variant}: files {entries}")
        good += ok and hj_ok
    print(f"context entries: {sorted(os.listdir(ctx))}")
    print(f"CONTEXT_MATCHES_PIN {good} of {len(platforms)}")
    return 0 if af_ok and good == len(platforms) else 1


# --- the exported image -----------------------------------------------------------------------------------------
class Layout:
    """An OCI image layout in a tar, as `shards save` writes it."""

    def __init__(self, path):
        self.t = tarfile.open(path, "r:")
        self.members = {m.name.removeprefix("./"): m for m in self.t.getmembers()}

    def file(self, name):
        m = self.members.get(name)
        if m is None:
            fail(f"{name} isn't in the archive")
        return self.t.extractfile(m)

    def blob(self, digest):
        algo, hexd = digest.split(":", 1)
        data = self.file(f"blobs/{algo}/{hexd}").read()
        if hashlib.new(algo, data).hexdigest() != hexd:
            fail(f"blob {digest} doesn't hash to its name")
        return data

    def images(self):
        """Each image manifest index.json reaches: (platform, descriptor, manifest, the index it is in)."""
        out = []

        def walk(desc, parent):
            doc = json.loads(self.blob(desc["digest"]))
            mt = desc.get("mediaType") or doc.get("mediaType")
            if mt in INDEX_TYPES:
                for d in doc.get("manifests", []):
                    walk(d, desc["digest"])
            elif mt in MANIFEST_TYPES:
                out.append((plat_name(desc.get("platform") or {}), desc, doc, parent))

        for d in json.loads(self.file("index.json").read()).get("manifests", []):
            walk(d, None)
        return out


def cmd_index(tar):
    lay = Layout(tar)
    top = json.loads(lay.file("index.json").read())
    print(f"index.json: schemaVersion {top.get('schemaVersion')}, mediaType {top.get('mediaType')}, {len(top.get('manifests', []))} entries")
    held = []
    for d in top.get("manifests", []):
        doc = json.loads(lay.blob(d["digest"]))
        mt = d.get("mediaType") or doc.get("mediaType")
        names = {k: v for k, v in (d.get("annotations") or {}).items() if k.endswith("name")}
        print(f"  {d['digest']} {mt} size {d.get('size')} {names}")
        if mt not in INDEX_TYPES:
            continue
        plats = []
        for m in doc.get("manifests", []):
            kind = (m.get("annotations") or {}).get("vnd.docker.reference.type", "image")
            p = plat_name(m.get("platform") or {})
            print(f"    {m['digest']} {m.get('mediaType')} {p} size {m.get('size')} ({kind})")
            if kind == "image":
                plats.append(p)
        held.append((d["digest"], plats))
    for digest, plats in held:
        print(f"OCI_INDEX {digest} IMAGE_MANIFESTS {','.join(plats)}")
    ok = any(sorted(p) == ["linux/amd64", "linux/arm64"] for _, p in held)
    print(f"ONE_OCI_INDEX_HOLDS_LINUX_AMD64_AND_ARM64 {ok}")
    return 0 if ok else 1


def layer_tar(lay, desc):
    mt = desc.get("mediaType", "")
    algo, hexd = desc["digest"].split(":", 1)
    f = lay.file(f"blobs/{algo}/{hexd}")
    if mt.endswith("+gzip") or mt.endswith("tar.gzip"):
        return tarfile.open(fileobj=f, mode="r|gz")
    if mt.endswith("+zstd"):
        try:
            from compression import zstd  # Python 3.14
        except ImportError:
            fail(f"layer {desc['digest']} is zstd and this Python reads none: not scanned")
        return tarfile.open(fileobj=zstd.ZstdFile(f), mode="r|")
    if mt.endswith("layer.v1.tar") or mt.endswith("rootfs.diff.tar"):
        return tarfile.open(fileobj=f, mode="r|")
    return None


def scan_stream(f):
    """(sha256, size, the names it holds, its key-shaped values as (offset, length, prefix))."""
    h, n, names, vals, tail = hashlib.sha256(), 0, set(), {}, b""
    for c in iter(lambda: f.read(CHUNK), b""):
        h.update(c)
        buf, base = tail + c, n - len(tail)
        names.update(w.decode() for w in NAMES if w in buf)
        for m in KEYVAL.finditer(buf):
            s = m.group(0)
            off = base + m.start()
            prefix = s[: s.index(b"-", len(b"sk-ant-")) + 1].decode()
            if vals.get(off, (0,))[0] < len(s):
                vals[off] = (len(s), prefix)
        n += len(c)
        tail = buf[-OVERLAP:]
    return h.hexdigest(), n, names, sorted((o, ln, p) for o, (ln, p) in vals.items())


def cmd_secrets(tar, pin_path):
    pin = json.load(open(pin_path))
    pinned = {f["sha256"] for b in pin["builds"] for f in b["files"]}
    lay = Layout(tar)
    found, in_pinned, seen = [], {}, set()
    files = size = layers = blobs = 0
    for plat, desc, man, parent in lay.images():
        kind = (desc.get("annotations") or {}).get("vnd.docker.reference.type", "image")
        print(f"{plat or '-'} ({kind}) manifest {desc['digest']} in {parent}")
        cfg_bytes = lay.blob(man["config"]["digest"])
        cfg = json.loads(cfg_bytes)
        env = (cfg.get("config") or {}).get("Env") or []
        shown = []
        for e in env:
            name, _, val = e.partition("=")
            if any(w in name.upper() for w in WORDS):
                found.append(f"{plat} config Env names {name}")
                shown.append(f"{name}=<{len(val)} characters>")
            else:
                shown.append(e)
        print(f"  config {man['config']['digest']} Env {shown}")
        for w in NAMES:
            if w in cfg_bytes:
                found.append(f"{plat} config holds {w.decode()}")
        if KEYVAL.search(cfg_bytes):
            found.append(f"{plat} config holds a key-shaped value")
        for ld in man.get("layers", []):
            if ld["digest"] in seen:
                continue
            seen.add(ld["digest"])
            t = layer_tar(lay, ld)
            if t is None:  # not a filesystem layer (an attestation's statement): its bytes are scanned whole
                algo, hexd = ld["digest"].split(":", 1)
                sha, n, names, vals = scan_stream(lay.file(f"blobs/{algo}/{hexd}"))
                blobs += 1
                size += n
                print(f"  blob {ld['digest']} {ld.get('mediaType')} {n} bytes, names {sorted(names)}, key-shaped {len(vals)}")
                found += [f"{plat} blob {ld['digest']} names {w}" for w in sorted(names)]
                found += [f"{plat} blob {ld['digest']} a key-shaped value at {o} ({ln} characters, {p}…)" for o, ln, p in vals]
                continue
            layers += 1
            lf = ls = 0
            for m in t:
                path = "/" + m.name.removeprefix("./").lstrip("/")
                if os.path.basename(path.rstrip("/")) in SIGNIN:
                    found.append(f"{plat} layer {ld['digest'][:19]} holds a sign-in file {path}")
                if not m.isreg():
                    continue
                sha, n, names, vals = scan_stream(t.extractfile(m))
                files += 1
                lf += 1
                ls += n
                if sha in pinned:  # Anthropic's own build, as pin.json pins it: the names it reads, no value
                    in_pinned.setdefault(f"{path} ({sha[:12]})", set()).update(names)
                else:
                    found += [f"{plat} {path} names {w}" for w in sorted(names)]
                found += [f"{plat} {path} a key-shaped value at {o} ({ln} characters, {p}…)" for o, ln, p in vals]
            size += ls
            print(f"  layer {ld['digest']} {ld.get('mediaType')}: {lf} files, {ls} bytes")
    for path, names in sorted(in_pinned.items()):
        print(f"pinned binary {path}, sha256 equal to pin.json's: names {sorted(names)}, no key-shaped value")
    print(f"scanned {layers} layers, {blobs} other blobs, {files} files, {size} bytes")
    for f in found:
        print(f"FOUND {f}")
    print(f"SCAN {'clean' if not found else 'FOUND'} ({len(found)} findings)")
    return 0 if not found else 1


def resolve(fs, path):
    """`path` in `fs` (path: (kind, link target)) as the kernel walks it, each symlink followed: (where it ends, kind)
    or (None, why)."""
    parts, cur, hops = [p for p in path.split("/") if p], "/", 0
    while parts:
        nxt = os.path.join(cur, parts.pop(0))
        kind, target = fs.get(nxt, (None, None))
        if kind is None and any(k.startswith(nxt + "/") for k in fs):
            kind = "directory"  # a layer may list a directory's files without the directory itself
        if kind is None:
            return None, "absent"
        if kind == "symlink":
            hops += 1
            if hops > 40:  # the kernel's own bound on a path's links (MAXSYMLINKS, 40)
                return None, "a symlink loop"
            base = "/" if target.startswith("/") else cur
            parts = [p for p in os.path.normpath(os.path.join(base, target)).split("/") if p] + parts
            cur = "/"
            continue
        cur = nxt
    return cur, fs.get(cur, ("directory", None))[0]


def cmd_paths(tar, *paths):
    """Whether each image's filesystem, its layers applied in order (whiteouts included), holds each path."""
    lay = Layout(tar)
    for plat, desc, man, _ in lay.images():
        if (desc.get("annotations") or {}).get("vnd.docker.reference.type", "image") != "image":
            continue
        fs = {}
        for ld in man.get("layers", []):
            t = layer_tar(lay, ld)
            if t is None:
                continue
            for m in t:
                p = "/" + m.name.removeprefix("./").strip("/")
                d, base = os.path.split(p)
                if base == ".wh..wh..opq":
                    fs = {k: v for k, v in fs.items() if not k.startswith(d.rstrip("/") + "/")}
                elif base.startswith(".wh."):
                    gone = os.path.join(d, base[len(".wh."):])
                    fs = {k: v for k, v in fs.items() if k != gone and not k.startswith(gone + "/")}
                elif m.issym():
                    fs[p] = ("symlink", m.linkname)
                else:
                    fs[p] = ("file" if m.isreg() or m.islnk() else "directory" if m.isdir() else "other", None)
        for p in paths:
            where, kind = resolve(fs, p)
            told = kind if where is None else f"{kind} at {where}" if where != p else kind
            print(f"{plat} {desc['digest'][:19]} {p}: {told}")
    return 0


# --- the process record -----------------------------------------------------------------------------------------
ROW = re.compile(r"^(\S+)\s{2,}(.+?)\s{2,}(\d+)\s{2,}(\S+)\s{2,}(.*)$")


def cmd_exec(record, hj_path):
    h = json.load(open(hj_path))
    cmd, name = h["run"]["command"], h["name"]
    # The harness's PATH begins /harness/<name>:/harness/<name>/bin, and the fetcher writes the program at bin/.
    prog = cmd[0] if cmd[0].startswith("/") else f"/harness/{name}/bin/{cmd[0]}"
    want = " ".join([prog] + cmd[1:])
    rows = [m.groups() for m in map(ROW.match, open(record).read().splitlines()) if m]
    member = f"harness {name}"
    hits = [r for r in rows if r[1] == member and r[3] == "exec" and r[4] == want]
    print(f"harness.json run.command {json.dumps(cmd)}")
    print(f"HARNESS_EXEC_WANTED {want}")
    for r in hits:
        ends = [x for x in rows if x[2] == r[2] and x[3] == "exit"]
        print(f"HARNESS_EXEC pid {r[2]} at {r[0]}; its end: {ends[0][4] if ends else 'still running'}")
    print(f"HARNESS_EXEC_AS_DECLARED {bool(hits)}")
    return 0 if hits else 1


# --- host folders -----------------------------------------------------------------------------------------------
def tree(d, depth=3):
    for root, dirs, fs in os.walk(d):
        rel = os.path.relpath(root, d)
        if rel != "." and rel.count(os.sep) + 1 >= depth:
            dirs[:] = []
        dirs.sort()
        for n in sorted(dirs + fs):
            p = os.path.join(root, n)
            st = os.lstat(p)
            kind = "dir " if stat.S_ISDIR(st.st_mode) else "file" if stat.S_ISREG(st.st_mode) else "link"
            print(f"  {kind} {st.st_mode & 0o7777:04o} uid {st.st_uid} {st.st_size:>8} {os.path.relpath(p, d)}")


def cmd_tree(d):
    tree(d)
    return 0


def cmd_homedir(d):
    tree(d)
    x = os.path.isfile(os.path.join(d, "x"))
    gc_path = os.path.join(d, ".gitconfig")
    gc = open(gc_path).read() if os.path.isfile(gc_path) else ""
    git_ok = re.search(r"(?m)^\s*name\s*=\s*kiln-probe\s*$", gc) is not None
    print(f"--- .gitconfig\n{gc}", end="" if gc.endswith("\n") or not gc else "\n")
    cj_path = os.path.join(d, ".claude.json")
    mcp = None
    if os.path.isfile(cj_path):
        cj = json.load(open(cj_path))
        mcp = (cj.get("mcpServers") or {}).get("kiln-probe")
        print(f"--- .claude.json: its keys {sorted(cj)}")
        print(f"--- .claude.json mcpServers.kiln-probe: {json.dumps(mcp)}")
    else:
        print("--- .claude.json: missing")
    print(f"HOMEDIR_HOLDS x={x} gitconfig_names_kiln_probe={git_ok} claude_json_holds_kiln_probe={mcp is not None}")
    return 0 if x and git_ok and mcp is not None else 1


# --- an ELF's loader --------------------------------------------------------------------------------------------
def cmd_interp(path):
    with open(path, "rb") as f:
        hdr = f.read(64)
        if hdr[:4] != b"\x7fELF" or hdr[4] != 2 or hdr[5] != 1:
            fail(f"{path}: not a little-endian ELF64")
        phoff = struct.unpack_from("<Q", hdr, 0x20)[0]
        phentsize, phnum = struct.unpack_from("<HH", hdr, 0x36)
        for i in range(phnum):
            f.seek(phoff + i * phentsize)
            ph = f.read(phentsize)
            if struct.unpack_from("<I", ph, 0)[0] == 3:  # PT_INTERP
                off, filesz = struct.unpack_from("<Q", ph, 8)[0], struct.unpack_from("<Q", ph, 32)[0]
                f.seek(off)
                print(f.read(filesz).rstrip(b"\0").decode())
                return 0
    print("none")
    return 1


COMMANDS = {"context": cmd_context, "index": cmd_index, "secrets": cmd_secrets, "paths": cmd_paths, "exec": cmd_exec, "homedir": cmd_homedir, "tree": cmd_tree, "interp": cmd_interp}

if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        fail(__doc__)
    sys.exit(COMMANDS[sys.argv[1]](*sys.argv[2:]))
