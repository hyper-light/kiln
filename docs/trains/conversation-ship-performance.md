# Train notes: conversation and Ship performance

This train serves W21-R02, W21-R13, W21-R15, W26-R15 and W26-AC10.
The person authorized shipping the performance work and fixing the seven failures from its
earlier broad Rust run on 2026-11-09.

## Behavior

Conversations acquire their initial visible message window and publish a consistent initial
snapshot, then request older history by cursor when it is needed. Cursor pages have one owner:
returning to Latest cancels the native request, and a late result cannot change the current
window. HTTP cursor reads bypass the generic shared GET wrapper so its native AbortSignal is
preserved. Browser-only history metadata is ignored by the separate BP transport.

The timeline mounts a bounded window with pinned rows for ongoing interaction. Subagent previews
have one visible copy of the current text. Agent layout is fitted synchronously before the new
surface is captured for its transition; animations retain their authored duration. Route code,
fonts and entry modules are available earlier, while Inspector and Ship load their expensive
sections when needed. Intent-based navigation preparation uses the existing canonical resources;
there is no separate proactive conversation data cache.

Authenticated bootstrap data is request-scoped and private. API JSON and static assets negotiate
compression with Accept-Encoding, preserve Vary and content types, and reject requests that
explicitly refuse every available coding. The UI server carries precompressed assets and its
entry preload inventory in the production pack.

Review fix limits now use a durable publication boundary and version, including when publishing
and fixing occur in the same second. Migration `20261109-0100_w26_fix_boundary` adds nullable
columns and publication triggers; legacy comments retain a conservative timestamp fallback.
Routing and Ship share the same deterministic counter.

## Validation

The combined UI passed typecheck, lint, all 660 unit tests and the production build. Its 204
selected browser cases completed with 194 passes, ten existing skips and zero failures or
retries, in Chromium and WebKit at desktop and phone widths. These include synchronous first
geometry, native transitions, history cancellation, subagent cards, Plan, Inspector and Ship.

Linux all-targets Clippy passed. Native macOS and Windows checks cover their platform boundaries.
The relevant Linux unit and integration suites include actual Compose logs, the 200-job Docker
pool burst with its original scaling assertions, harness switching, socket cleanup, review
limits, conversation history and HTTP compression. Full raw outcomes and source manifests are
kept in the shipping evidence; the final release record identifies the exact tested source.

The earlier production benchmark is a separate, explicitly identified cohort. It used seven
trials per arm against t3code, with matched 20-message API snapshots. Kiln's default UI window
was 16 messages versus t3code's 20. DOM readiness, native paint and authored animation completion
are distinct observations. Those measurements must not be relabeled as measurements of this
ported landing tree.

## Release procedure

Production is the dedicated bare-metal server at `192.168.1.90:7070`, supervised by
`kiln-server.service`. Its executable is `~/.local/share/kiln-server/bin/kiln`, separate from
every runner. As documented in the Mission Control attention train, the retired KilnCD
Kubernetes app does not update this serving process. Use the existing server-only atomic binary
promotion path; do not restart, replace or upgrade a runner.

Build the exact main landing commit with the production release profile, its Linux Bun and the
validated UI pack. Preserve the previous executable. Before promotion, start the candidate
against a private online SQLite backup in a network namespace with local agents, discovery,
CI/CD, updates and cluster replication disabled. Verify readiness, embedded commit, compression,
Agents and Ship pages, representative asset hashes and the publication migration. Then start
the previous executable against the same migrated private database to verify rollback.

After promotion, verify the actual serving commit, readiness, compression and built UI hashes.
Observe sixty real ten-second health samples with no additional restart or OOM event. Record
session inventory and runner policies before and after. Preserve rollback and stop only
task-owned test processes. The release evidence records whether these steps have actually
completed; this procedure alone is not a deployment claim.
