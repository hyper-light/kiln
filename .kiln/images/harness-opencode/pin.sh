#!/usr/bin/env bash
# Pin OpenCode 2's official Linux builds for kiln's built-in opencode Agentfile, or check a pin.
#
#   pin.sh [latest | X.Y.Z] [--out FILE] [--from DIR] [--keep]
#   pin.sh --check [FILE]
#
# Plan 01a1ce705e2f7773af54b299f1de42f5 (step 22 of plan 01a1cd8e), step 1. Each choice below
# cites a finding of its research, 01a1ce705e307593bd11f26bf9f7b9a9 (fN).
#
# pin.json (beside this script unless --out) is written only once every check has passed:
# - the version: OpenCode 2's update endpoint, the one its installer reads (f7). GitHub's
#   "latest" release is OpenCode 1 and is never read. 1.x is refused: kiln's ACP adapter needs
#   opencode/child-session-updates, which only OpenCode 2 has (f6).
# - each archive: its SHA256SUMS line; GitHub's digest and size for the asset; its Sigstore build
#   provenance, signed by the release workflow below at the commit and run the endpoint names
#   (gh attestation verify, f9); and one entry only, a regular file named opencode (f8).
# - LICENSE at the tag's commit: opencode is MIT, and its archives don't carry the notice (f29).
# --from DIR takes the archives from DIR instead of downloading them; every check still runs.
# --check downloads what a pin names and checks each file, member and attestation against it.
# Needs bash 3.2 or later, curl, python3, and gh signed in to GitHub (the attestation API).
# Downloads go to a new directory under ~/.cache/kiln-pins, removed on exit unless --keep.
set -euo pipefail

readonly REPO=anomalyco/opencode
readonly ENDPOINT=https://opencode.ai/update/api/latest/cli/npm
# The workflow that builds the release archives and attests their provenance (f9).
readonly SIGNER=anomalyco/opencode/.github/workflows/release.yml
# Steps 19 to 28's default base (step 19's research f29), recorded with the pin for its recipe.
readonly BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10
# The smoke an image's harness must pass, in pin.json for whatever runs it (step 19's image
# lookup, CI's legs), in the shape every harness's pin gives it, its program, args and stdout:
# `opencode --version` prints `opencode v<version>` (f32). Every opencode command first creates
# its directories under HOME (f18), so the smoke runs where HOME is writable, as a harness's is in
# its microVM (HOME=/tmp, its scratch, f16).
readonly SMOKE_PROGRAM=bin/opencode SMOKE_ARGS=--version SMOKE_PREFIX="opencode v"
# Platform, variant and release asset of each build. amd64 takes the baseline build, made for
# x86-64 CPUs without AVX2; in 2.2.7 it is the same bytes as x64 (f11).
readonly BUILDS="linux/amd64 gnu opencode-linux-x64-baseline.tar.gz
linux/amd64 musl opencode-linux-x64-baseline-musl.tar.gz
linux/arm64 gnu opencode-linux-arm64.tar.gz
linux/arm64 musl opencode-linux-arm64-musl.tar.gz"

fail() {
    printf 'pin.sh: FAILED %s: %s\n' "$1" "$2" >&2
    exit 1
}

say() { printf '%s\n' "$*"; }

# fetch URL FILE: the body of an HTTPS URL.
fetch() {
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --output "$2" "$1" || fail download "$1"
}

# sha FILE, bytes FILE: a file's SHA-256 and size.
sha() {
    python3 -I -c 'import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], "rb") as f:
    for b in iter(lambda: f.read(1 << 20), b""):
        h.update(b)
print(h.hexdigest())' "$1"
}

bytes() { python3 -I -c 'import os, sys; print(os.path.getsize(sys.argv[1]))' "$1"; }

# check_version V: X.Y.Z of OpenCode 2 or later.
check_version() {
    python3 -I -c 'import re, sys
m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", sys.argv[1])
if not m:
    sys.exit("not X.Y.Z")
if int(m.group(1)) < 2:
    sys.exit("OpenCode 1 has no opencode/child-session-updates")' "$1" 2>"$WORK/version.err" ||
        fail version "$1: $(cat "$WORK/version.err")"
}

# endpoint_fields FILE: the version, commit, run id and ref the update endpoint names.
endpoint_fields() {
    python3 -I -c 'import json, re, sys
d = json.load(open(sys.argv[1]))
g = (d.get("metadata") or {}).get("github") or {}
v, sha, run, ref = d.get("version", ""), g.get("sha", ""), str(g.get("run_id", "")), g.get("ref", "")
if not (re.fullmatch(r"\d+\.\d+\.\d+", v) and re.fullmatch(r"[0-9a-f]{40}", sha) and run.isdigit() and ref):
    sys.exit("unexpected answer")
print(v, sha, run, ref)' "$1" || fail endpoint "$ENDPOINT gave no version, commit, run and ref"
}

# check_sums SUMS NAME FILE: FILE's SHA-256 is NAME's one line in SHA256SUMS.
check_sums() {
    local got
    got=$(sha "$3")
    python3 -I -c 'import sys
lines = [l.split() for l in open(sys.argv[1]) if l.split()[1:2] == [sys.argv[2]]]
if len(lines) != 1:
    sys.exit("%d lines for %s" % (len(lines), sys.argv[2]))
if lines[0][0] != sys.argv[3]:
    sys.exit("listed %s, got %s" % (lines[0][0], sys.argv[3]))' "$1" "$2" "$got" 2>"$WORK/sums.err" ||
        fail sha256sums "$2: $(cat "$WORK/sums.err")"
}

# check_asset RELEASE NAME FILE: FILE has the SHA-256 and size GitHub gives the release's asset.
check_asset() {
    local got size
    got=$(sha "$3")
    size=$(bytes "$3")
    python3 -I -c 'import json, sys
assets = [a for a in json.load(open(sys.argv[1])).get("assets", []) if a.get("name") == sys.argv[2]]
if len(assets) != 1:
    sys.exit("no asset %s" % sys.argv[2])
a = assets[0]
if a.get("digest") != "sha256:" + sys.argv[3] or a.get("size") != int(sys.argv[4]):
    sys.exit("GitHub has %s, %s bytes; got sha256:%s, %s bytes" % (a.get("digest"), a.get("size"), sys.argv[3], sys.argv[4]))' \
        "$1" "$2" "$got" "$size" 2>"$WORK/asset.err" || fail github-digest "$2: $(cat "$WORK/asset.err")"
}

# check_attestation FILE REPO WORKFLOW COMMIT RUN: FILE's build provenance is signed by WORKFLOW in
# REPO, from COMMIT in run RUN (either may be empty: any). Prints its certificate's SAN, source
# commit, source ref and run URI.
check_attestation() {
    local name
    name=$(basename "$1")
    gh attestation verify "$1" --repo "$2" --signer-workflow "$3" --format json \
        >"$WORK/attestation.json" 2>"$WORK/attestation.err" ||
        fail attestation "$name: no provenance signed by $3 in $2 ($(head -c 300 "$WORK/attestation.err" | tr '\n' ' '))"
    python3 -I -c 'import json, sys
want_commit, want_run = sys.argv[2], sys.argv[3]
for r in json.load(open(sys.argv[1])):
    c = r["verificationResult"]["signature"]["certificate"]
    run_uri = c.get("runInvocationURI", "")
    if want_commit and c.get("sourceRepositoryDigest") != want_commit:
        continue
    if want_run and "/actions/runs/%s/" % want_run not in run_uri:
        continue
    print(c["subjectAlternativeName"], c["sourceRepositoryDigest"], c.get("sourceRepositoryRef", ""), run_uri)
    sys.exit(0)
sys.exit("signed, but not from commit %s in run %s" % (want_commit or "any", want_run or "any"))' \
        "$WORK/attestation.json" "$4" "$5" 2>"$WORK/attestation.err" ||
        fail attestation "$name: $(cat "$WORK/attestation.err")"
}

# check_member FILE: FILE is a gzip tarball of one entry, a regular file named opencode. Prints
# that file's SHA-256 and size. It is read as a stream, never written out.
check_member() {
    python3 -I -c 'import hashlib, sys, tarfile
try:
    t = tarfile.open(sys.argv[1], "r:gz")
    members = t.getmembers()
except (tarfile.TarError, OSError, EOFError) as e:
    sys.exit("not a gzip tarball: %s" % e)
if len(members) != 1:
    sys.exit("%d entries (%s), not one" % (len(members), ", ".join(m.name for m in members)))
m = members[0]
if m.name != "opencode":
    sys.exit("its entry is %r, not opencode" % m.name)
if not m.isreg():
    sys.exit("opencode is not a regular file")
f = t.extractfile(m)
h, n = hashlib.sha256(), 0
for b in iter(lambda: f.read(1 << 20), b""):
    h.update(b)
    n += len(b)
if n != m.size:
    sys.exit("opencode is %d bytes of %d" % (n, m.size))
print(h.hexdigest(), n)' "$1" 2>"$WORK/member.err" || fail member "$(basename "$1"): $(cat "$WORK/member.err")"
}

# pin VERSION_ARG OUT FROM: every check, then OUT.
pin() {
    local arg=$1 out=$2 from=$3
    fetch "$ENDPOINT" "$WORK/endpoint.json"
    local fields e_version e_commit e_run e_ref
    fields=$(endpoint_fields "$WORK/endpoint.json") || exit 1
    read -r e_version e_commit e_run e_ref <<<"$fields"
    local version want_commit= want_run=
    if [ "$arg" = latest ]; then version=$e_version; else version=$arg; fi
    check_version "$version"
    # The endpoint names one release's commit and run: the latest's.
    if [ "$version" = "$e_version" ]; then want_commit=$e_commit want_run=$e_run; fi
    say "endpoint $ENDPOINT: version $e_version, commit $e_commit, run $e_run, $e_ref"
    say "pinning $version"

    gh api "repos/$REPO/releases/tags/v$version" >"$WORK/release.json" 2>"$WORK/gh.err" ||
        fail release "v$version: $(head -c 200 "$WORK/gh.err")"
    local tag_commit
    tag_commit=$(gh api "repos/$REPO/commits/v$version" --jq .sha 2>"$WORK/gh.err") ||
        fail release "the commit of tag v$version: $(head -c 200 "$WORK/gh.err")"
    local releases=https://github.com/$REPO/releases/download/v$version
    fetch "$releases/SHA256SUMS" "$WORK/SHA256SUMS"
    say "tag v$version: commit $tag_commit; SHA256SUMS sha256 $(sha "$WORK/SHA256SUMS")"

    : >"$WORK/builds.tsv"
    local platform variant asset file sum size m_sum m_size san commit ref run_uri att member
    while read -r platform variant asset; do
        if [ -n "$from" ]; then
            file=$from/$asset
            [ -f "$file" ] || fail download "$asset is not in $from"
        else
            file=$WORK/$asset
            fetch "$releases/$asset" "$file"
        fi
        check_sums "$WORK/SHA256SUMS" "$asset" "$file"
        check_asset "$WORK/release.json" "$asset" "$file"
        att=$(check_attestation "$file" "$REPO" "$SIGNER" "$want_commit" "$want_run") || exit 1
        read -r san commit ref run_uri <<<"$att"
        member=$(check_member "$file") || exit 1
        read -r m_sum m_size <<<"$member"
        sum=$(sha "$file")
        size=$(bytes "$file")
        # One release run builds all four: each attests the same commit and run (f9).
        want_commit=$commit want_run=${run_uri#*/actions/runs/}
        want_run=${want_run%%/*}
        say "$asset: $size bytes, sha256 $sum; SHA256SUMS, GitHub digest, provenance ($san, $commit, run $want_run) ok; opencode $m_size bytes, sha256 $m_sum"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$platform" "$variant" "$releases/$asset" "$sum" "$size" "$m_sum" "$m_size" >>"$WORK/builds.tsv"
    done <<<"$BUILDS"

    local license=https://raw.githubusercontent.com/$REPO/$tag_commit/LICENSE
    fetch "$license" "$WORK/LICENSE"
    [ "$(head -n 1 "$WORK/LICENSE")" = "MIT License" ] || fail license "$license is no longer the MIT License"
    say "LICENSE at $tag_commit: MIT, $(bytes "$WORK/LICENSE") bytes, sha256 $(sha "$WORK/LICENSE")"

    python3 -I -c 'import hashlib, json, sys, time
(work, version, tag_commit, signer, repo, base, license_url, san, commit, ref, run_uri, endpoint,
 smoke_program, smoke_args, smoke_prefix) = sys.argv[1:16]
sums = {}
for line in open(work + "/SHA256SUMS"):
    f = line.split()
    if len(f) == 2:
        sums[f[1]] = f[0]
def same(a, b):
    return sums.get(a) is not None and sums.get(a) == sums.get(b)
data = open(work + "/LICENSE", "rb").read()
lic = {"url": license_url, "sha256": hashlib.sha256(data).hexdigest(), "size": len(data), "path": "LICENSE", "mode": "0644"}
builds = []
for row in open(work + "/builds.tsv"):
    platform, variant, url, sha, size, m_sha, m_size = row.rstrip("\n").split("\t")
    builds.append({"platform": platform, "variant": variant, "files": [
        {"url": url, "sha256": sha, "size": int(size), "path": "bin/opencode", "mode": "0755",
         "unpack": {"format": "tar.gz", "member": "opencode", "sha256": m_sha, "size": int(m_size)}},
        dict(lic)]})
pin = {
    "harness": "opencode",
    "version": version,
    "read": {"from": endpoint, "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
             "answer": json.load(open(work + "/endpoint.json"))},
    "tag": {"name": "v" + version, "commit": tag_commit},
    "verified": {
        "sha256sums": {"url": "https://github.com/%s/releases/download/v%s/SHA256SUMS" % (repo, version),
                       "sha256": hashlib.sha256(open(work + "/SHA256SUMS", "rb").read()).hexdigest()},
        "github_digest": True,
        "provenance": {"repo": repo, "signer_workflow": signer, "san": san, "source_commit": commit,
                       "source_ref": ref, "run": run_uri, "predicate": "https://slsa.dev/provenance/v1"},
        "x64_is_baseline": {"gnu": same("opencode-linux-x64.tar.gz", "opencode-linux-x64-baseline.tar.gz"),
                            "musl": same("opencode-linux-x64-musl.tar.gz", "opencode-linux-x64-baseline-musl.tar.gz")},
    },
    "base": base,
    "smoke": {"program": smoke_program, "args": smoke_args.split(), "stdout": smoke_prefix + version},
    "builds": builds,
}
json.dump(pin, open(work + "/pin.json", "w"), indent=2)
open(work + "/pin.json", "a").write("\n")' \
        "$WORK" "$version" "$tag_commit" "$SIGNER" "$REPO" "$BASE" "$license" "$san" "$commit" "$ref" "$run_uri" "$ENDPOINT" \
        "$SMOKE_PROGRAM" "$SMOKE_ARGS" "$SMOKE_PREFIX" ||
        fail write "pin.json"
    say "x64 is baseline: $(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["verified"]["x64_is_baseline"])' "$WORK/pin.json")"
    cp "$WORK/pin.json" "$out.tmp.$$" && mv -f "$out.tmp.$$" "$out" || fail write "$out"
    say "wrote $out"
}

# check FILE: download what the pin names and check each file, its member and its provenance.
check() {
    local pinfile=$1
    python3 -I -c 'import json, sys
p = json.load(open(sys.argv[1]))
v = p["verified"]["provenance"]
print(v["source_commit"], v["run"].split("/actions/runs/")[1].split("/")[0], len(p["builds"]))
for b in p["builds"]:
    for f in b["files"]:
        u = f.get("unpack") or {}
        print(b["platform"], b["variant"], f["url"], f["sha256"], f["size"], u.get("sha256", "-"), u.get("size", "-"))' \
        "$pinfile" >"$WORK/check.txt" || fail check "$pinfile is no pin"
    local commit run total ok=0 platform variant url want_sum want_size want_m_sum want_m_size n=0
    read -r commit run total <"$WORK/check.txt"
    local last=
    while read -r platform variant url want_sum want_size want_m_sum want_m_size; do
        n=$((n + 1))
        local file=$WORK/check-$n
        fetch "$url" "$file"
        [ "$(sha "$file")" = "$want_sum" ] && [ "$(bytes "$file")" = "$want_size" ] ||
            fail check "$url: not the pinned bytes"
        if [ "$want_m_sum" != - ]; then
            local member m_sum m_size
            member=$(check_member "$file") || exit 1
            read -r m_sum m_size <<<"$member"
            [ "$m_sum" = "$want_m_sum" ] && [ "$m_size" = "$want_m_size" ] || fail check "$url: not the pinned member"
            check_attestation "$file" "$REPO" "$SIGNER" "$commit" "$run" >/dev/null
            ok=$((ok + 1))
            say "$platform $variant: $(basename "$url") and its opencode match the pin; provenance $commit, run $run"
        fi
        rm -f "$file"
    done < <(tail -n +2 "$WORK/check.txt")
    say "CHECKED $ok of $total builds"
    [ "$ok" = "$total" ] || fail check "$ok of $total builds"
}

main() {
    local arg=latest out= from= keep= mode=pin
    while [ $# -gt 0 ]; do
        case "$1" in
        --out) out=${2:?--out FILE}; shift 2 ;;
        --from) from=${2:?--from DIR}; shift 2 ;;
        --keep) keep=1; shift ;;
        --check) mode=check; shift ;;
        -h | --help) sed -n '2,6p' "$0"; return 0 ;;
        -*) fail usage "unknown option $1" ;;
        *) arg=$1; shift ;;
        esac
    done
    local here
    here=$(cd "$(dirname "$0")" && pwd)
    mkdir -p "$HOME/.cache/kiln-pins"
    WORK=$(mktemp -d "$HOME/.cache/kiln-pins/opencode.XXXXXX")
    if [ -n "$keep" ]; then say "keeping $WORK"; else trap 'rm -rf "$WORK"' EXIT; fi
    say "pin.sh $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(uname -sm), gh $(gh --version | head -n 1 | cut -d' ' -f3)"
    if [ "$mode" = check ]; then
        if [ "$arg" = latest ]; then arg=$here/pin.json; fi
        check "$arg"
    else
        pin "$arg" "${out:-$here/pin.json}" "$from"
    fi
}

# Sourced (by pin_test.sh), it only defines the checks.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
