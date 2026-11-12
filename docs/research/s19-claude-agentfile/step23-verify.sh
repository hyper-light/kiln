#!/usr/bin/env bash
# Step 2's record, then step 3's checks (plan 01a1cf48e8157494ac9a51f7a564f526), in the runner's kiln clone:
# 1. the agentfiles and harness::claude tests by name at s19-claude-agentfile (step 2 committed);
# 2. step 23's real-Shards static check of every built-in, with shards v0.7.0 checked against its SHA256SUMS;
# 3. file mutations, each test failing for its own reason: (A) FIXED_ENV drifts from harness.json and the
#    pin's base from the Agentfile's; (B) the HOME volume moved under /tmp; then everything restored and green;
# 4. step 3's patch (the fetcher): the agentfiles and harness::claude tests and clippy on crates/kiln.
set -uo pipefail
carrier=$PWD
K=$HOME/.cache/s19-kiln
export CARGO_TARGET_DIR=$HOME/.cache/s19-target
cd "$K" || exit 1
git fetch -q "$carrier/s19.bundle" refs/heads/s19-claude-agentfile && git checkout -q -B s19-claude-agentfile FETCH_HEAD && git reset -q --hard FETCH_HEAD && git clean -fdq
echo "TREE $(git rev-parse HEAD)"; git status --short
t() { nice -n 10 cargo test -p kiln --lib -- "$@"; }

echo "== 1. tests by name"
t agentfiles:: harness::claude:: >"$carrier/names.log" 2>&1; echo "NAMES_EXIT $?"
grep -E '^test result' "$carrier/names.log"; grep -E '^test .*(step19|step23|every_builtin)' "$carrier/names.log"

echo "== 2. the real Shards"
d=$HOME/.cache/s19-shards; mkdir -p "$d"
if [[ ! -x $d/shards ]]; then
  curl -fsSL --proto '=https' -o "$d/shards.part" https://github.com/hyper-light/shards/releases/download/v0.7.0/shards-linux-amd64 \
    && echo "a9c9092ac5f6a7c6b306a47ef5c9d27f48f6541885abe823c0063abc2d549388  $d/shards.part" | sha256sum -c - && chmod 0755 "$d/shards.part" && mv "$d/shards.part" "$d/shards"
fi
sha256sum "$d/shards"; "$d/shards" --version 2>&1 | head -1
KILN_TEST_SHARDS=$d/shards t agentfiles::builtin::tests::step23_a_real_shards_checks_every_builtin --nocapture >"$carrier/shards.log" 2>&1; echo "SHARDS_CHECK_EXIT $?"
grep -E '^test |test result|skipped|panicked|Error:' "$carrier/shards.log" | head -12

echo "== 3. mutations"
files=(crates/kiln/src/harness/claude.rs .kiln/images/harness-claude/Agentfile .kiln/images/harness-claude/pin.json)
for f in "${files[@]}"; do cp "$f" "$carrier/$(basename "$f").bak"; done
sed -i 's/("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1")/("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "0")/' crates/kiln/src/harness/claude.rs
sed -i 's/4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10/4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da11/' .kiln/images/harness-claude/pin.json
git diff --stat | tail -3
t agentfiles:: harness::claude:: >"$carrier/mutants-a.log" 2>&1; echo "MUTANTS_A_EXIT $?"
grep -E '^test .*FAILED|^test result' "$carrier/mutants-a.log"; grep -E "env \[|BASE isn't" "$carrier/mutants-a.log" | cut -c1-260 | head -6
for f in "${files[@]}"; do cp "$carrier/$(basename "$f").bak" "$f"; touch "$f"; done
sed -i 's|VOLUME --target-kind=harness /home/claude FOR claude|VOLUME --target-kind=harness /tmp/claude FOR claude|' .kiln/images/harness-claude/Agentfile
git diff --stat | tail -3
t agentfiles:: harness::claude:: >"$carrier/mutants-b.log" 2>&1; echo "MUTANTS_B_EXIT $?"
grep -E '^test .*FAILED|^test result' "$carrier/mutants-b.log"; grep -E 'a volume at|tmp/claude' "$carrier/mutants-b.log" | cut -c1-260 | head -6
for f in "${files[@]}"; do cp "$carrier/$(basename "$f").bak" "$f"; touch "$f"; done
touch crates/kiln/src/agentfiles/builtin.rs
echo "RESTORED $(git status --porcelain | wc -l) changes"
t agentfiles:: harness::claude:: >"$carrier/restored.log" 2>&1; echo "RESTORED_EXIT $?"; grep -E '^test result' "$carrier/restored.log"

echo "== 4. step 3"
git apply "$carrier/step3.patch" && echo "STEP3 patch $(sha256sum "$carrier/step3.patch" | cut -d' ' -f1)"; git status --short
t agentfiles:: harness::claude:: >"$carrier/step3-tests.log" 2>&1; echo "STEP3_TEST_EXIT $?"
grep -E '^test result|^test .*(step19|FAILED)|panicked|error(\[|:)' "$carrier/step3-tests.log" | head -40
nice -n 10 cargo clippy -q -p kiln --all-targets -- -D warnings >"$carrier/step3-clippy.log" 2>&1; echo "STEP3_CLIPPY_EXIT $?"; tail -40 "$carrier/step3-clippy.log"
