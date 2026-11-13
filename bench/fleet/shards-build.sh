#!/bin/sh
# shards-build.sh REV [OUT]: a Shards build of a known revision on this host, for the fleet harness
# (README, "A known Shards on each host").
#
# REV, a full commit SHA, is fetched alone from Shards' public repository and built as Shards'
# release workflow builds it (.github/workflows/release.yml at REV: `cargo build --locked --release
# -p shards`, `SHARDS_GIT_COMMIT` its first seven characters, so `shards --version` names it),
# since every measurement keeps `release` (Shards' CLAUDE.md). The GPU's renderer is left out
# (`SHARDS_GPU_LIBS` unset): such a `shards` carries no GPU VM process and refuses a GPU
# (crates/shards/build.rs), which no fleet run asks for. zig is the version REV's own
# scripts/install-zig pins, found on PATH or under ~/.local, else installed by that script into the
# build's own directory.
#
# The binary goes to OUT (default ~/.cache/kiln-fleet/shards/REV), copied beside it and renamed
# into place (a signed binary rewritten in place is killed at launch on macOS), with BUILD.txt: the
# revision, the host, the toolchain, its SHA-256 and what `--version` says. The build's own
# directory (SHARDS_BUILD_WORK, default ~/.cache/kiln-fleet/build-REV) is removed at the end,
# whatever happened. Runs under sh on Linux and macOS and under Git Bash on Windows.
set -eu

rev=${1:-}
case $rev in
"" | *[!0-9a-f]*) echo "usage: shards-build.sh FULL_SHA [OUT]: a full lowercase commit SHA" >&2; exit 2 ;;
esac
[ "${#rev}" -eq 40 ] || { echo "shards-build.sh: $rev is not a full 40-character SHA" >&2; exit 2; }
out=${2:-$HOME/.cache/kiln-fleet/shards/$rev}
work=${SHARDS_BUILD_WORK:-$HOME/.cache/kiln-fleet/build-$rev}
exe=
case $(uname -s) in MINGW* | MSYS* | CYGWIN*) exe=.exe ;; esac

[ ! -e "$work" ] || { echo "shards-build.sh: $work exists: each build takes a directory of its own" >&2; exit 1; }
mkdir -p "$work"
trap 'cd / && rm -rf "$work"' EXIT
began=$(date +%s)

git init -q "$work/src"
git -C "$work/src" fetch -q --depth 1 https://github.com/hyper-light/shards.git "$rev"
git -C "$work/src" checkout -q --detach FETCH_HEAD
got=$(git -C "$work/src" rev-parse HEAD)
[ "$got" = "$rev" ] || { echo "shards-build.sh: fetched $got, not $rev" >&2; exit 1; }

want=$(sed -n 's/^version=//p' "$work/src/scripts/install-zig")
have() { command -v zig >/dev/null 2>&1 && [ "$(zig version 2>/dev/null)" = "$want" ]; }
if ! have; then
	for d in "$HOME"/.local/zig-*"$want"*; do
		[ -x "$d/zig$exe" ] && PATH="$d:$PATH" && break
	done
fi
if ! have; then
	PATH="$(TMPDIR="$work" RUNNER_TEMP="$work" sh "$work/src/scripts/install-zig"):$PATH"
fi
have || { echo "shards-build.sh: no zig $want" >&2; exit 1; }
export PATH

cd "$work/src"
SHARDS_GIT_COMMIT=$(printf %.7s "$rev") CARGO_TARGET_DIR="$work/target" nice -n 10 cargo build --locked --release -p shards

mkdir -p "$out"
cp "$work/target/release/shards$exe" "$out/.shards$exe.part"
mv -f "$out/.shards$exe.part" "$out/shards$exe"
if command -v sha256sum >/dev/null 2>&1; then
	sum=$(sha256sum "$out/shards$exe" | cut -d' ' -f1)
else
	sum=$(shasum -a 256 "$out/shards$exe" | cut -d' ' -f1)
fi
{
	echo "revision $rev"
	echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ) in $(($(date +%s) - began)) s on $(uname -n) ($(uname -srm))"
	echo "toolchain $(rustc -V) (rust-toolchain.toml at the revision); zig $(zig version)"
	echo "profile release; SHARDS_GIT_COMMIT $(printf %.7s "$rev"); SHARDS_GPU_LIBS unset (no GPU VM process)"
	echo "sha256 $sum"
	echo "version $("$out/shards$exe" --version)"
} >"$out/BUILD.txt"
cat "$out/BUILD.txt"
