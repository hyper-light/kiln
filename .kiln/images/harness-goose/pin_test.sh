#!/usr/bin/env bash
# pin.sh's checks, each shown to pass Goose's real archive and to refuse what it must (plan
# 01a1ce705e3d72af83ef15ae83b62f45, step 1's tests): an attestation from another tag, a byte
# changed, an archive with a second member or a symlink for goose, and a release missing an
# architecture, the last two through main, which must then leave no pin. Needs what pin.sh needs
# (gh, jq, curl, tar, network); downloads one archive, under ~/.cache.
#
#   .kiln/images/harness-goose/pin_test.sh

set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=pin.sh
source "$here/pin.sh"
set +e

failed=0
pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failed=1; }

# Runs a command in a subshell with pin.sh's -e; passes when it fails and its stderr holds `want`.
refuses() {
    local name=$1 want=$2 err rc
    shift 2
    err=$( ( set -e; "$@" ) 2>&1 >/dev/null )
    rc=$?
    if ((rc != 0)) && [[ $err == *"$want"* ]]; then pass "$name: ${err##*$'\n'}"; else fail "$name: exit $rc, stderr: $err"; fi
}

cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
t=$(mktemp -d "$cache/kiln-pin-goose-test.XXXXXX")
trap 'rm -rf "$t"' EXIT

tag=$(resolve_tag stable) || { fail "resolve stable"; exit 1; }
release=$(release_json "$tag")
IFS=$'\t' read -r url size sha <<<"$(asset_of "$release" x86_64)"
real="$t/$(basename "$url")"
fetch "$url" "$real" || { fail "download $url"; exit 1; }

# Goose's real archive passes every check.
if (set -e; check_archive "$real" "$size" "$sha"); then pass "real archive: size and SHA-256 as GitHub records"; else fail "real archive's size or SHA-256"; fi
commit=$( (set -e; verify_attestation "$real" "$tag" "$sha") 2>/dev/null )
if [[ $commit =~ ^[0-9a-f]{40}$ ]]; then pass "real archive: attested at refs/tags/$tag, commit $commit"; else fail "real archive's attestation"; fi
member=$( (set -e; member_of "$real") 2>/dev/null )
if [[ $member == ./goose || $member == goose ]]; then pass "real archive: one regular member, $member"; else fail "real archive's member: $member"; fi

# An attestation for another release's tag.
refuses "attestation for another tag" "attestation doesn't verify" verify_attestation "$real" v0.0.0 "$sha"

# One byte changed.
flip() {
    local b
    b=$(od -An -tu1 -j "$2" -N 1 "$1" | tr -d ' ')
    printf "$(printf '\\%03o' $((b ^ 255)))" | dd of="$1" bs=1 seek="$2" count=1 conv=notrunc 2>/dev/null
}
cp "$real" "$t/flipped.tar.gz"
flip "$t/flipped.tar.gz" "$((size / 2))"
refuses "a byte changed" "SHA-256" check_archive "$t/flipped.tar.gz" "$size" "$sha"

# A second regular member, and a symlink named goose.
mkdir -p "$t/two" "$t/link"
printf 'x' >"$t/two/goose"
printf 'y' >"$t/two/extra"
tar -czf "$t/two.tar.gz" -C "$t/two" ./goose ./extra
refuses "a second member" "expected one regular file" member_of "$t/two.tar.gz"
ln -s /etc/passwd "$t/link/goose"
tar -czf "$t/link.tar.gz" -C "$t/link" ./goose
refuses "a symlink named goose" "is no regular file" member_of "$t/link.tar.gz"

# Through main: a release missing aarch64's archive, then a download with a byte changed. Neither
# leaves a pin.
missing() {
    release_json() {
        if [[ $1 == stable ]]; then gh api "repos/$REPO/releases/tags/stable"; else gh api "repos/$REPO/releases/tags/$1" | jq 'del(.assets[] | select(.name | test("aarch64-unknown-linux-musl")))'; fi
    }
    main stable --out "$t/missing.json"
}
refuses "a release missing aarch64" "has no single goose-aarch64-unknown-linux-musl.tar.gz" missing
[[ ! -e $t/missing.json ]] && pass "a release missing aarch64: no pin written" || fail "a release missing aarch64 left $t/missing.json"
corrupt() {
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" && flip "$2" 1000; }
    main stable --out "$t/corrupt.json"
}
refuses "a download with a byte changed" "SHA-256" corrupt
[[ ! -e $t/corrupt.json ]] && pass "a download with a byte changed: no pin written" || fail "a download with a byte changed left $t/corrupt.json"

if ((failed)); then echo "pin_test: FAILED"; exit 1; fi
echo "pin_test: all passed"
