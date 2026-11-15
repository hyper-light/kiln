# Experiments: a live split of an agent's new sessions between deployments

As the person defined it (2026-11-14): "you deploy an A|B experiment with the given A|B|etc. groups at the specified
proportion/percentage. Each time a session starts, that session will attach to/spawn one of the variants/groups with
respect to its percentage." Variants are deployments (Agentfile, engine, vCPUs, memory), measured on their own
measures, with no model tokens (decision 4); footprint is peak and steady, and a host variant is allowed (decision 8).
Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P7.1, P7.2. Rules: [principles.md](principles.md).

## A variant is a swatch and a name, never a letter

- Its swatch: its place along kiln's prism, the colour of its slice of the split bar, a short bar (letters in squares
  are kiln's research grades). The current one is ringed.
- Its name: one a person gave it (renamed in place), else what sets it apart from the others, every dimension that
  differs in the order Agentfile, engine, vCPUs, memory: "2 vCPU · 4 GiB" beside "4 vCPU · 8 GiB", or
  "docs-site · 2 vCPU · 4 GiB" among rust-dev variants. Two alike in all of that get their place: "(2)".
- `ui/src/views/harness/variants.ts`, tested in `ui/tests/variants.test.ts`. As many variants as are added.

## Where

The EXPERIMENT group of an agent's dropdown ([agents.md](agents.md)): one line (the split's swatches, weights,
sessions, its state, pause) until opened; then a row per variant: swatch and name, measures (warm ms, peak GiB,
failed %, each its median with a ✓ where its 95% interval beats every other's), its weight slider, Promote (asked in
place, Undo in its toast). Dragging a weight moves the others in proportion (always 100) and stages a new split,
hatched until Apply; Discard drops it. New experiment (⋯) sets one up in place: variants added without a cap, each a
deployment and a weight, then Start. A session's variant shows as its swatch and name on its row, in the Agents tab
and beside its version.

## Open

- Replay (re-running past sessions' inputs on another deployment): designed in the standalone mockups, not built.
- The server's own experiment routes; the mock's experiments live in `ui/mock/agentdefs.ts` and the UI changes them
  locally.
