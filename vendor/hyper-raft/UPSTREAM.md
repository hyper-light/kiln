# hyper-raft's shared crates, vendored

The crates Focal's bus is built on (`vendor/focal-bus`), at hyper-raft's `main`.

- **Upstream:** https://github.com/hyper-light/hyper-raft, commit
  `c6baec2de3d0520fc422c6b8f112480c2de817e8` (2026-10-25), MIT (`LICENSE`): hyper-raft's `main`, and
  the revision Focal's `bus-replicated` at `b6cf8e3` takes its shared crates at (Focal's
  `vendor/README.md`). kiln holds one copy, which Mantle (kiln's T10) builds on too.
- **What is here:** upstream's root manifest and the fifteen crates, whole, at upstream's paths:
  `hyper-block`, `hyper-datagram`, `hyper-durable`, `hyper-liveness`, `hyper-log`, `hyper-measure`,
  `hyper-quic`, `hyper-raft`, `hyper-rt`, `hyper-rt-transport`, `hyper-seal`, `hyper-sim`,
  `hyper-timing`, `hyper-tls`, `hyper-transport`.
- **How it's built:** Quiver's root `Cargo.toml` points the names Focal's bus takes them by at these
  folders (`[patch.crates-io]`); kiln, vendoring Quiver, holds the same revision with the same
  changes, its own crates built on it too.

## Changes from upstream

Each change is recorded in `vendor/patches/hyper-raft/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `crates/hyper-durable/src/lib.rs`, `crates/hyper-durable/Cargo.toml` | `RamStore` compiled only for the crate's own tests or under the `test-store` feature, which a dev-dependency asks for (the crate's own tests ask through one on itself) | the write-ahead log is mandatory in Quiver and kiln: `GroupStore` on a real device, `RamStore` test-only, and a build that names it outside a test fails (kiln's CP-T2.1, T-T2.1.2; Quiver's `crates/quiver/tests/ram_store.rs`) | `0001-ramstore-for-tests-alone-the-write-ahead-log-mandatory.patch` |
| `crates/hyper-log` (`lib.rs`, `group.rs`, `device.rs`, `owner/`, `tests/told.rs`, `tests/log.rs`); `crates/hyper-rt` (`sim.rs`, `udp/mod.rs`, `tests/udp.rs`); `crates/hyper-rt-transport` (`driver.rs`, `lib.rs`, `tests/simulation.rs`); `crates/hyper-raft` (`mutant.rs`, `raft.rs`, `tests/check.rs`); `crates/hyper-durable` (`replica.rs`, `Cargo.toml`) | The simulation's seams. hyper-log's `Waits::Told`: a frame only at a cut (`Log::cut`), its confirmation made one way, a waking fetch only at a serve (`Log::serve`); its restore at open and a caller that waits on its write or fetch tell it by waiting. hyper-rt's `sim::anchor` and hyper-rt-transport's `shard_instant`: one clock for every driver on a simulated shard. A simulated bind takes the port asked for in its shard. `Mutant::OwnLogCommit`, reached through `Replica::plant` (`mutants` features) | Quiver's persisted class runs in a deterministic simulation (kiln's CP-T2.2 step 6, `docs/design/persisted-sim.md`). No production build takes these paths: told and serve need `Waits::Told`, the anchor and binds a simulated shard, the mutant a feature no consumer enables. Offered upstream as hyper-raft's local `cp/t2.2-told` | `0002-the-simulation-s-seams-a-told-log-one-clock-on-simulated.patch` |
| `crates/hyper-rt` (`sim.rs`, `tests/udp.rs`) | A simulated path may duplicate datagrams: `SimPath::with_duplicates(ppm)`, each copy after a delay of its own and kept in the flow's order where the path keeps it, counted in `SimFabricStats::duplicated`. A path told nothing draws nothing for it | Quiver's simulation of its persisted class puts duplicated messages on the fabric (AC-T2.2.5). Simulated paths only; every other scenario runs as it did. Offered upstream on the same branch | `0003-a-simulated-path-may-duplicate-datagrams.patch` |
| `crates/hyper-liveness/src/pair.rs` | A liveness stream whose floor is nothing (its first flush answered at the instant it was asked, its timer not yet measured) takes the interval it has now while its schedule keeps none, never nothing, and the interval a heartbeat sent keeps is never nothing | In Quiver's simulation of its persisted class a node restarted into its group kept a spacing of nothing: every heartbeat due at the instant it was asked, none proven, none sent, no wake asked, its timer never measured, every peer's heartbeat refused as Unmeasured, and its leader suspected it for good (six seeds the bus's and hyper-raft's newer fixes left stuck; `docs/design/persisted-sim.md`). Nothing changes for a floor above nothing. Offered upstream on the local branch `cp/t2.2-liveness` (`a91f1e0`, on `c6baec2`), its test failing without it | `0004-a-liveness-stream-whose-floor-is-nothing-still-sends.patch` |
| `crates/hyper-rt` (`sim.rs`, `tests/sim_path.rs`) | A simulated loss burst ends in time on a quiet flow: `SimLoss::in_time(slot_ns)` steps the Gilbert–Elliott chain once for each slot a flow stays quiet past the first, as well as once per datagram, at most `QUIET_STEPS_MOST` (1,024) at once. A process not told steps and draws as it did | Stepped per datagram alone, a flow that sent rarely stayed in a burst for as long as it sent little: in Quiver's simulation of its persisted class a restarted publisher's QUIC handshake to its leader, its probes backing off to 16 s apart, met one burst for 30 s and had not connected when the run ended (seeds 6302 and 7452; `docs/design/persisted-sim.md`). Simulated paths only; every other scenario runs as it did. Offered upstream on `cp/t2.2-told` (`5b13b2b`), its test failing without it | `0005-a-simulated-loss-burst-ends-in-time-on-a-quiet-flow.patch` |
| `crates/hyper-quic/src/bloom_token_log.rs` | A token log's first period begins where the first token it checks began its own, its expiry less the lifetime, that token in the second filter, where it began at that token's expiry: a valid token issued before the first checked is taken once, and refused the second time, as any token is | The log refused such a token as too far in the past, and the client went on unvalidated (Retry under load, the anti-amplification limit) though its token was good. Tokens carry their issue time in whole seconds of the wall clock: in Quiver's simulation of its persisted class whether a run's tokens straddled a second decided whether a server refused one, and a tenth of the seeds ran apart from one process to another (`docs/design/persisted-sim.md`). Offered upstream on the local branch `cp/t2.2-token-log` (`20cdaf9`, on `c6baec2`), its test failing without it | `0006-a-token-log-takes-a-valid-token-issued-before-the-first-it.patch` |

The first change is Quiver's and kiln's policy, not hyper-raft's, so it isn't offered upstream: hyper-durable
calls `RamStore` slates' case (each write complete as it's submitted), and gating it in hyper-raft
would reach slates. Focal's agent agrees (2026-10-22), and notes that Focal names `RamStore` only in a
test module (`focal-consensus`'s `shell_tests.rs`): sharing this copy, it would ask for `test-store`
as a dev-dependency, as kiln's sync tests do.

## Taking a newer upstream

`python3 scripts/vendor.py hyper-raft <commit>` takes these crates at that commit and re-applies the
changes above by three-way merge; take it at the revision Focal's bus takes.
