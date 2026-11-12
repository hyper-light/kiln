#!/usr/bin/env bash
# Plan step 20 [kiln], step 1's mutation tests (kiln plan 01a1ce705d7076648c1d9f9ee26214d3). Each
# case copies a directory `pin.sh fetch` filled (its files hard links; a mutated file is written anew,
# so the original stays as fetched), changes it one way, and runs `pin.sh verify` with a sentinel
# pin.json as its output. A mutation passes when verify exits non-zero, its refusal names the check the
# case aims at (a pattern), the sentinel is unchanged and no temporary file is left beside it; the
# unchanged copy (the control) must pin. Some cases also forge GitHub's metadata and SHA256SUMS, so
# that only a deeper check can refuse them.
#
# verify reads a package only after cosign has verified it, and no crafted archive carries OpenAI's
# signature, so cosign refuses every crafted package first. Each is therefore also given to
# `pin.sh inspect`, the archive checks verify runs next, which must refuse it by name.
#
#   pin-mutations.sh FETCHED_DIR PIN_SH
set -uo pipefail

src=${1:?usage: pin-mutations.sh FETCHED_DIR PIN_SH}
pin=${2:?usage: pin-mutations.sh FETCHED_DIR PIN_SH}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
work=$(mktemp -d "$cache/s20-pin-mutations.XXXXXX")
trap 'rm -rf "$work"' EXIT
x86=codex-package-x86_64-unknown-linux-musl.tar.gz
arm=codex-package-aarch64-unknown-linux-musl.tar.gz
rgv=$(jq -r .tag_name "$src/ripgrep-release.json")
rgarm=ripgrep-$rgv-aarch64-unknown-linux-musl.tar.gz
tag=$(jq -r .tag_name "$src/release.json")
version=${tag#rust-v}
workflow='https://github\.com/openai/codex/\.github/workflows/rust-release\.yml'
payload='cosign refuses it .*matching bundle to payload'
passed=0
failed=0

sha() { sha256sum "$1" | cut -d' ' -f1; }
# A copy of the fetched directory: its files hard links, its TUF cache its own.
copy() {
    local d=$work/$1 f
    mkdir -p "$d"
    for f in "$src"/*; do
        case "$(basename "$f")" in
            tuf) cp -a "$f" "$d/" ;;
            *.log | inspect.err) ;;
            *) ln "$f" "$d/" ;;
        esac
    done
    printf '%s\n' "$d"
}
# Write FILE anew from stdin, breaking its hard link.
rewrite() { cat >"$1.new" && mv -f "$1.new" "$1"; }
# Release metadata (FILE.json) says NAME has FROM's digest and size.
forge_asset() {
    local json=$1 name=$2 from=$3
    jq --arg n "$name" --arg d "sha256:$(sha "$from")" --argjson s "$(stat -c %s "$from")" \
        '(.assets[] | select(.name == $n)) |= (.digest = $d | .size = $s)' "$json" | rewrite "$json"
}
# codex-package_SHA256SUMS lists FROM's digest for NAME, and GitHub's metadata the new list's.
forge_sums() {
    local d=$1 name=$2 from=$3 h
    h=$(sha "$from")
    awk -v n="$name" -v h="$h" '$2 == n {print h "  " $2; next} {print}' "$d/codex-package_SHA256SUMS" |
        rewrite "$d/codex-package_SHA256SUMS"
    forge_asset "$d/release.json" codex-package_SHA256SUMS "$d/codex-package_SHA256SUMS"
}
# A small tar.gz: a valid codex-package.json (unless KIND changes it) and one hostile member; or, for
# ripgrep's kinds, a top directory TARGET without rg, or with a second top directory beside it.
craft() {
    python3 -I - "$@" <<'PY'
import io, json, sys, tarfile
out, kind, version, target = sys.argv[1:5]
pkg = {"layoutVersion": 1, "version": version, "target": target, "entrypoint": "bin/codex"}
if kind == "version":
    pkg["version"] = "0.0.1"
if kind == "target":
    pkg["target"] = "aarch64-unknown-linux-musl"
if kind == "layout":
    pkg["layoutVersion"] = 2

def add(t, name, data=b"", mode=0o644, typ=tarfile.REGTYPE, link=""):
    ti = tarfile.TarInfo(name)
    ti.type, ti.mode, ti.linkname = typ, mode, link
    ti.size = len(data) if typ == tarfile.REGTYPE else 0
    t.addfile(ti, io.BytesIO(data) if typ == tarfile.REGTYPE else None)

with tarfile.open(out, "w:gz", format=tarfile.PAX_FORMAT) as t:
    if kind == "no-rg":
        add(t, f"{target}/README.md", b"no rg here\n")
    elif kind == "two-tops":
        add(t, f"{target}/README.md", b"one top directory\n")
        add(t, "another/README.md", b"and another\n")
    else:
        add(t, "codex-package.json", json.dumps(pkg).encode())
    if kind == "symlink":
        add(t, "bin/codex", typ=tarfile.SYMTYPE, link="/bin/sh", mode=0o777)
    if kind == "hard-link":
        add(t, "bin/codex", typ=tarfile.LNKTYPE, link="codex-package.json", mode=0o755)
    if kind == "absolute":
        add(t, "/etc/codex-pin-test", b"x\n")
    if kind == "climbing":
        add(t, "../codex-pin-test", b"x\n")
    if kind == "set-id":
        add(t, "bin/codex", b"\x7fELF", mode=0o4755)
PY
}
# Put a crafted archive of KIND in the x86_64 slot, its digests forged so only cosign and the archive
# checks remain.
crafted() {
    local d=$1 kind=$2
    craft "$d/$x86.new" "$kind" "$version" x86_64-unknown-linux-musl && mv -f "$d/$x86.new" "$d/$x86"
    forge_asset "$d/release.json" "$x86" "$d/$x86"
    forge_sums "$d" "$x86" "$d/$x86"
}
# Put a crafted ripgrep archive of KIND in arm64's ripgrep slot, GitHub's digest and ripgrep's .sha256
# forged to match it.
crafted_rg() {
    local d=$1 kind=$2
    craft "$d/$rgarm.new" "$kind" "$rgv" "${rgarm%.tar.gz}" && mv -f "$d/$rgarm.new" "$d/$rgarm"
    forge_asset "$d/ripgrep-release.json" "$rgarm" "$d/$rgarm"
    printf '%s  %s\n' "$(sha "$d/$rgarm")" "$rgarm" | rewrite "$d/$rgarm.sha256"
}
# Run verify on D. WANT is pin (the control) or an extended regular expression its refusal must match.
# With IPAT, D's x86_64 archive is also given to `pin.sh inspect`, whose refusal must match IPAT.
check() {
    local label=$1 d=$2 want=$3 ipat=${4:-} out before after left rc irc=- verdict=FAIL why
    out=$d/out
    mkdir -p "$out"
    printf 'sentinel\n' >"$out/pin.json"
    before=$(sha "$out/pin.json")
    "$pin" verify "$d" "$out/pin.json" >"$d/verify.out" 2>"$d/verify.err"
    rc=$?
    after=$(sha "$out/pin.json")
    left=$(find "$out" -name '.pin.json.*' | wc -l)
    if [ "$want" = pin ]; then
        [ "$rc" -eq 0 ] && [ "$before" != "$after" ] && [ "$left" -eq 0 ] &&
            jq -e '(.builds | map(.platform)) == ["linux/amd64", "linux/arm64"]
                and ([.builds[].files | length] == [1, 2])
                and .builds[0].files[0].unpack == {format: "tar.gz", strip: 0}
                and .builds[1].files[0].unpack.skip == ["codex-path/rg"]
                and .builds[1].files[1].path == "codex-path/rg"' "$out/pin.json" >/dev/null && verdict=PASS
        why=$(tail -1 "$d/verify.out")
    else
        [ "$rc" -ne 0 ] && [ "$before" = "$after" ] && [ "$left" -eq 0 ] && grep -Eq -- "$want" "$d/verify.err" && verdict=PASS
        why=$(tail -1 "$d/verify.err")
        if [ -n "$ipat" ]; then
            "$pin" inspect package "$d/$x86" x86_64 "$version" x86_64-unknown-linux-musl >"$d/inspect.out" 2>"$d/inspect.err"
            irc=$?
            { [ "$irc" -ne 0 ] && [ ! -s "$d/inspect.out" ] && grep -Eq -- "$ipat" "$d/inspect.err"; } || verdict=FAIL
            why="$why | inspect: $(tail -1 "$d/inspect.err")"
        fi
    fi
    printf 'MUTATION %-34s exit=%s pin.json=%s temp=%s inspect=%s %s :: %s\n' "$label" "$rc" \
        "$([ "$before" = "$after" ] && echo unchanged || echo written)" "$left" "$irc" "$verdict" "$why"
    if [ "$verdict" = PASS ]; then passed=$((passed + 1)); else failed=$((failed + 1)); fi
}

d=$(copy control)
check control "$d" pin

d=$(copy tag)
jq '.tag_name = "rust-v0.167.1"' "$d/release.json" | rewrite "$d/release.json"
check "another tag's identity" "$d" "cosign refuses it for $workflow@refs/tags/rust-v0\.167\.1: .*([Ii]dentit|SAN|[Ss]ubject)"

d=$(copy bundle)
cat "$src/$arm.sigstore" | rewrite "$d/$x86.sigstore"
forge_asset "$d/release.json" "$x86.sigstore" "$d/$x86.sigstore"
check "the other platform's bundle" "$d" "$payload"

d=$(copy flipped)
python3 -I -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[4] ^= 0xFF; open(sys.argv[2], "wb").write(b)' "$src/$x86" "$d/$x86.new"
mv -f "$d/$x86.new" "$d/$x86"
check "one flipped byte" "$d" "$x86: sha256 [0-9a-f]{64}, GitHub says"

d=$(copy flipped-forged)
python3 -I -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[4] ^= 0xFF; open(sys.argv[2], "wb").write(b)' "$src/$x86" "$d/$x86.new"
mv -f "$d/$x86.new" "$d/$x86"
forge_asset "$d/release.json" "$x86" "$d/$x86"
forge_sums "$d" "$x86" "$d/$x86"
check "one flipped byte, digests forged" "$d" "$payload"

d=$(copy sums)
awk -v n="$x86" -v h="$(sha "$src/$arm")" '$2 == n {print h "  " $2; next} {print}' "$src/codex-package_SHA256SUMS" |
    rewrite "$d/codex-package_SHA256SUMS"
check "a SHA256SUMS line changed" "$d" "codex-package_SHA256SUMS: sha256 [0-9a-f]{64}, GitHub says"

d=$(copy sums-forged)
awk -v n="$x86" -v h="$(sha "$src/$arm")" '$2 == n {print h "  " $2; next} {print}' "$src/codex-package_SHA256SUMS" |
    rewrite "$d/codex-package_SHA256SUMS"
forge_asset "$d/release.json" codex-package_SHA256SUMS "$d/codex-package_SHA256SUMS"
check "a SHA256SUMS line changed, forged" "$d" "$x86: sha256 [0-9a-f]{64}, codex-package_SHA256SUMS lists"

for c in "symlink|member 'bin/codex' is not a regular file or a directory" \
    "hard-link|member 'bin/codex' is not a regular file or a directory" \
    "absolute|member '/etc/codex-pin-test' leaves the archive" \
    "climbing|member '\.\./codex-pin-test' leaves the archive" \
    "set-id|member 'bin/codex' has a set-ID bit" \
    "version|codex-package\.json says version '0\.0\.1', not '$version'" \
    "target|codex-package\.json says target 'aarch64-unknown-linux-musl', not 'x86_64-unknown-linux-musl'" \
    "layout|codex-package\.json says layoutVersion 2, not 1"; do
    kind=${c%%|*}
    d=$(copy "crafted-$kind")
    crafted "$d" "$kind"
    check "crafted: $kind" "$d" "$payload" "${c#*|}"
done

d=$(copy one-platform)
jq --arg a "$arm" 'del(.assets[] | select(.name == $a or .name == ($a + ".sigstore")))' "$d/release.json" | rewrite "$d/release.json"
check "a release with one platform" "$d" "release $tag has no $arm \(linux/arm64\)"

d=$(copy rg-sha256)
printf '%s  %s\n' "$(sha "$src/$x86")" "$rgarm" | rewrite "$d/$rgarm.sha256"
check "ripgrep's .sha256 changed" "$d" "$rgarm: sha256 [0-9a-f]{64}, ripgrep's \.sha256 says"

d=$(copy rg-github)
jq --arg n "$rgarm" '(.assets[] | select(.name == $n)).digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000"' \
    "$d/ripgrep-release.json" | rewrite "$d/ripgrep-release.json"
check "ripgrep's GitHub digest changed" "$d" "$rgarm: sha256 [0-9a-f]{64}, GitHub says 0{64}"

d=$(copy rg-no-rg)
crafted_rg "$d" no-rg
check "a ripgrep archive with no rg" "$d" "$rgarm: no regular file ${rgarm%.tar.gz}/rg in the archive"

d=$(copy rg-two-tops)
crafted_rg "$d" two-tops
check "a ripgrep archive, two top dirs" "$d" "$rgarm: member 'another/README\.md' is outside ${rgarm%.tar.gz}/"

printf 'MUTATIONS passed=%s failed=%s\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
