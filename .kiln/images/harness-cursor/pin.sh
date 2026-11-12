#!/usr/bin/env bash
# Pins Cursor's Linux builds of its CLI agent, cursor-agent, for kiln's built-in Agentfile
# harness-cursor (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27; its own plan
# 01a1ce705e5c7504b88732f443d99288, step 1).
#
# The version is the one Cursor's installer, https://cursor.com/install, downloads: its one
# DOWNLOAD_URL line names it. VERSION, if given, names another. Each Linux build is
# https://downloads.cursor.com/lab/<version>/linux/<x64|arm64>/agent-cli-package.tar.gz.
#
# Cursor publishes no signature for these, only <archive URL>.sha256 beside each, on the same
# origin. A build is pinned only when:
#   - its .sha256 answers 200 and is the one line `<64 lowercase hex>  agent-cli-package.tar.gz`;
#   - the archive answers 200 and its SHA-256 equals that;
#   - every entry is a regular file or a directory under dist-package/, the directory the
#     installer strips, and dist-package/cursor-agent is a regular file its owner may execute.
# pin.json records the check as an unsigned, same-origin checksum: it shows a download is the
# archive that origin serves, not who built it.
#
# Each build's one file unpacks with {format tar.gz, strip 1}, and pin.json gives two numbers
# measured here from the archive it pins, which kiln's unpacker holds the archive to:
#   entries         how many entries the archive holds, dist-package/ itself among them;
#   unpacked_bytes  the sum of its regular files' sizes.
#
#   pin.sh [VERSION]   writes pin.json beside this script and prints what it read and checked
#
# Needs bash, curl, and python3 3.11 or later (its standard library alone). Downloads go to a
# temporary directory under ${XDG_CACHE_HOME:-~/.cache}, one archive at a time, removed on exit
# (/tmp is a RAM disk on kiln's Linux runners).
#
# For its tests alone (docs/research/s27-cursor-agentfile/step1/pin_test.sh):
# PIN_TEST_BASE_URL=https://HOST[:PORT][/PATH] stands in for both https://cursor.com (the
# installer) and https://downloads.cursor.com (the archives).
#
# Exit status, as sysexits.h names them: 0 pinned; 64 usage; 65 what Cursor served failed a
# check; 69 a fetch got no 200 answer, or a program pin.sh needs is missing. Any other: pin.sh
# itself failed, as its message says. No failure leaves a pin.json or changes the one there.
set -euo pipefail
# Byte-wise character classes: [A-Za-z] is ASCII alone.
export LC_ALL=C

INSTALLER=https://cursor.com/install
DOWNLOADS=https://downloads.cursor.com
# The base the Agentfile's ARG BASE defaults to: the shared layout's (kiln plan step 19).
BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10
# kiln's platform, then Cursor's name for its architecture, as its installer maps `uname -m`.
PLATFORMS=(linux/amd64:x64 linux/arm64:arm64)
HOW="Each archive's SHA-256 equals the one in the .sha256 Cursor publishes beside it (the archive's URL with .sha256 added, on the same origin, over HTTPS). Cursor publishes no signature for its CLI, so this shows a download is the archive that origin serves, not who built it."

say() { printf 'pin.sh: %s\n' "$*"; }
die() {
  local status=$1
  shift
  printf 'pin.sh: %s\n' "$*" >&2
  exit "$status"
}

case ${1-} in
  -h | --help)
    awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
    exit 0
    ;;
  -*) die 64 "unknown option $1 (usage: pin.sh [VERSION])" ;;
esac
[ $# -le 1 ] || die 64 "one VERSION at the most (usage: pin.sh [VERSION])"

# segment S: whether S is one URL path segment of [0-9A-Za-z._-] (RFC 3986 §3.3), and neither .
# nor .., the dot-segments a URL's path drops or climbs with (RFC 3986 §5.2.4).
segment() { [[ $1 =~ ^[0-9A-Za-z._-]+$ && $1 != . && $1 != .. ]]; }

if [ $# -eq 1 ] && ! segment "$1"; then
  printf -v quoted '%q' "$1"
  die 64 "VERSION $quoted is not one URL path segment of [0-9A-Za-z._-]"
fi
for program in curl python3; do
  command -v "$program" > /dev/null || die 69 "pin.sh needs $program"
done
python3 -I -c 'import sys; sys.exit(sys.version_info < (3, 11))' || die 69 "pin.sh needs python3 3.11 or later (hashlib.file_digest)"

if [ -n "${PIN_TEST_BASE_URL:-}" ]; then
  INSTALLER=$PIN_TEST_BASE_URL/install
  DOWNLOADS=$PIN_TEST_BASE_URL
  say "PIN_TEST_BASE_URL is set: fetching from $PIN_TEST_BASE_URL, not Cursor's origins"
fi

here=$(cd "$(dirname "$0")" && pwd)
out=$here/pin.json
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
T=$(mktemp -d "$cache/cursor-pin.XXXXXX")
tmp=
trap 'rm -rf "$T" ${tmp:+"$tmp"}' EXIT

IFS= read -r -d '' PY << 'EOF' || true
import hashlib, io, json, re, stat, sys, tarfile, unicodedata, zlib

PACKAGE = "agent-cli-package.tar.gz"
TOP = "dist-package"
AGENT = TOP + "/cursor-agent"
# kiln writes the harness's harness.json beside the stripped tree (plan step 2).
HARNESS_JSON = TOP + "/harness.json"
# A whole .sha256 as sha256sum writes it: the digest, two spaces, the name, a newline.
LINE = 64 + 2 + len(PACKAGE) + 1
KINDS = {tarfile.SYMTYPE: "symbolic link", tarfile.LNKTYPE: "hard link", tarfile.CHRTYPE: "character device",
         tarfile.BLKTYPE: "block device", tarfile.FIFOTYPE: "FIFO", tarfile.CONTTYPE: "contiguous file",
         tarfile.GNUTYPE_SPARSE: "sparse file"}


def digest(path):
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest(), f.tell()


def installer(path):
    with open(path, "rb") as f:
        lines = [line.strip() for line in f if line.strip().startswith(b"DOWNLOAD_URL=")]
    if not lines:
        sys.exit("it has no DOWNLOAD_URL line")
    if len(lines) > 1:
        sys.exit(f"it has {len(lines)} DOWNLOAD_URL lines, so which version it downloads is unclear")
    m = re.fullmatch(rb'DOWNLOAD_URL="https://downloads\.cursor\.com/lab/([^/"]+)/\$\{OS\}/\$\{ARCH\}/' + re.escape(PACKAGE.encode()) + b'"', lines[0])
    if not m:
        sys.exit(f"its DOWNLOAD_URL line is not the form pin.sh reads: {lines[0].decode(errors='replace')}")
    print(m.group(1).decode(errors="replace"))


def sidecar(path):
    with open(path, "rb") as f:
        data = f.read(LINE + 1)
    m = re.fullmatch(rb"([0-9a-f]{64})  " + re.escape(PACKAGE.encode()) + rb"\n", data)
    if not m:
        sys.exit(f"it reads {data[:LINE]!r}, not the one line `<64 lowercase hex>  {PACKAGE}`")
    print(m.group(1).decode())


def claim(path, folded):
    # Names that differ only in case or Unicode normalization are one file where kiln may unpack
    # (APFS by default, and NTFS for case): the tree there would not be the archive's.
    key = unicodedata.normalize("NFC", path).casefold()
    other = folded.setdefault(key, path)
    if other != path:
        sys.exit(f"{path} and {other} are one path on a filesystem that folds case or normalizes names")


def entry(m, kinds, folded):
    """Checks one entry, given what earlier ones made: kinds maps each path made so far, its own
    or a parent's, to "file", "dir" or "parent"; folded maps each one's folded name to it."""
    name = m.name
    if m.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE):
        sys.exit(f"{name} is a {KINDS.get(m.type, 'tar type ' + repr(m.type))}: only regular files and directories unpack")
    parts = name.split("/")
    if name.startswith("/"):
        sys.exit(f"{name} is an absolute path")
    if ".." in parts:
        sys.exit(f"{name} has a .. component")
    if "" in parts or "." in parts:
        sys.exit(f"{name} has an empty or . component")
    if parts[0] != TOP:
        sys.exit(f"{name} is outside {TOP}/")
    is_dir = m.type == tarfile.DIRTYPE
    if len(parts) == 1 and not is_dir:
        sys.exit(f"{name} is not a directory, and the strip leaves nothing of it")
    if name == HARNESS_JSON:
        sys.exit(f"{name} is where kiln writes the harness's harness.json")
    if m.mode & (stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX):
        sys.exit(f"{name} has a set-ID or sticky bit (mode {m.mode:o})")
    for i in range(1, len(parts)):
        parent = "/".join(parts[:i])
        made = kinds.get(parent)
        if made == "file":
            sys.exit(f"{name} is under {parent}, a regular file")
        if made is None:
            claim(parent, folded)
            kinds[parent] = "parent"
    made = kinds.get(name)
    if made in ("file", "dir"):
        sys.exit(f"{name} is in the archive twice")
    if made == "parent" and not is_dir:
        sys.exit(f"{name} is a regular file, but an earlier entry is under it")
    if made is None:
        claim(name, folded)
    kinds[name] = "dir" if is_dir else "file"
    return is_dir


def archive(path, want):
    got, size = digest(path)
    if got != want:
        sys.exit(f"its SHA-256 is {got}, not {want}, the one its .sha256 gives")
    kinds, folded = {}, {}
    files = dirs = unpacked = 0
    agent = None
    try:
        with tarfile.open(path, mode="r:gz") as tf:
            for m in tf:
                if entry(m, kinds, folded):
                    dirs += 1
                else:
                    files += 1
                    unpacked += m.size
                if m.name == AGENT:
                    agent = m.isreg() and bool(m.mode & stat.S_IXUSR)
            # Python's tarfile ends an archive at a header it can't read; so does nothing else.
            # What follows the last entry must be the end-of-archive blocks and their zero
            # padding alone, read to the end so gzip checks its CRC-32 and length (RFC 1952).
            tf.fileobj.seek(tf.offset)
            while chunk := tf.fileobj.read(io.DEFAULT_BUFFER_SIZE):
                if chunk.strip(b"\0"):
                    sys.exit("it holds data past the end of its tar archive")
    except (tarfile.TarError, OSError, EOFError, zlib.error) as e:
        sys.exit(f"it is not a whole tar.gz: {e}")
    if agent is None:
        sys.exit(f"it holds no {AGENT}")
    if not agent:
        sys.exit(f"{AGENT} is not a regular file its owner may execute")
    print(size, files + dirs, unpacked, files, dirs)


def write(out, version, source, at, installer_url, installer_sha, how, base, *fields):
    read = {"from": source}
    if source == "installer":
        read |= {"url": installer_url, "sha256": installer_sha}
    builds = []
    for i in range(0, len(fields), 6):
        platform, url, sha, size, entries, unpacked = fields[i:i + 6]
        unpack = {"format": "tar.gz", "strip": 1, "unpacked_bytes": int(unpacked), "entries": int(entries)}
        builds.append({"platform": platform, "files": [{"url": url, "sha256": sha, "size": int(size), "path": ".", "unpack": unpack}]})
    pin = {
        "harness": "cursor",
        "version": version,
        "version_read": read | {"at": at},
        "verified": {"checksum": "same-origin", "signed": False, "how": how},
        "base": base,
        "builds": builds,
    }
    # "x": a new file, made with the umask's mode as any other in the checkout.
    with open(out, "x") as f:
        json.dump(pin, f, indent=2)
        f.write("\n")


cmd, *args = sys.argv[1:]
{"installer": installer, "sidecar": sidecar, "archive": archive, "write": write,
 "sha256": lambda path: print(digest(path)[0])}[cmd](*args)
EOF
py() { python3 -I -c "$PY" "$@"; }

# fetch URL FILE: URL's body in FILE, over HTTPS alone (--proto holds redirects to it too, as
# curl(1) says of --proto-redir), with TLS 1.2 or later (RFC 8996 deprecates 1.0 and 1.1).
# Anything but a final 200 fails.
fetch() {
  local code
  code=$(curl --proto '=https' --tlsv1.2 -sSL -o "$2" -w '%{http_code}' -- "$1") || die 69 "fetching $1 failed"
  [ "$code" = 200 ] || die 69 "$1 answered $code, not 200"
}

read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
if [ $# -eq 1 ]; then
  version=$1 source=argument installer_sha=
  say "version $version, as asked"
else
  fetch "$INSTALLER" "$T/install"
  version=$(py installer "$T/install" 2>&1) || die 65 "$INSTALLER: $version"
  if ! segment "$version"; then
    printf -v quoted '%q' "$version"
    die 65 "$INSTALLER: its DOWNLOAD_URL names version $quoted, not one URL path segment of [0-9A-Za-z._-]"
  fi
  source=installer installer_sha=$(py sha256 "$T/install")
  say "$INSTALLER (SHA-256 $installer_sha) downloads version $version"
fi

builds=()
for platform_arch in "${PLATFORMS[@]}"; do
  platform=${platform_arch%%:*} arch=${platform_arch#*:}
  url=$DOWNLOADS/lab/$version/linux/$arch/agent-cli-package.tar.gz
  fetch "$url.sha256" "$T/sha256"
  sha=$(py sidecar "$T/sha256" 2>&1) || die 65 "$url.sha256: $sha"
  fetch "$url" "$T/archive"
  measured=$(py archive "$T/archive" "$sha" 2>&1) || die 65 "$url: $measured"
  rm -f "$T/archive"
  read -r size entries unpacked files dirs <<< "$measured"
  say "$platform: $url"
  say "  SHA-256 $sha, as its .sha256 says; $size bytes; $entries entries ($files files, $dirs directories), $unpacked bytes unpacked"
  builds+=("$platform" "$url" "$sha" "$size" "$entries" "$unpacked")
done

tmp=$out.tmp.$$
py write "$tmp" "$version" "$source" "$read_at" "$INSTALLER" "$installer_sha" "$HOW" "$BASE" "${builds[@]}"
mv -f "$tmp" "$out"
tmp=
say "pinned cursor-agent $version for linux/amd64 and linux/arm64 in $out, SHA-256 $(py sha256 "$out")"
