#!/bin/bash
# Byte-exact excerpts of the sources the research cites, each line prefixed path:line.
set -u
E=$HOME/.local/share/s22-opencode-research/evidence
K=$HOME/Projects/kiln
SH=/Users/adalundhe/Projects/shards/.claude/worktrees/g121
OC=$HOME/.cache/g121-s22/oc2
ex() { # repo rev file ranges...
  local repo=$1 rev=$2 f=$3; shift 3
  local body; body=$(git -C "$repo" show "$rev:$f") || { echo "MISSING $rev:$f"; return; }
  for r in "$@"; do printf '%s\n' "$body" | awk -v a="${r%-*}" -v b="${r#*-}" -v p="$f" 'NR>=a && NR<=b {print p":"NR": "$0}'; done
}
KR=$(git -C "$K" rev-parse origin/main)
{ echo "kiln origin/main $KR ($(git -C "$K" log -1 --format=%ci "$KR")); step 30 branch p/agentfile-store-v1 at d8b829869"
  ex "$K" "$KR" crates/kiln/src/harness/kinds.rs 55-55 101-112 166-171
  ex "$K" "$KR" crates/kiln/src/harness/acp.rs 249-252 466-476
  ex "$K" "$KR" crates/kiln/Cargo.toml 36-36 113-113 133-133
  ex "$K" "$KR" crates/kiln/src/cd/targets/aws_static.rs 134-136
  ex "$K" "$KR" docs/ci.md 4413-4414
  ex "$K" "$KR" crates/kiln/src/runner/runtime.rs 3767-3767 3946-3946
  ex "$K" "$KR" AGENTS.md 44-71
  ex "$K" d8b829869 crates/kiln/src/agentfiles/builtin.rs 1-4 8-18
} > "$E/e_src_kiln.txt"
SR=$(git -C "$SH" rev-parse origin/main)
{ echo "shards origin/main $SR ($(git -C "$SH" log -1 --format=%ci "$SR"))"
  ex "$SH" "$SR" crates/init/src/domains.rs 1-25 40-42 486-497 2447-2453
  ex "$SH" "$SR" docs/architecture/AGENTFILE_ARCH.md 171-184 256-276
  ex "$SH" "$SR" docs/examples/harness-claude/Agentfile 9-24
  ex "$SH" "$SR" docs/research/measurements/harness-bringup-v1/records/codex-login-volume/harness.json 1-1
  ex "$SH" "$SR" docs/development-rules.md 41-46 110-123 133-138
  echo "c3b7bde8b: $(git -C "$SH" log -1 --format='%h %ad %s' --date=short c3b7bde8b)"
  git -C "$SH" merge-base --is-ancestor c3b7bde8b v0.7.0 && echo "startup.rs (c3b7bde8b) IN v0.7.0" || echo "startup.rs (c3b7bde8b) NOT in v0.7.0"
  git -C "$SH" merge-base --is-ancestor 1da9132b7 "$SR" && echo "1da9132b7 IN origin/main $SR"
} > "$E/e_src_shards.txt"
OR=$(git -C "$OC" rev-parse HEAD)
{ echo "opencode v2.2.7 = $OR (https://github.com/anomalyco/opencode/tree/v2.2.7)"
  ex "$OC" "$OR" packages/cli/src/acp/capabilities.ts 1-15
  ex "$OC" "$OR" packages/cli/src/commands/handlers/acp.ts 24-35
  ex "$OC" "$OR" packages/cli/src/services/standalone.ts 17-33
  ex "$OC" "$OR" packages/cli/src/server-process.ts 72-73 117-121
  ex "$OC" "$OR" packages/cli/src/services/updater.ts 534-542
  ex "$OC" "$OR" packages/cli/src/commands/handlers/default.ts 62-71
  ex "$OC" "$OR" packages/cli/src/database-path.ts 4-13
  ex "$OC" "$OR" packages/cli/src/index.ts 148-156
  ex "$OC" "$OR" packages/util/src/global-roots.ts 4-19
  ex "$OC" "$OR" packages/util/src/global.ts 12-27 63-80
  ex "$OC" "$OR" packages/core/src/ripgrep/binary.ts 14-38 120-139
  ex "$OC" "$OR" packages/core/src/filesystem/search.ts 225-234
  ex "$OC" "$OR" packages/core/src/plugin/provider/sdk-factory.ts 5-11
  ex "$OC" "$OR" packages/core/src/provider.ts 204-226
  ex "$OC" "$OR" packages/core/src/persistent-pty/binary.bun.ts 8-13 15-28
  ex "$OC" "$OR" packages/cli/script/build.ts 40-58
  ex "$OC" "$OR" .github/workflows/release.yml 584-604
  ex "$OC" "$OR" services/site/src/docs/content/index.mdx 33-38
  ex "$OC" "$OR" services/site/src/docs/content/cli/providers.mdx 128-140
  ex "$OC" "$OR" services/site/src/docs/content/cli/acp.mdx 6-20
  ex "$OC" "$OR" services/site/src/docs/content/cli/index.mdx 72-83
  ex "$OC" "$OR" packages/web/src/content/docs/index.mdx 10-13
  ex "$OC" "$OR" packages/web/src/content/docs/cli.mdx 756-758
  ex "$OC" "$OR" LICENSE 1-5
} > "$E/e_src_opencode.txt"
wc -l "$E"/e_src_*.txt
