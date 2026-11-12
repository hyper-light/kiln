#!/usr/bin/env bash
# Pins the Claude Code builds kiln's built-in Claude Code Agentfile installs (plan
# 01a1cd8e862e7169a074db3d5fa33f3d, step 19; its plan 01a1cf48e8157494ac9a51f7a564f526, research
# 01a1cf48e8157494b048e1fb2c437f08), and writes pin.json beside this script:
#
#   .kiln/images/harness-claude/pin.sh [stable|latest|X.Y.Z] [--out PATH]
#
# The builds are Anthropic's own Linux binaries: x64 and arm64, each for glibc and for musl (research
# f10). Each release's manifest.json lists every binary's SHA-256 and size and is signed by
# Anthropic's release key, so verifying the signature verifies every binary it lists (f11;
# code.claude.com/docs/en/setup, "Binary integrity and code signing"). The pin is taken from the
# manifest only once GnuPG's machine-readable status (gnupg doc/DETAILS) reports a GOODSIG and a
# VALIDSIG whose primary key is the one that page publishes. Nothing is downloaded but the channel,
# the manifest, its signature and the key. stable and latest are Anthropic's channels; stable is
# about a week old and skips releases with major regressions (f12). Only this script needs gpg, jq
# and network; kiln's fetch at build time needs HTTPS alone, and checks each binary's SHA-256 and
# size as it streams.
#
# The pin also records the base image the Agentfile builds on by default, buildpack-deps:trixie-scm
# (git, an ssh client, curl and CA certificates on both arches, with no RUN: f46), read by digest with
# a manifest HEAD, which doesn't count as a Docker Hub pull. Work files go under ~/.cache (kiln's
# AGENTS.md: /tmp on its Linux machines is a RAM disk).

set -euo pipefail

readonly RELEASES=https://downloads.claude.ai/claude-code-releases
readonly KEY_URL=https://downloads.claude.ai/keys/claude-code.asc
# Anthropic's release key, as code.claude.com/docs/en/setup publishes it ("Verify the manifest
# signature", step 1).
readonly KEY_FPR=31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE
readonly BASE_REPO=library/buildpack-deps
readonly BASE_TAG=trixie-scm
readonly SEMVER='^[0-9]+\.[0-9]+\.[0-9]+$'
# Anthropic's Linux platforms, and the OCI platform and libc variant each one serves.
readonly PLATFORMS=(
    "linux-x64 linux/amd64 gnu"
    "linux-x64-musl linux/amd64 musl"
    "linux-arm64 linux/arm64 gnu"
    "linux-arm64-musl linux/arm64 musl"
)
# What the EXIT trap removes: script-wide, as the trap runs after main has returned.
work=""
partial=""

log() { printf 'pin.sh: %s\n' "$*" >&2; }
die() { log "refused: $*"; exit 1; }

sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }

# The x.y.z version a channel names: stable and latest are Anthropic's files holding one version.
resolve_version() {
    local channel=$1 file version
    case $channel in
        stable | latest)
            file="$work/$channel"
            fetch "$RELEASES/$channel" "$file" || die "$RELEASES/$channel didn't answer"
            version=$(tr -d '[:space:]' <"$file")
            [[ $version =~ $SEMVER ]] || die "the $channel channel names no x.y.z version (\"$(head -c 80 "$file" | tr -cd '[:print:]')\")"
            printf '%s\n' "$version" ;;
        *)
            [[ $channel =~ $SEMVER ]] || die "$channel: expected stable, latest or x.y.z"
            printf '%s\n' "$channel" ;;
    esac
}

# The manifest's signature is Anthropic's. For each signature GnuPG reports one of GOODSIG, EXPSIG,
# EXPKEYSIG, REVKEYSIG, BADSIG or ERRSIG, and for a good one also VALIDSIG, whose tenth argument is
# the primary key's fingerprint (gnupg doc/DETAILS). So: no bad, expired or revoked one, at least
# one GOODSIG, and every VALIDSIG naming Anthropic's primary key. Prints the VALIDSIG line.
verify_manifest() {
    local manifest=$1 sig=$2 key=$3 home=$4 status bad validsig
    mkdir -p -m 0700 "$home"
    gpg --homedir "$home" --batch --quiet --import "$key" >/dev/null 2>&1 || die "$KEY_URL holds no key gpg can import"
    status=$(gpg --homedir "$home" --batch --status-fd 1 --verify "$sig" "$manifest" 2>/dev/null) || true
    bad=$(grep -E '^\[GNUPG:\] (BADSIG|ERRSIG|EXPSIG|EXPKEYSIG|REVKEYSIG|NO_PUBKEY) ' <<<"$status" | head -1) || true
    [[ -z $bad ]] || die "manifest.json's signature: $bad"
    grep -q -E '^\[GNUPG:\] GOODSIG ' <<<"$status" || die "manifest.json's signature: gpg reports no GOODSIG"
    validsig=$(grep -E '^\[GNUPG:\] VALIDSIG ' <<<"$status") || die "manifest.json's signature: gpg reports no VALIDSIG"
    while read -r _ _ _ _ _ _ _ _ _ _ _ primary _; do
        [[ $primary == "$KEY_FPR" ]] || die "manifest.json is signed by a key whose primary is $primary, not Anthropic's $KEY_FPR"
    done <<<"$validsig"
    head -1 <<<"$validsig"
}

# The verified manifest is of the version asked, and lists each Linux build: its binary `claude`, a
# SHA-256 and a size (Anthropic's installer reads platforms[<platform>].checksum and .size). Prints,
# per build, its Anthropic platform, OCI platform, variant, SHA-256 and size, tab-separated.
check_manifest() {
    local manifest=$1 version=$2 got row p platform variant binary sha size
    got=$(jq -r '.version // empty' "$manifest" 2>/dev/null) || die "manifest.json isn't JSON"
    [[ $got == "$version" ]] || die "manifest.json is version \"$got\", not $version"
    for row in "${PLATFORMS[@]}"; do
        read -r p platform variant <<<"$row"
        binary=$(jq -r --arg p "$p" '.platforms[$p].binary // empty' "$manifest")
        sha=$(jq -r --arg p "$p" '.platforms[$p].checksum // empty' "$manifest")
        size=$(jq -r --arg p "$p" '.platforms[$p].size // empty' "$manifest")
        [[ $binary == claude ]] || die "manifest.json lists no claude binary for $p (\"$binary\")"
        [[ $sha =~ ^[0-9a-f]{64}$ ]] || die "manifest.json gives $p no SHA-256 (\"$sha\")"
        [[ $size =~ ^[1-9][0-9]*$ ]] || die "manifest.json gives $p no size (\"$size\")"
        printf '%s\t%s\t%s\t%s\t%s\n' "$p" "$platform" "$variant" "$sha" "$size"
    done
}

# The base's digest, by a manifest HEAD (an anonymous pull token; no pull is counted).
base_ref() {
    local token digest
    token=$(curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:$BASE_REPO:pull" | jq -r '.token // empty')
    [[ -n $token ]] || die "Docker Hub gave no token for $BASE_REPO"
    digest=$(curl -fsSI -H "Authorization: Bearer $token" \
        -H 'Accept: application/vnd.oci.image.index.v1+json' \
        -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' \
        "https://registry-1.docker.io/v2/$BASE_REPO/manifests/$BASE_TAG" | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }')
    [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || die "Docker Hub gave no digest for ${BASE_REPO#library/}:$BASE_TAG"
    printf '%s:%s@%s\n' "${BASE_REPO#library/}" "$BASE_TAG" "$digest"
}

cleanup() {
    if [[ -n $work ]]; then
        # Ends the agent gpg may have started for the temporary home.
        gpgconf --homedir "$work/gnupg" --kill all >/dev/null 2>&1 || true
        rm -rf "$work"
    fi
    if [[ -n $partial ]]; then rm -f "$partial"; fi
}

main() {
    local channel=stable out=""
    while (($#)); do
        case $1 in
            --out) out=${2:?--out takes a path}; shift 2 ;;
            -*) die "unknown option $1" ;;
            *) channel=$1; shift ;;
        esac
    done
    if [[ -z $out ]]; then
        [[ -n ${BASH_SOURCE[0]:-} ]] || die "--out is needed when the script comes from stdin"
        out="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pin.json"
    fi
    local tool
    for tool in gpg gpgconf jq curl; do command -v "$tool" >/dev/null || die "$tool is not installed"; done
    command -v sha256sum >/dev/null || command -v shasum >/dev/null || die "neither sha256sum nor shasum is installed"

    local cache
    cache=${XDG_CACHE_HOME:-$HOME/.cache}
    mkdir -p "$cache"
    work=$(mktemp -d "$cache/kiln-pin-claude.XXXXXX")
    trap cleanup EXIT

    local version read_at murl
    version=$(resolve_version "$channel")
    read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    log "$channel is $version (read $read_at)"
    murl="$RELEASES/$version/manifest.json"
    fetch "$murl" "$work/manifest.json" || die "$murl didn't answer"
    fetch "$murl.sig" "$work/manifest.json.sig" || die "$murl.sig didn't answer"
    fetch "$KEY_URL" "$work/key.asc" || die "$KEY_URL didn't answer"

    local validsig rows builds
    validsig=$(verify_manifest "$work/manifest.json" "$work/manifest.json.sig" "$work/key.asc" "$work/gnupg")
    log "manifest.json $version: $validsig"
    rows=$(check_manifest "$work/manifest.json" "$version")
    builds=$(while IFS=$'\t' read -r p platform variant sha size; do
        log "$p ($platform $variant): $size bytes, SHA-256 $sha"
        jq -n --arg platform "$platform" --arg variant "$variant" --arg url "$RELEASES/$version/$p/claude" \
            --arg sha "$sha" --argjson size "$size" \
            '{platform: $platform, variant: $variant, files: [{url: $url, sha256: $sha, size: $size, path: "bin/claude", mode: "0755"}]}'
    done <<<"$rows" | jq -s '.')

    local base pin
    base=$(base_ref)
    log "base $base"
    pin=$(jq -n --arg version "$version" --arg channel "$channel" --arg read_at "$read_at" \
        --arg murl "$murl" --arg msha "$(sha256_of "$work/manifest.json")" --arg ssha "$(sha256_of "$work/manifest.json.sig")" \
        --arg key_url "$KEY_URL" --arg fpr "$KEY_FPR" --arg validsig "$validsig" \
        --arg commit "$(jq -r '.commit // empty' "$work/manifest.json")" --arg built "$(jq -r '.buildDate // empty' "$work/manifest.json")" \
        --arg base "$base" --argjson builds "$builds" \
        '{version: $version, channel: $channel, read_at: $read_at,
          verified: {method: "gpg-signed-manifest", manifest: {url: $murl, sha256: $msha}, signature: {url: ($murl + ".sig"), sha256: $ssha},
            signer: {url: $key_url, fingerprint: $fpr}, status: $validsig, commit: $commit, built: $built},
          base: $base, builds: $builds}')
    # Written beside its place and renamed into it, so a failure never leaves half a pin.
    partial="$out.partial.$$"
    printf '%s\n' "$pin" >"$partial"
    mv "$partial" "$out"
    partial=""
    log "wrote $out"
}

# Run when executed (or read from stdin); a test sources it for its functions.
if [[ -z ${BASH_SOURCE[0]:-} || ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
