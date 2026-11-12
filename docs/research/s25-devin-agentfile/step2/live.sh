#!/usr/bin/env bash
# Step 25 step 2's live check: `kiln agentfiles context devin` against Cognition's pinned bundles, each
# per-arch directory held to its bundle as the bundle itself lists and holds it.
set -euo pipefail
export LC_ALL=C
top=$HOME/.cache/s25-devin-tree-pin
cd "$top/kiln"
export CARGO_TARGET_DIR=$top/target CARGO_BUILD_JOBS=16
echo "== kiln $(git rev-parse HEAD) with the change applied; $(uname -srm); $(tar --version | head -1)"
cargo build -p kiln --bin kiln --locked
kiln=$CARGO_TARGET_DIR/debug/kiln
ctx=$top/ctx-$(date -u +%Y%m%dT%H%M%SZ)
echo "== $kiln agentfiles context devin $ctx --platform linux/amd64,linux/arm64"
"$kiln" agentfiles context devin "$ctx" --platform linux/amd64,linux/arm64
echo "== $ctx holds: $(ls -A "$ctx" | tr '\n' ' ')"
cmp "$ctx/Agentfile" .kiln/images/harness-devin/Agentfile && echo "Agentfile is kiln's"
b=$top/bundles
mkdir -p "$b"
pin=.kiln/images/harness-devin/pin.json
fail=0
for arch in amd64 arm64; do
    read -r url sha size < <(python3 -I -c 'import json,sys
d=json.load(open(sys.argv[1]))
f=[f for x in d["builds"] if x["platform"]=="linux/"+sys.argv[2] for f in x["files"]][0]
print(f["url"], f["sha256"], f["size"])' "$pin" "$arch")
    f=$b/devin-$arch.tar.gz
    [ -f "$f" ] || curl -fsSL -o "$f" "$url"
    echo "$sha  $f" | sha256sum -c --quiet - && [ "$(stat -c %s "$f")" = "$size" ] && echo "-- $arch: $url downloaded again, sha256 $sha, $size bytes, as pinned"
    d=$ctx/devin-$arch
    echo "-- $d holds: $(ls -A "$d" | tr '\n' ' '); bin/ holds: $(ls -A "$d/bin" | tr '\n' ' ')"
    if diff <( (tar -tzf "$f" | sed 's,/$,,'; echo harness.json) | sort) <(cd "$d" && find . -mindepth 1 | sed 's,^\./,,' | sort) >"$b/paths-$arch.diff"; then
        echo "-- $arch: $(cd "$d" && find . -mindepth 1 | wc -l) paths, exactly the bundle's $(tar -tzf "$f" | wc -l) entries and harness.json"
    else
        echo "-- $arch: PATHS DIFFER"; head -20 "$b/paths-$arch.diff"; fail=1
    fi
    want=$(tar -xzOf "$f" bin/devin | sha256sum | cut -d' ' -f1)
    wsize=$(tar -tvzf "$f" bin/devin | awk '{print $3}')
    got=$(sha256sum <"$d/bin/devin" | cut -d' ' -f1)
    read -r mode gsize < <(stat -c '%a %s' "$d/bin/devin")
    echo "-- $arch: bin/devin mode $mode, $gsize bytes, sha256 $got; the bundle's: $wsize bytes, sha256 $want"
    [ "$got" = "$want" ] && [ "$gsize" = "$wsize" ] && [ "$mode" = 755 ] || { echo "-- $arch: BIN/DEVIN DIFFERS"; fail=1; }
    rm -rf "$b/x-$arch" && mkdir -p "$b/x-$arch" && tar -xzf "$f" -C "$b/x-$arch"
    if diff -r --no-dereference -x harness.json "$b/x-$arch" "$d" >/dev/null; then echo "-- $arch: every file's bytes are the bundle's"; else echo "-- $arch: BYTES DIFFER"; fail=1; fi
    cmp "$d/harness.json" .kiln/images/harness-devin/harness.json && echo "-- $arch: harness.json is kiln's"
    echo "-- $arch: set-ID or sticky: $(find "$d" -perm /7000 | wc -l); kinds and modes: $(find "$d" -mindepth 1 -printf '%y%m\n' | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
done
echo "== live check $([ $fail = 0 ] && echo PASSED || echo FAILED)"
exit $fail
