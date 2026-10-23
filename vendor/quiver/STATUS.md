# Status

Updated 2026-10-22. A piece is done when its tests pass in the gate (`scripts/gates.sh`: Linux, with the
macOS and Windows lints) and the evidence its requirement names is recorded.

## Implemented

- **The bar** (kiln's CP-M1): a Cargo workspace whose lints deny panics, this repository's rules
  (`CLAUDE.md`), the requirement registry (`docs/requirements.toml`), the design (`docs/design/`), the
  gate (`scripts/gates.sh`, run by kiln CI from `.kiln/ci.yaml`), and the vendoring tool
  (`scripts/vendor.py`, kiln's).
- **Focal's bus, vendored** (CP-T2.1): `bus-replicated` at `f8052a5` in `vendor/focal-bus`, and
  hyper-raft at the revision it takes, `25cf889`, hyper-raft's `main` since its two lines met, in
  `vendor/hyper-raft` (each one's `UPSTREAM.md` lists Quiver's recorded changes;
  `python3 scripts/vendor.py check` proves both).
- **`quiver-api`** (CP-T2.1): the `Topics` trait other code reaches Quiver through, its simulator,
  and its conformance suite of 19 clauses (`docs/design/topics.md`, held to the code by a doc-truth
  test).
- **The ephemeral class on one node** (CP-T2.1, `quiver::Node`): topics of messages in this process,
  each message a Focal batch charged to Focal's memory budget; reads from any offset kept,
  subscriptions that share what they read, a typed lag notice for a subscriber that falls behind,
  and no publish that waits on a reader (`docs/design/focal-bus.md`). It passes the suite; a stalled
  subscriber among 100 delays no other (`crates/quiver/tests/stalled.rs`). Kiln's live feed of
  every session rides it.
- **GroupStore mandated** (CP-T2.1, T-T2.1.2): hyper-durable's `RamStore` is for tests alone, and a
  build that names it outside one fails (`crates/quiver/tests/ram_store.rs`).
- **The `quiver` binary** says what it is (`quiver --version`, `quiver --help`), and nothing more yet.
- **The bench** (`crates/quiver-bench`, `bench/README.md`): the ephemeral class's fan-out and soak,
  and the comparison, class for class, with NATS core, Redis Pub/Sub and Kafka on the same machine
  (`bench/compare.sh`, each broker in Docker on CPUs of its own), every system on the same open-loop
  schedule, measured by the same harness.

## Next

- **CP-T2.1's measurements**: the fan-out bench, the hour's soak and the comparison
  (`bench/README.md`) on fedora, in a quiet window. The bar for the comparison: Quiver's p99 and
  p99.9 an order of magnitude under the best competitor's, load by load.
- **CP-T2.2**: The persisted class's 1.4 s tail fixed at its cause (a disk stall never costs leadership), cheaper elections, and kiln's CI and deploy events on the persisted class, on Focal's replicated part.
- **CP-T2.3**: Typed acknowledgements stating the fate reached and its loss bound.

## Open

Everything else the track's plan names (kiln plan `01a157fdefc37396957ea10adb51a19f`): each goal, its criteria and how each is
measured are in `docs/requirements.toml`.
