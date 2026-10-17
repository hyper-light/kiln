#!/usr/bin/env bash
# Regenerate ci/release/macos-stubs from what kiln actually imports.
#
# Run it when a macOS build fails to link with "undefined symbol" after a dependency change. It
# links once with empty stubs and `-undefined dynamic_lookup`, reads the symbols left to be looked
# up at load time, and files each one under the library that exports it. Needs llvm-objdump.
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD
stubs=$root/ci/release/macos-stubs
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cf=/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation
iokit=/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit
objc=/usr/lib/libobjc.A.dylib

tbd() { # file install-name [symbol ...]
  local file=$1 name=$2
  shift 2
  mkdir -p "$(dirname "$file")"
  {
    printf -- "--- !tapi-tbd\ntbd-version:     4\ntargets:         [ arm64-macos, x86_64-macos ]\ninstall-name:    '%s'\n" "$name"
    if [ $# -gt 0 ]; then
      printf "exports:\n  - targets:         [ arm64-macos, x86_64-macos ]\n    symbols:         [ %s ]\n" "$(printf '%s, ' "$@" | sed 's/, $//')"
    fi
    printf -- "...\n"
  } > "$file"
}

# Pass 1: empty stubs; whatever they don't provide is looked up at load time.
tbd "$work/stubs/Frameworks/CoreFoundation.framework/CoreFoundation.tbd" "$cf"
tbd "$work/stubs/Frameworks/IOKit.framework/IOKit.tbd" "$iokit"
tbd "$work/stubs/lib/libobjc.tbd" "$objc"
KILN_MACOS_STUBS=$work/stubs KILN_MACOS_LINK_ARGS="-C link-arg=-Wl,-undefined,dynamic_lookup" KILN_DIST=$work/dist \
  CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target} ci/release/build.sh aarch64-apple-darwin > "$work/build.log" 2>&1 \
  || { tail -30 "$work/build.log" >&2; exit 1; }

flat=$(llvm-objdump --macho --bind --lazy-bind --weak-bind "$work/dist/kiln-aarch64-apple-darwin" | awk '/flat-namespace/ {print $NF}' | sort -u)

# Pass 2: the real stubs.
mapfile -t cfs < <(grep -E '^_k?CF' <<< "$flat" || true)
mapfile -t ios < <(grep -E '^_k?IO' <<< "$flat" || true)
mapfile -t objcs < <(grep -E '^_(objc_|sel_|class_|OBJC_)' <<< "$flat" || true)
other=$(grep -vE '^_(k?CF|k?IO|objc_|sel_|class_|OBJC_)' <<< "$flat" || true)
tbd "$stubs/Frameworks/CoreFoundation.framework/CoreFoundation.tbd" "$cf" "${cfs[@]}"
tbd "$stubs/Frameworks/IOKit.framework/IOKit.tbd" "$iokit" "${ios[@]}"
tbd "$stubs/lib/libobjc.tbd" "$objc" "${objcs[@]}"
echo "CoreFoundation ${#cfs[@]}, IOKit ${#ios[@]}, libobjc ${#objcs[@]} symbols"
if [ -n "$other" ]; then
  echo "symbols from libraries without a stub yet (add one for their library):" >&2
  echo "$other" >&2
  exit 1
fi
