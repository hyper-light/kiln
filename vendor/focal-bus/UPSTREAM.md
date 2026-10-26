# Focal's event bus, vendored

The bus Quiver is built on (docs/design/focal-bus.md, kiln's CP-T2.1).

- **Upstream:** https://github.com/hyper-light/focal, branch `bus-replicated`, commit
  `b6cf8e33b5119b5beef8fe9723455caacb30ade5` (2026-10-25), MIT (`LICENSE`): taken for the persisted
  class's simulation (kiln's CP-T2.2 step 6), whose stuck followers this bus's `425dcda` and the
  hyper-raft it takes fix (`docs/design/persisted-sim.md`); Focal's agent was told (2026-10-26). The
  first commit was agreed with Focal's agent (docs/design/focal-bus.md, "What Focal's agent said").
- **What is here:** the bus and the crates of Focal's it builds on, laid out as upstream lays them
  out:
  - `crates/focal-bus`: the event bus (Focal's docs 32 to 34): the batch, a part's log and streams,
    the node over QUIC, the replicated part over hyper-durable;
  - `crates/focal-memory`: the memory budget every byte of the bus is charged to;
  - `crates/focal-seal` and `crates/focal-platform`: the node log's keys and the files a part's
    image is installed through;
  - `crates/focal-timing`: the bus's tests' waits (a dev-dependency).
- **What it builds on:** hyper-raft's shared crates at `c6baec2` (hyper-raft's `main`), the revision
  this commit of the bus takes (Focal's `vendor/README.md`), vendored apart at `vendor/hyper-raft`.
  Focal's own copies of them (its `vendor/hyper-*`) and its other vendored crates (`aws-lc-rs`, `aws-lc-sys`) are not taken:
  the bus names the shared crates as from crates.io, and Quiver's root `Cargo.toml` (kiln's, in kiln)
  points each name at its one copy with `[patch.crates-io]`.
- **Its own workspace:** `Cargo.toml` is upstream's root manifest. Quiver's workspace excludes
  `vendor/`, so Quiver's lints don't apply here, and the crates' `workspace = true` keys resolve
  here.

## Changes from upstream

Each change is recorded in `vendor/patches/focal-bus/`, applied in this order on upstream's tree
(`vendor/MANIFEST.toml` names the commit and the paths taken; kiln's docs/vendoring.md). Each change to
the bus's code is offered to Focal as a branch, and Focal's agent is told of it; a vendoring change
alone (as the first below) isn't.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.toml` | The shared crates (`hyper-*`) named by version, without a path into Focal's `vendor/` | Quiver takes hyper-raft once, beside the bus (`vendor/hyper-raft`), and kiln holds one copy of each hyper-light product: whoever builds the bus points each name at its copy | `0001-the-shared-crates-by-name-at-the-copy-of-hyper-raft-beside.patch` |
| `crates/focal-bus/src/member.rs` | A member's liveness stream takes room for one peer where its group has none | Quiver's persisted class runs on one node as a group of one (kiln's CP-T2.2 step 4, `docs/design/persisted.md`): hyper-liveness refuses a stream of no peers, and hyper-raft elects a sole voter on its own vote. Offered to Focal | `0002-a-member-s-liveness-stream-takes-room-for-one-peer-so-a.patch` |
| `crates/focal-bus/src/member.rs`, `crates/focal-bus/Cargo.toml` | A member's log on a given file (`NodeFile`); a member opened on hyper-sim's block device under hyper-log's `Waits::Told` (`Member::open_simulated`, the `sim` feature); `Member::cut`, `serve` and `in_flight`; `Member::plant` (the `mutants` feature); the owed acknowledgements redirected in order as a member stops leading | Quiver's persisted class runs in a deterministic simulation (kiln's CP-T2.2 step 6, `docs/design/persisted-sim.md`): its host cuts and serves each member's log at the virtual time the node's disk draws, and a `HashMap`'s order sent the redirects differently each run. No production build takes the simulated paths. Offered to Focal | `0003-a-member-s-log-on-a-simulated-device-its-cuts-and-work-in.patch` |
| `crates/focal-bus/src/member.rs` | Under the `sim` feature, the member's diagnosis (`Member::core_state`): the core's role, term, leader, commit and last index, its stall, the members it suspects, its configuration and the peers its stream is attached to, each member's progress where it leads; its outbox; its Raft traffic per peer by kind and fate (sent, queued, dropped, flushed; stepped or refused and why); the heartbeats its stream refused and why; its waits as its stream judged them; the stream's flush, granularity, wake and pairs. Nothing outside the `sim` feature changes | Quiver's simulation of its persisted class (kiln's CP-T2.2 step 6, `docs/design/persisted-sim.md`) traces a node that never catches up to its cause with it: the stuck outbox Focal's `425dcda` fixed and the unmeasured stream hyper-raft's `3e54e50` fixed were found so. Offered with the simulation's other seams | `0004-the-simulation-s-diagnosis-of-a-member.patch` |
| `crates/focal-bus/src/pki.rs` | A directory's identities may be made as Ed25519 (`KeyKind`, `load_or_make_with`); `load_or_make` makes them as before, ECDSA P-256 with SHA-256 (rcgen's default), and a directory loads alike whatever they were made with | An ECDSA certificate verify is a DER document whose length is as random as its nonce, 70 to 72 bytes, so a handshake's flight was a byte longer or shorter from one run of a seed to the next: in Quiver's simulation of its persisted class (kiln's CP-T2.2 step 6, `docs/design/persisted-sim.md`) a tenth of the seeds ran apart across processes, the first handshakes apart in every run. Ed25519's signatures are 64 bytes every time (RFC 8032), and the simulation makes its identities so; twelve runs of a seed then sent the same datagrams. Offered to Focal; whether Focal's own identities should be Ed25519 is Focal's to say | `0005-identities-may-be-made-as-ed25519.patch` |

## Taking a newer upstream

`python3 scripts/vendor.py focal-bus <commit>` takes these crates at that commit, re-applies the
changes above by three-way merge, refreshes them, and runs the gate. A change that no longer applies
stops it, naming the change and the files, with nothing changed here. A new edit here is recorded
with `python3 scripts/vendor.py record focal-bus <title> --why <why>` and gets its row in the table;
`python3 scripts/vendor.py check` (in the gate) fails while an edit isn't recorded. A newer commit of the
bus may take hyper-raft at another revision: take hyper-raft at that revision with it.
