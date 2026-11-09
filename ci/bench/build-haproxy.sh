#!/bin/bash
# HAProxy's selection kernels (ci/bench/drivers/haproxy_pick.c) over HAProxy's own objects at the
# pinned release (ci/bench/README.md). HAProxy's Makefile builds only in its tree, so the pinned
# source is copied to ~/.cache/kiln-lb/bench/build/haproxy-src and built there if it isn't, as
# distributions build it: `make TARGET=linux-glibc` (its -O2, its default USE_ options, threads
# on; no OpenSSL, Lua or PCRE). Then the driver, with HAProxy's own COPTS, linked with every
# object of HAProxy's link line, haproxy.o with its main renamed.
# Output: ~/.cache/kiln-lb/bench/bin/haproxy_pick.
set -euo pipefail
B="$HOME/.cache/kiln-lb/bench" HERE="$(builtin cd "$(dirname "$0")" && pwd)"
SRC="$B/src/haproxy" OBJ="$B/build/haproxy-src" TAG=v3.4.7 SHA=8872a2604595140544774c2a6309a41186fc0f44
[ -d "$SRC" ] || git clone -q --depth 1 -b "$TAG" http://git.haproxy.org/git/haproxy-3.4.git "$SRC"
[ "$(git -C "$SRC" rev-parse HEAD)" = "$SHA" ] || { echo "haproxy: $SRC is not $SHA" >&2; exit 1; }
[ -d "$OBJ" ] || cp -a "$SRC" "$OBJ"
[ "$(git -C "$OBJ" rev-parse HEAD)" = "$SHA" ] || { echo "haproxy: $OBJ is not $SHA" >&2; exit 1; }
MAKE=(make -C "$OBJ" TARGET=linux-glibc)
[ -x "$OBJ/haproxy" ] || nice -n 19 "${MAKE[@]}" -j4 >/dev/null
mkdir -p "$B/bin" "$B/build/haproxy-driver"
# HAProxy's compile options, link options and objects, as its Makefile has them (`make opts`).
# COPTS keeps its escaped quotes (-DCONFIG_HAPROXY_VERSION=\"3.4.7\"), so it goes through eval.
opt() { "${MAKE[@]}" -s opts | sed -n "s/^$1=\"\\(.*\\)\"\$/\\1/p"; }
copts=$(opt COPTS) ldopts=$(opt LDOPTS) objs="$(opt OPTIONS_OBJS) $(opt OBJS)"
# haproxy.o's main renamed, so the driver's is the program's.
objcopy --redefine-sym main=ha_bench_haproxy_main "$OBJ/src/haproxy.o" "$B/build/haproxy-driver/haproxy.o"
link=()
for o in $objs; do
  if [ "$o" = src/haproxy.o ]; then link+=("$B/build/haproxy-driver/haproxy.o"); else link+=("$OBJ/$o"); fi
done
# COPTS' -Iinclude is relative to HAProxy's tree: compile from there.
(builtin cd "$OBJ" && eval "cc $copts -Wno-declaration-after-statement -I '$HERE/drivers' -c '$HERE/drivers/haproxy_pick.c' -o '$B/build/haproxy-driver/haproxy_pick.o'")
cc -o "$B/bin/haproxy_pick" "$B/build/haproxy-driver/haproxy_pick.o" "${link[@]}" $ldopts -lm
echo "built $B/bin/haproxy_pick (haproxy $TAG $SHA)"
