#!/usr/bin/env bash
# zig for a build BOLT will rewrite (goal 11, plan 01a1a87b2e56 step 5; ci/release/bolt.sh): every link zig makes
# keeps its relocations (--emit-relocs), which BOLT needs to move functions, and gets a build id, by which perf2bolt
# matches a profile to the binary whatever it was called when it ran.
#
# zig 0.17's cc refuses both (`unsupported linker arg: --emit-relocs`, and so -q, --symbol-ordering-file and
# -z keep-text-section-prefix). So a link runs as zig runs it, saying the ld.lld command it ran (ZIG_VERBOSE_LINK=1),
# and then again as that command with --emit-relocs and --build-id=sha1, by zig's own ld.lld: the same objects,
# libraries and options, into the same output. Every other zig command runs as it is.
#
# ci/release/build.sh gives it to cargo-zigbuild as CARGO_ZIGBUILD_ZIG_COMMAND, with the zig it wraps in
# KILN_ZIG_REAL. A link zig didn't run (its cache had the output) says so and keeps zig's output, without
# relocations; `bolt.sh check` refuses such a binary. zig prints the command as its words joined by spaces, so the
# build's paths (target dir, zig's cache, TMPDIR) must have none.
set -uo pipefail
real=${KILN_ZIG_REAL:?KILN_ZIG_REAL names the zig this wraps}

# Whether `zig <args>` is a cc that links: an output, and nothing that stops before linking.
links() {
  [ "${1:-}" = cc ] || return 1
  shift
  local a out=false
  for a in "$@"; do
    case $a in
      -c | -S | -E | -M | -MM | -fsyntax-only | --version | -v | '-###' | -print-* | --print-*) return 1 ;;
      -o | -o?*) out=true ;;
    esac
  done
  $out
}

links "$@" || exec "$real" "$@"

said=$(mktemp "${TMPDIR:-/tmp}/zig-relocs.XXXXXX") || exit 1
trap 'rm -f "$said"' EXIT
ZIG_VERBOSE_LINK=1 "$real" "$@" 2> "$said"
status=$?
# What zig said, but for the link command it was asked to print.
grep -v '^ld\.lld ' "$said" >&2
[ "$status" -eq 0 ] || exit "$status"
line=$(grep '^ld\.lld ' "$said" | tail -n 1)
if [ -z "$line" ]; then
  echo "zig-relocs: zig ran no link (its cache had the output): this output keeps no relocations" >&2
  exit 0
fi
read -r -a cmd <<< "$line"
"$real" "${cmd[@]}" --emit-relocs --build-id=sha1
