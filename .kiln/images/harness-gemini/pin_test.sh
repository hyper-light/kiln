#!/usr/bin/env bash
# pin.sh's checks (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 21; plan
# 01a1ce705e2374dfbac201fe6ba98184, step 1's tests), on a Linux runner with GNU tar:
#   A. pin from npm, GitHub and nodejs.org; then download every pinned file again and check its
#      SHA-256 and size against pin.json, each Node member's own SHA-256 and size as GNU tar extracts
#      it, and the npm tree's totals as GNU tar lists it;
#   B. serve those same records and files from a local HTTP server, change one thing at a time, and
#      check that pin.sh refuses each change, names its check and writes no pin.json; an unchanged
#      copy (the control) pins the same files as A;
#   C. the same server with npm's record signed by a key of the fixture's own, which its registry
#      lists, so the tarball can change and still verify: a control that pins, then one change to
#      the tarball's entries at a time that the unpacker would refuse, each refused at pin time.
#
#   .kiln/images/harness-gemini/pin_test.sh [WORK_DIR]
#
# Writes WORK_DIR (default: a new folder under ~/.cache) /pin.json (A's pin) and /tests.log. The one
# server it starts is stopped by the PID recorded as it started. Needs what pin.sh needs, and GNU tar.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
pin=$here/pin.sh
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
W=${1:-$(mktemp -d "$cache/pin-test-harness-gemini.XXXXXX")}
mkdir -p "$W"
cd "$W"
exec > >(tee "$W/tests.log") 2>&1
echo "# pin.sh tests on $(hostname) ($(uname -srm)), $(date -u +%FT%TZ)"
echo "# pin.sh sha256 $(sha256sum "$pin" | cut -d' ' -f1); curl $(curl --version | head -1 | cut -d' ' -f2), gpgv $(gpgv --version | head -1 | awk '{print $NF}'), $(openssl version | cut -d' ' -f1-2), $(tar --version | head -1), python $(python3 -c 'import platform; print(platform.python_version())')"

server=
cleanup() {
    [[ -n $server ]] && kill "$server" 2>/dev/null || true
}
trap cleanup EXIT
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
print("smoke", d["smoke"])
for b in d["builds"]:
    print("build", b["platform"], [(f["path"], f.get("mode"), f["unpack"]) for f in b["files"]])' "$W/pin.json"

echo
echo "## A. every pinned file downloaded again, and what GNU tar finds in it, against pin.json"
python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1]))
for b in d["builds"]:
    for f in b["files"]:
        u = f["unpack"]
        print(f["url"], f["sha256"], f["size"], u.get("member", "-"), u.get("sha256", "-"), u.get("size", "-"), u.get("entries", "-"), u.get("unpacked_bytes", "-"))' "$W/pin.json" >"$W/pinned.txt"
mkdir -p "$W/again"
ok=0 n=0 checks=0 checked=0
declare -A fetched=()
while read -r url want_sha want_size member member_sha member_size entries bytes; do
    f=$W/again/$(basename "$url")
    if [[ -z ${fetched[$url]:-} ]]; then
        fetched[$url]=1
        n=$((n + 1))
        curl -fsSL -o "$f" "$url"
        got_sha=$(sha256sum "$f" | cut -d' ' -f1) got_size=$(wc -c <"$f" | tr -d ' ')
        if [[ $got_sha == "$want_sha" && $got_size == "$want_size" ]]; then
            ok=$((ok + 1))
            echo "match: $url sha256 $got_sha size $got_size"
        else
            echo "MISMATCH: $url sha256 $got_sha size $got_size, pinned $want_sha $want_size"
        fi
    fi
    checks=$((checks + 1))
    if [[ $member != - ]]; then
        got=$(tar -xzOf "$f" "$member" | sha256sum | cut -d' ' -f1) size=$(tar -xzOf "$f" "$member" | wc -c | tr -d ' ')
        if [[ $got == "$member_sha" && $size == "$member_size" ]]; then
            checked=$((checked + 1))
            echo "member: $member sha256 $got size $size, as pinned"
        else
            echo "MEMBER MISMATCH: $member sha256 $got size $size, pinned $member_sha $member_size"
        fi
    else
        got_entries=$(tar -tzf "$f" | wc -l | tr -d ' ')
        got_bytes=$(tar -tzvf "$f" | awk '$1 ~ /^-/ { s += $3 } END { print s + 0 }')
        if [[ $got_entries == "$entries" && $got_bytes == "$bytes" ]]; then
            checked=$((checked + 1))
            echo "tree: $(basename "$url") $got_entries entries, $got_bytes bytes in its files, as pinned"
        else
            echo "TREE MISMATCH: $(basename "$url") $got_entries entries, $got_bytes bytes, pinned $entries $bytes"
        fi
    fi
done <"$W/pinned.txt"
echo "distinct pinned files whose SHA-256 and size match pin.json: $ok of $n"
echo "pinned members and trees GNU tar finds as pinned: $checked of $checks"

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
    if [[ $expect == ok* ]]; then
        # ok: the same builds as A's; ok-resigned: the same but for the rebuilt tarball's own digest
        # and size (its entries and totals the same).
        if [[ $rc == 0 && -f $W/out/$name.json ]] && python3 -I -c '
import json, sys
a, b, how = json.load(open(sys.argv[1])), json.load(open(sys.argv[2])), sys.argv[3]
key = lambda d: [(x["platform"], [(f["path"], f.get("mode"), f["unpack"]) + ((f["sha256"], f["size"]) if how == "ok" or f["path"] != "." else ()) for f in x["files"]]) for x in d["builds"]]
same = key(a) == key(b) and a["version"] == b["version"] and a["smoke"] == b["smoke"] and a["verified"]["node"]["signer"] == b["verified"]["node"]["signer"]
sys.exit(0 if same else 1)' "$W/pin.json" "$W/out/$name.json" "$expect"; then
            passed=$((passed + 1))
            echo "PASS $name: exit 0, pin.json written, the same builds, files, smoke and signer as A's"
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
echo "## C. npm's record signed by the fixture's own key, the tarball changed one entry at a time"
openssl ecparam -name prime256v1 -genkey -noout -out "$W/fixture-key.pem" 2>/dev/null
pub=$(openssl ec -in "$W/fixture-key.pem" -pubout -outform DER 2>/dev/null | base64 -w0)
# resign CASE KIND: the case's tarball rebuilt from the real one with one change (KIND), npm's record
# given its integrity and one signature by the fixture key, and the case's registry listing that key
# alone. KIND none rebuilds the tarball unchanged.
resign() {
    local name=$1 kind=$2 tgz=$W/fixture/$1/npm/@google/gemini-cli/-/gemini-cli-$version.tgz
    mkcase "$name"
    rm "$tgz"
    python3 -I - "$W/again/gemini-cli-$version.tgz" "$tgz" "$kind" <<'PY'
import io, sys, tarfile
src, out, kind = sys.argv[1:4]
def entry(name, data=b"x\n", mode=0o644, type=tarfile.REGTYPE, link=""):
    m = tarfile.TarInfo(name)
    m.size, m.mode, m.type, m.linkname = (len(data) if type in (tarfile.REGTYPE, tarfile.CONTTYPE) else 0), mode, type, link
    return m, (io.BytesIO(data) if m.size else None)
extra = {
    "none": [],
    "symlink": [entry("package/bundle/link.js", type=tarfile.SYMTYPE, link="gemini.js")],
    "outside": [entry("other/x.js")],
    "twice": [entry("package/README.md")],
    "folds": [entry("package/readme.md")],
    "folds-dir": [entry("package/Bundle/x.js")],
    "harness-json": [entry("package/harness.json")],
    "settings": [entry("package/System-Settings.json")],
    "node-dir": [entry("package/node/bin/node", mode=0o755)],
    "sticky": [entry("package/bundle/s.js", mode=0o1644)],
    "set-id": [entry("package/bundle/s.js", mode=0o4755)],
    "contiguous": [entry("package/bundle/c.js", type=tarfile.CONTTYPE)],
    "nameless": [entry("package")],
}[kind]
with tarfile.open(src, "r:gz") as t, tarfile.open(out, "w:gz", compresslevel=1, format=tarfile.GNU_FORMAT) as o:
    for m in t.getmembers():
        o.addfile(m, t.extractfile(m))
    for m, data in extra:
        o.addfile(m, data)
PY
    python3 -I - "$W/fixture/$name/$doc" "$W/fixture/$name/npm/-/npm/v1/keys" "$tgz" "$W/fixture-key.pem" "$pub" <<'PY'
import base64, hashlib, json, os, subprocess, sys
doc, keys, tgz, key, pub = sys.argv[1:6]
d = json.load(open(doc))
integrity = "sha512-" + base64.b64encode(hashlib.sha512(open(tgz, "rb").read()).digest()).decode()
message = f"{d['name']}@{d['version']}:{integrity}".encode()
sig = subprocess.run(["openssl", "dgst", "-sha256", "-sign", key], input=message, capture_output=True, check=True).stdout
d["dist"]["integrity"] = integrity
d["dist"]["signatures"] = [{"keyid": "SHA256:fixture", "sig": base64.b64encode(sig).decode()}]
os.unlink(doc)
json.dump(d, open(doc, "w"))
os.unlink(keys)
json.dump({"keys": [{"expires": None, "keyid": "SHA256:fixture", "keytype": "ecdsa-sha2-nistp256", "scheme": "ecdsa-sha2-nistp256", "key": pub}]}, open(keys, "w"))
PY
}
resign resigned-control none
runcase resigned-control ok-resigned
for c in "symlink:is a link" "outside:lies outside package/" "twice:is there twice" "folds:one path where names fold" \
    "folds-dir:one path where names fold" "harness-json:lands where kiln writes" "settings:lands where kiln writes" \
    "node-dir:lands where kiln writes" "sticky:has the sticky bit" "set-id:has a set-ID bit" \
    "contiguous:neither a regular file nor a directory" "nameless:a file the strip leaves no name"; do
    resign "${c%%:*}" "${c%%:*}"
    runcase "${c%%:*}" "${c#*:}"
done

echo
echo "cases passing: $passed of $cases (2 controls, $((cases - 2)) changes)"
echo "pin.sh work directories left in $cache: $(( $(leftover) - before ))"
echo "A's pin.json sha256 $(sha256sum "$W/pin.json" | cut -d' ' -f1)"
