#!/usr/bin/env bash
# Pins OpenAI's Codex CLI for kiln's built-in Codex Agentfile: pin.json beside this script (kiln plan
# 01a1cd8e862e7169a074db3d5fa33f3d, step 20; its plan 01a1ce705d7076648c1d9f9ee26214d3, step 1).
#
#   pin.sh [latest | rust-vX.Y.Z]            fetch and verify, then write pin.json beside this script
#   pin.sh fetch DIR [latest | rust-vX.Y.Z]  fetch everything verify reads into DIR (new or empty)
#   pin.sh verify DIR [OUT]                  verify DIR, then write OUT (default: pin.json beside this)
#   pin.sh inspect package|ripgrep ARCHIVE ARCH VERSION EXPECT
#                                            an archive's own checks alone, as verify runs them (EXPECT:
#                                            a package's target, or ripgrep's one top directory)
#
# Each platform's archive is OpenAI's whole package, codex-package-<target>.tar.gz, as OpenAI's own
# Linux installer installs it (chatgpt.com/codex/install.sh). It is checked three ways: GitHub's asset
# digest, the release's codex-package_SHA256SUMS, and its keyless cosign bundle, whose certificate
# must name openai/codex's release workflow at the tag, issued by GitHub Actions. cosign is the
# version OpenAI signs with (its linux-code-sign action at the tag), from sigstore's release, checked
# against sigstore's checksums.
#
# Every member must be a regular file or directory, inside the archive, with no set-ID bit, and no ELF
# file may ask an interpreter, for the Agentfile has no libc variant. There are two exceptions:
# - the voice resources, which OpenAI builds for -gnu even in a -musl package and kiln never starts (its
#   Codex adapter refuses /voice), each named in the output;
# - a package's rg: OpenAI takes ripgrep's glibc build for aarch64, which no musl base can start and
#   which would shadow the base's own rg (Codex runs codex-path/rg whenever it is a file). For a
#   platform whose packaged rg needs an ELF interpreter, the pin adds ripgrep's own static
#   <arch>-unknown-linux-musl build, of the version OpenAI's package pins (scripts/codex_package/rg at
#   the tag), to take its place.
#
# verify checks digests first, then cosign, and only then reads a package's members, codex-package.json
# and programs: no package is parsed before its signature holds (ripgrep's archive, once both its
# digests match). Every check must pass before pin.json is written, renamed into place whole.
#
# Its files take the shared shape (kiln plan 01a1cdf3cc7a70e4b3ca2ec610352383's layout, step 25's
# unpack): each its url, sha256, size and path, and its unpack. The package is a tree at the harness's
# root; ripgrep's rg is one member at codex-path/rg, the package's own skipped. The pin also names the
# version smoke a built image is checked by: bin/codex --version prints `codex-cli <version>`, with no
# credential and no network.
#
# Needs bash, curl, jq, python3, sha256sum and GNU coreutils, on Linux amd64 or arm64.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# OpenAI's release repository; the identity its release workflow signs as; GitHub Actions' OIDC issuer
# (.github/workflows/rust-release.yml and .github/actions/linux-code-sign at each tag).
REPO=openai/codex
WORKFLOW=https://github.com/openai/codex/.github/workflows/rust-release.yml
ISSUER=https://token.actions.githubusercontent.com
# kiln's platforms, OpenAI's target for each (as install.sh's vendor_target picks it) and its CPU.
PLATFORMS=("linux/amd64 x86_64-unknown-linux-musl x86_64" "linux/arm64 aarch64-unknown-linux-musl aarch64")
# The base the built-in Agentfiles layer over: step 19's (kiln plan 01a1cdf3cc7a70e4b3ca2ec610352383),
# a Docker Official Image for both arches with git, ssh and CA certificates. The Agentfile's ARG BASE
# default is held to it by step 2's test.
BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10

die() {
    printf 'pin.sh: %s\n' "$*" >&2
    exit 1
}
say() { printf '%s\n' "$*"; }
get() { curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1" || die "could not download $1"; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
bytes() { stat -c %s "$1"; }
# A release's asset NAME as "url digest size", or nothing.
asset() { jq -r --arg n "$2" '.assets[] | select(.name == $n) | "\(.browser_download_url) \(.digest // "-") \(.size)"' "$1"; }
# The cosign version OpenAI's Linux signing action installs.
cosign_version() {
    local v
    v=$(sed -nE 's/^[[:space:]]*cosign-release:[[:space:]]*(v[0-9]+\.[0-9]+\.[0-9]+)[[:space:]]*$/\1/p' "$1/linux-code-sign.yml" | head -1)
    [ -n "$v" ] || die "no cosign-release in OpenAI's linux-code-sign action"
    printf '%s\n' "$v"
}
cosign_arch() {
    case "$(uname -m)" in
        x86_64) echo amd64 ;;
        aarch64) echo arm64 ;;
        *) die "cosign runs here on Linux amd64 or arm64, not $(uname -m)" ;;
    esac
}
# The ripgrep version OpenAI's package pins for Linux (both arches must agree).
rg_version() {
    local v
    v=$(sed '1{/^#!/d}' "$1/rg.dotslash" | jq -r '[.platforms["linux-x86_64", "linux-aarch64"].providers[0].url
        | capture("/releases/download/(?<v>[^/]+)/").v] | unique | if length == 1 then .[0] else empty end') || true
    [ -n "$v" ] || die "OpenAI's scripts/codex_package/rg names no single ripgrep version for Linux"
    printf '%s\n' "$v"
}

fetch() {
    local dir=${1:?usage: pin.sh fetch DIR [latest | rust-vX.Y.Z]} want=${2:-latest} tag url rgv
    mkdir -p "$dir"
    [ -z "$(ls -A "$dir")" ] || die "$dir is not empty"
    case "$want" in
        latest) get "https://api.github.com/repos/$REPO/releases/latest" "$dir/release.json" ;;
        rust-v[0-9]*.[0-9]*.[0-9]*) get "https://api.github.com/repos/$REPO/releases/tags/$want" "$dir/release.json" ;;
        *) die "a release is latest or rust-vX.Y.Z, not $want" ;;
    esac
    tag=$(jq -r '.tag_name // ""' "$dir/release.json")
    [[ $tag =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "the release's tag '$tag' is not rust-vX.Y.Z"
    get "https://raw.githubusercontent.com/$REPO/$tag/.github/actions/linux-code-sign/action.yml" "$dir/linux-code-sign.yml"
    get "https://raw.githubusercontent.com/$REPO/$tag/scripts/codex_package/rg" "$dir/rg.dotslash"
    local cver carch
    cver=$(cosign_version "$dir")
    carch=$(cosign_arch)
    get "https://github.com/sigstore/cosign/releases/download/$cver/cosign-linux-$carch" "$dir/cosign"
    get "https://github.com/sigstore/cosign/releases/download/$cver/cosign_checksums.txt" "$dir/cosign_checksums.txt"
    local name p plat target arch
    for name in codex-package_SHA256SUMS; do
        read -r url _ _ <<<"$(asset "$dir/release.json" "$name")"
        [ -n "${url:-}" ] || die "release $tag has no $name"
        get "$url" "$dir/$name"
    done
    for p in "${PLATFORMS[@]}"; do
        read -r plat target arch <<<"$p"
        for name in "codex-package-$target.tar.gz" "codex-package-$target.tar.gz.sigstore"; do
            read -r url _ _ <<<"$(asset "$dir/release.json" "$name")"
            [ -n "${url:-}" ] || die "release $tag has no $name ($plat)"
            get "$url" "$dir/$name"
        done
    done
    rgv=$(rg_version "$dir")
    get "https://api.github.com/repos/BurntSushi/ripgrep/releases/tags/$rgv" "$dir/ripgrep-release.json"
    for p in "${PLATFORMS[@]}"; do
        read -r plat target arch <<<"$p"
        name=ripgrep-$rgv-$arch-unknown-linux-musl.tar.gz
        read -r url _ _ <<<"$(asset "$dir/ripgrep-release.json" "$name")"
        [ -n "${url:-}" ] || die "ripgrep $rgv has no $name"
        get "$url" "$dir/$name"
        get "$url.sha256" "$dir/$name.sha256"
    done
    say "fetched $tag into $dir"
}

# Members, codex-package.json and programs of one archive, read in one pass: prints JSON facts, or
# names what is wrong on stderr and exits 1.
inspect() {
    python3 -I - "$@" <<'PY'
import hashlib, json, struct, sys, tarfile

# Bytes read of each file of a package: an ELF's header, program headers and interpreter path lie at its
# start (the ELF spec), well within 64 KiB for these; one that puts them further is refused by name, not
# misread.
HEAD = 1 << 16
MACHINES = {"x86_64": 62, "aarch64": 183}
# The one part of a package whose programs may ask an ELF interpreter: OpenAI builds its voice resources
# for the -gnu target even in a -musl package (.github/scripts/build-codex-package-archive.sh at the tag),
# and kiln never starts them (its Codex adapter refuses /voice). codex-path/rg, the other, is replaced
# where it asks one.
VOICE = "codex-resources/voice/"

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def interpreter(buf, name, arch):
    """The ELF interpreter `name` asks for ("" for none); it must be a 64-bit ELF file for `arch`."""
    if len(buf) < 64 or buf[:4] != b"\x7fELF" or buf[4] != 2 or buf[5] != 1:
        fail(f"{name} is not a 64-bit little-endian ELF file")
    if struct.unpack_from("<H", buf, 18)[0] != MACHINES[arch]:
        fail(f"{name} is not built for {arch}")
    phoff = struct.unpack_from("<Q", buf, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", buf, 54)
    if phentsize < 56 or phoff + phentsize * phnum > len(buf):
        fail(f"{name}: its program headers lie past the first {len(buf)} bytes")
    for i in range(phnum):
        p_type, _, p_offset, _, _, p_filesz = struct.unpack_from("<IIQQQQ", buf, phoff + i * phentsize)
        if p_type == 3:  # PT_INTERP
            if p_offset + p_filesz > len(buf):
                fail(f"{name}: its interpreter's path lies past the first {len(buf)} bytes")
            return buf[p_offset:p_offset + p_filesz].rstrip(b"\0").decode(errors="replace")
    return ""

kind, path, arch, version, expect = sys.argv[1:6]
if kind == "package":
    programs = ["bin/codex", "bin/codex-code-mode-host", "bin/codex-exec-server", "codex-resources/bwrap", "codex-path/rg"]
    needs = {"codex-package.json": None}
else:
    # ripgrep's rg is pinned as one member, with its own SHA-256 and size: read whole.
    programs = [f"{expect}/rg"]
    needs = {programs[0]: None}
read, seen, files, dirs, voice = {}, set(), 0, 0, {}
try:
    with tarfile.open(path, "r|gz") as t:
        for m in t:
            name = m.name[2:] if m.name.startswith("./") else m.name
            parts = name.rstrip("/").split("/")
            if name in ("", "."):
                continue
            if name.startswith("/") or ".." in parts:
                fail(f"member {m.name!r} leaves the archive")
            if kind == "ripgrep" and parts[0] != expect:
                fail(f"member {m.name!r} is outside {expect}/")
            key = "/".join(parts)
            if key in seen:
                fail(f"member {m.name!r} appears twice")
            seen.add(key)
            # Every member of a package is laid out; of ripgrep's archive, rg alone.
            if kind == "package" or key in needs:
                if not (m.isreg() or m.isdir()):
                    fail(f"member {m.name!r} is not a regular file or a directory")
                if m.mode & 0o6000:
                    fail(f"member {m.name!r} has a set-ID bit (mode {m.mode:o})")
            files += m.isreg()
            dirs += m.isdir()
            if not m.isreg() or not (kind == "package" or key in needs):
                continue
            if key in programs and not m.mode & 0o111:
                fail(f"member {m.name!r} is not executable (mode {m.mode:o})")
            f = t.extractfile(m)
            if key in needs:
                read[key] = f.read() if needs[key] is None else f.read(needs[key])
                continue
            # Every other file of a package: none may need an ELF interpreter but voice's and rg.
            head = f.read(HEAD)
            if key in programs:
                read[key] = head
            if head[:4] == b"\x7fELF":
                ip = interpreter(head, key, arch)
                if ip and key != "codex-path/rg":
                    if not key.startswith(VOICE):
                        fail(f"{key} needs the ELF interpreter {ip}, and the Agentfile has no libc variant for it")
                    voice[key] = ip
except (tarfile.TarError, OSError, EOFError) as e:
    fail(f"not a readable tar.gz: {e}")

def need(n):
    if n not in read:
        fail(f"no regular file {n} in the archive")
    return read[n]

facts = {"files": files, "dirs": dirs}
if kind == "package":
    try:
        pkg = json.loads(need("codex-package.json"))
    except ValueError as e:
        fail(f"codex-package.json is not JSON: {e}")
    if not isinstance(pkg, dict):
        fail("codex-package.json is not a JSON object")
    for k, v in {"layoutVersion": 1, "version": version, "target": expect, "entrypoint": "bin/codex"}.items():
        if pkg.get(k) != v:
            fail(f"codex-package.json says {k} {pkg.get(k)!r}, not {v!r}")
    # Each program is an ELF file for the platform, and asks no interpreter (rg may: the pin replaces it).
    for n in programs[:-1]:
        interpreter(need(n), n, arch)
    facts["rg_interpreter"] = interpreter(need("codex-path/rg"), "codex-path/rg", arch)
    facts["voice"] = voice
else:
    rg = need(programs[0])
    ip = interpreter(rg, programs[0], arch)
    if ip:
        fail(f"{programs[0]} needs the ELF interpreter {ip}")
    facts["rg"] = {"member": programs[0], "sha256": hashlib.sha256(rg).hexdigest(), "size": len(rg)}
print(json.dumps(facts))
PY
}

verify() {
    local dir=${1:?usage: pin.sh verify DIR [OUT]} out=${2:-$here/pin.json}
    [ -d "$dir" ] || die "no directory $dir"
    local rel=$dir/release.json tag version published
    tag=$(jq -r '.tag_name // ""' "$rel")
    [[ $tag =~ ^rust-v([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "the release's tag '$tag' is not rust-vX.Y.Z"
    version=${BASH_REMATCH[1]}
    [ "$(jq -r '.draft' "$rel")" = false ] || die "release $tag is a draft"
    [ "$(jq -r '.prerelease' "$rel")" = false ] || die "release $tag is a prerelease"
    published=$(jq -r '.published_at' "$rel")

    local cver carch csha clisted
    cver=$(cosign_version "$dir")
    carch=$(cosign_arch)
    csha=$(sha "$dir/cosign")
    clisted=$(awk -v n="cosign-linux-$carch" '$2 == n {print $1}' "$dir/cosign_checksums.txt")
    [ "$csha" = "$clisted" ] || die "cosign $cver: sha256 $csha, sigstore's checksums list '$clisted'"
    chmod 0755 "$dir/cosign"
    say "OK cosign $cver, cosign-linux-$carch sha256 $csha as sigstore lists it (OpenAI signs with $cver)"

    local sums=$dir/codex-package_SHA256SUMS sums_url sums_digest got
    read -r sums_url sums_digest _ <<<"$(asset "$rel" codex-package_SHA256SUMS)"
    [ -n "${sums_url:-}" ] || die "release $tag has no codex-package_SHA256SUMS"
    got=$(sha "$sums")
    [ "sha256:$got" = "$sums_digest" ] || die "codex-package_SHA256SUMS: sha256 $got, GitHub says ${sums_digest#sha256:}"
    say "OK codex-package_SHA256SUMS sha256 $got, as GitHub says"

    local identity=$WORKFLOW@refs/tags/$tag builds='[]' p plat target arch rgv=''
    for p in "${PLATFORMS[@]}"; do
        read -r plat target arch <<<"$p"
        local name=codex-package-$target.tar.gz file url digest size listed burl bdigest facts files
        file=$dir/$name
        read -r url digest size <<<"$(asset "$rel" "$name")"
        [ -n "${url:-}" ] || die "release $tag has no $name ($plat)"
        [ -f "$file" ] || die "$name was not fetched"
        got=$(sha "$file")
        [ "sha256:$got" = "$digest" ] || die "$name: sha256 $got, GitHub says ${digest#sha256:}"
        [ "$(bytes "$file")" = "$size" ] || die "$name: $(bytes "$file") bytes, GitHub says $size"
        listed=$(awk -v n="$name" '$2 == n {print $1}' "$sums")
        [ "$got" = "$listed" ] || die "$name: sha256 $got, codex-package_SHA256SUMS lists '$listed'"
        read -r burl bdigest _ <<<"$(asset "$rel" "$name.sigstore")"
        [ -n "${burl:-}" ] || die "release $tag has no $name.sigstore"
        [ "sha256:$(sha "$file.sigstore")" = "$bdigest" ] || die "$name.sigstore: sha256 $(sha "$file.sigstore"), GitHub says ${bdigest#sha256:}"
        TUF_ROOT=$dir/tuf "$dir/cosign" verify-blob --bundle "$file.sigstore" --certificate-identity "$identity" \
            --certificate-oidc-issuer "$ISSUER" "$file" >"$dir/cosign-$target.log" 2>&1 ||
            die "$name: cosign refuses it for $identity: $(tail -1 "$dir/cosign-$target.log")"
        facts=$(inspect package "$file" "$arch" "$version" "$target" 2>"$dir/inspect.err") || die "$name: $(cat "$dir/inspect.err")"
        say "OK $plat $name sha256 $got, $size bytes, $(jq -r '"\(.files) files and \(.dirs) directories"' <<<"$facts"): GitHub, SHA256SUMS and cosign ($identity)"
        jq -r --arg p "$plat" '.voice | to_entries[] | "NOTE \($p) \(.key) asks \(.value): voice, built for -gnu, which kiln never starts"' <<<"$facts"
        files=$(jq -n --arg url "$url" --arg sha256 "$got" --argjson size "$size" \
            '[{url: $url, sha256: $sha256, size: $size, path: ".", unpack: {format: "tar.gz", strip: 0}}]')

        local interp
        interp=$(jq -r '.rg_interpreter' <<<"$facts")
        if [ -n "$interp" ]; then
            [ -n "$rgv" ] || rgv=$(rg_version "$dir")
            [ "$(jq -r '.tag_name' "$dir/ripgrep-release.json")" = "$rgv" ] || die "the ripgrep release fetched is not $rgv"
            local rname=ripgrep-$rgv-$arch-unknown-linux-musl.tar.gz rfile rurl rdigest rsize rgot rlisted
            rfile=$dir/$rname
            read -r rurl rdigest rsize <<<"$(asset "$dir/ripgrep-release.json" "$rname")"
            [ -n "${rurl:-}" ] || die "ripgrep $rgv has no $rname"
            [ -f "$rfile" ] && [ -f "$rfile.sha256" ] || die "$rname was not fetched"
            rgot=$(sha "$rfile")
            [ "sha256:$rgot" = "$rdigest" ] || die "$rname: sha256 $rgot, GitHub says ${rdigest#sha256:}"
            [ "$(bytes "$rfile")" = "$rsize" ] || die "$rname: $(bytes "$rfile") bytes, GitHub says $rsize"
            rlisted=$(awk 'NR == 1 {print $1}' "$rfile.sha256")
            [ "$rgot" = "$rlisted" ] || die "$rname: sha256 $rgot, ripgrep's .sha256 says '$rlisted'"
            local rfacts
            rfacts=$(inspect ripgrep "$rfile" "$arch" "$rgv" "${rname%.tar.gz}" 2>"$dir/inspect.err") ||
                die "$rname: $(cat "$dir/inspect.err")"
            say "OK $plat $rname sha256 $rgot, $rsize bytes: GitHub and ripgrep's .sha256; its rg, $(jq -r '"sha256 \(.rg.sha256), \(.rg.size) bytes"' <<<"$rfacts"), is static and takes the place of the package's codex-path/rg, which needs $interp"
            files=$(jq --arg url "$rurl" --arg sha256 "$rgot" --argjson size "$rsize" --argjson rg "$(jq -c .rg <<<"$rfacts")" \
                '(.[0].unpack.skip = ["codex-path/rg"])
                 + [{url: $url, sha256: $sha256, size: $size, path: "codex-path/rg", mode: "0755",
                     unpack: {format: "tar.gz", member: $rg.member, sha256: $rg.sha256, size: $rg.size}}]' <<<"$files")
        fi
        builds=$(jq --arg platform "$plat" --argjson files "$files" '. + [{platform: $platform, files: $files}]' <<<"$builds")
    done

    local tmp ripgrep=null
    [ -z "$rgv" ] || ripgrep=$(jq -n --arg v "$rgv" --arg url "https://raw.githubusercontent.com/$REPO/$tag/scripts/codex_package/rg" \
        --arg sha256 "$(sha "$dir/rg.dotslash")" '{version: $v, pinned_by: {url: $url, sha256: $sha256}}')
    tmp=$(mktemp "$(dirname "$out")/.pin.json.XXXXXX")
    if ! jq -n --arg version "$version" --arg tag "$tag" --arg published "$published" --arg read_at "$(date -u +%FT%TZ)" \
        --arg cosign "$cver" --arg cosign_file "cosign-linux-$carch" --arg cosign_sha256 "$csha" \
        --arg identity "$identity" --arg issuer "$ISSUER" --arg sums_url "$sums_url" --arg sums_sha256 "$(sha "$sums")" \
        --argjson ripgrep "$ripgrep" --arg base "$BASE" --argjson builds "$builds" \
        '{version: $version, tag: $tag, published: $published, read_at: $read_at,
          verified: ({cosign: {version: $cosign, file: $cosign_file, sha256: $cosign_sha256},
                      identity: $identity, issuer: $issuer, sums: {url: $sums_url, sha256: $sums_sha256}}
                     + if $ripgrep == null then {} else {ripgrep: $ripgrep} end),
          base: $base,
          smoke: {program: "bin/codex", args: ["--version"], stdout: ("codex-cli " + $version)},
          builds: $builds}' >"$tmp"; then
        rm -f "$tmp"
        die "could not write $out"
    fi
    chmod 0644 "$tmp"
    mv -f "$tmp" "$out"
    say "PINNED codex $version ($tag) for $(jq -r '[.builds[].platform] | join(" and ")' "$out") in $out"
}

case "${1:-latest}" in
    fetch) shift; fetch "$@" ;;
    verify) shift; verify "$@" ;;
    inspect) shift; inspect "$@" ;;
    -h | --help) sed -n '2,10p' "$0" ;;
    *)
        cache=${XDG_CACHE_HOME:-$HOME/.cache}
        mkdir -p "$cache"
        work=$(mktemp -d "$cache/kiln-pin-codex.XXXXXX")
        trap 'rm -rf "$work"' EXIT
        fetch "$work/fetched" "${1:-latest}"
        verify "$work/fetched"
        ;;
esac
