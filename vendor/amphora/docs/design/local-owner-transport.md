# Embedded local owner transport (CP-T1.5, U05/U06)

The canonical database owns one engine, controller and durability pipeline. A second process
uses a product-owned local client; it never opens another engine. Database startup selects the
owner or client automatically and retains the listener in shared ownership. The listener
holds a weak-upgrade factory, not a strong Database reference, so individual clone drops cannot
stop it and the last handle can release ownership. Connections retain the database only for their
bounded lifetime. This module is not an installed daemon.

All supported platforms use the same endpoint: IPv4 loopback on an ephemeral port, a protected
companion directory and a fixed 34-byte port/capability record. A separate OS-locked private
lockfile serializes publication. private-fs creates private Unix modes and protected Windows
current-user-only DACLs, verifies access and retained handle identity, and refuses links/reparse
points. It also verifies macOS extended ACLs, which ordinary mode checks cannot establish.
The record is atomically replaced under the lock and retained after shutdown as one bounded
stale record. It contains no remotely routable address. The lockfile is separate because a
Windows exclusive byte-range lock also denies other processes' reads.

An additional private `database-owner` lease is acquired before opening the engine or creating
SQL state. The engine, flusher and readers retain the lease until their actual drain. Existing
hard-linked database files and dangling symlinks are refused; an OS-file-identity registry also
reuses an existing same-process owner for aliases. The embedding host must protect its database
directory namespace. In particular, private-fs validates the final Windows sidecar DACL and
reparse traversal, but does not validate ancestor DELETE_CHILD grants; this protocol does not
make an attacker-writable database directory safe.

Windows binds SO_EXCLUSIVEADDRUSE before bind through socket2's safe API; Linux/macOS do not
request reuse of an already bound listening endpoint. The exact upstream socket2 revision is
pinned because the safe Windows exclusivity setter merged after its latest published release.
There are no product unsafe blocks or production subprocesses.

The client authenticates the owner before revealing a client proof: random client/server nonces
and direction-separated HMAC-SHA256 proofs bind protocol version, nonces and requested lifetime.
The capability never crosses the socket. Thus a stale record whose port was reused by another
process cannot authenticate that process or expose the capability. Windows exclusive binding
prevents another local socket from intercepting or relaying traffic after the handshake. OS-local
stream isolation protects subsequent SQL/Records frames; this is not a network transport.

The wire is a four-byte big-endian length followed by a postcard envelope. Variant order is
append-only. Frames are limited to RESULT_MAX plus 1 MiB of serialization overhead. Serialization
grows a buffer only up to that fixed cap; small control messages keep small allocations. Reads and
writes reset their kernel timeout to the remaining absolute deadline after every partial transfer.
All platforms use TcpStream::connect_timeout. Backlog saturation refuses before submission.
No retry loop replays writes or closures.

Before a new application operation, a client checks owner identity. If the old process has
exited, the client acquires the same exclusive lease, reopens the existing durable data through
its original local or replicated factory, and publishes the replacement endpoint. All clones
see that promoted owner. Authenticated identity includes the complete replication reopening
configuration; an explicitly configured attachment must match it. Identity probes may retry;
an application request that loses its response returns `Refusal::UnknownOutcome` and is never
replayed. `attach_replicated` lets a host attach before attempting exclusive bootstrap inspection;
`ensure_owner` lets its carrier coordinator discover local takeover before advertising a carrier.

Resource limits are admission policy, not measured tuning claims: 32 connections, at most eight
Records waits, 30 seconds total per authenticated connection, two seconds to authenticate,
4096 commands per adaptive session, 256 bytes per authentication frame, and at most 33 MiB per
data frame. Collection decoders bound statement/parameter/transaction arrays before allocation
and charge SQL result collection overhead as well as value payload. Overload closes an unadmitted
connection; the client returns a typed connection refusal before submission. The persistent
accept loop checks shutdown every 10 ms (bounded idle shutdown latency); each admitted worker
releases its permit on every exit. Waits use existing Namespace waiting outside the SQL writer;
they cannot consume all connection slots. Frame allocation caps bound per-process transport
memory at 32 connections times bounded wire and decoded values. Authentication frames have a separate 256-byte cap.

Adaptive SQL is executed inside existing Database.call. The client closure runs once. Intermediate
query responses are provisional; Commit ends the closure and the final Done waits until the
real durable callback. Socket loss, deadline expiry, exhausted command budget or an explicit
Abort before Commit causes the closure to fail and the existing pipeline to roll back. Loss of
the final response after Commit is an unknown outcome; callers must not replay FnOnce. Records
CAS, SQL batches and migrations route directly to existing Database/Namespace methods.

SQL-session Records get/CAS commands also run inside the same Database.call, with provisional
revisions and typed refusals. Namespace import pages, progress and readiness route existing owner
methods. Remote async watches use one process-wide reactor with at most 4096 registrations,
64 metadata checks per request, and a 64 KiB reply limit before decoding. Each registration has
a thirty-second deadline and a 4096-probe limit. A metadata batch uses at most one connection,
does not enter the SQL writer, and consumes no synchronous-wait slot. Ten milliseconds between
checks is explicit notification policy, not a throughput claim. Polling only inspects a local
result/waker. Dropping a registration releases admission immediately; dropping the last
subscriber of an active batch interrupts that socket. Read-only metadata probes may retry
transient connection failures within those bounds. Synchronous Records waits remain limited
to eight; their owner-side watches check cancellation at most every 10 ms.

The local writer checks admission deadlines before invoking an application closure and installs
the existing SQL progress guard during application evaluation, with a check every 1024 VM
instructions. It drops the guard before transaction cleanup and durability work. The replicated
actor owns its corresponding evaluation guard. A trusted closure's arbitrary blocking host code
cannot be preempted; expiration after submission therefore remains an unknown outcome. Producer
result accounting charges owned-value overhead as well as payload before commit.

Falsification: a second independent engine acquiring the same canonical database; a successful
response before durable publication; a disconnected adaptive session leaving rows; one clone's
drop stopping another; an unauthenticated client reaching SQL; a Records wait preventing a SQL
client's admission; or any attacker-controlled allocation beyond the frame/API budgets.
`tests/transport.rs` exercises independent-process ownership, CAS, adaptive SQL, application rollback
and disconnect rollback. It also exercises an existing client becoming the next standalone and
single-node replicated owner after a predecessor process exits, retaining SQL rows and ledger
revisions. Endpoint tests check private record reading, competing owner refusal, protected
pre-bootstrap leases and hard-link refusal. Builds and behavior tests must run
on each supported target before claiming validation.

Primary evidence: [Rust File locking](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)
documents exclusive nonblocking file locking and release on handle closure. These references were
checked 2026-10-29. This design does not claim to stop a malicious process running as the same uid,
which already has access to the database files and capability.

Windows evidence: [Microsoft Winsock socket exclusivity](https://learn.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse)
explains port takeover with SO_REUSEADDR and the protection afforded by SO_EXCLUSIVEADDRUSE.
[private-fs's security contract](https://github.com/orielhaim/private-fs/blob/main/SECURITY.md) and
[0.2.1 API](https://docs.rs/private-fs/0.2.1/private_fs/) describe protected user-only DACL creation,
handle-relative operations, validation and refusal semantics. [RFC 2104](https://www.rfc-editor.org/rfc/rfc2104.html)
specifies the HMAC construction; direction/domain labels and nonce binding are this protocol's
choice, falsified by a reflected or replayed proof authenticating a new connection.

## The tests' budgets (2026-10-31)

None of the transport tests is about latency. Their budgets were real-clock guesses, and a loaded
disk or a stopped process overran them:
- the client's `until()`: five seconds in `transport/common.rs`, ten in `transport/watch.rs` and
  `tests/transport.rs`;
- the one-second session `wrong_capability_never_acquires_database` asked for in its `Hello`;
- `serve`'s fixed two-second greeting.

Once `compact_step_on` has sent a mutation, it answers UnknownOutcome for an answer read past its
deadline. That is correct, since the mutation's fate is then unknown, but the test took it for a
failure. At 2e7728d, stopping the test past its budget gave that failure, and so did the base
327a8f9. Measured under load, the test failed 5 times in 200 runs at loads 24 to 37, each run
inside its five seconds and past its one-second session.

Every operation of these tests now waits under one hang guard, `HANG`: 120 seconds. It is a
measured constant (CLAUDE.md section 5): twenty times the slowest whole run of any transport test,
measured over 200 runs of each of the 12 tests at loads 19 to 39 (5,856 ms, kiln plan
01a1905036ce70ebae3484dc384f8129 step 2). Each test's run bounds every operation in it. `HANG`
replaces:
- `transport/common.rs`: `until()` in `wrong_capability_never_acquires_database` (11 uses: the
  greeting's writes and reads, the Capabilities request, `compact_step_on`);
  `frame_length_is_refused_before_reading_payload` (5);
  `stale_record_cannot_authenticate_a_reused_port` (4, `connect` and `read_limit`);
  `a_stop_and_continue_leaves_a_waiting_read_whole` and `..._send_whole` (1 each). Their
  deadline need only outlast their stop, which the test makes a fraction of a second.
- `transport/watch.rs`: `until()` in `completion_before_poll_does_not_need_a_registered_waker` (2),
  `registration_overflow_is_typed_and_drop_releases_capacity` (2) and
  `sixty_four_dormant_watches_share_one_reactor_and_leave_sql_usable` (2).
  `hostile_watch_batch_lengths_are_bounded_before_element_decode` has none.
- `tests/transport.rs`: `until()` in
  `independent_processes_share_cas_and_adaptive_sql_and_abort_disconnect` (14),
  `sdk_client_keeps_ledger_and_becomes_owner_after_predecessor_exits` (12),
  `separate_memory_databases_do_not_share_sql_or_records` (7) and the child half they start,
  `transport_child` (10).

A session can't be given `HANG`. `serve` refuses a session longer than `SESSION_MAX`, 30 s, the
longest the protocol allows, and the production client asks for at most that. So the session
`wrong_capability_never_acquires_database` asked for in its `Hello` (1,000 ms) is now
`SESSION_MAX`: the protocol's own bound, five times the slowest run measured.

`HANG` also bounds the tests' own waits:
- `pair()`'s loopback connect (two seconds);
- the send test's peer read (five seconds), whose timeout only keeps a short send from hanging the
  read;
- the waits that took their deadline from `until()`: `wait_asleep` polling a thread's state, the
  watch fixture's wait for its notice, and `tests/transport.rs`'s wait for a replica to be ready.

One clock read stays as it is: `successor.replica_status(Instant::now())`, in
`sdk_client_keeps_ledger_and_becomes_owner_after_predecessor_exits`. Its deadline has already passed
when it is read, the call must refuse with Timeout, and so it answers the same at any load.

`serve` takes its greeting's budget from its caller. The owner passes `GREETING_MAX`, the two
seconds to authenticate of the resource limits above: its bound on a peer that connects and says
nothing. A test that runs `serve` itself passes `HANG`. The client gives the owner the same
`GREETING_MAX`: its bound on a listener that accepts and says nothing, such as another program on a
stale record's port. Both stay, because they are admission policy, and the tests that go through
the production client or owner meet them: `stale_record_cannot_authenticate_a_reused_port`, the
watch tests and `tests/transport.rs`. A greeting reads the owner's 34-byte record, connects over
loopback, and exchanges three frames of at most 256 bytes with an HMAC each way. In the same
measurement, the slowest whole run of the test that is little more than one greeting,
`stale_record_cannot_authenticate_a_reused_port`, took 589 ms. A process stopped for longer than
`GREETING_MAX` inside a greeting answers Timeout, as production must.

This is not "a longer timeout" in CLAUDE.md section 4's sense. No test's verdict rests on `HANG`:
each test's subject completes in milliseconds, and its assertion is about the outcome. The guard
fires only when nothing has happened for twenty times the slowest run measured, which is a hang,
not a slow disk. Its value is measured, not guessed.

Evidence: kiln plan 01a1905036ce70ebae3484dc384f8129 and its research
01a1905036d67622b5262d141422fdcf: the failure and its message at 16c8ed9 and 327a8f9, the budgets
of the 13 tests, and the measured tails. Dean and Barroso, "The Tail at Scale" (CACM 56(2), 2013):
on shared machines the slow tail, not the typical time, decides whether a budget holds.

Falsified by:
- a transport test that fails with Timeout or UnknownOutcome before its `HANG` has passed, unless
  it was stopped for longer than `GREETING_MAX` inside a greeting;
- a transport test whose outcome changes with the machine's load;
- a measured whole run past a twentieth of `HANG`. That last one calls for measuring again, not for
  a larger guard.
