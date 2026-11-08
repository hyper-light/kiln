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
#
# The Linux binaries' allocator (goal 11, plan 01a1a87b2e56 step 4; docs/runtime-lab.md, "The allocator
# (step 4)"): `jemalloc`, kiln's `jemalloc` feature (jemalloc 5.3 with kiln::os::alloc's settings for giving
# memory back), or `musl`, musl's own malloc. The default is jemalloc, the lab's pick: on the static musl
# build, resident memory 523 to 333 MB, and no latency number worse beyond noise. KILN_LINUX_ALLOCATOR=musl
# leaves it out of one build.
# A lab's build may take `jemalloc-prof`: jemalloc with its heap profile built in (kiln's `jemalloc-prof`
# feature, off until a run's _RJEM_MALLOC_CONF says prof:true; goal 11, plan 01a1a87b2e56 step 5), never a
# release's.
#
# The Linux x86-64 binary's code layout (goal 11, plan 01a1a87b2e56 step 5; ci/release/bolt.sh): off unless
# KILN_RELEASE_BOLT says, which builds x86_64-unknown-linux-musl keeping its relocations and symbols (strip off,
# linked by ci/release/zig-relocs.sh: --emit-relocs and a build id) in a target dir of its own (<target dir>/bolt,
# so the release's own build stays built), and then:
# - `relocs`: dist/kiln-x86_64-unknown-linux-musl is that build stripped, the release as it would be: what a lab
#   profiles (its build id matches the unstripped one's, BOLT's input, which stays at
#   <target dir>/bolt/x86_64-unknown-linux-musl/release/kiln) and the A/B's side without BOLT;
# - a profile's path (perf2bolt's or an instrumented window's .fdata, of that very build; or a .yaml, which BOLT
#   matches to a nearby build's code): dist/kiln-x86_64-unknown-linux-musl is the build laid out by it
#   (bolt.sh optimize) and stripped, once bolt.sh same says it's the same program (one build, one `--version`);
#   what the profile covered beside BOLT's log (<target dir>/bolt/x86_64-unknown-linux-musl/release/kiln.bolt.txt).
#   BOLT failing fails the build: nothing ships laid out otherwise than asked.
# - an order's path (a .order file, `bolt.sh order`'s: the functions a profile saw run, hottest first): the build
#   linked with those functions first, in that order (LLD's --symbol-ordering-file, through zig-relocs.sh), and
#   stripped. The layout without BOLT: whole functions, none split.
# cargo relinks only what it knows changed, and an order is no input it knows of, so a binary in that target dir
# linked in another order than the one asked (none, for `relocs` and a profile) is removed first, and relinked.
# Other targets build as ever.
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD

linux_allocator=${KILN_LINUX_ALLOCATOR:-jemalloc}
case $linux_allocator in
  musl | jemalloc | jemalloc-prof) ;;
  *) echo "KILN_LINUX_ALLOCATOR is musl, jemalloc or jemalloc-prof (a lab's), not $linux_allocator" >&2; exit 1 ;;
esac

all=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-pc-windows-msvc)
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=("${all[@]}")
for t in "${targets[@]}"; do
  case $t in x86_64-apple-darwin) echo "kiln doesn't ship for Intel Macs: only aarch64-apple-darwin" >&2; exit 1 ;; esac
done

bolt=${KILN_RELEASE_BOLT:-}
if [ -n "$bolt" ]; then
  if [ "$bolt" != relocs ]; then
    # A path as the caller gave it, from where they were before the cd above.
    case $bolt in /*) ;; *) bolt=$OLDPWD/$bolt ;; esac
    [ -s "$bolt" ] || { echo "KILN_RELEASE_BOLT is relocs, a profile's path or an order's: $bolt is no profile" >&2; exit 1; }
  fi
  case " ${targets[*]} " in
    *" x86_64-unknown-linux-musl "*) ;;
    *) echo "KILN_RELEASE_BOLT lays out x86_64-unknown-linux-musl, which this build doesn't make" >&2; exit 1 ;;
  esac
fi

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
# They drop the exported symbols list rustc gives a dependency's cdylib (crc-fast's:
# `-Wl,-exported_symbols_list -Wl,<file>`): zig's linker reads the argument after the file as the
# list ("unable to read exported symbols list"), and kiln links the rlib, never that dylib.
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
  if [ "\$a" = -Wl,-exported_symbols_list ] && [ "\$n" -gt 0 ]; then
    shift; n=\$((n - 1))
    continue
  fi
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
      alloc=()
      if [ "$linux_allocator" != musl ]; then
        alloc=(--features "$linux_allocator")
        # jemalloc's page must be at least the system's, or it refuses to start ("Unsupported system page
        # size"), and built here its configure can't ask the machine it will run on, so it takes 4 KiB.
        # x86-64's pages are 4 KiB; arm64 Linux runs 4, 16 or 64 KiB pages (16 on Apple silicon and the
        # Raspberry Pi 5's kernel, 64 on some servers'), so its jemalloc takes 64 KiB (2^16), which runs on
        # all three. tikv-jemalloc-sys reads it per target.
        export AARCH64_UNKNOWN_LINUX_MUSL_JEMALLOC_SYS_WITH_LG_PAGE=16
      fi
      if [ -n "$bolt" ] && [ "$t" = x86_64-unknown-linux-musl ]; then
        # BOLT's input (above): its relocations and symbols kept, linked through zig-relocs.sh, in an order's
        # order if one is asked (above), else the linker's own. Which it was linked in is marked beside it: none, or
        # the order's digest.
        built=$target_dir/bolt/$t/release
        input=$built/kiln
        order='' mark=none
        case $bolt in *.order) order=$bolt mark=$(sha256sum < "$bolt" | cut -c1-64) ;; esac
        if [ "$(cat "$built/kiln.linked-order" 2> /dev/null)" != "$mark" ]; then
          # Linked in another order, or in one not marked: the executable goes (cargo's deps/kiln-<hash>, which
          # cargo then builds again, and its copy), and the rest of the build stays.
          find "$built/deps" -maxdepth 1 -type f -regextype posix-extended -regex '.*/kiln-[0-9a-f]+' -delete 2> /dev/null || true
          rm -f "$input"
        fi
        rm -f "$built/kiln.linked-order"
        # cargo-zigbuild's cache of its own (CARGO_ZIGBUILD_CACHE_DIR): the linker wrapper it writes there exports the
        # zig it runs, and its name says the target, not that zig, so in the shared cache (~/.cache/cargo-zigbuild) any
        # other build of this target on the machine rewrites it with the plain zig while this one builds, and the link
        # skips zig-relocs.sh (2026-11-06 on adacachy: another x86_64 musl build did, at 07:28:38, mid-build: no .rela.text,
        # which bolt.sh check refused).
        KILN_LINK_ORDER=$order KILN_ZIG_REAL="$zig_home/zigbin/zig" CARGO_ZIGBUILD_ZIG_COMMAND="$root/ci/release/zig-relocs.sh" \
          CARGO_ZIGBUILD_CACHE_DIR="$target_dir/bolt/zigbuild" CARGO_PROFILE_RELEASE_STRIP=false CARGO_TARGET_DIR="$target_dir/bolt" \
          "$zigbuild" zigbuild --release --locked -p kiln --target "$t" ${alloc[@]+"${alloc[@]}"}
        # Relinked through zig-relocs.sh (its relocations say so), so in the order asked: marked as such.
        ci/release/bolt.sh check "$input"
        echo "$mark" > "$built/kiln.linked-order"
        case $bolt in
          relocs | *.order) "${KILN_STRIP:-strip}" --strip-all -o "$dist/kiln-$t" "$input" ;;
          *)
            ci/release/bolt.sh optimize "$input" "$bolt" "$dist/kiln-$t"
            # The layout ships only as the same program: one build (its id, BOLT flipping its last bit), one `--version`,
            # its code moved (bolt.sh same), where this machine runs an x86-64 build; elsewhere the release's own smoke
            # runs it.
            if [ "$(uname -m)" = x86_64 ]; then
              ci/release/bolt.sh same "$input" "$dist/kiln-$t" || { rm -f "$dist/kiln-$t"; exit 1; }
            else
              echo "build.sh: $(uname -m) can't run $dist/kiln-$t: bolt.sh same left to the release's smoke" >&2
            fi
            ;;
        esac
      else
        "$zigbuild" zigbuild --release --locked -p kiln --target "$t" ${alloc[@]+"${alloc[@]}"}
        cp "$target_dir/$t/release/kiln" "$dist/kiln-$t"
      fi
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
