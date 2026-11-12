#!/usr/bin/env bash
# Gives back this step's runner space (R16): ~/.cache/s25-step3, which only this step's scripts
# write, all of it regenerable (build targets, checkouts at depth 1, the context, Shards' store).
# The evidence lives in the worktree, not here. Says what it held first.
set -euo pipefail
ROOT="$HOME/.cache/s25-step3"
[ -d "$ROOT" ] || { echo "$(hostname): nothing at $ROOT"; exit 0; }
echo "$(hostname): $(du -sh "$ROOT" | cut -f1) at $ROOT"
du -sh "$ROOT"/* 2>/dev/null | sort -h | tail -6
rm -rf "$ROOT"
[ -e "$ROOT" ] && echo "still there" || echo "removed"
df -h "$HOME" | tail -1
