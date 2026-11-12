#!/usr/bin/env bash
# Pins Gemini CLI's Linux build and the Node.js it runs on into pin.json beside this script, for
# kiln's built-in Gemini Agentfile. kiln plan 01a1ce705e2374dfbac201fe6ba98184 (step 21 of
# 01a1cd8e862e7169a074db3d5fa33f3d), step 1; research 01a1ce705e2472df9f3dd81be0531c9f.
#
#   pin.sh [latest | <npm dist-tag> | <x.y.z>]    default: npm's latest
#
# Gemini CLI's Linux build is its npm package, run by Node.js (f5, f6). It is checked as npm
# documents it (f7): every registry signature, ECDSA P-256 over <name>@<version>:<integrity>, with
# the registry key of its keyid from /-/npm/v1/keys; then the tarball's SHA-512 against that
# integrity. Google publishes no provenance attestation for it.
#
# Its Node.js is derived, not chosen (R03, f9, f10). The major is the newest one in the node-version
# matrix of the 'Test (Linux)' job in Gemini's .github/workflows/ci.yml at the pinned tag whose end in
# nodejs/Release's schedule.json is after today; the release is that major's newest in nodejs.org's
# index. Its SHASUMS256.txt.asc is checked with gpgv against nodejs/release-keys'
# gpg-only-active-keys keyring at a recorded commit, and each archive against the SHA-256 the signed
# text gives (f11). The .tar.gz builds are pinned: kiln unpacks them with crates it has (f14).
#
# pin.json lists, per platform and libc, the files a build context needs, in the unpack shape of
# crates/kiln/src/agentfiles/unpack.rs (step 25's): Node's bin/node and LICENSE picked as members,
# so its archive's symlinks never reach a context (f15), and the npm tarball unpacked whole with its
# package/ prefix stripped. The musl builds carry no Node: Alpine's own comes from the Agentfile
# (f11, f16). pin.json is written only when every check passed.
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
import base64, datetime, hashlib, json, os, re, subprocess, sys, tarfile

env = os.environ
WORK = env["PIN_WORK"]
REG, RAW, DIST = env["PIN_NPM_REGISTRY"], env["PIN_GEMINI_RAW"], env["PIN_NODE_DIST"]
SCHEDULE, KEYS, API = env["PIN_NODE_SCHEDULE"], env["PIN_RELEASE_KEYS"], env["PIN_GITHUB_API"]
PKG, PKG_PATH = "@google/gemini-cli", "@google%2fgemini-cli"
# The package's own bin entry: the launchers start Node on it (plan step 2).
ENTRY = "bundle/gemini.js"
# Node's platforms by its own names, and the OCI platform each serves.
PLATFORMS = (("x64", "linux/amd64"), ("arm64", "linux/arm64"))
SEMVER = r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?"


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
    """What step 25's unpack refuses, refused here at pin time: links, devices and other kinds, set-ID
    bits, and paths that leave the directory."""
    name = m.name
    if name.startswith("/") or ".." in name.split("/"):
        fail(f"{url}: {name} leaves the directory")
    if m.issym() or m.islnk():
        fail(f"{url}: {name} is a link")
    if not (m.isfile() or m.isdir()):
        fail(f"{url}: {name} is neither a regular file nor a directory")
    if m.mode & 0o6000:
        fail(f"{url}: {name} has a set-ID bit")


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

# 3. The tarball: its SHA-512 is the signed integrity; what it holds is what unpack takes.
tgz = fetch(tarball, "gemini.tgz")
got = "sha512-" + base64.b64encode(digest(tgz, "sha512").digest()).decode()
if got != integrity:
    fail(f"{tarball}: SHA-512 {got} is not the signed integrity {integrity}")
tgz_sha256, tgz_size = digest(tgz, "sha256").hexdigest(), os.path.getsize(tgz)
t, entries = open_tar(tgz, tarball)
files = 0
with t:
    for m in entries:
        check_entry(m, tarball)
        if not m.name.startswith("package/"):
            fail(f"{tarball}: {m.name} lies outside package/")
        files += m.isfile()
    names = {m.name for m in entries if m.isfile()}
    for needed in ("package/" + ENTRY, "package/package.json"):
        if needed not in names:
            fail(f"{tarball} holds no {needed}")
    try:
        manifest = json.load(t.extractfile("package/package.json"))
    except (OSError, ValueError, tarfile.TarError) as e:
        fail(f"{tarball}'s package.json is not JSON: {e}")
if manifest.get("name") != PKG or manifest.get("version") != version:
    fail(f"{tarball}'s package.json names {manifest.get('name')}@{manifest.get('version')}")
if (manifest.get("bin") or {}).get("gemini") != ENTRY:
    fail(f"{tarball}'s package.json starts gemini from {(manifest.get('bin') or {}).get('gemini')!r}, not {ENTRY}")

# 4. The Node.js major: the newest Gemini's Linux CI tests whose end is after today.
ci_url = f"{RAW}/v{version}/.github/workflows/ci.yml"
with open(fetch(ci_url, "ci.yml"), encoding="utf-8") as f:
    ci = f.read()


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

# 7. Node's archives, each the signed SHA-256, each holding a regular bin/node and LICENSE.
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
    t.close()
    top = f"node-{node}-linux-{arch}"
    picks = []
    for member, mode in (("bin/node", "0755"), ("LICENSE", "0644")):
        m = next((e for e in entries if e.name == f"{top}/{member}"), None)
        if m is None:
            fail(f"{url} holds no {top}/{member}")
        check_entry(m, url)
        if not m.isfile():
            fail(f"{url}: {top}/{member} is not a regular file")
        picks.append({"url": url, "sha256": got, "size": os.path.getsize(path), "path": f"node/{member}", "mode": mode, "unpack": {"format": "tar.gz", "member": f"{top}/{member}"}})
    node_files[platform] = picks

# 8. pin.json, written whole or not at all.
gemini = {"url": tarball, "sha256": tgz_sha256, "size": tgz_size, "path": ".", "unpack": {"format": "tar.gz", "strip": 1}}
base_ref, base_digest = env["PIN_BASE"].split("@", 1)
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
        },
        "node": {
            "how": "The newest Node.js major in the node-version matrix of the 'Test (Linux)' job of Gemini CLI's ci.yml at its tag whose end in nodejs/Release's schedule is after the day pinned, at that major's newest release; SHASUMS256.txt.asc verified by gpgv against nodejs/release-keys' gpg-only-active-keys keyring at its commit, and each archive's SHA-256 is the signed one.",
            "version": node,
            "tested_majors": majors,
            "major": major,
            "end": end,
            "ci": {"url": ci_url, "sha256": hashlib.sha256(ci.encode()).hexdigest()},
            "schedule": {"url": SCHEDULE, "sha256": digest(os.path.join(WORK, "schedule.json"), "sha256").hexdigest()},
            "shasums": {"url": shasums_url, "sha256": digest(asc, "sha256").hexdigest()},
            "signer": signer,
            "keyring": {"url": keyring_url, "commit": commit, "sha256": digest(keyring, "sha256").hexdigest()},
        },
    },
    "base": {"ref": base_ref, "digest": base_digest},
    "smoke": {"program": "bin/gemini", "args": ["--version"], "stdout": version},
    "builds": [{"platform": p, "variant": "gnu", "files": node_files[p] + [gemini]} for _, p in PLATFORMS]
    + [{"platform": p, "variant": "musl", "files": [gemini]} for _, p in PLATFORMS],
}
out = env["PIN_OUT"]
tmp = f"{out}.tmp.{os.getpid()}"
with open(tmp, "w", encoding="utf-8") as f:
    json.dump(pin, f, indent=2)
    f.write("\n")
os.replace(tmp, out)
print(f"pinned Gemini CLI {version} ({channel}) on Node.js {node} (tested {majors}, {major} ends {end}, signed by {signer}) into {out}")
EOF

[[ $PIN_BASE =~ ^[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]] || die "PIN_BASE must be <ref>@sha256:<64 hex digits>, not $PIN_BASE"
python3 -I -c "$PY"
