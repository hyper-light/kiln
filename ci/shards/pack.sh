#!/usr/bin/env bash
# Builds the Shards kiln carries (`crate::shards`, docs/shards.md; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16)
# from vendor/shards, for one target (this machine's by default), into target/shards-carried/<target>/ (or
# KILN_SHARDS_DIR), where kiln's build.rs embeds them:
# - `shards`, built to be carried (SHARDS_HELPERS=beside, Shards' D237): the guests it runs (shards-init and the
#   in-VM server) inside it, built by its own build script, whose C zig compiles (scripts/install-zig installs the
#   zig Shards pins where none is on PATH);
# - its VM and network processes (shards-vm-process, shards-net-process), as Shards' build.rs builds its own: on
#   Linux static (musl) whatever C library `shards` links, linked by rust-lld (vendor/shards/.cargo/config.toml),
#   their C (the network process's AWS-LC, Shards' D242) compiled for musl as that build script compiles it.
# Built in vendor/shards, so Shards' toolchain file applies there: its Rust, and the musl targets its guests need.
# KILN_SHARDS_PROFILE is the cargo profile: release by default, as kiln's releases; test-release for a test build.
# Its cargo target directory is target/shards (or $CARGO_TARGET_DIR/shards), apart from kiln's.
#
#   ci/shards/pack.sh [target]
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD
host=$(rustc -vV | sed -n 's/^host: //p')
target=${1:-$host}
profile=${KILN_SHARDS_PROFILE:-release}
case $profile in dev) dir_name=debug ;; *) dir_name=$profile ;; esac
out=${KILN_SHARDS_DIR:-$root/target/shards-carried/$target}
built=${CARGO_TARGET_DIR:-$root/target}/shards
case $target in
  *-unknown-linux-gnu) helpers=${target%-unknown-linux-gnu}-unknown-linux-musl ;;
  *) helpers=$target ;;
esac
exe=""
case $target in *-windows-*) exe=.exe ;; esac

# The zig Shards pins (its scripts/zig-pin, which its scripts/install-zig installs): one compiler for the guests' C wherever they're built, so their
# bytes, which name a microVM's template, are the same everywhere (Shards' crates/shards/build.rs).
zig_version=$(sed -n 's/^zig_version=//p' vendor/shards/scripts/zig-pin)
# install-zig keeps it where zig-pin says (its user's cache), checked by its digest, and prints its directory.
if [ "$(zig version 2>/dev/null)" != "$zig_version" ]; then
  zig_bin=$(sh vendor/shards/scripts/install-zig)
  export PATH="$zig_bin:$PATH"
fi
echo "pack: shards for $target (its helpers for $helpers), profile $profile, with $(zig version 2>/dev/null | sed 's/^/zig /')"

# The helpers' C for a musl target, as Shards' build.rs compiles it for the helpers it builds itself (its `zig_c`): by
# the pinned zig on every host, through Shards' scripts/zig-cc and zig-ar with ZIG_TARGET, unless CC_<target> names
# a compiler already. Without it cc-rs looks for <arch>-linux-musl-gcc, which a glibc host has none of, and
# aws-lc-sys's build stops (Ubuntu 24.04 in a container on adacachy, 2026-11-18: `failed to find tool
# "x86_64-linux-musl-gcc"`).
helper_c=()
case $helpers in
  *-unknown-linux-musl)
    var=$(printf %s "$helpers" | tr - _)
    cc_var=CC_$var
    if [ -z "${!cc_var+set}" ]; then
      helper_c=("CC_$var=$root/vendor/shards/scripts/zig-cc" "AR_$var=$root/vendor/shards/scripts/zig-ar"
        "ZIG_TARGET=${helpers%%-*}-linux-musl")
    fi
    ;;
esac
# `shards` itself compiles C too (AWS-LC, through aws-lc-rs): built for a musl target (a release's Linux binary,
# static as kiln's is), by the same compiler as its helpers.
shards_c=()
if [ "$target" = "$helpers" ]; then
  shards_c=(${helper_c[@]+"${helper_c[@]}"})
fi

# NASM for Windows x64 where none is on PATH: AWS-LC is assembled from source there (vendor/shards/.cargo/config.toml,
# AWS_LC_SYS_PREBUILT_NASM), which takes NASM. Its official 3.02 build, checked by its digest, as Shards' own CI pins it
# (its .github/workflows/release.yml), kept in this user's cache.
case $target in
  x86_64-pc-windows-msvc)
    if ! command -v nasm >/dev/null 2>&1; then
      nasm_home=$HOME/.cache/kiln-nasm
      if [ ! -f "$nasm_home/nasm-3.02/nasm.exe" ]; then
        mkdir -p "$nasm_home"
        curl -fsSL --retry 5 --retry-all-errors --retry-delay 10 -o "$nasm_home/nasm.zip.part" \
          https://www.nasm.us/pub/nasm/releasebuilds/3.02/win64/nasm-3.02-win64.zip
        echo "161d0bfaff53c2f9e9f3e69fd0672323ebabafd1268976a5cec11be92a19aee7  $nasm_home/nasm.zip.part" | sha256sum -c --quiet
        rm -rf "$nasm_home/nasm-3.02"
        unzip -q "$nasm_home/nasm.zip.part" -d "$nasm_home"
        rm -f "$nasm_home/nasm.zip.part"
      fi
      export PATH="$nasm_home/nasm-3.02:$PATH"
    fi
    ;;
esac

# On Windows each carries its own C runtime (+crt-static), as kiln's Windows binary needs only what Windows 10 and later
# have: built as Shards builds its own, each imports vcruntime140.dll, the Visual C++ runtime, which a clean Windows
# lacks (jupiter, 2026-11-19); Hex builds its own so for the same reason (its .cargo/config.toml).
crt=()
case $target in
  *-pc-windows-msvc) crt=("CARGO_TARGET_$(printf %s "$target" | tr a-z- A-Z_)_RUSTFLAGS=-C target-feature=+crt-static") ;;
esac

cd vendor/shards
env ${crt[@]+"${crt[@]}"} ${helper_c[@]+"${helper_c[@]}"} cargo build --locked --profile "$profile" -p shards-vm-process \
  -p shards-net-process --target "$helpers" --target-dir "$built"
env ${crt[@]+"${crt[@]}"} ${shards_c[@]+"${shards_c[@]}"} SHARDS_HELPERS=beside cargo build --locked --profile "$profile" -p shards \
  --target "$target" --target-dir "$built"

mkdir -p "$out"
for f in "$helpers/$dir_name/shards-vm$exe" "$helpers/$dir_name/shards-net$exe" "$target/$dir_name/shards$exe"; do
  name=$(basename "$f")
  # The same bytes as last time stay as they are: kiln's build.rs watches these files (rerun-if-changed), and one
  # copied again, newer than its last run, would run it and build kiln again at every cargo command.
  if cmp -s "$built/$f" "$out/$name"; then
    continue
  fi
  cp "$built/$f" "$out/$name.part"
  if [ ${#crt[@]} -gt 0 ] && grep -a -q -i 'vcruntime140' "$out/$name.part"; then
    echo "pack: $name imports the Visual C++ runtime (vcruntime140.dll), which a clean Windows lacks" >&2
    rm -f "$out/$name.part"
    exit 1
  fi
  mv -f "$out/$name.part" "$out/$name"
done
ls -l "$out"
