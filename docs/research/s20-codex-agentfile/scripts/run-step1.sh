#!/usr/bin/env bash
# Plan step 20 [kiln], step 1 on a Linux runner (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): pin.sh
# fetch, then verify (the pin itself), then the mutations, then the fresh downloads; every word into
# OUT/step1.txt, the pin into OUT/pin.json, the exit codes' verdict into OUT/done.
#
#   run-step1.sh OUT [latest | rust-vX.Y.Z]     (pin.sh, pin-mutations.sh and check-pin.sh beside it)
set -uo pipefail

out=${1:?usage: run-step1.sh OUT [latest | rust-vX.Y.Z]}
want=${2:-latest}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "$out"
codes=()
{
    echo "# Plan step 20 [kiln] step 1 on $(hostname), $(uname -sr) $(uname -m), $(date -u +%FT%TZ)"
    for f in pin.sh pin-mutations.sh check-pin.sh run-step1.sh; do echo "# $f sha256 $(sha256sum "$here/$f" | cut -d' ' -f1)"; done
    echo "## pin.sh fetch $want"
    "$here/pin.sh" fetch "$out/fetched" "$want"
    codes+=("fetch=$?")
    echo "## pin.sh verify (the pin)"
    "$here/pin.sh" verify "$out/fetched" "$out/pin.json"
    codes+=("verify=$?")
    echo "## the mutations"
    "$here/pin-mutations.sh" "$out/fetched" "$here/pin.sh"
    codes+=("mutations=$?")
    echo "## fresh downloads of every pinned archive"
    "$here/check-pin.sh" "$out/pin.json"
    codes+=("fresh=$?")
    echo "## pin.json"
    cat "$out/pin.json"
    echo "# exits: ${codes[*]}"
    echo "# done $(date -u +%FT%TZ)"
} >"$out/step1.txt" 2>&1
case " ${codes[*]} " in
    *"=1"* | *"=2"* | *"=3"*) echo "FAILED ${codes[*]}" >"$out/done" ;;
    *) echo "OK ${codes[*]}" >"$out/done"; rm -rf "$out/fetched" ;;
esac
