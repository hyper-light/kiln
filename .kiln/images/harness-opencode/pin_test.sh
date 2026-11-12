#!/usr/bin/env bash
# pin.sh's tests: each check accepts the real archives and refuses what it must, and a run that
# fails writes no pin.
#
#   pin_test.sh [DIR]
#
# DIR holds the four archives a `pin.sh --keep` run downloaded (else they are downloaded here).
# Needs what pin.sh needs: curl, python3 and gh signed in.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=pin.sh
. "$here/pin.sh"

VERSION=2.2.7
mkdir -p "$HOME/.cache/kiln-pins"
WORK=$(mktemp -d "$HOME/.cache/kiln-pins/opencode-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
T=$WORK/t
mkdir -p "$T"
passed=0 failed=0

ok() { passed=$((passed + 1)); printf 'PASS %s\n' "$1"; }
bad() { failed=$((failed + 1)); printf 'FAIL %s: %s\n' "$1" "$2"; }

# refuses NAME CHECK CMD...: CMD exits non-zero and names the failed check.
refuses() {
    local name=$1 check=$2 rc=0
    shift 2
    ("$@") >"$T/out" 2>"$T/err" || rc=$?
    if [ "$rc" -eq 0 ]; then
        bad "$name" "it passed"
    elif ! grep -q "FAILED $check:" "$T/err"; then
        bad "$name" "exit $rc without FAILED $check: $(head -c 300 "$T/err")"
    else
        ok "$name ($(head -n 1 "$T/err" | cut -c1-160))"
    fi
}

# writes_no_pin NAME CHECK ARGS...: a whole run with ARGS fails at CHECK and leaves no pin.
writes_no_pin() {
    local name=$1 check=$2
    shift 2
    rm -f "$T/pin.json"
    refuses "$name" "$check" "$here/pin.sh" "$@" --out "$T/pin.json"
    if [ -e "$T/pin.json" ]; then bad "$name" "a pin was written"; fi
}

releases=https://github.com/$REPO/releases/download/v$VERSION
gh api "repos/$REPO/releases/tags/v$VERSION" >"$WORK/release.json"
fetch "$releases/SHA256SUMS" "$WORK/SHA256SUMS"
fetch "$ENDPOINT" "$WORK/endpoint.json"
read -r e_version e_commit e_run e_ref <<<"$(endpoint_fields "$WORK/endpoint.json")"
[ "$e_version" = "$VERSION" ] || say "note: the endpoint now names $e_version; checks below hold $VERSION's commit and run from its archives"

DIR=${1:-}
if [ -z "$DIR" ]; then
    DIR=$WORK/archives
    mkdir -p "$DIR"
    while read -r _ _ asset; do fetch "$releases/$asset" "$DIR/$asset"; done <<<"$BUILDS"
fi
real=$DIR/opencode-linux-x64-baseline-musl.tar.gz
[ -f "$real" ] || { echo "no $real" >&2; exit 2; }

# The checks accept the real archives.
for asset in $(printf '%s\n' "$BUILDS" | cut -d' ' -f3); do
    if (check_sums "$WORK/SHA256SUMS" "$asset" "$DIR/$asset" &&
        check_asset "$WORK/release.json" "$asset" "$DIR/$asset" &&
        check_attestation "$DIR/$asset" "$REPO" "$SIGNER" "" "" >/dev/null &&
        check_member "$DIR/$asset" >/dev/null) 2>"$T/err"; then
        ok "accepts $asset"
    else
        bad "accepts $asset" "$(cat "$T/err")"
    fi
done

# Versions: 1.x and anything not X.Y.Z.
writes_no_pin "refuses OpenCode 1.19.5" version 1.19.5
writes_no_pin "refuses 2.2" version 2.2

# A missing asset.
refuses "a missing asset fails to download" download fetch "$releases/opencode-linux-riscv64.tar.gz" "$T/missing"
refuses "GitHub lists no such asset" github-digest check_asset "$WORK/release.json" opencode-linux-riscv64.tar.gz "$real"
mkdir -p "$T/three"
for asset in $(printf '%s\n' "$BUILDS" | cut -d' ' -f3 | head -n 3); do ln -f "$DIR/$asset" "$T/three/$asset"; done
writes_no_pin "a run missing an archive" download "$VERSION" --from "$T/three"

# One byte changed.
mkdir -p "$T/flipped"
for asset in $(printf '%s\n' "$BUILDS" | cut -d' ' -f3); do ln -f "$DIR/$asset" "$T/flipped/$asset"; done
rm -f "$T/flipped/opencode-linux-x64-baseline-musl.tar.gz"
python3 -I -c 'import sys
d = bytearray(open(sys.argv[1], "rb").read())
d[len(d) // 2] ^= 0x01
open(sys.argv[2], "wb").write(d)' "$real" "$T/flipped/opencode-linux-x64-baseline-musl.tar.gz"
refuses "one byte flipped: SHA256SUMS" sha256sums check_sums "$WORK/SHA256SUMS" opencode-linux-x64-baseline-musl.tar.gz "$T/flipped/opencode-linux-x64-baseline-musl.tar.gz"
refuses "one byte flipped: GitHub's digest" github-digest check_asset "$WORK/release.json" opencode-linux-x64-baseline-musl.tar.gz "$T/flipped/opencode-linux-x64-baseline-musl.tar.gz"
writes_no_pin "a run with one byte flipped" sha256sums "$VERSION" --from "$T/flipped"

# Provenance from another signer workflow, another repository, or another commit or run.
refuses "another signer workflow" attestation check_attestation "$real" "$REPO" anomalyco/opencode/.github/workflows/check.yml "" ""
refuses "another repository" attestation check_attestation "$real" cli/cli cli/cli/.github/workflows/deployment.yml "" ""
refuses "another commit" attestation check_attestation "$real" "$REPO" "$SIGNER" 0000000000000000000000000000000000000000 ""
refuses "another run" attestation check_attestation "$real" "$REPO" "$SIGNER" "" 1

# Archives that are not one regular file named opencode.
python3 -I -c 'import io, sys, tarfile
def make(path, entries):
    with tarfile.open(path, "w:gz") as t:
        for name, kind in entries:
            i = tarfile.TarInfo(name)
            if kind == "file":
                data = b"#!/bin/sh\necho opencode\n"
                i.size, i.mode = len(data), 0o755
                t.addfile(i, io.BytesIO(data))
            elif kind == "link":
                i.type, i.linkname = tarfile.SYMTYPE, "/bin/sh"
                t.addfile(i)
            elif kind == "dir":
                i.type, i.mode = tarfile.DIRTYPE, 0o755
                t.addfile(i)
d = sys.argv[1]
make(d + "/one.tar.gz", [("opencode", "file")])
make(d + "/two.tar.gz", [("opencode", "file"), ("evil", "file")])
make(d + "/named.tar.gz", [("opencode-cli", "file")])
make(d + "/dotted.tar.gz", [("./opencode", "file")])
make(d + "/up.tar.gz", [("../opencode", "file")])
make(d + "/link.tar.gz", [("opencode", "link")])
make(d + "/dir.tar.gz", [("opencode", "dir")])
open(d + "/plain.tar.gz", "wb").write(b"not a tarball\n")' "$T"
python3 -I -c 'import sys; open(sys.argv[2], "wb").write(open(sys.argv[1], "rb").read(1 << 20))' "$real" "$T/truncated.tar.gz"
if out=$(check_member "$T/one.tar.gz" 2>"$T/err"); then ok "accepts one regular file named opencode ($out)"; else bad "accepts one opencode" "$(cat "$T/err")"; fi
refuses "a second entry" member check_member "$T/two.tar.gz"
refuses "no entry named opencode" member check_member "$T/named.tar.gz"
refuses "./opencode" member check_member "$T/dotted.tar.gz"
refuses "../opencode" member check_member "$T/up.tar.gz"
refuses "opencode a symlink" member check_member "$T/link.tar.gz"
refuses "opencode a directory" member check_member "$T/dir.tar.gz"
refuses "not a tarball" member check_member "$T/plain.tar.gz"
refuses "a truncated archive" member check_member "$T/truncated.tar.gz"

printf '%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
