# hyper-raft's shared crates, vendored

The crates Focal's bus, Quiver and Mantle are built on (`vendor/focal-bus`, `vendor/quiver`,
`vendor/mantle`), at one revision of hyper-raft for all of them.

- **Upstream:** https://github.com/hyper-light/hyper-raft, commit
  `e0613f524b0df198cc61dc1e82b33c22d604cf7b` (2026-11-13), MIT (`LICENSE`): hyper-raft's branch
  `p/q35-handshake-progress-v1`, not its `main` (`045cf3ff`, which it is one commit past; the branch
  becomes `main` once its consumers' suites pass), the revision Hex and Shards' control link
  (`crates/remote`) take: its hyper-quic gives a connection its negotiated idle timeout (patch Q34,
  `main`'s) and says how far a handshake has advanced (patch Q35, the branch's own), taken with Hex's
  `e19198bd` and Shards' `fa953e3c` (kiln `7096a82cf`; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step
  17). Before it, `045cf3ff` (2026-11-13), hyper-raft's `main` with Q34, taken with Hex's `c7603b0b`
  (kiln `64f4fb537`); before that, `fb74b59` (2026-11-07), which holds what
  Mantle's `main` at `87ab09e1` snapshots its hyper-* crates at (`5e40eb9`: `0a182ae` and the driver
  fix, a driver reading what its sockets took before it judges its timers, upstream's
  docs/transport.md §4ap), taken with Quiver's `966ea139` for kiln's refresh of Mantle, so kiln holds
  one copy (CP-M1); before that, `main` at `f45fc56` and `8203466` (2026-11-07, a
  member's catch-up read off the log's device and kept a run ahead, for kiln's API runtime program),
  and `0856e05` (2026-11-02), the merge of hyper-raft's `main` (`22d5037`, kiln's CP-T2.2 line) and
  Mantle's `progress-notes-reachable` (`0a182ae`, what Mantle's `main` at `17eb8032` snapshots), made
  for kiln's CP-T10.2; before that, `25cf889` (2026-10-22), `main` since its `main` and `fate-commit`
  lines met (the merge `7d5e536`), which Focal's `bus-replicated` at `f8052a5` took.
- **What is here:** upstream's root manifest and sixteen crates, whole, at upstream's paths:
  `hyper-block`, `hyper-datagram`, `hyper-durable`, `hyper-liveness`, `hyper-log`, `hyper-measure`,
  `hyper-quic`, `hyper-raft`, `hyper-rt`, `hyper-rt-transport`, `hyper-seal`, `hyper-sim`,
  `hyper-swim` (Mantle's, since 0856e05), `hyper-timing`, `hyper-tls`, `hyper-transport`.
- **How it's built:** Quiver's root `Cargo.toml` points the names Focal's bus takes them by at these
  folders (`[patch.crates-io]`); kiln, vendoring Quiver and Mantle, holds the same revision with the
  same changes, its own crates built on it too, but for hyper-rt: kiln builds every crate here on its
  own copy of it, `crates/kiln-rt` (the second change below), and `crates/hyper-rt` stays upstream's.

## Changes from upstream

Each change is recorded in `vendor/patches/hyper-raft/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `crates/hyper-durable/src/lib.rs`, `crates/hyper-durable/Cargo.toml` | `RamStore` compiled only for the crate's own tests or under the `test-store` feature, which a dev-dependency asks for (the crate's own tests ask through one on itself) | the write-ahead log is mandatory in Quiver and kiln: `GroupStore` on a real device, `RamStore` test-only, and a build that names it outside a test fails (kiln's CP-T2.1, T-T2.1.2; Quiver's `crates/quiver/tests/ram_store.rs`) | `0001-ramstore-for-tests-alone-the-write-ahead-log-mandatory.patch` |
| `crates/hyper-block/Cargo.toml`, `crates/hyper-rt-transport/Cargo.toml` | hyper-rt taken by path from kiln's own runtime (`../../../../crates/kiln-rt`), not from `../hyper-rt` | kiln's runtime is its own copy of hyper-rt (kiln's `crates/kiln-rt/ORIGIN.md`), which kiln's `[patch.crates-io]` gives every crate that names hyper-rt: these two take it by path, which a patch can't reach, and two path packages named `hyper-rt` 0.1.0 can't share a lock file. `crates/hyper-rt` here stays upstream's, and no crate kiln builds takes it (hyper-tls takes it only for its own tests, which kiln doesn't build) | `0002-hyper-rt-from-kiln-s-own-runtime-crates-kiln-rt.patch` |
| `crates/hyper-rt-transport/src/driver.rs`, `crates/hyper-rt-transport/tests/received.rs` | `Driver::poll_received` and `Driver::received`: `poll_event`'s turn split at its seam, an event the turn's receive half made handed out before the turn's sends, which the caller then makes with `flush` in the same turn (the method's doc says what it owes, and what holding it costs); `poll_event` built from the same pieces (`receive`, the receive half, given the turn's context and whether it's the poll's last, as upstream's turn takes them since `fb74b59`; `turns`, the loop), doing what it did; a test of both | kiln's net thread hands what comes in to its shards before the turn's sends (kiln's goal 10, plan 01a1a87b2dae step 5): `poll_event` sends before it hands out a turn's first event, so each frame waited for the acknowledgements, retransmissions and data its turn sent | `0003-driver-poll-received-a-turn-s-events-before-its-sends.patch` |
| `crates/hyper-tls/src/conn.rs` | `ConnectionCommon::set_max_fragment_size`: the largest record a connection sends, set as it goes (header and all; `None`, TLS's own), as the configuration's `max_fragment_size` sets it when the connection is made | kiln's browser protocol socket keeps each TLS record at most its chunk, which follows the path's rate (kiln's goal 3, plan 01a1a832eb2f711f9c6f51d88effe89d step 3): the configuration's is fixed when the connection is made, before the server knows the connection carries that socket or what its chunk is, and a 16 KiB record holds an urgent frame 2.6 s at 50 kbit/s | `0004-hyper-tls-a-connection-s-largest-record-set-as-it-goes.patch` |
| `crates/hyper-transport/src/endpoint.rs` | `Endpoint::negotiated`: the TLS 1.3 suite and key exchange group the connection to a peer negotiated, read from hyper-quic's handshake data | Hex's change (its `vendor/patches/hyper-raft/0002`), which its `hex-quic` calls to assert that every link of the joint kiln, Hex and Shards plan takes `TLS13_AES_256_GCM_SHA384` alone (Ada, 2026-11-11): kiln holds it so Hex builds on kiln's one hyper-raft (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16) | `0005-hyper-transport-says-which-suite-and-group-a-connection.patch` |
| `crates/hyper-transport/src/endpoint.rs`, `crates/hyper-rt-transport/src/driver.rs` | `Endpoint::handle_datagram_to`: a datagram's local address handed to hyper-quic (`handle_datagram` passes none, as before); the driver hands it each arrival's (`Arrival::to`) and sends each transmit from its `src_ip` (`Batched::send_segments_from`) | a server bound to the unspecified address on a host with more than one address answered from its route's source, which a QUIC client does not accept from a server (RFC 9000 §9): kiln's server on adacachy held 192.168.1.77 beside its advertised 192.168.1.90, and every runner's direct link fell to the HTTP tunnel and dropped (kiln goal 90 R2, 2026-11-21). The driver's half calls kiln-rt's `Arrival::to` and `send_segments_from` (kiln-rt's ORIGIN.md row), so it goes upstream with that fix, on the person's word | `0006-hyper-transport-and-its-driver-answer-from-the-address-a.patch` |
| hyper-timing/src/folds.rs | Expose exact accumulated node time | AD-1 restores failure history after restart without rounding the measured MTBF | 0007-liveness-history-exposure.patch |
| `crates/hyper-raft/src/node.rs`, `crates/hyper-durable/src/replica.rs` | Allow the current leader to resign in its existing term when its detector loses the voting quorum | AD-1 requires a partitioned session leader to step down even if it cannot hear a higher term | `0008-minority-leader-stepdown.patch` |
| `crates/hyper-durable/src/replica.rs` | Retain an explicit campaign hold across wake and transfer | AD-1 requires a measured voting quorum and an execution-capable candidate | `0009-session-campaign-hold.patch` |
| `crates/hyper-liveness/src/{lib,pair}.rs` | Expose the process incarnation from the peer’s most recent accepted heartbeat | AD-1 requires a quorum to identify the same owner process before transferring a session | `0010-expose-the-observed-peer-incarnation.patch` |
| `crates/hyper-durable/src/replica.rs`, `crates/hyper-raft/src/raft.rs`, `crates/hyper-durable/tests/shell.rs` | Serve outstanding snapshot requests after a durable compaction and resume appends when the follower confirms the image | A competing session founder needs the certified winner’s snapshot to recover while its group is idle | `0011-send-requested-snapshots-when-compaction-becomes-durable.patch` |

The first change is Quiver's and kiln's policy, not hyper-raft's, so it isn't offered upstream: hyper-durable
calls `RamStore` slates' case (each write complete as it's submitted), and gating it in hyper-raft
would reach slates. Focal's agent agrees (2026-10-22), and notes that Focal names `RamStore` only in a
test module (`focal-consensus`'s `shell_tests.rs`): sharing this copy, it would ask for `test-store`
as a dev-dependency, as kiln's sync tests do.

The second change is kiln's layout alone, so it isn't offered upstream either: upstream's crates take
their sibling, as they should. A fix kiln makes in its copy of hyper-rt goes upstream as kiln-rt's
rules say (kiln's `crates/kiln-rt/ORIGIN.md`).

The third change is offered upstream as it stands: it applies to `fb74b59` as resolved here (its receive
half upstream's new one), where the crate's tests pass with it and its clippy is clean (2026-11-12). It adds a method and changes no other: Quiver,
Focal's bus and Mantle hold this revision's driver byte for byte and call `event` and `poll_event`,
whose turns are the same; only kiln's net thread calls `received`. Quiver records kiln's changes to
hyper-raft (`scripts/vendor.py check` holds its pins to them), so it takes this one too.

The fifth change is Hex's own, taken as Hex recorded it (2026-11-11): it adds a method and changes no other.
Hex's other two changes to hyper-transport are not kiln's: one takes hyper-quic without its qlog traces, which
in kiln's build would turn serde_json's `preserve_order` off for every crate kiln links (hyper-transport's
dependency is what turns it on now, kiln's own asking hyper-quic without its default features); the other
grows hyper-transport's spare buffers as they're needed rather than at the start, a change of memory, not of
what Hex calls. kiln's copy of Hex records kiln's changes (vendor/hex/UPSTREAM.md); the three are for Hex and
kiln to agree on. Quiver, which records kiln's changes to hyper-raft, takes this one too
(vendor/quiver/UPSTREAM.md).

An earlier change, IOCP's widened AFD poll issued only once the old one's cancel completes (Jupiter,
2026-10-24), was a fix of hyper-raft's own: hyper-raft carries it since `53a28a0` (with its test,
`392407f`), so the re-vendor at `0856e05` dropped it.

## Taking a newer upstream

`python3 scripts/vendor.py hyper-raft <commit>` takes these crates at that commit and re-applies the
changes above by three-way merge; take it at the revision Quiver and Mantle take, which move with it.
