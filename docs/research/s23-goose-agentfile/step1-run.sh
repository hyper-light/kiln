#!/usr/bin/env bash
# Step 1's run on a Linux runner: pin stable, test pin.sh's refusals, check the pin independently.
set -u
echo "# step 1 run, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(gh --version | head -1), $(jq --version), $(tar --version | head -1), $(bash --version | head -1)"
echo "# repo copy at $(pwd), HEAD $(git rev-parse HEAD 2>/dev/null)"
D=.kiln/images/harness-goose
echo "===== pin.sh stable"
"$D/pin.sh" stable; echo "PIN_EXIT=$?"
echo "===== pin_test.sh"
"$D/pin_test.sh"; echo "PIN_TEST_EXIT=$?"
echo "===== independent check: each pinned URL fetched again, archive and extracted goose hashed against pin.json"
c=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/s23-pin-check.XXXXXX"); ok=0
for i in 0 1; do
  p=$(jq -r ".builds[$i].platform" "$D/pin.json"); u=$(jq -r ".builds[$i].files[0].url" "$D/pin.json")
  curl -fsSL -o "$c/a.tar.gz" "$u"; mkdir -p "$c/x$i"; tar -xzf "$c/a.tar.gz" -C "$c/x$i"
  as=$(sha256sum "$c/a.tar.gz" | cut -d' ' -f1); az=$(stat -c %s "$c/a.tar.gz"); ms=$(sha256sum "$c/x$i/goose" | cut -d' ' -f1); mz=$(stat -c %s "$c/x$i/goose")
  m=$(jq -r --arg as "$as" --argjson az "$az" --arg ms "$ms" --argjson mz "$mz" ".builds[$i].files[0] | (.sha256 == \$as and .size == \$az and .unpack.sha256 == \$ms and .unpack.size == \$mz)" "$D/pin.json")
  echo "CHECK $p archive sha256=$as size=$az member sha256=$ms size=$mz matches_pin=$m"; [ "$m" = true ] && ok=$((ok + 1))
done
rm -rf "$c"; echo "BUILDS_MATCHING_PIN=$ok"
echo "===== shellcheck"
if command -v shellcheck >/dev/null; then shellcheck -x "$D/pin.sh" "$D/pin_test.sh"; echo "SHELLCHECK_EXIT=$?"; else echo "SHELLCHECK: not installed here"; fi
echo "===== pin.json sha256 $(sha256sum "$D/pin.json" | cut -d' ' -f1)"
echo "=====PIN_JSON_BEGIN"; cat "$D/pin.json"; echo "=====PIN_JSON_END"
