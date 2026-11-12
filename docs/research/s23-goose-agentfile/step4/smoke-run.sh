#!/usr/bin/env bash
# Step 4's runs (plan 01a1ce705e3d72af83ef15ae83b62f45): prints a self-contained script that carries
# this tree's smoke.sh, pin.json, Agentfile and harness.json to a runner and runs smoke.sh there with
# the shards build-shards.sh built. Run from the worktree:
#
#   docs/research/s23-goose-agentfile/step4/smoke-run.sh all > smoke-linux.sh    # Linux with KVM
#   docs/research/s23-goose-agentfile/step4/smoke-run.sh arch > smoke-mac.sh     # a Mac Studio
#   kiln run --no-sync --on RUNNER -- "$(cat smoke-linux.sh)"
set -euo pipefail
mode=${1:?all or arch}
W=$(git rev-parse --show-toplevel)
G=$W/.kiln/images/harness-goose
put() { # name file
    printf "cat > \"\$I/%s\" <<'S23_%s_EOF'\n" "$1" "${1//[^A-Za-z]/_}"
    cat "$2"
    printf 'S23_%s_EOF\n' "${1//[^A-Za-z]/_}"
}
cat <<'HEAD'
#!/usr/bin/env bash
# Step 4's run on a runner (plan 01a1ce705e3d72af83ef15ae83b62f45): smoke.sh with this tree's inputs.
set -u
mkdir -p "$HOME/.cache/s23-step4"
I=$(mktemp -d "$HOME/.cache/s23-step4/inputs.XXXXXX")
HEAD
put smoke.sh "$W/docs/research/s23-goose-agentfile/step4/smoke.sh"
put pin.json "$G/pin.json"
put Agentfile "$G/Agentfile"
put harness.json "$G/harness.json"
cat <<TAIL
mode=$mode
TAIL
cat <<'TAIL'
if command -v sha256sum >/dev/null; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi
echo "# step 4 run, $(date -u +%FT%TZ), host $(hostname) $(uname -srm), mode $mode"
(cd "$I" && sum smoke.sh pin.json Agentfile harness.json)
S="$HOME/.cache/s23-step4/target/test-release/shards"
echo "# shards $("$S" --version 2>&1 | head -1), sha256 $(sum "$S" | cut -d' ' -f1)"
W="$HOME/.cache/s23-step4/run-$(date -u +%Y%m%dT%H%M%SZ)"
bash "$I/smoke.sh" "$S" "$I/pin.json" "$I/Agentfile" "$I/harness.json" "$W" "$mode"
echo "SMOKE_EXIT=$?"
TAIL
