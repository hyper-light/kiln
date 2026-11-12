#!/usr/bin/env bash
# Pins Gemini CLI's Linux build and the Node.js it runs on into pin.json beside this script, for
# kiln's built-in Agentfile harness-gemini (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 21; its
# own plan 01a1ce705e2374dfbac201fe6ba98184, research 01a1ce705e2472df9f3dd81be0531c9f).
#
#   pin.sh [latest | <npm dist-tag> | <x.y.z>]    default: npm's latest
#
# Gemini CLI's Linux build is its npm package, run by Node.js (f5, f6). It is checked as npm
# documents it (f7): every registry signature, ECDSA P-256 over <name>@<version>:<integrity>, with
# the registry key of its keyid from /-/npm/v1/keys; then the tarball's SHA-512 against that
# integrity. Google publishes no provenance attestation for it, and npm no SHA-256: the one pinned is
# that of the download whose SHA-512 is the signed integrity.
#
# Its Node.js is derived, not chosen (R03, f9, f10). The major is the newest one in the node-version
# matrix of the 'Test (Linux)' job in Gemini's .github/workflows/ci.yml at the pinned tag whose end in
# nodejs/Release's schedule.json is after today; the release is that major's newest in nodejs.org's
# index. Its SHASUMS256.txt.asc is checked with gpgv against nodejs/release-keys'
# gpg-only-active-keys keyring at a recorded commit, and each archive against the SHA-256 the signed
# text gives (f11). The .tar.gz builds are pinned: kiln unpacks them with crates it has (f14). Node's
# Linux builds for both platforms are glibc's alone (its x64 musl build is experimental, and it has
# no arm64 one: f11), so the builds have no libc variant, as Cursor's and pi's have none.
#
# pin.json lists, per platform, the files a build context needs, in the form kiln's one unpacker takes
# (crates/kiln/src/agentfiles/unpack.rs, as steps 26 and 27 agreed it):
#   - the npm tarball, a tree at the harness's root with its package/ stripped, held to the two
#     totals measured here: entries (its directories and regular files, as tar -t lists them) and
#     unpacked_bytes (its regular files' bytes);
#   - Node's bin/node and LICENSE, each a member written at node/ with its pinned mode and held to
#     its own SHA-256 and size, so its archive's symlinks never reach a context (f15) and its node
#     stays off the harness's PATH (f37).
# Each entry is one the unpacker writes (a directory or a regular file, no set-ID or sticky bit, a
# path inside the tree, none twice or folding to another's name), and none lands where kiln writes:
# harness.json and system-settings.json beside the tree, node/ for the members. And the smoke an
# image's Gemini CLI must pass, in the shape every harness's pin gives it: its node on its script,
# by the paths Shards gives them (/harness/gemini), `--version`, printing the bare version (f19).
# pin.json is written only when every check passed.
#
# Needs bash, curl, gpgv, openssl and python3. Environment (tests point the URLs at a local server):
#   PIN_NPM_REGISTRY         default https://registry.npmjs.org
#   PIN_GEMINI_RAW           default https://raw.githubusercontent.com/google-gemini/gemini-cli
#   PIN_NODE_DIST            default https://nodejs.org/dist
#   PIN_NODE_SCHEDULE        default https://raw.githubusercontent.com/nodejs/Release/main/schedule.json
#   PIN_RELEASE_KEYS         default https://raw.githubusercontent.com/nodejs/release-keys
#   PIN_RELEASE_KEYS_COMMIT  the nodejs/release-keys commit read; default main's, from PIN_GITHUB_API
#   PIN_GITHUB_API           default https://api.github.com
#   PIN_BASE                 the base image the pin is made for, <ref>@sha256:<digest>; default the
#                            shared default of every built-in harness
#   PIN_OUT                  the file written; default pin.json beside this script
set -euo pipefail

die() {
    printf 'pin.sh: %s\n' "$*" >&2
    exit 1
}
[[ $# -le 1 ]] || die "usage: pin.sh [latest | <npm dist-tag> | <x.y.z>]"
for c in curl gpgv openssl python3; do
    command -v "$c" >/dev/null 2>&1 || die "needs $c"
done

here=$(cd "$(dirname "$0")" && pwd)
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
work=$(mktemp -d "$cache/pin-harness-gemini.XXXXXX")
trap 'rm -rf "$work"' EXIT

export PIN_WANT=${1:-latest}
export PIN_WORK=$work
export PIN_OUT=${PIN_OUT:-$here/pin.json}
export PIN_BASE=${PIN_BASE:-buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10}
export PIN_NPM_REGISTRY=${PIN_NPM_REGISTRY:-https://registry.npmjs.org}
export PIN_GEMINI_RAW=${PIN_GEMINI_RAW:-https://raw.githubusercontent.com/google-gemini/gemini-cli}
export PIN_NODE_DIST=${PIN_NODE_DIST:-https://nodejs.org/dist}
export PIN_NODE_SCHEDULE=${PIN_NODE_SCHEDULE:-https://raw.githubusercontent.com/nodejs/Release/main/schedule.json}
export PIN_RELEASE_KEYS=${PIN_RELEASE_KEYS:-https://raw.githubusercontent.com/nodejs/release-keys}
export PIN_RELEASE_KEYS_COMMIT=${PIN_RELEASE_KEYS_COMMIT:-}
export PIN_GITHUB_API=${PIN_GITHUB_API:-https://api.github.com}

IFS= read -r -d '' PY <<'EOF' || true
import base64, datetime, hashlib, json, os, re, subprocess, sys, tarfile, unicodedata

env = os.environ
WORK = env["PIN_WORK"]
REG, RAW, DIST = env["PIN_NPM_REGISTRY"], env["PIN_GEMINI_RAW"], env["PIN_NODE_DIST"]
SCHEDULE, KEYS, API = env["PIN_NODE_SCHEDULE"], env["PIN_RELEASE_KEYS"], env["PIN_GITHUB_API"]
PKG, PKG_PATH = "@google/gemini-cli", "@google%2fgemini-cli"
# The package's own bin entry (package.json's bin.gemini, checked below): harness.json and the smoke
# start Node on it.
ENTRY = "bundle/gemini.js"
# Where Shards puts the harness's directory in the image: HARNESS gemini lands at /harness/<name> (f28).
HARNESS = "/harness/gemini"
# Node's two members, and their modes: its executable and its license text. Under node/, off the PATH
# Shards starts a harness with (its directory, then its bin/, then the system's: f28, f37).
MEMBERS = (("bin/node", "node/bin/node", "0755"), ("LICENSE", "node/LICENSE", "0644"))
# The built-in's own files, which kiln writes at the tree's root once it's unpacked.
KILN_FILES = ("harness.json", "system-settings.json")
# Node's platforms by its own names, and the OCI platform each serves.
PLATFORMS = (("x64", "linux/amd64"), ("arm64", "linux/arm64"))
SEMVER = r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?"
# The kinds kiln's unpacker writes (regular files and directories) by tar's typeflags: Python's
# isfile() would also take a contiguous or sparse file, which the unpacker refuses.
WRITTEN = (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE)


def fail(msg):
    print("pin.sh: " + msg, file=sys.stderr)
    sys.exit(1)


def fetch(url, name):
    path = os.path.join(WORK, name)
    r = subprocess.run(["curl", "-fsSL", "--proto-redir", "=https", "-o", path, url], capture_output=True, text=True)
    if r.returncode != 0:
        fail(f"could not fetch {url}: {r.stderr.strip()}")
    return path


def load(path, what):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        fail(f"{what} is not JSON: {e}")


def digest(path, algo):
    h = hashlib.new(algo)
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h


def utc(text, what):
    try:
        t = datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        fail(f"{what} is no date: {text!r}")
    return t if t.tzinfo else t.replace(tzinfo=datetime.timezone.utc)


def day(text, what):
    try:
        return datetime.date.fromisoformat(text)
    except (TypeError, ValueError):
        fail(f"{what} is no date: {text!r}")


def open_tar(path, url):
    """The archive, open, and its entries; the caller closes it."""
    try:
        t = tarfile.open(path, "r:gz")
        return t, t.getmembers()
    except (OSError, tarfile.TarError) as e:
        fail(f"{url} is no tar.gz: {e}")


def check_entry(m, url):
    """What kiln's unpacker refuses, refused here at pin time: an empty, absolute or climbing path,
    links, devices and every kind but a directory or a regular file, and set-ID and sticky bits."""
    name = m.name
    if not name or name.startswith("/") or ".." in name.split("/"):
        fail(f"{url}: {name!r} leaves the directory")
    if m.issym() or m.islnk():
        fail(f"{url}: {name} is a link")
    if m.type not in WRITTEN:
        fail(f"{url}: {name} is neither a regular file nor a directory (tar type {m.type!r})")
    if m.mode & 0o6000:
        fail(f"{url}: {name} has a set-ID bit")
    if m.mode & 0o1000:
        fail(f"{url}: {name} has the sticky bit")


def parts(name):
    """An entry's path as its names, `.` aside, as the unpacker compares paths."""
    return [p for p in name.split("/") if p not in ("", ".")]


def member_digest(t, m, url):
    """A member's own SHA-256, read from the archive."""
    h = hashlib.sha256()
    try:
        f = t.extractfile(m)
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    except (OSError, tarfile.TarError, AttributeError) as e:
        fail(f"{url}: {m.name} can't be read: {e}")
    return h.hexdigest()


now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
today = now.date()

# 1. The Gemini CLI version: npm's dist-tag, or the version asked.
want = env["PIN_WANT"]
if re.fullmatch(SEMVER, want):
    version, channel = want, "version"
else:
    tags = load(fetch(f"{REG}/-/package/{PKG_PATH}/dist-tags", "dist-tags.json"), "npm's dist-tags")
    version, channel = tags.get(want), want
    if not isinstance(version, str) or not re.fullmatch(SEMVER, version):
        fail(f"npm has no dist-tag {want!r} for {PKG}")

# 2. The registry's record of it and every signature on it.
doc = load(fetch(f"{REG}/{PKG_PATH}/{version}", "doc.json"), f"npm's record of {PKG}@{version}")
if doc.get("name") != PKG or doc.get("version") != version:
    fail(f"npm's record names {doc.get('name')}@{doc.get('version')}, not {PKG}@{version}")
dist = doc.get("dist") or {}
integrity, tarball, sigs = dist.get("integrity"), dist.get("tarball"), dist.get("signatures")
if not isinstance(integrity, str) or not re.fullmatch(r"sha512-[A-Za-z0-9+/]{86}==", integrity):
    fail(f"npm's record of {PKG}@{version} gives no single sha512 integrity: {integrity!r}")
if not isinstance(tarball, str) or not tarball.startswith(REG + "/"):
    fail(f"npm's record of {PKG}@{version} puts its tarball outside the registry: {tarball!r}")
if not isinstance(sigs, list) or not sigs:
    fail(f"npm's record of {PKG}@{version} carries no registry signature")
keys = load(fetch(f"{REG}/-/npm/v1/keys", "keys.json"), "npm's registry keys").get("keys")
if not isinstance(keys, list):
    fail("npm's registry keys list no keys")
message = os.path.join(WORK, "signed.txt")
with open(message, "w", encoding="utf-8") as f:
    f.write(f"{PKG}@{version}:{integrity}")
keyids = []
for i, s in enumerate(sigs):
    keyid = s.get("keyid") if isinstance(s, dict) else None
    key = next((k for k in keys if isinstance(k, dict) and k.get("keyid") == keyid), None)
    if key is None:
        fail(f"registry signature {i}: npm lists no key {keyid!r}")
    if key.get("keytype") != "ecdsa-sha2-nistp256" or key.get("scheme") != "ecdsa-sha2-nistp256":
        fail(f"registry signature {i}: key {keyid} is {key.get('keytype')}/{key.get('scheme')}, not ecdsa-sha2-nistp256")
    if key.get("expires") is not None and utc(key["expires"], f"key {keyid}'s expiry") <= now:
        fail(f"registry signature {i}: key {keyid} expired at {key['expires']}")
    try:
        der = base64.b64decode(key.get("key", ""), validate=True)
        sig = base64.b64decode(s.get("sig", ""), validate=True)
    except ValueError:
        fail(f"registry signature {i}: its key or signature is not base64")
    pem, sig_path = os.path.join(WORK, f"key{i}.pem"), os.path.join(WORK, f"sig{i}.der")
    b64 = base64.b64encode(der).decode()
    with open(pem, "w", encoding="ascii") as f:
        f.write("-----BEGIN PUBLIC KEY-----\n" + "\n".join(b64[j:j + 64] for j in range(0, len(b64), 64)) + "\n-----END PUBLIC KEY-----\n")
    with open(sig_path, "wb") as f:
        f.write(sig)
    r = subprocess.run(["openssl", "dgst", "-sha256", "-verify", pem, "-signature", sig_path, message], capture_output=True, text=True)
    if r.returncode != 0 or "Verified OK" not in r.stdout:
        fail(f"registry signature {i} by {keyid} does not verify over {PKG}@{version}:{integrity}")
    keyids.append(keyid)

# 3. The tarball: its SHA-512 is the signed integrity; what it holds is what the unpacker takes, as a
# tree whose leading package/ is stripped, and its totals are measured for the pin.
tgz = fetch(tarball, "gemini.tgz")
got = "sha512-" + base64.b64encode(digest(tgz, "sha512").digest()).decode()
if got != integrity:
    fail(f"{tarball}: SHA-512 {got} is not the signed integrity {integrity}")
tgz_sha256, tgz_size = digest(tgz, "sha256").hexdigest(), os.path.getsize(tgz)
t, entries = open_tar(tgz, tarball)
files, unpacked, paths, folded = 0, 0, {}, {}
with t:
    for m in entries:
        check_entry(m, tarball)
        names = parts(m.name)
        if not names or names[0] != "package":
            fail(f"{tarball}: {m.name} lies outside package/, the directory the strip drops")
        rel = names[1:]
        if not rel:
            if not m.isdir():
                fail(f"{tarball}: {m.name} is a file the strip leaves no name")
            continue
        path = "/".join(rel)
        if path in paths:
            fail(f"{tarball}: {m.name} is there twice")
        paths[path] = m
        # One path, or one directory on the way to it, where kiln may unpack on a file system that
        # folds case or normalizes names (APFS by default, NTFS for case), as Cursor's pin.sh refuses.
        for i in range(1, len(rel) + 1):
            prefix = "/".join(rel[:i])
            key = unicodedata.normalize("NFC", prefix).casefold()
            if folded.setdefault(key, prefix) != prefix:
                fail(f"{tarball}: {prefix} and {folded[key]} are one path where names fold")
        if path.casefold() in KILN_FILES or rel[0].casefold() == "node":
            fail(f"{tarball}: {m.name} lands where kiln writes the built-in's own files or Node's members")
        if m.isfile():
            files += 1
            unpacked += m.size
    for needed in (ENTRY, "package.json"):
        if needed not in paths or not paths[needed].isfile():
            fail(f"{tarball} holds no regular package/{needed}")
    try:
        manifest = json.load(t.extractfile(paths["package.json"]))
    except (OSError, ValueError, tarfile.TarError) as e:
        fail(f"{tarball}'s package.json is not JSON: {e}")
tree = {"format": "tar.gz", "strip": 1, "unpacked_bytes": unpacked, "entries": len(entries)}
if manifest.get("name") != PKG or manifest.get("version") != version:
    fail(f"{tarball}'s package.json names {manifest.get('name')}@{manifest.get('version')}")
if (manifest.get("bin") or {}).get("gemini") != ENTRY:
    fail(f"{tarball}'s package.json starts gemini from {(manifest.get('bin') or {}).get('gemini')!r}, not {ENTRY}")

# 4. The Node.js major: the newest Gemini's Linux CI tests whose end is after today.
ci_url = f"{RAW}/v{version}/.github/workflows/ci.yml"
ci_path = fetch(ci_url, "ci.yml")
try:
    with open(ci_path, encoding="utf-8") as f:
        ci = f.read()
except (OSError, ValueError) as e:
    fail(f"{ci_url} is not text: {e}")


def tested_majors(text):
    lines = text.splitlines()
    try:
        jobs = next(i for i, l in enumerate(lines) if re.match(r"^jobs:\s*(#.*)?$", l))
    except StopIteration:
        return None
    blocks, current = [], None
    for l in lines[jobs + 1:]:
        if re.match(r"^\S", l):
            break
        if re.match(r"^  [A-Za-z0-9_-]+:\s*(#.*)?$", l):
            current = []
            blocks.append(current)
        elif current is not None:
            current.append(l)
    job = next((b for b in blocks if any(re.match(r"^\s+name:\s*['\"]?Test \(Linux\)", l) for l in b)), None)
    if job is None:
        return None
    for i, l in enumerate(job):
        m = re.match(r"^(\s+)node-version:\s*(.*?)\s*(#.*)?$", l)
        if not m:
            continue
        if m.group(2).startswith("["):
            return sorted({int(x) for x in re.findall(r"(\d+)(?:\.x)?", m.group(2))})
        out = set()
        for item in job[i + 1:]:
            mi = re.match(r"^(\s*)-\s*['\"]?(\d+)(?:\.x)?['\"]?\s*(#.*)?$", item)
            if not mi or len(mi.group(1)) <= len(m.group(1)) - 2:
                break
            out.add(int(mi.group(2)))
        return sorted(out)
    return None


majors = tested_majors(ci)
if not majors:
    fail(f"{ci_url}: no node-version matrix in its 'Test (Linux)' job")
schedule = load(fetch(SCHEDULE, "schedule.json"), "nodejs/Release's schedule.json")
supported = []
for m in majors:
    s = schedule.get(f"v{m}")
    if isinstance(s, dict) and day(s.get("start"), f"v{m}'s start") <= today < day(s.get("end"), f"v{m}'s end"):
        supported.append(m)
if not supported:
    fail(f"no Node.js major that Gemini CLI {version} tests on Linux ({', '.join(map(str, majors))}) is still supported on {today}")
major = max(supported)
end = schedule[f"v{major}"]["end"]

# 5. That major's newest release, with a build for each platform.
index = load(fetch(f"{DIST}/index.json", "index.json"), "nodejs.org's index")


def numeric(v):
    return tuple(int(x) for x in v.lstrip("v").split("."))


releases = [r for r in index if isinstance(r, dict) and re.fullmatch(rf"v{major}\.\d+\.\d+", str(r.get("version")))]
if not releases:
    fail(f"nodejs.org's index lists no v{major} release")
release = max(releases, key=lambda r: numeric(r["version"]))
node = release["version"]
for arch, platform in PLATFORMS:
    if f"linux-{arch}" not in (release.get("files") or []):
        fail(f"Node.js {node} has no linux-{arch} build for {platform}")

# 6. Its SHASUMS256.txt.asc, signed by a key of the active release keyring.
commit = env["PIN_RELEASE_KEYS_COMMIT"]
if not commit:
    commit = load(fetch(f"{API}/repos/nodejs/release-keys/commits/main", "release-keys.json"), "GitHub's record of nodejs/release-keys").get("sha")
if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
    fail(f"nodejs/release-keys commit {commit!r} is not a full SHA-1")
keyring_url = f"{KEYS}/{commit}/gpg-only-active-keys/pubring.kbx"
keyring = fetch(keyring_url, "pubring.kbx")
shasums_url = f"{DIST}/{node}/SHASUMS256.txt.asc"
asc = fetch(shasums_url, "SHASUMS256.txt.asc")
signed = os.path.join(WORK, "SHASUMS256.txt")
gnupg = os.path.join(WORK, "gnupg")
os.mkdir(gnupg, 0o700)
r = subprocess.run(["gpgv", "--homedir", gnupg, "--keyring", os.path.abspath(keyring), "--status-fd", "1", "--output", signed, asc], capture_output=True, text=True)
valid = [l.split() for l in r.stdout.splitlines() if l.startswith("[GNUPG:] VALIDSIG ")]
if r.returncode != 0 or not valid:
    fail(f"{shasums_url} is not signed by a key of nodejs/release-keys' active keyring at {commit}: {r.stderr.strip()}")
signer = valid[0][-1]
sums = {}
with open(signed, encoding="utf-8") as f:
    for l in f:
        m = re.fullmatch(r"([0-9a-f]{64})\s+\*?(\S+)", l.strip())
        if m:
            sums[m.group(2)] = m.group(1)

# 7. Node's archives, each the signed SHA-256, each holding its bin/node and LICENSE once, as regular
# files, each pinned with its own SHA-256 and size (the unpacker holds a member to them).
node_files = {}
for arch, platform in PLATFORMS:
    name = f"node-{node}-linux-{arch}.tar.gz"
    if name not in sums:
        fail(f"{shasums_url} lists no {name} for {platform}")
    url = f"{DIST}/{node}/{name}"
    path = fetch(url, name)
    got = digest(path, "sha256").hexdigest()
    if got != sums[name]:
        fail(f"{url}: SHA-256 {got} is not the signed {sums[name]}")
    t, entries = open_tar(path, url)
    top = f"node-{node}-linux-{arch}"
    picks = []
    with t:
        for member, to, mode in MEMBERS:
            want = [top] + parts(member)
            found = [e for e in entries if parts(e.name) == want]
            if len(found) != 1:
                fail(f"{url} holds {top}/{member} {len(found)} times, not once")
            m = found[0]
            check_entry(m, url)
            if not m.isfile():
                fail(f"{url}: {top}/{member} is not a regular file")
            unpack = {"format": "tar.gz", "member": f"{top}/{member}", "sha256": member_digest(t, m, url), "size": m.size}
            picks.append({"url": url, "sha256": got, "size": os.path.getsize(path), "path": to, "mode": mode, "unpack": unpack})
    node_files[platform] = picks

# 8. pin.json, written whole or not at all.
gemini = {"url": tarball, "sha256": tgz_sha256, "size": tgz_size, "path": ".", "unpack": tree}
node_bin = next(to for member, to, _ in MEMBERS if member == "bin/node")
pin = {
    "harness": "gemini",
    "version": version,
    "channel": channel,
    "read_at": now.isoformat().replace("+00:00", "Z"),
    "verified": {
        "gemini": {
            "how": "Every npm registry signature, ECDSA P-256 over <name>@<version>:<integrity>, verifies with the registry key of its keyid, and the tarball's SHA-512 is that integrity. Google publishes no provenance attestation for the package.",
            "package": PKG,
            "integrity": integrity,
            "keys": f"{REG}/-/npm/v1/keys",
            "keyids": keyids,
            "files": files,
            "entry": ENTRY,
            "engines": (manifest.get("engines") or {}).get("node"),
        },
        "node": {
            "how": "The newest Node.js major in the node-version matrix of the 'Test (Linux)' job of Gemini CLI's ci.yml at its tag whose end in nodejs/Release's schedule is after the day pinned, at that major's newest release; SHASUMS256.txt.asc verified by gpgv against nodejs/release-keys' gpg-only-active-keys keyring at its commit, and each archive's SHA-256 is the signed one.",
            "version": node,
            "tested_majors": majors,
            "major": major,
            "end": end,
            "ci": {"url": ci_url, "sha256": digest(ci_path, "sha256").hexdigest()},
            "schedule": {"url": SCHEDULE, "sha256": digest(os.path.join(WORK, "schedule.json"), "sha256").hexdigest()},
            "shasums": {"url": shasums_url, "sha256": digest(asc, "sha256").hexdigest()},
            "signer": signer,
            "keyring": {"url": keyring_url, "commit": commit, "sha256": digest(keyring, "sha256").hexdigest()},
        },
    },
    "base": env["PIN_BASE"],
    "smoke": {"program": node_bin, "args": [f"{HARNESS}/{ENTRY}", "--version"], "stdout": version},
    "builds": [{"platform": p, "files": [gemini] + node_files[p]} for _, p in PLATFORMS],
}
out = env["PIN_OUT"]
tmp = f"{out}.tmp.{os.getpid()}"
try:
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(pin, f, indent=2)
        f.write("\n")
    os.replace(tmp, out)
except OSError as e:
    if os.path.exists(tmp):
        os.remove(tmp)
    fail(f"could not write {out}: {e}")
print(f"pinned Gemini CLI {version} ({channel}) on Node.js {node} (tested {majors}, {major} ends {end}, signed by {signer}) into {out}")
EOF

[[ $PIN_BASE =~ ^[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]] || die "PIN_BASE must be <ref>@sha256:<64 hex digits>, not $PIN_BASE"
python3 -I -c "$PY"
