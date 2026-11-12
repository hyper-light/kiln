#!/usr/bin/env bash
# Pins the Goose build kiln's built-in Goose Agentfile installs (plan 01a1cd8e862e7169a074db3d5fa33f3d,
# step 23; its plan 01a1ce705e3d72af83ef15ae83b62f45, research 01a1ce705e3d72af8563e07fa92a7460), and
# writes pin.json beside this script:
#
#   .kiln/images/harness-goose/pin.sh [stable|latest|vX.Y.Z] [--out PATH]
#
# The builds are Goose's own musl archives from aaif-goose/goose's GitHub release, one per
# architecture: static on both, so each runs on any Linux base (research f10). Each archive is held
# to Goose's own release record before it's pinned:
# - its size and SHA-256 equal GitHub's asset digest;
# - its GitHub artifact attestation, SLSA provenance from release.yml at the release's own tag,
#   verifies and lists the archive's digest (f8);
# - its only regular file is ./goose, whose SHA-256 and size the pin records, so kiln's fetcher can
#   check the binary it unpacks (f6).
# stable is the release whose musl archives the `stable` release serves; latest is the newest
# release. Only this script needs gh, jq and network; kiln's fetch at build time needs HTTPS alone.
#
# The pin also records the base image the Agentfile builds on by default: step 19's default base,
# buildpack-deps:trixie-scm (git, an ssh client, curl and CA certificates on both arches, which
# Goose's TLS needs: f18), read by digest with a manifest HEAD, which doesn't count as a Docker Hub
# pull. And it records the smoke an image's Goose must pass, for whatever runs it (step 19's image
# lookup, CI's legs), in the shape the other harnesses' pins give theirs: `bin/goose --version`
# prints its version after a space, the line 1.59.0's musl build wrote as 20 31 2e 35 39 2e 30 0a
# (f12); `stdout` is that line without its newline, its leading space kept.
# Big files go under ~/.cache (kiln's AGENTS.md: /tmp on its Linux machines is a RAM disk).

set -euo pipefail

readonly REPO=aaif-goose/goose
readonly SIGNER=aaif-goose/goose/.github/workflows/release.yml
readonly PREDICATE=https://slsa.dev/provenance/v1
readonly BASE_REPO=library/buildpack-deps
readonly BASE_TAG=trixie-scm
readonly MUSL='^goose-(x86_64|aarch64)-unknown-linux-musl\.tar\.gz$'
readonly SEMVER='^v[0-9]+\.[0-9]+\.[0-9]+$'
# What the EXIT trap removes: script-wide, as the trap runs after main has returned.
work=""
partial=""

log() { printf 'pin.sh: %s\n' "$*" >&2; }
die() { log "refused: $*"; exit 1; }

# Goose's archive architectures and the OCI platforms they serve.
platform_of() {
    case $1 in
        x86_64) echo linux/amd64 ;;
        aarch64) echo linux/arm64 ;;
        *) die "no platform for $1" ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
size_of() { wc -c <"$1" | tr -d ' '; }

# The release JSON (its assets with their digests) for a tag.
release_json() { gh api "repos/$REPO/releases/tags/$1"; }

# The newest releases, newest first: one page of GitHub's largest size.
releases_json() { gh api "repos/$REPO/releases?per_page=100"; }

# A release's two musl archives' names and digests, sorted by name, as JSON.
musl_digests() {
    jq -c --arg musl "$MUSL" '[.assets[] | select(.name | test($musl)) | {name, digest}] | sort_by(.name)' <<<"$1"
}

# The vX.Y.Z tag a channel names. Only vX.Y.Z tags release one version: `stable` (and `canary`)
# are releases whose assets are replaced as Goose releases.
resolve_tag() {
    local channel=$1 all stable want tag
    case $channel in
        v*)
            [[ $channel =~ $SEMVER ]] || die "$channel is not vX.Y.Z"
            printf '%s\n' "$channel" ;;
        latest)
            all=$(releases_json)
            tag=$(jq -r --arg semver "$SEMVER" '[.[] | select((.draft | not) and (.prerelease | not) and (.tag_name | test($semver)))][0].tag_name // empty' <<<"$all")
            [[ -n $tag ]] || die "no published vX.Y.Z release among $REPO's newest"
            printf '%s\n' "$tag" ;;
        stable)
            stable=$(release_json stable)
            want=$(musl_digests "$stable")
            [[ $(jq 'length' <<<"$want") == 2 ]] || die "the stable release doesn't serve both musl archives"
            all=$(releases_json)
            tag=$(jq -r --arg semver "$SEMVER" --arg musl "$MUSL" --argjson want "$want" '[.[]
                | select((.draft | not) and (.prerelease | not) and (.tag_name | test($semver)))
                | select(([.assets[] | select(.name | test($musl)) | {name, digest}] | sort_by(.name)) == $want)][0].tag_name // empty' <<<"$all")
            [[ -n $tag ]] || die "the stable release serves musl archives no vX.Y.Z release among $REPO's newest has"
            printf '%s\n' "$tag" ;;
        *) die "$channel: expected stable, latest or vX.Y.Z" ;;
    esac
}

# One architecture's musl archive in a release: its URL, size and SHA-256, tab-separated.
asset_of() {
    local release=$1 arch=$2 name row url size digest
    name="goose-$arch-unknown-linux-musl.tar.gz"
    row=$(jq -r --arg n "$name" '[.assets[] | select(.name == $n)]
        | if length == 1 then .[0] | [.browser_download_url, (.size | tostring), (.digest // "")] | @tsv else empty end' <<<"$release")
    [[ -n $row ]] || die "the release has no single $name"
    IFS=$'\t' read -r url size digest <<<"$row"
    [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || die "$name: GitHub gives no SHA-256 digest (\"$digest\")"
    [[ $url == "https://github.com/$REPO/releases/download/"* ]] || die "$name: its URL is not $REPO's release download ($url)"
    [[ $size =~ ^[0-9]+$ ]] || die "$name: GitHub gives no size (\"$size\")"
    printf '%s\t%s\t%s\n' "$url" "$size" "${digest#sha256:}"
}

fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }

# The archive is the one GitHub records: its size and its SHA-256.
check_archive() {
    local file=$1 size=$2 sha=$3 got
    got=$(size_of "$file")
    [[ $got == "$size" ]] || die "$(basename "$file"): $got bytes, GitHub records $size"
    got=$(sha256_of "$file")
    [[ $got == "$sha" ]] || die "$(basename "$file"): SHA-256 $got, GitHub records $sha"
}

# The archive's attestation verifies as signed by Goose's release workflow at this tag, and lists
# the archive's digest. Prints the source commit it names.
verify_attestation() {
    local file=$1 tag=$2 sha=$3 out commit
    out=$(gh attestation verify "$file" --repo "$REPO" --signer-workflow "$SIGNER" --source-ref "refs/tags/$tag" --format json) \
        || die "$(basename "$file"): its attestation doesn't verify for $SIGNER at refs/tags/$tag"
    commit=$(jq -r --arg d "$sha" --arg p "$PREDICATE" '[.[] | .verificationResult | select(.statement.predicateType == $p)
        | select([.statement.subject[].digest.sha256] | index($d)) | .signature.certificate.sourceRepositoryDigest][0] // empty' <<<"$out")
    [[ $commit =~ ^[0-9a-f]{40}$ ]] || die "$(basename "$file"): no verified $PREDICATE attestation lists its digest $sha"
    printf '%s\n' "$commit"
}

# The archive's one regular file is goose (a leading ./ allowed); its only other entries are folders.
# Prints the member's name as the archive has it.
member_of() {
    local file=$1 types names member="" n=0 type name
    types=$(tar -tvzf "$file" | cut -c1) || die "$(basename "$file"): not a gzip'd tar"
    names=$(tar -tzf "$file")
    [[ $(wc -l <<<"$types") == $(wc -l <<<"$names") ]] || die "$(basename "$file"): its listing doesn't parse"
    while IFS=$'\t' read -r type name; do
        case $type in
            d) ;;
            -) n=$((n + 1)); member=$name ;;
            *) die "$(basename "$file"): $name is no regular file or folder (type $type)" ;;
        esac
    done < <(paste <(printf '%s\n' "$types") <(printf '%s\n' "$names"))
    [[ $n == 1 && ($member == ./goose || $member == goose) ]] || die "$(basename "$file"): expected one regular file, ./goose; found $n ($member)"
    printf '%s\n' "$member"
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
    for tool in gh jq curl tar; do command -v "$tool" >/dev/null || die "$tool is not installed"; done
    command -v sha256sum >/dev/null || command -v shasum >/dev/null || die "neither sha256sum nor shasum is installed"

    local tag version read_at release cache
    tag=$(resolve_tag "$channel")
    version=${tag#v}
    read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    log "$channel is $tag (read $read_at)"
    release=$(release_json "$tag")
    cache=${XDG_CACHE_HOME:-$HOME/.cache}
    mkdir -p "$cache"
    work=$(mktemp -d "$cache/kiln-pin-goose.XXXXXX")
    trap 'rm -rf ${work:+"$work"} ${partial:+"$partial"}' EXIT

    local arch row url size sha file commit="" this member msha msize builds=()
    for arch in x86_64 aarch64; do
        row=$(asset_of "$release" "$arch")
        IFS=$'\t' read -r url size sha <<<"$row"
        file="$work/$(basename "$url")"
        fetch "$url" "$file"
        check_archive "$file" "$size" "$sha"
        log "$(basename "$file"): $size bytes, SHA-256 $sha, as GitHub records"
        this=$(verify_attestation "$file" "$tag" "$sha")
        [[ -z $commit || $commit == "$this" ]] || die "the archives' attestations name two commits: $commit and $this"
        commit=$this
        log "$(basename "$file"): attested by $SIGNER at refs/tags/$tag, commit $commit"
        member=$(member_of "$file")
        mkdir -p "$work/$arch"
        tar -xzf "$file" -C "$work/$arch" "$member"
        [[ -f $work/$arch/goose && ! -L $work/$arch/goose ]] || die "$(basename "$file"): $member didn't unpack as a regular file"
        msha=$(sha256_of "$work/$arch/goose")
        msize=$(size_of "$work/$arch/goose")
        log "$(basename "$file"): $member is $msize bytes, SHA-256 $msha"
        builds+=("$(jq -n --arg platform "$(platform_of "$arch")" --arg url "$url" --arg sha "$sha" --argjson size "$size" \
            --arg msha "$msha" --argjson msize "$msize" \
            '{platform: $platform, files: [{url: $url, sha256: $sha, size: $size, path: "bin/goose", mode: "0755",
              unpack: {format: "tar.gz", member: "goose", sha256: $msha, size: $msize}}]}')")
    done

    local base pin
    base=$(base_ref)
    log "base $base"
    pin=$(jq -n --arg version "$version" --arg channel "$channel" --arg read_at "$read_at" --arg repo "$REPO" \
        --arg signer "$SIGNER" --arg ref "refs/tags/$tag" --arg commit "$commit" --arg predicate "$PREDICATE" \
        --arg base "$base" --argjson builds "$(printf '%s\n' "${builds[@]}" | jq -s '.')" \
        '{version: $version, channel: $channel, read_at: $read_at,
          verified: {method: "github-attestation", repo: $repo, signer_workflow: $signer, source_ref: $ref, commit: $commit, predicate: $predicate},
          base: $base,
          smoke: {program: "bin/goose", args: ["--version"], stdout: (" " + $version)},
          builds: $builds}')
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
