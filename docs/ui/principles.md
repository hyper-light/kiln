# Kiln UI principles for the kiln × Hex × Shards surfaces

One line each. They bind the Sandbox/Host and Agents tabs, Mission control's dropdown, Runners and the Agentfile page.
"Built" says where this branch (p/kilnui-design-v1) stands against each rule.

## ROOT's seven (corrected 2026-11-14)

1. Expand in place: one expanded row per list, its header sticky, collapse restores scroll, j/k/Enter/Esc, the open row in the URL (`?open=`), Back collapses it. Built: in place, and `?open=` on Mission; not yet: one-at-a-time, sticky header, keys, Back.
2. Peeks open on hover, focus and a tap on the name, read-only with safe controls (open, pause, resume), never a destructive one. Built: none yet.
3. Every row icon has a tooltip and an aria-label; stop and remove confirm inline on the row ("Stop? ✓ ✕"), never in a modal; reversible acts (pause) skip the confirm and offer Undo in a toast. Built: yes, on every new row.
4. ⌘K, or Ctrl+K on Windows and Linux, with a visible header affordance; its entries come from kiln's ops registry, so the palette and the ops can't drift. Built: kiln's palette as it is; no entries for the new ops yet.
5. Editing makes a new definition version: running sessions keep theirs, new ones get it; saves are explicit and carry their base version; a save in between shows as an inline diff, never a silent overwrite; validation runs as you type. Built: versions and inline edits (mock); not yet: base-version conflict diff, live check.
6. Live telemetry: one multiplexed stream per page, no updates to rows off screen, DOM writes batched per frame, long lists virtualized, one SVG path per sparkline downsampled server-side to a point per pixel. Built: polling per resource (2–5 s) against the mock; the stream is the server's work.
7. A chat row is live only while on screen; further back it shows its state then, "as of" a time, with ↻ to go live; chat's controls carry the person's grants. Built: "as of" and ↻ on the definition card; not yet: on-screen liveness.

## The person's (2026-11-14), binding everywhere

- No sentences; labels of one to three words; three text sizes; nothing wraps (a directory is never cut: one path a line); one left edge; visuals first (bars, dots, glyphs); rows under 40px.
- No drawers, tabs-as-pages or popups: expand in place, controls on the row, rare acts behind ⋯.
- kiln's own colours and motion only: `--prism`, prism-breathe, the stand colours, status dots.
- A variant is never a letter (letters in squares are kiln's research grades): its prism swatch, the colour of its split slice, and a name, by default what sets it apart ("4 vCPU · 8 GiB"), renamed in place.
- Agent management lives in a row's ⫷ dropdown, after its subagents: SUBAGENTS, DEFINITION, SESSIONS, EXPERIMENT, in that order, one line each until opened, in the tree's own columns.
- Where we are now first (CPU, memory and disk as live bars, kiln's LimitMeter), then what it is and its controls, then plots over time, full width, last.
- Every row says where it runs: its engine glyph (microVM or host), its runner, and how it started (flame warm, layers template, snowflake cold) with its milliseconds.
- Engines decide what shows: on the host, no network, vCPU pause, Hex box, image, footprint, pool, start kind or secret names, and one line says "Runs on the host, without a microVM"; both get the bars, plots, restart, stop, volumes, allowed directories and scope.
- Network is default deny and says so: "nothing else" after the grants, "nothing granted" when there are none.
- kiln's one-click harness switch stays where and how it is; a component row points to it.
- A press shows at once (W21-R15); zero console errors; layout shift 0.
