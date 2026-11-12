# Step 2: Cursor's archives and the one unpacker

Plan 01a1ce705e5c7504b88732f443d99288, step 2 (v7), for kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 27.

**Ended, as the person's rule says** (ROOT's goal for step 27, 2026-11-12): "UNPACKING is step 25's agent's alone
… one unpacker for every step, landing on kiln main. Don't build your own: end your plan step that unpacks
archives, and build on kiln main once step 25's unpacker lands. Until then … merge everything else and say what
waits on it." Step 27 lands no unpack code. Cursor publishes no build that needs no unpacking (its Linux CLI
comes only as `agent-cli-package.tar.gz`), so everything else of step 27 is merged, and this is what waits.

## What waits on the unpacker

On kiln main without it, `kiln agentfiles context cursor` refuses Cursor's archive (step 19's fetcher,
`context::Error::Unpack`: "is an archive to unpack, which this build of kiln can't do yet"). Once the one
unpacker and the fetcher's hook are on main, the same command makes Cursor's context with no change from
step 27: its pin already says `unpack` as the agreed `Tree` (format tar.gz, strip 1, unpacked_bytes, entries).
Step 27's first step-2 agent's module stayed as notes (`NOTES.md`, `unpack.md`, and its logs in `evidence/`).

## Which unpacker, as of 2026-11-12 19:20 UTC

The one unpacker is step 25's module (a04cffd0c, `s25-unpack`, unmoved since 01:35 PST) as step 27's step-2
agent converted it to the form steps 26 and 27 agreed: 4d1f96a5d. Every branch that hooks an unpacker into the
fetcher builds on 4d1f96a5d, and none was on kiln main:

| Branch | Head | What it adds |
|---|---|---|
| step 28's `s28-unpack-hook` (since folded into step 26's, and deleted) | 8e571d7d8 | the fetcher's hook, 73fc55df9: archives through the unpacker, trees first; a failed context discarded as the unpacker discards its staging |
| step 26's `s26-unpack-core` | f845ece8c | step 28's hook, and a member held `alone` (10a6ffe1f) |
| step 20's `s20-unpack` | 457e5c81a | its own hook (40af1af26) and a tree's `skip` (c2f5e6e5e) |

Checked ahead of its landing (`premerge/`): this branch merged with 8e571d7d8 cleanly, its agentfiles tests
passed 60 of 60, and kiln's own binary made Cursor's context from Cursor's archives, each platform exactly the
pin's tree: 435 entries, 562236246 bytes (amd64) and 552211752 bytes (arm64), modes 0755 and 0644 alone, the
embedded harness.json, and `cursor-agent --version` printing the pin's version.

## What was decided about the refusals step 27 asked for

Both stand in the one unpacker as 4d1f96a5d made them, and in step 26's branch that carries it:

- **The sticky bit** (S_ISVTX, 0o1000) is refused, not cleared, as set-ID bits are: `Why::Sticky`, an entry's
  or a pinned `mode`'s. The code records whose word each is: "step 25's coordinator for set-ID, step 27 for
  both"; pin.sh refuses them at pin time too. Cursor's archives hold neither (`../evidence/archive-entries-x64.txt`:
  no set-ID entry; files 0644 and 0755, directories 0755).
- **A `harness.json` at the tree's root**, ASCII case aside as a case-insensitive file system takes names, is
  refused: `Why::HarnessFile`, "where kiln writes the harness's own". Cursor's archive has none.
