#!/usr/bin/env bash
# Tests .kiln/images/harness-pi/pin.sh (kiln plan 01a1ce705e62732b83f16295d74ee263, step 1) on a
# Linux runner:
#   A. pin from GitHub, then download every pinned file again and check its SHA-256 and size
#      against pin.json;
#   B. serve GitHub's real records and files from a local HTTP server, change one thing at a time,
#      and check that pin.sh refuses each change, names its check, and leaves pin.json as it was.
#
#   pin-tests.sh PIN_SH WORK_DIR
#
# Writes WORK_DIR/pin.json (A's pin) and WORK_DIR/tests.log. Each server it starts is stopped by
# the PID recorded as it started.
set -euo pipefail

pin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
W=$2
mkdir -p "$W"
cd "$W"
exec > >(tee "$W/tests.log") 2>&1
echo "# pin.sh tests on $(hostname) ($(uname -srm)), $(date -u +%FT%TZ)"
echo "# pin.sh sha256 $(sha256sum "$pin" | cut -d' ' -f1)"

servers=()
cleanup() {
    for p in "${servers[@]:-}"; do
        [[ -n $p ]] && kill "$p" 2>/dev/null || true
    done
}
trap cleanup EXIT

sha() { sha256sum "$1" | cut -d' ' -f1; }

echo
echo "## A. pin from GitHub"
PIN_OUT=$W/pin.json bash "$pin"
python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); print("pinned", d["tag"], "commit", d["commit"], "tools", [(t["name"], t["tag"]) for t in d["tools"]], "builds", [(b["platform"], len(b["files"])) for b in d["builds"]])' "$W/pin.json"

echo
echo "## A. every pinned file downloaded again and checked against pin.json"
python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); [print(f["url"], f["sha256"], f["size"]) for b in d["builds"] for f in b["files"]]' "$W/pin.json" >"$W/pinned.txt"
mkdir -p "$W/again"
ok=0 n=0
while read -r url want_sha want_size; do
    n=$((n + 1))
    f=$W/again/$n
    curl -fsSL -o "$f" "$url"
    got_sha=$(sha "$f") got_size=$(wc -c <"$f" | tr -d ' ')
    if [[ $got_sha == "$want_sha" && $got_size == "$want_size" ]]; then
        ok=$((ok + 1))
        echo "match: $url"
    else
        echo "MISMATCH: $url sha256 $got_sha size $got_size, pinned $want_sha $want_size"
    fi
done <"$W/pinned.txt"
echo "pinned files whose SHA-256 and size match pin.json: $ok of $n"

echo
echo "## B. fixture: GitHub's records and files, as pin.json names them"
read -r pi_tag rg_tag fd_tag < <(python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); t={x["name"]: x["tag"] for x in d["tools"]}; print(d["tag"], t["ripgrep"], t["fd"])' "$W/pin.json")
F=$W/fixture
mkdir -p "$F"
for spec in "earendil-works/pi $pi_tag" "BurntSushi/ripgrep $rg_tag" "sharkdp/fd $fd_tag"; do
    read -r repo tag <<<"$spec"
    mkdir -p "$F/api/repos/$repo/releases/tags" "$F/dl/$repo/releases/download/$tag"
    curl -fsSL -o "$F/api/repos/$repo/releases/tags/$tag" "https://api.github.com/repos/$repo/releases/tags/$tag"
    cp "$F/api/repos/$repo/releases/tags/$tag" "$F/api/repos/$repo/releases/latest"
done
mkdir -p "$F/api/repos/earendil-works/pi/git/ref/tags"
curl -fsSL -o "$F/api/repos/earendil-works/pi/git/ref/tags/$pi_tag" "https://api.github.com/repos/earendil-works/pi/git/ref/tags/$pi_tag"
for name in SHA256SUMS pi-linux-x64.tar.gz pi-linux-arm64.tar.gz; do
    curl -fsSL -o "$F/dl/earendil-works/pi/releases/download/$pi_tag/$name" "https://github.com/earendil-works/pi/releases/download/$pi_tag/$name"
done
for arch in x86_64 aarch64; do
    for name in "ripgrep-$rg_tag-$arch-unknown-linux-musl.tar.gz" "ripgrep-$rg_tag-$arch-unknown-linux-musl.tar.gz.sha256"; do
        curl -fsSL -o "$F/dl/BurntSushi/ripgrep/releases/download/$rg_tag/$name" "https://github.com/BurntSushi/ripgrep/releases/download/$rg_tag/$name"
    done
    name=fd-v${fd_tag#v}-$arch-unknown-linux-musl.tar.gz
    curl -fsSL -o "$F/dl/sharkdp/fd/releases/download/$fd_tag/$name" "https://github.com/sharkdp/fd/releases/download/$fd_tag/$name"
done
echo "fixture: $(find "$F" -type f | wc -l) files, $(du -sh "$F" | cut -f1)"

PI_REL=api/repos/earendil-works/pi/releases/tags/$pi_tag
RG_REL=api/repos/BurntSushi/ripgrep/releases/tags/$rg_tag
PI_DL=dl/earendil-works/pi/releases/download/$pi_tag
RG_DL=dl/BurntSushi/ripgrep/releases/download/$rg_tag

# Edits a run's copy. Each edit writes a new file and renames it into place, so the fixture the
# copy shares its inodes with (cp -al) stays as it was.
edit() { # RUN OP ARGS...
    python3 -I - "$@" <<'EOF'
import hashlib, io, json, os, sys, tarfile

run, op, *a = sys.argv[1:]

def put(rel, data):
    p = os.path.join(run, rel)
    tmp = p + ".new"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, p)

def get(rel):
    with open(os.path.join(run, rel), "rb") as f:
        return f.read()

def set_asset(rel_json, name, **kv):
    d = json.loads(get(rel_json))
    for asset in d["assets"]:
        if asset["name"] == name:
            asset.update(kv)
    put(rel_json, json.dumps(d).encode())

def restate(rel_json, rel_file):
    # GitHub's record of a file made to match the file as it now is.
    data = get(rel_file)
    set_asset(rel_json, os.path.basename(rel_file), size=len(data), digest="sha256:" + hashlib.sha256(data).hexdigest())

def resum(rel_sums, name, digest):
    # The checksum file's line for NAME ("<hex>  NAME") given another digest.
    out = []
    for line in get(rel_sums).decode().splitlines():
        if line.endswith(" " + name):
            line = digest + " " + line.split(" ", 1)[1]
        out.append(line)
    put(rel_sums, ("\n".join(out) + "\n").encode())

def retar(rel, keep, change=None):
    src = tarfile.open(fileobj=io.BytesIO(get(rel)), mode="r:gz")
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as dst:
        for m in src.getmembers():
            if not keep(m.name):
                continue
            data = src.extractfile(m).read() if m.isfile() else None
            if data is not None and change:
                data = change(m.name, data)
                m.size = len(data)
            dst.addfile(m, io.BytesIO(data) if data is not None else None)
    put(rel, buf.getvalue())

if op == "flip":  # FILE: one byte in the middle flipped
    data = bytearray(get(a[0]))
    data[len(data) // 2] ^= 0xFF
    put(a[0], bytes(data))
elif op == "json":  # FILE KEY VALUE: a top-level field of a release set
    d = json.loads(get(a[0]))
    d[a[1]] = json.loads(a[2])
    put(a[0], json.dumps(d).encode())
elif op == "asset":  # FILE NAME KEY VALUE: one asset's field set
    set_asset(a[0], a[1], **{a[2]: json.loads(a[3])})
elif op == "drop":  # FILE NAME: an asset taken out of the release
    d = json.loads(get(a[0]))
    d["assets"] = [x for x in d["assets"] if x["name"] != a[1]]
    put(a[0], json.dumps(d).encode())
elif op == "sumline":  # SUMS NAME DIGEST JSON: the checksum file's line for NAME changed, and GitHub's record of that file with it
    resum(a[0], a[1], a[2])
    restate(a[3], a[0])
elif op == "pkgversion":  # TARBALL VERSION SUMS JSON: pi/package.json's version changed, its checksums with it
    def change(name, data):
        if name != "pi/package.json":
            return data
        d = json.loads(data)
        d["version"] = a[1]
        return json.dumps(d).encode()
    retar(a[0], lambda n: True, change)
    restate(a[3], a[0])
    resum(a[2], os.path.basename(a[0]), hashlib.sha256(get(a[0])).hexdigest())
    restate(a[3], a[2])
elif op == "dropmember":  # TARBALL MEMBER SHAFILE JSON: a member taken out, its checksums with it
    retar(a[0], lambda n: n != a[1])
    restate(a[3], a[0])
    resum(a[2], os.path.basename(a[0]), hashlib.sha256(get(a[0])).hexdigest())
    restate(a[3], a[2])
elif op == "attest":  # SHA: GitHub's attestations API answers for this digest
    p = os.path.join(run, "api/repos/earendil-works/pi/attestations")
    os.makedirs(p, exist_ok=True)
    with open(os.path.join(p, "sha256:" + a[0]), "w") as f:
        f.write('{"attestations": [{}]}')
else:
    sys.exit("unknown op " + op)
EOF
}

serve() { # DIR: starts a server on a free port; sets port, adds its PID to servers
    local log=$1.server.log
    python3 -I -u -m http.server --bind 127.0.0.1 --directory "$1" 0 >"$log" 2>&1 &
    servers+=($!)
    srv=$!
    port=
    for _ in $(seq 1 100); do
        port=$(sed -n 's/.* port \([0-9]*\).*/\1/p' "$log" | head -1)
        [[ -n $port ]] && return 0
        sleep 0.1
    done
    echo "server for $1 did not start"
    return 1
}

refused=0 cases=0 passed=0 controls=0
case_() { # NAME EXPECT_RE ARGS...: run pin.sh against a run that has had its edit applied
    local name=$1 expect=$2
    shift 2
    local run=$W/runs/$name out=$W/runs/$name.pin.json rc=0
    serve "$run"
    cp "$W/pin.json" "$out"
    local before
    before=$(sha "$out")
    PIN_GITHUB_API=http://127.0.0.1:$port/api PIN_GITHUB_DOWNLOAD=http://127.0.0.1:$port/dl PIN_OUT=$out \
        bash "$pin" "$@" >"$run.stdout" 2>"$run.stderr" || rc=$?
    kill "$srv" 2>/dev/null || true
    local said
    said=$(tr '\n' ' ' <"$run.stderr")
    cases=$((cases + 1))
    if [[ $rc -ne 0 ]] && grep -Eq "$expect" "$run.stderr" && [[ $(sha "$out") == "$before" ]]; then
        refused=$((refused + 1))
        echo "refused: $name (exit $rc; pin.json unchanged): $said"
    else
        echo "NOT REFUSED AS EXPECTED: $name (exit $rc; pin.json $( [[ $(sha "$out") == "$before" ]] && echo unchanged || echo CHANGED)): $said"
    fi
}
mkrun() { # NAME: a run's copy of the fixture, sharing its inodes
    mkdir -p "$W/runs"
    cp -al "$F" "$W/runs/$1"
}

echo
echo "## B. mutations, one at a time"
x64_sha=$(sha "$F/$PI_DL/pi-linux-x64.tar.gz")
rg_x64=ripgrep-$rg_tag-x86_64-unknown-linux-musl
rg_a64=ripgrep-$rg_tag-aarch64-unknown-linux-musl

mkrun flip; edit "$W/runs/flip" flip "$PI_DL/pi-linux-x64.tar.gz"
case_ flip "pi-linux-x64.tar.gz has SHA-256 [0-9a-f]+; GitHub's digest is" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun sumline; edit "$W/runs/sumline" sumline "$PI_DL/SHA256SUMS" pi-linux-arm64.tar.gz "$x64_sha" "$PI_REL"
case_ sumline "pi-linux-arm64.tar.gz has SHA-256 [0-9a-f]+; SHA256SUMS lists $x64_sha" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun size; edit "$W/runs/size" asset "$PI_REL" pi-linux-x64.tar.gz size "$(($(wc -c <"$F/$PI_DL/pi-linux-x64.tar.gz") + 1))"
case_ size "pi-linux-x64.tar.gz is [0-9]+ bytes; GitHub records [0-9]+" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun digest; edit "$W/runs/digest" asset "$PI_REL" pi-linux-arm64.tar.gz digest "\"sha256:$x64_sha\""
case_ digest "pi-linux-arm64.tar.gz has SHA-256 [0-9a-f]+; GitHub's digest is $x64_sha" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun pkgversion; edit "$W/runs/pkgversion" pkgversion "$PI_DL/pi-linux-x64.tar.gz" 0.0.1 "$PI_DL/SHA256SUMS" "$PI_REL"
case_ pkgversion "pi-linux-x64.tar.gz's pi/package.json says 0.0.1" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun prerelease; edit "$W/runs/prerelease" json "$PI_REL" prerelease true
case_ prerelease "$pi_tag: a prerelease" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun draft; edit "$W/runs/draft" json "$PI_REL" draft true
case_ draft "$pi_tag: a draft, not a published release" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun noarm64; edit "$W/runs/noarm64" drop "$PI_REL" pi-linux-arm64.tar.gz
case_ noarm64 "0 assets named pi-linux-arm64.tar.gz, not one" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun norg; edit "$W/runs/norg" dropmember "$RG_DL/$rg_x64.tar.gz" "$rg_x64/rg" "$RG_DL/$rg_x64.tar.gz.sha256" "$RG_REL"
case_ norg "holds no $rg_x64/rg" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun rgsha; edit "$W/runs/rgsha" sumline "$RG_DL/$rg_a64.tar.gz.sha256" "$rg_a64.tar.gz" "$x64_sha" "$RG_REL"
case_ rgsha "$rg_a64.tar.gz has SHA-256 [0-9a-f]+; its .sha256 file says $x64_sha" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun attested; edit "$W/runs/attested" attest "$x64_sha"
case_ attested "GitHub holds an attestation for pi-linux-x64.tar.gz" "$pi_tag" "$rg_tag" "$fd_tag"

mkrun immutable; edit "$W/runs/immutable" json "$PI_REL" immutable true
case_ immutable "is an immutable release" "$pi_tag" "$rg_tag" "$fd_tag"

echo
echo "## B. a refused run against a pin.json that isn't there yet leaves none"
mkrun unwritten; edit "$W/runs/unwritten" flip "$PI_DL/pi-linux-arm64.tar.gz"
serve "$W/runs/unwritten"
rc=0
PIN_GITHUB_API=http://127.0.0.1:$port/api PIN_GITHUB_DOWNLOAD=http://127.0.0.1:$port/dl PIN_OUT=$W/runs/unwritten.pin.json \
    bash "$pin" "$pi_tag" "$rg_tag" "$fd_tag" 2>"$W/runs/unwritten.stderr" >/dev/null || rc=$?
kill "$srv" 2>/dev/null || true
cases=$((cases + 1))
if [[ $rc -ne 0 && ! -e $W/runs/unwritten.pin.json ]] && ! ls "$W/runs/" | grep -q 'unwritten.pin.json.tmp'; then
    refused=$((refused + 1))
    echo "refused: unwritten (exit $rc; no pin.json and no temporary file): $(tr '\n' ' ' <"$W/runs/unwritten.stderr")"
else
    echo "NOT REFUSED AS EXPECTED: unwritten (exit $rc)"
fi

echo
echo "## B. controls: the unchanged fixture, by tag and by latest"
same() { # OUT: equal to A's pin.json but for read_at and the hosts in its URLs
    python3 -I - "$W/pin.json" "$1" "$port" <<'EOF'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
port = sys.argv[3]
for d in (a, b):
    d.pop("read_at")
    for build in d["builds"]:
        for f in build["files"]:
            f["url"] = f["url"].replace("https://github.com/", "/").replace(f"http://127.0.0.1:{port}/dl/", "/")
sys.exit(0 if a == b else 1)
EOF
}
for how in tags latest; do
    mkrun "control-$how"
    serve "$W/runs/control-$how"
    out=$W/runs/control-$how.pin.json rc=0
    args=("$pi_tag" "$rg_tag" "$fd_tag")
    [[ $how == latest ]] && args=()
    PIN_GITHUB_API=http://127.0.0.1:$port/api PIN_GITHUB_DOWNLOAD=http://127.0.0.1:$port/dl PIN_OUT=$out \
        bash "$pin" "${args[@]}" >"$W/runs/control-$how.stdout" 2>"$W/runs/control-$how.stderr" || rc=$?
    controls=$((controls + 1))
    if [[ $rc -eq 0 ]] && same "$out"; then
        passed=$((passed + 1))
        echo "control $how: exit 0, pin.json equal to A's but for read_at and the URLs' host: $(cat "$W/runs/control-$how.stdout")"
    else
        echo "CONTROL $how FAILED: exit $rc: $(tr '\n' ' ' <"$W/runs/control-$how.stderr")"
    fi
    kill "$srv" 2>/dev/null || true
done

echo
echo "## summary"
echo "pinned files whose SHA-256 and size match pin.json: $ok of $n"
echo "mutations refused with their check named and pin.json left as it was: $refused of $cases"
echo "controls passed: $passed of $controls"
[[ $ok -eq $n && $n -eq 6 && $refused -eq $cases && $passed -eq $controls ]]
