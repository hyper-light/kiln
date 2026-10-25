#!/usr/bin/env bash
# Build kiln for the platforms it ships: every one from one Linux machine, or macOS's on a Mac.
#
#   ci/release/build.sh [target ...]          (default: every target below)
#
# On Linux, zig is the cross toolchain throughout (through cargo-zigbuild):
# - Linux: static musl binaries.
# - Windows: built with the GNU ABI and shipped as the `x86_64-pc-windows-msvc` download the
#   install script asks for. It needs only DLLs that come with Windows 10 and later.
# - macOS: linked against ci/release/macos-stubs. These are text stubs listing the public symbols
#   kiln imports from CoreFoundation, IOKit and libobjc; macOS binds them to the real libraries
#   at load time, so no Apple SDK is needed. If a dependency starts importing a new symbol,
#   regenerate the stubs with ci/release/regen-macos-stubs.sh. The minimum OS is macOS 11.
# On a Mac, the macOS targets are built natively instead, with its own toolchain and SDK (what
# .kiln/release.yaml does); other targets still need Linux.
#
# Expects ui/dist/client to exist (`npm run --prefix ui build`). Each binary carries the UI server
# for its platform: Bun and the UI server's files, packed here first (ui/scripts/pack-ui-server.mjs,
# vendor/bun/README.md; KILN_UI_SERVER=off builds without). Writes dist/kiln-<target>[.exe] and
# dist/SHA256SUMS. KILN_DIST, KILN_MACOS_STUBS and KILN_MACOS_LINK_ARGS override the output
# directory, the stubs and extra macOS link arguments (regen-macos-stubs.sh uses them).
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD

all=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-pc-windows-msvc)
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=("${all[@]}")
for t in "${targets[@]}"; do
  case $t in x86_64-apple-darwin) echo "kiln doesn't ship for Intel Macs: only aarch64-apple-darwin" >&2; exit 1 ;; esac
done

native_mac=false
if [ "$(uname -s)" = Darwin ]; then
  native_mac=true
  # Run by a launchd service (a kiln runner), PATH has none of the usual tools in it.
  export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
  for t in "${targets[@]}"; do
    case $t in *-apple-darwin) ;; *) echo "on a Mac only the macOS targets build: $t needs Linux" >&2; exit 1 ;; esac
  done
fi

[ -f ui/dist/client/_shell.html ] || { echo "ui/dist is missing: run npm run --prefix ui build first" >&2; exit 1; }

# The UI server each binary carries: Bun for every target (downloaded once per machine, into
# KILN_BUN_CACHE, and checked against vendor/bun/bun.json), and the UI server's files.
if [ "${KILN_UI_SERVER:-}" != off ]; then
  packs=()
  for t in "${targets[@]}"; do packs+=(--target "$t"); done
  node ui/scripts/pack-ui-server.mjs --require "${packs[@]}"
fi

target_dir=${CARGO_TARGET_DIR:-$root/target}
dist=${KILN_DIST:-$root/dist}
mkdir -p "$dist"

# On a Mac: its own toolchain and SDK, no zig.
if $native_mac; then
  for t in "${targets[@]}"; do
    echo "::: $t"
    rustup target add "$t" >/dev/null
    MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --locked -p kiln --target "$t"
    cp "$target_dir/$t/release/kiln" "$dist/kiln-$t"
  done
  (cd "$dist" && shasum -a 256 kiln-* > SHA256SUMS && cat SHA256SUMS)
  exit 0
fi

# zig and cargo-zigbuild, pinned, in a private virtualenv (both are published on PyPI).
zig_version=0.17.0
zigbuild_version=0.23.4
zig_home=${KILN_ZIG_HOME:-$HOME/.cache/kiln-zig}
if ! "$zig_home/bin/python" -m ziglang version 2>/dev/null | grep -qx "$zig_version" \
  || ! "$zig_home/bin/cargo-zigbuild" --help >/dev/null 2>&1; then
  python3 -m venv "$zig_home"
  "$zig_home/bin/pip" install --quiet "ziglang==$zig_version" "cargo-zigbuild==$zigbuild_version"
fi
mkdir -p "$zig_home/zigbin" "$zig_home/macos11"
printf '#!/bin/sh\nexec "%s/bin/python" -m ziglang "$@"\n' "$zig_home" > "$zig_home/zigbin/zig"
chmod +x "$zig_home/zigbin/zig"
export PATH="$zig_home/zigbin:$zig_home/bin:$PATH"
export CARGO_ZIGBUILD_ZIG_COMMAND="$zig_home/zigbin/zig"
zigbuild="$zig_home/bin/cargo-zigbuild"

# macOS: cargo-zigbuild doesn't let us pick the minimum OS (zig then defaults to its newest), so
# these wrappers pass zig an explicit `<arch>-macos.11.0` target for both compiling C and linking.
# They also pass each framework once: rustc repeats `-framework X` for every crate that links X,
# zig's linker writes a load command per occurrence, and dyld refuses a binary that loads the same
# library twice ("duplicate linked dylib").
for arch in aarch64; do
  cat > "$zig_home/macos11/cc-$arch" <<WRAPPER
#!/bin/sh
seen=" "
n=\$#
while [ "\$n" -gt 0 ]; do
  a=\$1; shift; n=\$((n - 1))
  if [ "\$a" = -framework ] && [ "\$n" -gt 0 ]; then
    f=\$1; shift; n=\$((n - 1))
    case "\$seen" in *" \$f "*) continue ;; esac
    seen="\$seen\$f "
    set -- "\$@" -framework "\$f"
    continue
  fi
  set -- "\$@" "\$a"
done
exec "$zigbuild" zig cc -- -g -fno-sanitize=all -target $arch-macos.11.0-none "\$@"
WRAPPER
  chmod +x "$zig_home/macos11/cc-$arch"
done
printf '#!/bin/sh\nexec "%s" zig ar -- "$@"\n' "$zigbuild" > "$zig_home/macos11/ar"
chmod +x "$zig_home/macos11/ar"
stubs=${KILN_MACOS_STUBS:-$root/ci/release/macos-stubs}

for t in "${targets[@]}"; do
  echo "::: $t"
  case $t in
    *-unknown-linux-musl)
      rustup target add "$t" >/dev/null
      "$zigbuild" zigbuild --release --locked -p kiln --target "$t"
      cp "$target_dir/$t/release/kiln" "$dist/kiln-$t"
      ;;
    *-apple-darwin)
      rustup target add "$t" >/dev/null
      arch=${t%%-*}
      upper=$(echo "$t" | tr 'a-z-' 'A-Z_')
      env "CC_$t=$zig_home/macos11/cc-$arch" "AR_$t=$zig_home/macos11/ar" \
        "CARGO_TARGET_${upper}_LINKER=$zig_home/macos11/cc-$arch" \
        "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-arg=-F$stubs/Frameworks -C link-arg=-L$stubs/lib ${KILN_MACOS_LINK_ARGS:-}" \
        MACOSX_DEPLOYMENT_TARGET=11.0 \
        cargo build --release --locked -p kiln --target "$t"
      cp "$target_dir/$t/release/kiln" "$dist/kiln-$t"
      ;;
    x86_64-pc-windows-msvc)
      rustup target add x86_64-pc-windows-gnu >/dev/null
      "$zigbuild" zigbuild --release --locked -p kiln --target x86_64-pc-windows-gnu
      cp "$target_dir/x86_64-pc-windows-gnu/release/kiln.exe" "$dist/kiln-$t.exe"
      ;;
    *)
      echo "don't know how to build $t" >&2
      exit 1
      ;;
  esac
done

(cd "$dist" && sha256sum kiln-* > SHA256SUMS && cat SHA256SUMS)
