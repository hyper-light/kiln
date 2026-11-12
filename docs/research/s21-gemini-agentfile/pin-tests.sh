#!/usr/bin/env bash
# Tests .kiln/images/harness-gemini/pin.sh (kiln plan 01a1ce705e2374dfbac201fe6ba98184, step 1) on a
# Linux runner:
#   A. pin from npm, GitHub and nodejs.org, then download every pinned file again and check its
#      SHA-256 and size against pin.json;
#   B. serve those same records and files from a local HTTP server, change one thing at a time, and
#      check that pin.sh refuses each change, names its check and writes no pin.json; an unchanged
#      copy (the control) pins the same files as A.
#
#   pin-tests.sh PIN_SH WORK_DIR
#
# Writes WORK_DIR/pin.json (A's pin) and WORK_DIR/tests.log. The one server it starts is stopped by
# the PID recorded as it started.
set -euo pipefail

pin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
W=$2
mkdir -p "$W"
cd "$W"
exec > >(tee "$W/tests.log") 2>&1
echo "# pin.sh tests on $(hostname) ($(uname -srm)), $(date -u +%FT%TZ)"
echo "# pin.sh sha256 $(sha256sum "$pin" | cut -d' ' -f1); curl $(curl --version | head -1 | cut -d' ' -f2), gpgv $(gpgv --version | head -1 | awk '{print $NF}'), $(openssl version | cut -d' ' -f1-2), python $(python3 -c 'import platform; print(platform.python_version())')"

server=
cleanup() {
    [[ -n $server ]] && kill "$server" 2>/dev/null || true
}
trap cleanup EXIT
cache=${XDG_CACHE_HOME:-$HOME/.cache}
leftover() { find "$cache" -maxdepth 1 -name 'pin-harness-gemini.*' | wc -l | tr -d ' '; }
before=$(leftover)

echo
echo "## A. pin from npm, GitHub and nodejs.org"
PIN_OUT=$W/pin.json bash "$pin"
python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1]))
n = d["verified"]["node"]
print("pinned", d["version"], "channel", d["channel"], "node", n["version"], "tested", n["tested_majors"], "major", n["major"], "ends", n["end"], "signer", n["signer"])
for b in d["builds"]:
    print("build", b["platform"], b["variant"], [(f["path"], f["unpack"]) for f in b["files"]])' "$W/pin.json"

echo
echo "## A. every pinned file downloaded again and checked against pin.json"
python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1]))
seen = {}
for b in d["builds"]:
    for f in b["files"]:
        seen[f["url"]] = (f["sha256"], f["size"])
for url, (sha, size) in seen.items():
    print(url, sha, size)' "$W/pin.json" >"$W/pinned.txt"
mkdir -p "$W/again"
ok=0 n=0
while read -r url want_sha want_size; do
    n=$((n + 1))
    f=$W/again/$(basename "$url")
    curl -fsSL -o "$f" "$url"
    got_sha=$(sha256sum "$f" | cut -d' ' -f1) got_size=$(wc -c <"$f" | tr -d ' ')
    if [[ $got_sha == "$want_sha" && $got_size == "$want_size" ]]; then
        ok=$((ok + 1))
        echo "match: $url sha256 $got_sha size $got_size"
    else
        echo "MISMATCH: $url sha256 $got_sha size $got_size, pinned $want_sha $want_size"
    fi
done <"$W/pinned.txt"
echo "distinct pinned files whose SHA-256 and size match pin.json: $ok of $n"

echo
echo "## B. fixture: the same records and files, served from 127.0.0.1"
read -r version node commit < <(python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1]))
n = d["verified"]["node"]
print(d["version"], n["version"], n["keyring"]["commit"])' "$W/pin.json")
F=$W/fixture/base
mkdir -p "$F/npm/-/package/@google/gemini-cli" "$F/npm/@google/gemini-cli/-" "$F/npm/-/npm/v1" \
    "$F/raw/v$version/.github/workflows" "$F/dist/$node" "$F/keys/$commit/gpg-only-active-keys"
curl -fsSL -o "$F/npm/-/package/@google/gemini-cli/dist-tags" "https://registry.npmjs.org/-/package/@google%2fgemini-cli/dist-tags"
curl -fsSL -o "$F/npm/@google/gemini-cli/$version" "https://registry.npmjs.org/@google%2fgemini-cli/$version"
curl -fsSL -o "$F/npm/-/npm/v1/keys" "https://registry.npmjs.org/-/npm/v1/keys"
ln "$W/again/gemini-cli-$version.tgz" "$F/npm/@google/gemini-cli/-/gemini-cli-$version.tgz"
curl -fsSL -o "$F/raw/v$version/.github/workflows/ci.yml" "https://raw.githubusercontent.com/google-gemini/gemini-cli/v$version/.github/workflows/ci.yml"
curl -fsSL -o "$F/schedule.json" "https://raw.githubusercontent.com/nodejs/Release/main/schedule.json"
curl -fsSL -o "$F/dist/index.json" "https://nodejs.org/dist/index.json"
curl -fsSL -o "$F/dist/$node/SHASUMS256.txt.asc" "https://nodejs.org/dist/$node/SHASUMS256.txt.asc"
for arch in x64 arm64; do
    ln "$W/again/node-$node-linux-$arch.tar.gz" "$F/dist/$node/node-$node-linux-$arch.tar.gz"
done
curl -fsSL -o "$F/keys/$commit/gpg-only-active-keys/pubring.kbx" "https://raw.githubusercontent.com/nodejs/release-keys/$commit/gpg-only-active-keys/pubring.kbx"
# Another version's record, for its signatures: npm's preview, else its nightly.
other=$(python3 -I -c '
import json, sys
t = json.load(open(sys.argv[1]))
print(next(v for k, v in sorted(t.items()) if k in ("preview", "nightly") and v != sys.argv[2]))' "$F/npm/-/package/@google/gemini-cli/dist-tags" "$version")
curl -fsSL -o "$W/other-doc.json" "https://registry.npmjs.org/@google%2fgemini-cli/$other"
echo "fixture: Gemini CLI $version, Node.js $node, release-keys $commit; other version for signatures: $other"

port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
python3 -I -m http.server --bind 127.0.0.1 --directory "$W/fixture" "$port" >"$W/server.log" 2>&1 &
server=$!
echo "server: pid $server on 127.0.0.1:$port"
for _ in $(seq 1 50); do
    curl -fsS -o /dev/null "http://127.0.0.1:$port/base/schedule.json" 2>/dev/null && break
    sleep 0.2
done
local=http://127.0.0.1:$port

# A case is a copy of the fixture whose files are hard links; a changed file is unlinked and written
# anew, so no other case sees the change. Its npm record names the case's own tarball URL (the
# signatures cover the integrity, not the URL).
mkcase() {
    cp -al "$F" "$W/fixture/$1"
    python3 -I -c '
import json, os, sys
path, url = sys.argv[1], sys.argv[2]
d = json.load(open(path))
d["dist"]["tarball"] = url
os.unlink(path)
json.dump(d, open(path, "w"))' "$W/fixture/$1/npm/@google/gemini-cli/$version" "$local/$1/npm/@google/gemini-cli/-/gemini-cli-$version.tgz"
}
# Rewrite one file of a case with a python expression over its bytes (b).
change() {
    python3 -I -c '
import os, sys
path, expr = sys.argv[1], sys.argv[2]
b = open(path, "rb").read()
new = eval(expr, {"b": b, "json": __import__("json"), "re": __import__("re")})
os.unlink(path)
open(path, "wb").write(new)' "$1" "$2"
}
mkdir -p "$W/out"
passed=0 cases=0
runcase() {
    local name=$1 expect=$2 rc=0
    cases=$((cases + 1))
    PIN_NPM_REGISTRY=$local/$name/npm PIN_GEMINI_RAW=$local/$name/raw PIN_NODE_DIST=$local/$name/dist \
        PIN_NODE_SCHEDULE=$local/$name/schedule.json PIN_RELEASE_KEYS=$local/$name/keys \
        PIN_RELEASE_KEYS_COMMIT=$commit PIN_GITHUB_API=http://127.0.0.1:1 PIN_OUT=$W/out/$name.json \
        bash "$pin" "$version" >"$W/out/$name.stdout" 2>"$W/out/$name.stderr" || rc=$?
    local said
    said=$(tr '\n' ' ' <"$W/out/$name.stderr")
    if [[ $expect == ok ]]; then
        if [[ $rc == 0 && -f $W/out/$name.json ]] && python3 -I -c '
import json, sys
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
key = lambda d: [(x["platform"], x["variant"], [(f["path"], f["sha256"], f["size"], f["unpack"]) for f in x["files"]]) for x in d["builds"]]
sys.exit(0 if key(a) == key(b) and a["version"] == b["version"] and a["verified"]["node"]["signer"] == b["verified"]["node"]["signer"] else 1)' "$W/pin.json" "$W/out/$name.json"; then
            passed=$((passed + 1))
            echo "PASS $name: exit 0, pin.json written, the same builds, files and signer as A's"
        else
            echo "FAIL $name: exit $rc, wrote $(test -f "$W/out/$name.json" && echo pin.json || echo nothing): $said"
        fi
    elif [[ $rc != 0 && ! -e $W/out/$name.json ]] && grep -q -F -- "$expect" "$W/out/$name.stderr"; then
        passed=$((passed + 1))
        echo "PASS $name: exit $rc, no pin.json, said: $said"
    else
        echo "FAIL $name: exit $rc, pin.json $(test -e "$W/out/$name.json" && echo written || echo absent), wanted \"$expect\", said: $said"
    fi
}

echo
echo "## B. the control and one change at a time"
doc=npm/@google/gemini-cli/$version
mkcase control
runcase control ok

mkcase integrity-flipped
change "$W/fixture/integrity-flipped/$doc" '(lambda d: (d["dist"].__setitem__("integrity", d["dist"]["integrity"][:20] + ("A" if d["dist"]["integrity"][20] != "A" else "B") + d["dist"]["integrity"][21:]), json.dumps(d).encode())[1])(json.loads(b))'
runcase integrity-flipped "registry signature 0 by"

mkcase other-signatures
change "$W/fixture/other-signatures/$doc" "(lambda d, o: (d['dist'].__setitem__('signatures', o['dist']['signatures']), json.dumps(d).encode())[1])(json.loads(b), json.load(open('$W/other-doc.json')))"
runcase other-signatures "registry signature 0 by"

mkcase tarball-flipped
change "$W/fixture/tarball-flipped/npm/@google/gemini-cli/-/gemini-cli-$version.tgz" 'b[:len(b)//2] + bytes([b[len(b)//2] ^ 1]) + b[len(b)//2 + 1:]'
runcase tarball-flipped "is not the signed integrity"

mkcase shasums-edited
change "$W/fixture/shasums-edited/dist/$node/SHASUMS256.txt.asc" "re.sub(rb'^([0-9a-f])([0-9a-f]{63}  node-$node-linux-x64\\.tar\\.gz)$', lambda m: (b'0' if m.group(1) != b'0' else b'1') + m.group(2), b, count=1, flags=re.M)"
runcase shasums-edited "is not signed by a key of nodejs/release-keys' active keyring"

mkcase empty-keyring
change "$W/fixture/empty-keyring/keys/$commit/gpg-only-active-keys/pubring.kbx" "b''"
runcase empty-keyring "is not signed by a key of nodejs/release-keys' active keyring"

mkcase node-flipped
change "$W/fixture/node-flipped/dist/$node/node-$node-linux-arm64.tar.gz" 'b[:len(b)//2] + bytes([b[len(b)//2] ^ 1]) + b[len(b)//2 + 1:]'
runcase node-flipped "is not the signed"

mkcase no-arm64
change "$W/fixture/no-arm64/dist/index.json" "(lambda idx: (next(r for r in idx if r['version'] == '$node')['files'].remove('linux-arm64'), json.dumps(idx).encode())[1])(json.loads(b))"
runcase no-arm64 "has no linux-arm64 build"

mkcase majors-ended
change "$W/fixture/majors-ended/raw/v$version/.github/workflows/ci.yml" "b.replace(b\"- '22.x'\", b\"- '18.x'\").replace(b\"- '24.x'\", b\"- '16.x'\")"
runcase majors-ended "is still supported on"

echo
echo "cases passing: $passed of $cases (1 control, $((cases - 1)) changes)"
echo "pin.sh work directories left in $cache: $(( $(leftover) - before ))"
echo "A's pin.json sha256 $(sha256sum "$W/pin.json" | cut -d' ' -f1)"
