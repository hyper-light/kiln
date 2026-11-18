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

cd vendor/shards
env ${helper_c[@]+"${helper_c[@]}"} cargo build --locked --profile "$profile" -p shards-vm-process -p shards-net-process \
  --target "$helpers" --target-dir "$built"
SHARDS_HELPERS=beside cargo build --locked --profile "$profile" -p shards --target "$target" --target-dir "$built"

mkdir -p "$out"
for f in "$helpers/$dir_name/shards-vm$exe" "$helpers/$dir_name/shards-net$exe" "$target/$dir_name/shards$exe"; do
  name=$(basename "$f")
  cp "$built/$f" "$out/$name.part"
  mv -f "$out/$name.part" "$out/$name"
done
ls -l "$out"
