#!/usr/bin/env bash
# Step 2's re-pin (plan 01a1ce705e3d72af83ef15ae83b62f45), once pin.sh writes the smoke: prints a
# self-contained script that carries this tree's pin.sh and pin_test.sh and main's pin.json to a Linux
# runner, pins stable there, tests pin.sh's refusals, checks the pin independently, and compares it
# with main's. Run from the worktree:
#
#   docs/research/s23-goose-agentfile/step2b-pin-run.sh > step2b-pin.sh
#   kiln run --no-sync --needs linux/amd64 --on kiln-a094a12d.attlocal.net -- "$(cat step2b-pin.sh)"
set -euo pipefail
W=$(git rev-parse --show-toplevel)
D=$W/.kiln/images/harness-goose
cat <<'HEAD'
#!/usr/bin/env bash
# Step 2's re-pin on a Linux runner (plan 01a1ce705e3d72af83ef15ae83b62f45): pin.sh and pin_test.sh exactly as they will be committed, pin stable, test the refusals, check the pin independently, and show what changed against the pin on main.
set -u
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/s23-step2b.XXXXXX")
trap 'rm -rf "$W"' EXIT
D="$W/.kiln/images/harness-goose"
mkdir -p "$D"
HEAD
printf "cat > \"\$D/pin.sh\" <<'S23_PIN_SH_EOF'\n"; cat "$D/pin.sh"; printf "S23_PIN_SH_EOF\n"
printf "cat > \"\$D/pin_test.sh\" <<'S23_PIN_TEST_SH_EOF'\n"; cat "$D/pin_test.sh"; printf "S23_PIN_TEST_SH_EOF\n"
printf "cat > \"\$W/main-pin.json\" <<'S23_MAIN_PIN_EOF'\n"; git -C "$W" show origin/main:.kiln/images/harness-goose/pin.json; printf "S23_MAIN_PIN_EOF\n"
cat <<'TAIL'
chmod +x "$D/pin.sh" "$D/pin_test.sh"
cd "$W"
echo "# step 2 re-pin, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), $(gh --version | head -1), $(jq --version), $(tar --version | head -1), $(bash --version | head -1)"
echo "# scripts as run: pin.sh sha256 $(sha256sum "$D/pin.sh" | cut -d" " -f1), pin_test.sh sha256 $(sha256sum "$D/pin_test.sh" | cut -d" " -f1); main's pin.json sha256 $(sha256sum "$W/main-pin.json" | cut -d" " -f1)"
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
echo "===== the smoke it wrote, and what differs from main's pin besides it and read_at"
jq -c '.smoke' "$D/pin.json"
echo "SMOKE_AS_PLANNED=$(jq -r '.smoke == {program: "bin/goose", args: ["--version"], stdout: (" " + .version)}' "$D/pin.json")"
echo "REST_AS_MAIN=$(jq -n --slurpfile a "$W/main-pin.json" --slurpfile b "$D/pin.json" '($a[0] | del(.read_at)) == ($b[0] | del(.read_at, .smoke))')"
echo "===== pin.json sha256 $(sha256sum "$D/pin.json" | cut -d' ' -f1)"
echo "=====PIN_JSON_BEGIN"; cat "$D/pin.json"; echo "=====PIN_JSON_END"
TAIL
