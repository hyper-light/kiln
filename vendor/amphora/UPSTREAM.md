# Amphora, vendored

Upstream: https://github.com/hyper-light/amphora at `29be700ba8f42d573cd2ee0c4e7c414095238025`. Kiln uses one copy, excluding upstream nested vendor and bench trees.

| Files | Change | Why | Recorded as |
|---|---|---|---|
| `crates/amphora/src/database.rs`, `database/application.rs`, `database/remote.rs`, `transport/common.rs`, `transport/wire.rs`, `crates/amphora/tests/transport.rs` | Retained standalone application writer floor and append-only bounded attachment declaration; check inside original serialized writer transactions | U05: the database owner lease excludes another engine but does not fence already-open older attached SQL/Records writers; maintenance requires a real positive writer boundary without inventing physical process drain | `0001-u05-retained-application-writer-floor.patch` |

The floor is an application format declaration, not binary attestation, access-control approval, Session/provider physical retirement or a substitute for the old-format offline cutover. Internal floor metadata is protected by the existing `__amphora_*` SQL identifier reservation. Only the original local non-restoring owner can return the opaque boundary after its ordinary durable callback. It retains that exact owner/lease; attached and replicated handles cannot mint it.

Every accepted application transaction is checked at the actual standalone writer before invoking its closure/mutation, so prior adaptive sessions settle before the floor barrier and queued older requests cannot cross it. New tagged clients share the same owner. The appended declaration retains existing frame, authentication, connection, command and absolute deadline bounds; its ACK is only schema acceptance. No thread, controller, database or retry was added.

This held patch is uncompiled and unrun. Replicated proposal/application fencing and remote-owner boundary receipt production remain unsupported. Ordinary default SDK operation without an installed floor remains available. Re-vendor with `scripts/vendor.py amphora <revision>` and retain this recorded patch until equivalent upstream support is selected. No upstream publication or operational activation occurred.

The consumer also compares retained Database/application identity without claiming readiness, and the same independent-process fixture holds an acknowledged old SQL transaction while requesting its floor. Thread start is not asserted to prove queue admission.

The replicated Machine now refuses an installed application floor before baseline/marker startup, preparation and compaction. Forwarded page deltas, completed seed images and snapshots are checked inside their unpublished-page transaction before the applied marker commits. These are fail-closed guards, not replicated floor support: no old-member page claim is inferred and no replicated boundary receipt is produced. The existing raw-page rollback fixture also checks floor-bearing snapshot refusal without publishing its catalog, and direct replicated reopen refusal. No-floor replication keeps its original route.
