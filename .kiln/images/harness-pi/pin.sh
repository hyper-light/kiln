#!/usr/bin/env bash
# Pins pi's own Linux builds, and the ripgrep and fd builds pi's tools-manager would otherwise
# fetch from GitHub's latest releases at run time, into pin.json beside this script.
# kiln plan 01a1ce705e62732b83f16295d74ee263 (step 28 of 01a1cd8e862e7169a074db3d5fa33f3d);
# research 01a1ce705e62732b86d3534d788ae242.
#
#   pin.sh [PI_TAG [RG_TAG [FD_TAG]]]    each defaults to its project's latest release
#
# Nothing signs pi's builds: no signature, no attestation, and its releases are not immutable
# (research f6). What can be checked is checked: each SHA-256 against the release's SHA256SUMS
# and against the digest GitHub records at upload; ripgrep's against its .sha256 files too; fd's
# against GitHub's alone (fd publishes no checksum file). If pi starts attesting its builds or
# making its releases immutable, pin.sh stops, so that the stronger check gets added rather than
# skipped.
#
# The pin also records the smoke an image's pi must pass, in the shape every harness's pin gives its
# own: `pi --version`, pi being the executable its tarball's tree puts at the harness directory's root
# (the tarball's pi/pi, which pin.sh requires), prints its bare version and a newline (1.5.2's x64
# build wrote 31 2e 35 2e 32 0a: docs/research/s28-pi-agentfile/step2/); `stdout` is that line without
# its newline.
#
# Needs bash, curl, tar, python3 and sha256sum or shasum. Environment:
#   PIN_GITHUB_API, PIN_GITHUB_DOWNLOAD  where releases are read and downloaded (tests point them
#                                        at a local server); default api.github.com, github.com
#   PIN_BASE   the base image the pin is made for, <ref>@sha256:<digest> as the Agentfile's ARG BASE
#              names it; default the shared default of every built-in harness (research f10)
#   PIN_OUT    the file written; default pin.json beside this script
set -euo pipefail

api=${PIN_GITHUB_API:-https://api.github.com}
dl=${PIN_GITHUB_DOWNLOAD:-https://github.com}
here=$(cd "$(dirname "$0")" && pwd)
out=${PIN_OUT:-$here/pin.json}
base=${PIN_BASE:-buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10}
pi_repo=earendil-works/pi rg_repo=BurntSushi/ripgrep fd_repo=sharkdp/fd

die() {
    printf 'pin.sh: %s\n' "$*" >&2
    exit 1
}
for c in curl tar python3; do
    command -v "$c" >/dev/null 2>&1 || die "needs $c"
done
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "needs sha256sum or shasum"
fi
[[ $base =~ ^([^@[:space:]]+)@(sha256:[0-9a-f]{64})$ ]] || die "PIN_BASE must be <ref>@sha256:<64 hex digits>, not $base"

# The JSON half: reading GitHub's records and checksum files, and writing pin.json.
IFS= read -r -d '' PY <<'EOF' || true
import json, os, re, sys

def fail(msg):
    print("pin.sh: " + msg, file=sys.stderr)
    sys.exit(1)

def load(path, what):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        fail(f"{what} is not JSON: {e}")

cmd, args = sys.argv[1], sys.argv[2:]
if cmd == "release":  # FILE REPO: a release, not a draft or prerelease -> tag, published_at, immutable
    path, repo = args
    d = load(path, f"{repo}'s release")
    tag = d.get("tag_name")
    if not isinstance(tag, str) or not tag:
        fail(f"{repo}: the release names no tag")
    if d.get("draft") is not False:
        fail(f"{repo} {tag}: a draft, not a published release")
    if d.get("prerelease") is not False:
        fail(f"{repo} {tag}: a prerelease")
    print(tag, d.get("published_at") or "-", "true" if d.get("immutable") is True else "false")
elif cmd == "asset":  # FILE REPO NAME -> the asset's size and GitHub's digest
    path, repo, name = args
    d = load(path, f"{repo}'s release")
    found = [a for a in d.get("assets") or [] if isinstance(a, dict) and a.get("name") == name]
    if len(found) != 1:
        fail(f"{repo} {d.get('tag_name')}: {len(found)} assets named {name}, not one")
    size, digest = found[0].get("size"), found[0].get("digest")
    if not isinstance(size, int) or size < 0:
        fail(f"{repo} {d.get('tag_name')}: GitHub records no size for {name}")
    if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        fail(f"{repo} {d.get('tag_name')}: GitHub records no SHA-256 for {name}")
    print(size, digest)
elif cmd == "sum":  # FILE NAME -> the SHA-256 a checksum file gives NAME ("<hex>  NAME" or "<hex> *NAME")
    path, name = args
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except (OSError, ValueError) as e:
        fail(f"{os.path.basename(path)} is unreadable: {e}")
    hits = [m.group(1) for m in (re.fullmatch(r"([0-9a-f]{64}) [ *](.+)", l) for l in lines) if m and m.group(2) == name]
    if len(hits) != 1:
        fail(f"{os.path.basename(path)} has {len(hits)} lines for {name}, not one")
    print(hits[0])
elif cmd == "version":  # package.json on stdin -> its version
    try:
        v = json.load(sys.stdin).get("version")
    except ValueError as e:
        fail(f"pi/package.json is not JSON: {e}")
    if not isinstance(v, str):
        fail("pi/package.json names no version")
    print(v)
elif cmd == "ref":  # FILE -> a git ref's (or tag object's) object type and sha
    o = load(args[0], "a git ref").get("object") or {}
    print(o.get("type") or "-", o.get("sha") or "-")
elif cmd == "tree":  # ARCHIVE PREFIX -> entries and unpacked bytes of a tree unpacked with PREFIX/ stripped
    # Measured here, at pin time, so the unpacker can hold the archive to its own counts (R03).
    import tarfile
    path, prefix = args
    entries = unpacked = 0
    try:
        with tarfile.open(path, "r:gz") as t:
            for m in t:
                name = m.name.rstrip("/")
                if name != prefix and not name.startswith(prefix + "/"):
                    fail(f"{os.path.basename(path)} holds {m.name}, outside {prefix}/")
                if not (m.isfile() or m.isdir()):
                    fail(f"{os.path.basename(path)} holds {m.name}, neither a regular file nor a directory")
                entries += 1
                unpacked += m.size if m.isfile() else 0
    except (OSError, tarfile.TarError) as e:
        fail(f"{os.path.basename(path)} is not a readable tar.gz: {e}")
    print(entries, unpacked)
elif cmd == "member":  # ARCHIVE MEMBER -> the member's SHA-256 and size; it must be a regular file
    import hashlib, tarfile
    path, member = args
    try:
        with tarfile.open(path, "r:gz") as t:
            try:
                m = t.getmember(member)
            except KeyError:
                fail(f"{os.path.basename(path)} holds no {member}")
            if not m.isfile():
                fail(f"{os.path.basename(path)}: {member} is not a regular file")
            digest = hashlib.sha256(t.extractfile(m).read()).hexdigest()  # a tool binary: a few MB
    except (OSError, tarfile.TarError) as e:
        fail(f"{os.path.basename(path)} is not a readable tar.gz: {e}")
    print(digest, m.size)
elif cmd == "write":  # OUT, facts on stdin -> pin.json, written to a temporary file and renamed
    out = args[0]
    meta, builds = {}, {}
    for line in sys.stdin.read().splitlines():
        kind, *f = line.split("\t")
        if kind == "meta":
            meta[f[0]] = f[1]
        elif kind == "file":
            platform, url, sha, size, path, mode, unpack = f
            entry = {"url": url, "sha256": sha, "size": int(size), "path": path}
            if mode:
                entry["mode"] = mode
            entry["unpack"] = json.loads(unpack)
            builds.setdefault(platform, []).append(entry)
    pin = {
        "harness": "pi",
        "version": meta["version"],
        "tag": meta["tag"],
        "commit": meta["commit"],
        "published_at": meta["published_at"],
        "read_at": meta["read_at"],
        "verified": [
            f"pi {meta['tag']}: each Linux tarball's SHA-256 equals its line in the release's SHA256SUMS "
            "and the digest GitHub recorded at upload, its size equals GitHub's, and its pi/package.json "
            f"says {meta['version']}; it holds only regular files and directories under pi/, whose count "
            "and bytes are pinned for the unpacker",
            f"pi {meta['tag']}: no signature or attestation exists for these builds (GitHub's attestations "
            "API has none for either digest) and the release is not immutable, so these checksums are the "
            "strongest check its publisher offers",
            f"ripgrep {meta['rg_tag']}: each tarball's SHA-256 equals GitHub's digest and the release's "
            ".sha256 file, and the archive holds rg, a regular file whose SHA-256 and size are pinned",
            f"fd {meta['fd_tag']}: each tarball's SHA-256 equals GitHub's digest (fd publishes no checksum "
            "file), and the archive holds fd, a regular file whose SHA-256 and size are pinned",
        ],
        "base": meta["base"],
        "tools": [
            {"name": "ripgrep", "repo": meta["rg_repo"], "tag": meta["rg_tag"], "published_at": meta["rg_published"]},
            {"name": "fd", "repo": meta["fd_repo"], "tag": meta["fd_tag"], "published_at": meta["fd_published"]},
        ],
        "smoke": {"program": "pi", "args": ["--version"], "stdout": meta["version"]},
        "builds": [{"platform": p, "files": files} for p, files in builds.items()],
    }
    tmp = f"{out}.tmp.{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(pin, f, indent=2)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, out)
    except OSError as e:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        fail(f"could not write {out}: {e}")
else:
    fail(f"unknown command {cmd}")
EOF
py() { python3 -I -c "$PY" "$@"; }

# Big files go under ~/.cache, never /tmp: on kiln's Linux runners /tmp is a shared RAM disk.
mkdir -p "$HOME/.cache"
work=$(mktemp -d "$HOME/.cache/pin-harness-pi.XXXXXX")
trap 'rm -rf "$work"' EXIT

get() { # URL FILE
    curl -fsSL --proto-redir '=https' -o "$2" "$1" || die "could not fetch $1"
}
release() { # REPO TAG FILE: the release (the latest when TAG is empty); sets tag, published, immutable
    local path=releases/latest
    [[ -z $2 ]] || path=releases/tags/$2
    get "$api/repos/$1/$path" "$3"
    local r
    r=$(py release "$3" "$1")
    read -r tag published immutable <<<"$r"
    [[ -z $2 || $tag == "$2" ]] || die "$1: asked for $2, GitHub answered with $tag"
}
fetch() { # REPO TAG RELEASE NAME: downloads NAME, checked against GitHub's size and digest; sets sha, bytes
    local m
    m=$(py asset "$3" "$1" "$4")
    local want_size=${m% *} want_digest=${m#* }
    get "$dl/$1/releases/download/$2/$4" "$work/$4"
    bytes=$(wc -c <"$work/$4" | tr -d ' ')
    sha=$(sha256 "$work/$4")
    [[ $bytes == "$want_size" ]] || die "$1 $2: $4 is $bytes bytes; GitHub records $want_size"
    [[ sha256:$sha == "$want_digest" ]] || die "$1 $2: $4 has SHA-256 $sha; GitHub's digest is ${want_digest#sha256:}"
}
members() { # ARCHIVE: its member names, one a line, in $work/members
    tar -tzf "$1" >"$work/members" || die "$(basename "$1") is not a readable tar.gz"
}
facts=$work/facts
: >"$facts"
fact() { # one tab-separated line for py write
    local IFS=$'\t'
    printf '%s\n' "$*" >>"$facts"
}

# pi
release "$pi_repo" "${1:-}" "$work/pi.json"
pi_tag=$tag pi_published=$published
pi_version=${pi_tag#v}
[[ $pi_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$pi_repo: tag $pi_tag is not v<major>.<minor>.<patch>"
[[ $immutable == false ]] || die "$pi_repo $pi_tag is an immutable release, which GitHub attests: verify that attestation (gh release verify) and teach pin.sh to check it"
get "$api/repos/$pi_repo/git/ref/tags/$pi_tag" "$work/ref.json"
read -r kind commit <<<"$(py ref "$work/ref.json")"
if [[ $kind == tag ]]; then
    get "$api/repos/$pi_repo/git/tags/$commit" "$work/tag.json"
    read -r kind commit <<<"$(py ref "$work/tag.json")"
fi
[[ $kind == commit && $commit =~ ^[0-9a-f]{40}$ ]] || die "$pi_repo: tag $pi_tag leads to no commit"
fetch "$pi_repo" "$pi_tag" "$work/pi.json" SHA256SUMS
for a in x64:amd64 arm64:arm64; do
    name=pi-linux-${a%:*}.tar.gz
    fetch "$pi_repo" "$pi_tag" "$work/pi.json" "$name"
    listed=$(py sum "$work/SHA256SUMS" "$name")
    [[ $sha == "$listed" ]] || die "$pi_repo $pi_tag: $name has SHA-256 $sha; SHA256SUMS lists $listed"
    code=$(curl -sS -o /dev/null -w '%{http_code}' "$api/repos/$pi_repo/attestations/sha256:$sha") || die "could not ask GitHub whether $name is attested"
    case $code in
    404) ;;
    200) die "$pi_repo $pi_tag: GitHub holds an attestation for $name: verify it (gh attestation verify) and teach pin.sh to check it" ;;
    *) die "$pi_repo $pi_tag: GitHub's attestations API answered $code for $name, so whether it is attested is unknown" ;;
    esac
    t=$(py tree "$work/$name" pi)
    read -r entries unpacked <<<"$t"
    members "$work/$name"
    grep -qxF pi/pi "$work/members" || die "$pi_repo $pi_tag: $name holds no pi/pi"
    v=$(tar -xzOf "$work/$name" pi/package.json | py version) || die "$pi_repo $pi_tag: could not read $name's pi/package.json"
    [[ $v == "$pi_version" ]] || die "$pi_repo $pi_tag: $name's pi/package.json says $v"
    fact file "linux/${a#*:}" "$dl/$pi_repo/releases/download/$pi_tag/$name" "$sha" "$bytes" . "" \
        "{\"format\":\"tar.gz\",\"strip\":1,\"unpacked_bytes\":$unpacked,\"entries\":$entries}"
done

# ripgrep and fd: the <arch>-unknown-linux-musl builds pi's tools-manager downloads (research f17)
release "$rg_repo" "${2:-}" "$work/rg.json"
rg_tag=$tag rg_published=$published
release "$fd_repo" "${3:-}" "$work/fd.json"
fd_tag=$tag fd_published=$published
for a in x86_64:amd64 aarch64:arm64; do
    target=${a%:*}-unknown-linux-musl
    name=ripgrep-$rg_tag-$target.tar.gz
    fetch "$rg_repo" "$rg_tag" "$work/rg.json" "$name"
    rg_sha=$sha rg_bytes=$bytes
    fetch "$rg_repo" "$rg_tag" "$work/rg.json" "$name.sha256"
    listed=$(py sum "$work/$name.sha256" "$name")
    [[ $rg_sha == "$listed" ]] || die "$rg_repo $rg_tag: $name has SHA-256 $rg_sha; its .sha256 file says $listed"
    member=ripgrep-$rg_tag-$target/rg
    m=$(py member "$work/$name" "$member")
    read -r msha msize <<<"$m"
    fact file "linux/${a#*:}" "$dl/$rg_repo/releases/download/$rg_tag/$name" "$rg_sha" "$rg_bytes" bin/rg 0755 \
        "{\"format\":\"tar.gz\",\"member\":\"$member\",\"sha256\":\"$msha\",\"size\":$msize}"

    name=fd-v${fd_tag#v}-$target.tar.gz
    fetch "$fd_repo" "$fd_tag" "$work/fd.json" "$name"
    member=fd-v${fd_tag#v}-$target/fd
    m=$(py member "$work/$name" "$member")
    read -r msha msize <<<"$m"
    fact file "linux/${a#*:}" "$dl/$fd_repo/releases/download/$fd_tag/$name" "$sha" "$bytes" bin/fd 0755 \
        "{\"format\":\"tar.gz\",\"member\":\"$member\",\"sha256\":\"$msha\",\"size\":$msize}"
done

fact meta version "$pi_version"
fact meta tag "$pi_tag"
fact meta commit "$commit"
fact meta published_at "$pi_published"
fact meta read_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fact meta base "$base"
fact meta rg_repo "$rg_repo"
fact meta rg_tag "$rg_tag"
fact meta rg_published "$rg_published"
fact meta fd_repo "$fd_repo"
fact meta fd_tag "$fd_tag"
fact meta fd_published "$fd_published"
py write "$out" <"$facts"
printf 'pin.sh: pinned pi %s (ripgrep %s, fd %s) for linux/amd64 and linux/arm64 in %s\n' "$pi_tag" "$rg_tag" "$fd_tag" "$out"
