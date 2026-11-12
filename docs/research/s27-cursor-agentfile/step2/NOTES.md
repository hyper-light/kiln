# Step 2's work, kept as notes (not landed)

Step 27's step-2 agent (kiln agent 01a1cf3fbf0c72f38531c455cc4647f2) built Cursor's Tree on step 25's
unpacker as kiln commit 4d1f96a5dfcdc130393b656f6eca4c7d79c399c3, on branch s27-unpack (parent kiln
main 94639db7b, signed): the unpack enum with required fields, a tree staged and renamed into place
whole, totals checked as entries come, modes 0755/0644, the refusals Cursor's plan asks for, 13 unit
tests passing on Linux x86_64 (adacachy2) and macOS arm64 (adas-mac-studio.local-2), 14 of 14
mutations killed, clippy passing on adacachy2. Its own record is unpack.md here, with evidence/.

ROOT ruled on 2026-11-12 that step 25's unpacker (branch s25-unpack, agent 01a1ceb7914e702aa9059e6dfdd3c79a)
is the one unpacker for steps 19 to 28, landing on kiln's main on its own, its fields optional so that
every pin as written parses. So this commit is not landed: it stays on s27-unpack as a reference for
step 25. Step 20's s20-unpack (b41d1f018) merged it before the ruling; ROOT was told.

Step 27's step 2 is met when step 25's unpacker is on main and unpacks a Cursor-shaped archive.

## Cursor's Tree needs, for step 25 to weigh

From step 27's plan (step 2) and Cursor's pinned packages (pin.json at b3edce7e2: linux/amd64
`{"format": "tar.gz", "strip": 1, "unpacked_bytes": 562236246, "entries": 435}`, linux/arm64
`{"format": "tar.gz", "strip": 1, "unpacked_bytes": 552211752, "entries": 435}`):

- (a) Refuse the sticky bit as well as set-ID (0o1000 beside 0o6000), not clear it with `mode & 0o777`.
  Cursor's packages hold none, so this changes nothing for them; it's the plan's refusal list.
- (b) Refuse an entry landing on the per-arch directory's own harness.json, whatever order materialize
  writes in: as `Exists` when harness.json is there first (step 25's order), explicitly when the tree
  goes in first. s27-unpack refuses it ASCII case aside (`Harness.JSON`), as a case-insensitive file
  system would collide them.
- (c) Counts, as pin.sh measured them and s27-unpack checks them: `entries` = every directory and
  regular-file entry, dist-package/ itself included, pax and GNU long-name headers not (`tar -tzf | wc
  -l`; x64 435 = 353 files + 82 directories); `unpacked_bytes` = the regular files' header sizes summed
  (x64 562236246, arm64 552211752). s27-unpack also holds both as entries come, so nothing past the
  pinned bound is written, and again at the end.
- dist-package/ itself is an entry: passed over as the leading directory the strip drops, its mode the
  tree root's. Cursor's modes are only 0644 and 0755, so `mode & 0o777` and 0755/0644 agree on them.
- The plan's "the per-arch directory appears only once the whole archive has unpacked" holds under step
  25's merge only if materialize stages the per-arch directory itself and renames it into place; the
  check on Cursor's archive should look for that.

## The branch

s27-unpack (worktree ~/Projects/kiln/.claude/worktrees/s27-unpack) is kept and unmerged at
4d1f96a5dfcdc130393b656f6eca4c7d79c399c3: step 20's s20-unpack (b41d1f018) merged it, and its tests
and mutants may serve step 25.
