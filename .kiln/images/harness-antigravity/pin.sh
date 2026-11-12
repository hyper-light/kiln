#!/usr/bin/env bash
# Pins Google's Linux builds of the Antigravity CLI (agy) for kiln's built-in Agentfile
# harness-antigravity (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26).
#
# Verified as Google's own installer verifies them (https://antigravity.google/cli/install.sh):
# the platform's manifest from Google's updater, read over HTTPS, names the version, the
# archive's URL and its SHA-512. Google publishes no signature for the CLI. The updater keeps a
# manifest for its current version only, so that is the only version with a publisher checksum
# and the only one this pins: asked for another, it refuses.
#
# The archives are Google's static musl builds (no loader or libc from the base image), each a
# .tar.gz whose one entry is the regular file `antigravity`, installed as bin/agy, the name
# Google's installer gives it. The pin says the archive holds it alone (`"alone": true`, checked
# here), so kiln's unpacker refuses an archive with any other entry (plan step 3).
#
# The pin also records the smoke an image's agy must pass, in the shape every harness's pin gives
# its own (kiln plan step 19's layout): `bin/agy --version` prints the version and a newline (1.5.6's
# musl builds wrote 31 2e 35 2e 36 0a in a microVM on both platforms: step 4's record); `stdout` is
# that line without its newline.
#
#   pin.sh [VERSION]   writes pin.json beside this script, or at $PIN_OUT
#
# Needs bash, curl, sha256sum, sha512sum and python3 (its standard library alone, for JSON and
# the archive's entry). PIN_FIXTURE=DIR answers every fetch from DIR instead of the network, for
# pin_test.sh: DIR/updater/ mirrors the updater's paths and DIR/storage/ the archives' paths
# under $STORAGE.
#
# Exit status: 0 pinned; 2 the version asked for has no publisher checksum; 3 a check failed;
# 64 usage.
set -euo pipefail

UPDATER=https://antigravity-cli-auto-updater-974169037036.us-central1.run.app
STORAGE=https://storage.googleapis.com/antigravity-public/antigravity-cli/
# The base the Agentfile's ARG BASE defaults to: the shared layout's (kiln plan step 19).
BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10
# kiln's platform, then the updater's name for Google's static musl build of it (install.sh
# picks linux_${arch}_musl where it finds musl).
PLATFORMS=(linux/amd64:linux_amd64_musl linux/arm64:linux_arm64_musl)
HOW="Each archive's SHA-512 equals the one its platform's manifest from Google's updater gives over HTTPS, as Google's install.sh checks it; Google publishes no signature for the CLI."

say() { printf 'pin.sh: %s\n' "$*"; }
fail() { printf 'pin.sh: %s\n' "$*" >&2; exit 3; }

case "${1:-}" in
  -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
esac
[ $# -le 1 ] || { printf 'usage: pin.sh [VERSION]\n' >&2; exit 64; }
asked=${1:-}

here=$(cd "$(dirname "$0")" && pwd)
out=${PIN_OUT:-$here/pin.json}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
T=$(mktemp -d "$cache/agy-pin.XXXXXX")
tmp=
trap 'rm -rf "$T" ${tmp:+"$tmp"}' EXIT

# fetch URL FILE: the body of URL in FILE, over HTTPS only (redirects included).
fetch() {
  local url=$1 file=$2
  if [ -n "${PIN_FIXTURE:-}" ]; then
    case $url in
      "$UPDATER"/*) cp -- "$PIN_FIXTURE/updater/${url#"$UPDATER"/}" "$file" 2>/dev/null || fail "fixture has no $url" ;;
      "$STORAGE"*) cp -- "$PIN_FIXTURE/storage/${url#"$STORAGE"}" "$file" 2>/dev/null || fail "fixture has no $url" ;;
      *) fail "fixture serves no $url" ;;
    esac
  else
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --retry 3 -o "$file" -- "$url" || fail "fetching $url failed"
  fi
}

# json FILE EXPR: EXPR (Python over the parsed document `d`) printed, or the check fails.
json() {
  python3 -I -c 'import json, sys
d = json.load(open(sys.argv[1]))
print(eval(sys.argv[2], {"__builtins__": {}}, {"d": d}))' "$1" "$2" 2>/dev/null || fail "$1 is not the JSON expected ($2)"
}

sha256() { sha256sum -- "$1" | cut -d' ' -f1; }
sha512() { sha512sum -- "$1" | cut -d' ' -f1; }
size() { wc -c < "$1" | tr -d ' '; }

read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
fetch "$UPDATER/releases" "$T/releases.json"
version=$(json "$T/releases.json" 'd[0]["version"]')
eid=$(json "$T/releases.json" 'd[0]["execution_id"]')
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "/releases' first version is not x.y.z: $version"
[[ $eid =~ ^[0-9]+$ ]] || fail "/releases' first execution id is not a number: $eid"
say "/releases: newest $version, execution id $eid"

records=()
for entry in "${PLATFORMS[@]}"; do
  platform=${entry%%:*} name=${entry#*:}
  manifest_url=$UPDATER/manifests/$name.json
  fetch "$manifest_url" "$T/$name.json"
  mversion=$(json "$T/$name.json" 'd["version"]')
  url=$(json "$T/$name.json" 'd["url"]')
  want512=$(json "$T/$name.json" 'd["sha512"]')
  [ "$mversion" = "$version" ] || fail "manifest $name names $mversion, but /releases' newest is $version: the manifests and the release list disagree"
  if [ -n "$asked" ] && [ "$asked" != "$mversion" ]; then
    printf 'pin.sh: agy %s has no publisher checksum: Google'"'"'s updater serves a manifest for its current version, %s, alone\n' "$asked" "$mversion" >&2
    exit 2
  fi
  case $url in
    "$STORAGE$version-$eid/"*.tar.gz) ;;
    *) fail "manifest $name's URL $url is not under $STORAGE$version-$eid/ or not a .tar.gz" ;;
  esac
  [[ ${url#"$STORAGE"} =~ ^[A-Za-z0-9._/-]+$ ]] || fail "manifest $name's URL has unexpected characters: $url"
  [[ $want512 =~ ^[0-9a-f]{128}$ ]] || fail "manifest $name's sha512 is not 128 hex digits"

  archive=$T/$name.tar.gz
  fetch "$url" "$archive"
  got512=$(sha512 "$archive")
  [ "$got512" = "$want512" ] || fail "$url: SHA-512 $got512 is not the manifest's $want512"
  member=$(python3 -I -c 'import hashlib, sys, tarfile
try:
    with tarfile.open(sys.argv[1], mode="r|gz") as tf:
        seen = 0
        for m in tf:
            seen += 1
            if seen > 1:
                sys.exit("it holds more than one entry")
            if m.name != "antigravity":
                sys.exit(f"its entry is {m.name!r}, not antigravity")
            if not m.isreg():
                sys.exit("its entry antigravity is not a regular file")
            f = tf.extractfile(m)
            h, n = hashlib.sha256(), 0
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
                n += len(chunk)
            if n != m.size:
                sys.exit(f"its entry antigravity holds {n} bytes, not the {m.size} its header says")
            digest, size = h.hexdigest(), n
        if seen == 0:
            sys.exit("it holds no entry")
        print(digest, size)
except (tarfile.TarError, OSError, EOFError) as e:
    sys.exit(f"it is not a readable tar.gz: {e}")' "$archive" 2>&1) || fail "$url: $member"
  read -r member256 membersize <<< "$member"
  a256=$(sha256 "$archive") asize=$(size "$archive")
  say "$platform: $url"
  say "  SHA-512 equals the manifest's; archive SHA-256 $a256, $asize bytes; antigravity SHA-256 $member256, $membersize bytes"
  records+=("$platform" "$url" "$a256" "$got512" "$asize" "$member256" "$membersize" "$manifest_url" "$(sha256 "$T/$name.json")")
done

tmp=$(mktemp "$out.XXXXXX")
python3 -I -c 'import json, sys
a = sys.argv[1:]
harness, version, eid, how, read_at, base, releases_url, releases_sha = a[:8]
rest = a[8:]
builds, manifests = [], []
for i in range(0, len(rest), 9):
    platform, url, sha256, sha512, size, m256, msize, murl, msha = rest[i:i + 9]
    builds.append({"platform": platform, "files": [{"url": url, "sha256": sha256, "sha512": sha512, "size": int(size), "path": "bin/agy", "mode": "0755", "unpack": {"format": "tar.gz", "member": "antigravity", "alone": True, "sha256": m256, "size": int(msize)}}]})
    manifests.append({"url": murl, "sha256": msha})
pin = {
    "harness": harness,
    "version": version,
    "execution_id": eid,
    "verified": {
        "how": how,
        "read_at": read_at,
        "releases": {"url": releases_url, "sha256": releases_sha},
        "manifests": manifests,
    },
    "base": base,
    "smoke": {"program": "bin/agy", "args": ["--version"], "stdout": version},
    "builds": builds,
}
json.dump(pin, sys.stdout, indent=2)
print()' antigravity "$version" "$eid" "$HOW" "$read_at" "$BASE" "$UPDATER/releases" "$(sha256 "$T/releases.json")" "${records[@]}" > "$tmp" || { rm -f "$tmp"; fail "writing $out failed"; }
mv -f "$tmp" "$out"
say "pinned agy $version (execution id $eid) for linux/amd64 and linux/arm64 in $out, sha256 $(sha256 "$out")"
