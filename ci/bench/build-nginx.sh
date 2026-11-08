#!/bin/bash
# nginx's selection kernels (ci/bench/drivers/nginx_pick.c) over nginx's own objects at the pinned
# release (ci/bench/README.md). Builds nginx under ~/.cache/kiln-lb/bench/build/nginx if it isn't
# there, then the driver with nginx's own CFLAGS. Output: ~/.cache/kiln-lb/bench/bin/nginx_pick.
set -euo pipefail
B="$HOME/.cache/kiln-lb/bench" HERE="$(builtin cd "$(dirname "$0")" && pwd)"
SRC="$B/src/nginx" OBJ="$B/build/nginx" TAG=release-1.31.8 SHA=140aa942711868a8daeb95810bc9a81f272ada72
[ -d "$SRC" ] || git clone -q --depth 1 -b "$TAG" https://github.com/nginx/nginx "$SRC"
[ "$(git -C "$SRC" rev-parse HEAD)" = "$SHA" ] || { echo "nginx: $SRC is not $SHA" >&2; exit 1; }
if [ ! -x "$OBJ/nginx" ]; then
  (builtin cd "$SRC" && ./auto/configure --builddir="$OBJ" --with-cc-opt='-O2 -g' --without-http_rewrite_module --without-http_gzip_module >/dev/null && nice -n 19 make -j4 >/dev/null)
fi
mkdir -p "$B/bin" "$B/build/nginx-driver"
# nginx.o's main renamed, so the driver's is the program's.
objcopy --redefine-sym main=ngx_bench_nginx_main "$OBJ/src/core/nginx.o" "$B/build/nginx-driver/nginx.o"
objs=$(sed -n '/^'"${OBJ//\//\\/}"'\/nginx:/,/^$/p' "$OBJ/Makefile" | grep -o "$OBJ/[^ ]*\.o" | grep -v -e '/nginx\.o$' -e 'upstream_least_conn_module\.o$' -e 'upstream_random_module\.o$' | sort -u)
# The libraries on nginx's own link line (its Makefile's recipe for nginx).
libs=$(grep -oE '^[[:space:]]+-l[a-z0-9_]+' "$OBJ/Makefile" | sort -u | xargs)
incs="-I $SRC/src/core -I $SRC/src/event -I $SRC/src/event/modules -I $SRC/src/event/quic -I $SRC/src/os/unix -I $OBJ -I $SRC/src/http -I $SRC/src/http/modules -I $HERE/drivers"
cc -pipe -O -W -Wall -Wpointer-arith -Wno-unused-parameter -g -O2 -g $incs -c "$HERE/drivers/nginx_pick.c" -o "$B/build/nginx-driver/nginx_pick.o"
cc -o "$B/bin/nginx_pick" "$B/build/nginx-driver/nginx_pick.o" "$B/build/nginx-driver/nginx.o" $objs $libs -lm -Wl,-E
echo "built $B/bin/nginx_pick (nginx $TAG $SHA)"
