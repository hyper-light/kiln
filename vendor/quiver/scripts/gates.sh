#!/usr/bin/env bash
# Every gate of CLAUDE.md, in order; exits non-zero at the first failure. Run it before each commit;
# kiln CI runs it on every push and pull request (.kiln/ci.yaml).
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
gate() { printf '::: %s\n' "$*" >&2; "$@"; }
gate cargo fmt --all --check
# zig, for two C builds. Focal's bus brings aws-lc's C (hyper-raft's TLS and QUIC take it), which a
# lint of macOS or Windows compiles: zig is its C compiler there, through cargo-zigbuild, as kiln's
# release builds those platforms (kiln's ci/release/build.sh). And the bench's Kafka client builds
# librdkafka from source, whose configure insists on a C++ compiler: where the machine has none,
# zig's serves. Those on the PATH are taken; else the versions kiln's release pins, from PyPI into a
# virtualenv of their own (QUIVER_ZIG_HOME, ~/.cache/quiver-zig).
if ! command -v cargo-zigbuild >/dev/null || ! command -v zig >/dev/null; then
  zig_home=${QUIVER_ZIG_HOME:-$HOME/.cache/quiver-zig}
  if ! "$zig_home/bin/cargo-zigbuild" --help >/dev/null 2>&1; then
    python3 -m venv "$zig_home"
    "$zig_home/bin/pip" install --quiet "ziglang==0.17.0" "cargo-zigbuild==0.23.4"
  fi
  mkdir -p "$zig_home/zigbin"
  printf '#!/bin/sh\nexec "%s/bin/python" -m ziglang "$@"\n' "$zig_home" > "$zig_home/zigbin/zig"
  chmod +x "$zig_home/zigbin/zig"
  export PATH="$zig_home/zigbin:$zig_home/bin:$PATH"
fi
if ! command -v c++ >/dev/null; then
  cxx_home=${QUIVER_ZIG_HOME:-$HOME/.cache/quiver-zig}/cxx
  mkdir -p "$cxx_home"
  printf '#!/bin/sh\nexec zig c++ "$@"\n' > "$cxx_home/c++"
  chmod +x "$cxx_home/c++"
  export PATH="$cxx_home:$PATH"
fi
gate cargo clippy --workspace --all-targets --locked -- -D warnings
# The same lints for macOS and Windows (rust-toolchain.toml installs their targets).
gate cargo-zigbuild clippy --workspace --all-targets --locked --target aarch64-apple-darwin -- -D warnings
gate cargo-zigbuild clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings
gate bash scripts/check-production.sh
gate cargo test --workspace --locked
gate python3 scripts/vendor.py check
