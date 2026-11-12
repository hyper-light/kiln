#!/usr/bin/env bash
# pin.sh's checks, each shown to pass Anthropic's real release and to refuse what it must (plan
# 01a1cf48e8157494ac9a51f7a564f526, step 1's tests):
# - a byte of manifest.json changed;
# - the signature of another release;
# - the key replaced by another key, and a manifest that other key signed;
# - a manifest of another version, and one missing a platform;
# - through main, a manifest changed in transit, and a signed manifest missing a platform, neither
#   of which may leave a pin.
# Needs what pin.sh needs (gpg, jq, curl, network). It fetches only manifests, signatures and the
# key, under ~/.cache.
#
#   .kiln/images/harness-claude/pin_test.sh

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
t=$(mktemp -d "$cache/kiln-pin-claude-test.XXXXXX")
# pin.sh's own work folder, which its functions use.
work="$t/work"
mkdir -p "$work"
trap 'for h in "$t"/gnupg-*; do gpgconf --homedir "$h" --kill all >/dev/null 2>&1; done; rm -rf "$t"' EXIT

stable=$( (set -e; resolve_version stable) ) || { fail "resolve stable"; exit 1; }
latest=$( (set -e; resolve_version latest) ) || { fail "resolve latest"; exit 1; }
pass "channels: stable $stable, latest $latest"
get() { fetch "$RELEASES/$1/$2" "$t/$1-$2" || { fail "download $RELEASES/$1/$2"; exit 1; }; }
get "$stable" manifest.json
get "$stable" manifest.json.sig
fetch "$KEY_URL" "$t/key.asc" || { fail "download $KEY_URL"; exit 1; }

# Anthropic's real release passes every check.
v=$( (set -e; verify_manifest "$t/$stable-manifest.json" "$t/$stable-manifest.json.sig" "$t/key.asc" "$t/gnupg-real") 2>/dev/null )
if [[ $v == "[GNUPG:] VALIDSIG "*" $KEY_FPR" ]]; then pass "real manifest $stable: $v"; else fail "real manifest's signature: $v"; fi
rows=$( (set -e; check_manifest "$t/$stable-manifest.json" "$stable") 2>/dev/null )
if [[ $(wc -l <<<"$rows") == 4 ]]; then pass "real manifest $stable lists the four Linux builds"; else fail "real manifest's builds: $rows"; fi
base=$( (set -e; base_ref) 2>/dev/null )
if [[ $base =~ ^buildpack-deps:trixie-scm@sha256:[0-9a-f]{64}$ ]]; then pass "base: $base"; else fail "base: $base"; fi

# One byte of the manifest changed.
flip() {
    local b
    b=$(od -An -tu1 -j "$2" -N 1 "$1" | tr -d ' ')
    printf "$(printf '\\%03o' $((b ^ 1)))" | dd of="$1" bs=1 seek="$2" count=1 conv=notrunc 2>/dev/null
}
cp "$t/$stable-manifest.json" "$t/flipped.json"
flip "$t/flipped.json" "$(($(wc -c <"$t/flipped.json") / 2))"
refuses "a byte of manifest.json changed" "BADSIG" verify_manifest "$t/flipped.json" "$t/$stable-manifest.json.sig" "$t/key.asc" "$t/gnupg-flip"

# The signature of another release.
if [[ $latest != "$stable" ]]; then
    get "$latest" manifest.json.sig
    refuses "the signature of $latest on $stable's manifest" "BADSIG" verify_manifest "$t/$stable-manifest.json" "$t/$latest-manifest.json.sig" "$t/key.asc" "$t/gnupg-other-release"
else
    fail "stable and latest are both $stable: no other release's signature to try"
fi

# Another key: Anthropic's signature checked against it, and a manifest it signed checked against it.
other="$t/gnupg-other-key"
mkdir -p -m 0700 "$other"
gpg --homedir "$other" --batch --passphrase '' --quick-generate-key 'kiln pin test <pin-test@invalid>' ed25519 sign never >/dev/null 2>&1
gpg --homedir "$other" --batch --armor --export >"$t/other.asc"
gpg --homedir "$other" --batch --pinentry-mode loopback --passphrase '' --detach-sign -o "$t/other.sig" "$t/$stable-manifest.json" 2>/dev/null
refuses "Anthropic's signature against another key" "ERRSIG" verify_manifest "$t/$stable-manifest.json" "$t/$stable-manifest.json.sig" "$t/other.asc" "$t/gnupg-k1"
refuses "a manifest another key signed" "not Anthropic's $KEY_FPR" verify_manifest "$t/$stable-manifest.json" "$t/other.sig" "$t/other.asc" "$t/gnupg-k2"

# A manifest of another version, and one missing a platform.
refuses "a manifest of another version" "not 0.0.1" check_manifest "$t/$stable-manifest.json" 0.0.1
jq 'del(.platforms["linux-arm64-musl"])' "$t/$stable-manifest.json" >"$t/missing.json"
refuses "a manifest missing linux-arm64-musl" "lists no claude binary for linux-arm64-musl" check_manifest "$t/missing.json" "$stable"

# Through main: a manifest changed in transit; then a manifest missing a platform under a signature
# taken as good. Neither leaves a pin.
corrupt() {
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" && if [[ $1 == */manifest.json ]]; then flip "$2" 100; fi; }
    main stable --out "$t/corrupt.json"
}
refuses "main, a manifest changed in transit" "BADSIG" corrupt
if [[ ! -e $t/corrupt.json ]]; then pass "main, a manifest changed in transit: no pin written"; else fail "main left $t/corrupt.json"; fi
unlisted() {
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" && if [[ $1 == */manifest.json ]]; then jq 'del(.platforms["linux-x64"])' "$2" >"$2.cut" && mv "$2.cut" "$2"; fi; }
    verify_manifest() { printf '%s\n' "[GNUPG:] VALIDSIG taken as good for this test"; }
    main stable --out "$t/unlisted.json"
}
refuses "main, a signed manifest missing linux-x64" "lists no claude binary for linux-x64" unlisted
if [[ ! -e $t/unlisted.json ]]; then pass "main, a manifest missing linux-x64: no pin written"; else fail "main left $t/unlisted.json"; fi

if ((failed)); then echo "pin_test: FAILED"; exit 1; fi
echo "pin_test: all passed"
