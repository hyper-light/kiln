#!/usr/bin/env bash
# Pins Devin for Terminal's Linux builds into pin.json beside this script, in step 19's schema
# (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 1; research 01a1ce705e4d70089c04cee721525c8f).
#
#   pin.sh [current|VERSION]    current, the default, pins the version current/manifest.json names
#
# Cognition signs nothing here: no signature or checksum file sits beside a release's manifest or
# its bundles (each answers 403 or 404). What can be checked is checked: the SHA-256 the versioned
# manifest gives each Linux bundle, read over HTTPS; current/ byte for byte equal to that manifest
# when it names that version; and each bundle's entries, which must be directories and regular
# files on relative paths with no `..` and no set-ID bit, bin/devin among them. pin.json is
# written only once every check has passed. Should a signature file appear, or whether one exists
# be unknown, pin.sh refuses, so that checking it gets added rather than skipped.
#
# Needs Linux: bash, curl, sha256sum and GNU tar. DEVIN_PIN_BASE replaces
# https://static.devin.ai/cli for pin-tests.sh alone, which serves its fixtures over HTTPS from
# 127.0.0.1, its certificate trusted through CURL_CA_BUNDLE.
set -euo pipefail
export LC_ALL=C

die() {
    printf 'pin.sh: %s\n' "$*" >&2
    exit 1
}
say() {
    printf 'pin.sh: %s\n' "$*"
}

(($# <= 1)) || die "usage: pin.sh [current|VERSION]"
want=${1:-current}
base=${DEVIN_PIN_BASE:-https://static.devin.ai/cli}
base=${base%/}
# Step 19's default base for every built-in harness, by its index digest (plan 01a1cdf3cc7a70e4b3ca2ec610352383).
image_ref=buildpack-deps:trixie-scm
image_digest=sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10
targets=(x86_64-unknown-linux aarch64-unknown-linux)
platforms=(linux/amd64 linux/arm64)

# Only characters pin.json can hold unescaped.
[[ $base =~ ^https://[A-Za-z0-9._~%+:@/-]+$ ]] || die "base: DEVIN_PIN_BASE must be an https:// URL, not $base"
for c in curl sha256sum tar; do
    command -v "$c" >/dev/null 2>&1 || die "tools: pin.sh needs $c"
done
# GNU tar lists names as the archive holds them (--absolute-names keeps a leading / and `..`) and
# escapes control characters, so that each entry is one line.
[[ $(tar --version 2>/dev/null) == *"GNU tar"* ]] || die "tools: pin.sh needs GNU tar"

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
out=$here/pin.json
mkdir -p "$HOME/.cache"
work=$(mktemp -d "$HOME/.cache/devin-pin.XXXXXX")
tmp=
trap 'rm -rf "$work"; [[ -z $tmp ]] || rm -f "$tmp"' EXIT

get() { # URL FILE
    curl -fsSL --proto =https --proto-redir =https -o "$2" "$1" || die "fetch: could not download $1"
}
sha256() { # FILE: its SHA-256 in hex
    local line
    line=$(sha256sum <"$1")
    printf '%s' "${line%% *}"
}
matches() { # TEXT ERE: how often ERE matches TEXT, leftmost first, without overlaps
    local text=$1 n=0
    while [[ $text =~ $2 ]]; do
        n=$((n + 1))
        text=${text#*"${BASH_REMATCH[0]}"}
    done
    printf '%d' "$n"
}
field() { # TEXT KEY: the string KEY holds in TEXT, where KEY is a key exactly once
    local n re="\"$2\"[[:space:]]*:[[:space:]]*\"([^\"\\]*)\""
    n=$(matches "$1" "\"$2\"[[:space:]]*:")
    [[ $n == 1 && $1 =~ $re ]] || return 1
    printf '%s' "${BASH_REMATCH[1]}"
}
manifest() { # URL FILE: fetches a manifest, refused unless it is printable ASCII
    local other
    get "$1" "$2"
    # Bash drops NUL bytes from what it reads, so the key matching below sees only plain text.
    other=$(tr -d '\040-\176\t\r\n' <"$2" | wc -c)
    ((other == 0)) || die "manifest-text: $1 is not all printable ASCII ($((other)) other byte(s))"
}

read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
manifest "$base/current/manifest.json" "$work/current.json"
current_text=$(<"$work/current.json")
cv=$(field "$current_text" version) || die "manifest-version: $base/current/manifest.json names no single version"
if [[ $want == current ]]; then
    v=$cv
else
    v=$want
fi
# The version becomes part of every URL and of pin.json: Cognition's are dotted numbers.
[[ $v =~ ^[0-9]+(\.[0-9]+)*$ ]] || die "version-format: $v is not a version of dotted numbers"
manifest_url=$base/$v/manifest.json
manifest "$manifest_url" "$work/manifest.json"
text=$(<"$work/manifest.json")
named=$(field "$text" version) || die "manifest-version: $manifest_url names no single version"
[[ $named == "$v" ]] || die "manifest-version: $manifest_url names version $named, not $v"
manifest_sha=$(sha256 "$work/manifest.json")
current_sha=$(sha256 "$work/current.json")
if [[ $cv == "$v" ]]; then
    [[ $current_sha == "$manifest_sha" ]] || die "current: $base/current/manifest.json (sha256 $current_sha) names $v but differs from $manifest_url (sha256 $manifest_sha)"
    verified="sha256 from Cognition's versioned manifest over HTTPS, equal to current/ when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403 or 404)"
    say "current/ names $v; $manifest_url (sha256 $manifest_sha) is byte for byte current/, read $read_at"
else
    [[ $cv =~ ^[0-9]+(\.[0-9]+)*$ ]] || die "version-format: current/ names $cv, which is not a version of dotted numbers"
    verified="sha256 from Cognition's versioned manifest over HTTPS; current/ named $cv when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403 or 404)"
    say "pinning $v while current/ names $cv; $manifest_url (sha256 $manifest_sha), read $read_at"
fi

urls=() shas=()
for t in "${targets[@]}"; do
    key="\"$t\"[[:space:]]*:"
    n=$(matches "$text" "$key")
    [[ $n == 1 ]] || die "platform: $manifest_url has $n $t entries, not one"
    re="$key[[:space:]]*[{]([^{}]*)[}]"
    [[ $text =~ $re ]] || die "platform: $manifest_url's $t entry is not a flat object"
    entry=${BASH_REMATCH[1]}
    url=$(field "$entry" url) || die "platform: $manifest_url's $t entry has no single url"
    sha=$(field "$entry" sha256) || die "platform: $manifest_url's $t entry has no single sha256"
    [[ $url == "$base/$v/devin-$v-$t.tar.gz" ]] || die "url: $manifest_url gives $t the URL $url, not $base/$v/devin-$v-$t.tar.gz"
    [[ $sha =~ ^[0-9a-f]{64}$ ]] || die "sha256-form: $manifest_url gives $t the sha256 $sha, not 64 lowercase hex digits"
    urls+=("$url") shas+=("$sha")
done

# Cognition publishes no signature or checksum file beside a release, and a CDN answers 403 or 404
# for what it doesn't hold. One that exists (200) needs its verification written before pinning;
# any other answer leaves unknown whether one exists.
probes=("$base/$v/SHA256SUMS")
for u in "$manifest_url" "${urls[@]}"; do
    for s in .sig .asc .sha256 .sigstore.json; do
        probes+=("$u$s")
    done
done
n403=0 n404=0
for u in "${probes[@]}"; do
    code=$(curl -sS -o /dev/null -w '%{http_code}' --proto =https "$u") || die "signature-unknown: could not ask for $u, so whether Cognition publishes a signature there is unknown"
    case $code in
    403) n403=$((n403 + 1)) ;;
    404) n404=$((n404 + 1)) ;;
    200) die "signature: $u answers 200: Cognition publishes a signature or checksum there now; add its verification to pin.sh before pinning" ;;
    *) die "signature-unknown: $u answers $code, so whether Cognition publishes a signature there is unknown" ;;
    esac
done
say "no signature: each of ${#probes[@]} URLs answers 403 or 404 ($n403 answered 403, $n404 answered 404): $base/$v/SHA256SUMS, and .sig, .asc, .sha256 and .sigstore.json beside the manifest and both bundles"

sizes=()
for i in "${!targets[@]}"; do
    t=${targets[i]} url=${urls[i]} sha=${shas[i]}
    f=$work/devin-$v-$t.tar.gz
    get "$url" "$f"
    got=$(sha256 "$f")
    [[ $got == "$sha" ]] || die "digest: $url has SHA-256 $got; $manifest_url gives $sha"
    size=$(($(wc -c <"$f")))
    sizes+=("$size")

    tar --list --gzip --absolute-names --quoting-style=escape --file="$f" >"$work/names" 2>"$work/err" || die "list: tar could not list $url: $(<"$work/err")"
    tar --list --verbose --numeric-owner --gzip --absolute-names --quoting-style=escape --file="$f" >"$work/long" 2>"$work/err" || die "list: tar could not list $url: $(<"$work/err")"
    (($(wc -l <"$work/names") == $(wc -l <"$work/long"))) || die "list: tar's two listings of $url disagree"
    files=0 dirs=0 devin=
    while IFS= read -r path <&3 && IFS= read -r line <&4; do
        mode=${line%% *}
        case ${mode:0:1} in
        -) files=$((files + 1)) ;;
        d) dirs=$((dirs + 1)) ;;
        *) die "entry-type: $url holds $path, which is neither a directory nor a regular file ($mode)" ;;
        esac
        [[ $path != /* ]] || die "absolute: $url holds the absolute path $path"
        [[ /$path/ != */../* ]] || die "dotdot: $url holds $path, whose path has a .. component"
        [[ ${mode:3:1} != [sS] && ${mode:6:1} != [sS] ]] || die "set-id: $url holds $path with a set-ID bit ($mode)"
        if [[ ${mode:0:1} == - && ($path == bin/devin || $path == ./bin/devin) ]]; then
            read -r _ _ devin _ <<<"$line"
            devin="$mode, $devin bytes"
        fi
    done 3<"$work/names" 4<"$work/long"
    [[ -n $devin ]] || die "bin-devin: $url holds no bin/devin"
    say "${platforms[i]} ($t): $url, sha256 $got as the manifest gives it, $size bytes; $files regular files and $dirs directories, bin/devin $devin"
done

json() {
    local i sep
    printf '{\n  "harness": "devin",\n  "version": "%s",\n  "verified": "%s",\n' "$v" "$verified"
    printf '  "manifest": {\n    "url": "%s",\n    "sha256": "%s"\n  },\n' "$manifest_url" "$manifest_sha"
    printf '  "read_at": "%s",\n' "$read_at"
    printf '  "base": {\n    "ref": "%s",\n    "digest": "%s"\n  },\n' "$image_ref" "$image_digest"
    printf '  "builds": [\n'
    for i in "${!targets[@]}"; do
        sep=,
        ((i + 1 < ${#targets[@]})) || sep=
        printf '    {\n      "platform": "%s",\n      "files": [\n        {\n' "${platforms[i]}"
        printf '          "url": "%s",\n          "sha256": "%s",\n          "size": %s,\n' "${urls[i]}" "${shas[i]}" "${sizes[i]}"
        printf '          "path": ".",\n          "unpack": {\n            "format": "tar.gz"\n          }\n        }\n      ]\n    }%s\n' "$sep"
    done
    printf '  ]\n}\n'
}
tmp=$out.$$.tmp
json >"$tmp" || die "write: could not write $tmp"
mv -f -- "$tmp" "$out" || die "write: could not rename $tmp to $out"
tmp=
say "wrote $out (sha256 $(sha256 "$out")): devin $v for ${platforms[*]}"
