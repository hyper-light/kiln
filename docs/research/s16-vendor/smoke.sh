#!/usr/bin/env bash
# kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16: the VM process kiln carries boots a guest on this host
# (HVF, KVM or WHP), signed as kiln signs it on a Mac. Run from kiln's root, on the host to try:
#
#   docs/research/s16-vendor/smoke.sh
#
# 1. ci/shards/pack.sh: Shards' binary and its VM and network processes, from vendor/shards, for this host
#    (KILN_SHARDS_PROFILE: release unless given).
# 2. Shards' pinned kernel for this host (vendor/shards/crates/shards/src/kernel.rs `KERNEL`), fetched and its
#    SHA-256 checked, into $S16_DIR (default target/s16-smoke).
# 3. init.rs here, built static for this architecture's musl target: PID 1, which powers the VM off.
# 4. kiln's test of it: tests/shards_carried.rs writes the carried Shards out (signing it on a Mac), checks
#    each signature there, and boots the guest through shards-api with the carried `shards`; it passes when
#    the guest powers off. Its output says where the copy went and who signed it.
set -euo pipefail
cd "$(dirname "$0")/../../.."
root=$PWD
dir=${S16_DIR:-$root/target/s16-smoke}
mkdir -p "$dir"
host=$(rustc -vV | sed -n 's/^host: //p')
arch=${host%%-*}
case "$host" in
  *-pc-windows-*) kernel_line='cfg!(all(target_arch = "x86_64", windows))' ;;
  *) kernel_line="cfg!(target_arch = \"$arch\")" ;;
esac
# The pinned kernel: the Pinned after its cfg line in kernel.rs's KERNEL (the first, not KERNEL_KVM's or the GPU's).
read -r url sha <<<"$(awk -v want="$kernel_line" '
  /^pub const KERNEL:/ {in_k=1}
  in_k && index($0, want) {hit=1}
  in_k && hit && /url:/ {gsub(/[",]/, "", $2); url=$2}
  in_k && hit && /sha256:/ {gsub(/[",]/, "", $2); print url, $2; exit}
' vendor/shards/crates/shards/src/kernel.rs)"
[ -n "${url:-}" ] && [ -n "${sha:-}" ] || { echo "smoke: no pinned kernel for $host in kernel.rs" >&2; exit 1; }
kernel="$dir/$(basename "$url")"
if [ ! -f "$kernel" ] || [ "$( (sha256sum "$kernel" 2>/dev/null || shasum -a 256 "$kernel") | cut -d' ' -f1)" != "$sha" ]; then
  curl -fsSL --retry 5 --retry-all-errors -o "$kernel.part" "$url"
  got=$( (sha256sum "$kernel.part" 2>/dev/null || shasum -a 256 "$kernel.part") | cut -d' ' -f1)
  [ "$got" = "$sha" ] || { echo "smoke: $url is $got, not $sha" >&2; exit 1; }
  mv -f "$kernel.part" "$kernel"
fi
echo "smoke: kernel $kernel ($sha)"
# PID 1, with Shards' toolchain (its toolchain file lists the musl targets).
musl="$arch-unknown-linux-musl"
(cd vendor/shards && rustc --edition 2024 -O -C panic=abort -C linker=rust-lld --target "$musl" \
  "$root/docs/research/s16-vendor/init.rs" -o "$dir/init")
echo "smoke: init $dir/init"
ci/shards/pack.sh
KILN_TEST_KERNEL="$kernel" KILN_TEST_INIT="$dir/init" cargo test -p kiln --test shards_carried -- --nocapture
