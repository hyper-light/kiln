#!/bin/bash
# Builds the lab's h2o (a lab program, never a kiln dependency): h2o at a pinned commit, against
# the machine's OpenSSL, zlib and brotli, their headers from Fedora's own signed -devel packages
# unpacked under ~/.cache; cmake from PyPI through uvx. Prints the binary's version and digest.
set -euo pipefail
G=$HOME/.cache/kiln-bp/g1/tools
H2O_COMMIT=1b5ef77ebc3a6ec94ce297a87a9c644fa048ba3f
R=$G/rpm-dev/root
mkdir -p $R
# The headers, from the machine's own repositories, each package's signature checked.
(builtin cd $G/rpm-dev && dnf download -q --arch x86_64 openssl-devel openssl-devel-engine zlib-ng-compat-devel brotli-devel libuv-devel >/dev/null && rpm -K --quiet *.x86_64.rpm)
for r in $G/rpm-dev/*.x86_64.rpm; do (builtin cd $R && rpm2cpio $r | cpio -idmu --quiet); done
# The -devel packages' .so links point at libraries beside them; point them at the machine's.
for l in $R/usr/lib64/*.so; do t=$(readlink "$l"); [ -e "$R/usr/lib64/$t" ] || ln -sfn "/usr/lib64/$t" "$l"; done
S=$G/h2o/src
if [ ! -d $S/.git ]; then mkdir -p $G/h2o; git clone -q https://github.com/h2o/h2o $S; fi
git -C $S fetch -q --depth 1 origin $H2O_COMMIT 2>/dev/null || true
git -C $S checkout -q $H2O_COMMIT
B=$G/h2o/build
mkdir -p $B
# h2o's project names C++ too (only its fuzzers use it): a machine without g++ gets zig's.
CXX_ARG=()
if ! command -v c++ >/dev/null && command -v zig >/dev/null; then
  printf '#!/bin/sh\nexec zig c++ "$@"\n' > $G/h2o/zig-cxx; chmod +x $G/h2o/zig-cxx
  CXX_ARG=(-DCMAKE_CXX_COMPILER=$G/h2o/zig-cxx)
fi
export PKG_CONFIG_PATH=$R/usr/lib64/pkgconfig PKG_CONFIG_SYSROOT_DIR=$R
nice -n 10 uvx --from cmake==3.31.6 cmake -S $S -B $B "${CXX_ARG[@]}" -DCMAKE_BUILD_TYPE=Release -DWITH_MRUBY=OFF -DWITH_BUNDLED_SSL=OFF \
  -DOPENSSL_INCLUDE_DIR=$R/usr/include -DOPENSSL_SSL_LIBRARY=/usr/lib64/libssl.so.3 -DOPENSSL_CRYPTO_LIBRARY=/usr/lib64/libcrypto.so.3 \
  -DZLIB_INCLUDE_DIR=$R/usr/include -DZLIB_LIBRARY=/usr/lib64/libz.so.1 -DCMAKE_C_FLAGS="-I$R/usr/include" >$B/configure.log 2>&1 || { tail -30 $B/configure.log; exit 1; }
grep -i -E 'brotli|quic|libuv' $B/configure.log | head -5 || true
nice -n 10 uvx --from cmake==3.31.6 cmake --build $B -j 2 --target h2o >$B/build.log 2>&1 || { tail -30 $B/build.log; exit 1; }
$B/h2o --version | head -3
sha256sum $B/h2o
