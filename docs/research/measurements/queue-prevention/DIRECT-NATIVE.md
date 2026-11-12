# Direct native journal handover checkpoint

Policy receipt: Shards `origin/main` 7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506,
SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3,
implementer `/root/reach_deployment_plan`. Scope is the existing local SCM handover,
Host Saved/import/export, transfer-only Inbox helpers and their focused fixtures.
Coordinator owns compilation, integration, live native processes, faults and deployment.

The frozen source receipt is `direct-native-source.json`: seven named rustfmt parser
checks and named diff-check passed. No Rust compilation or tests were run by this lane.
Earlier R18 compiled and ran before this batch. It is not passing evidence for this batch.

The control owner now selects the existing Host handover future alongside the sink it
drains. `NativeHandover::Idle/Preparing` permits one native operation. Cancellation records
intent without dropping the future that owns the Saved/native descriptors. Completed
preparation enters Moving before cancellation is processed. The existing sink regression
source queues a stale frame in the real bounded sink and drives the actual operation;
it remains unrun at this checkpoint.

Ready advertises journal-import support. A nonempty journal refuses an old receiver before
native detach. Each local Session frame has exactly three native descriptors when native
process state exists, plus exactly one journal descriptor when its manifest exists. The
journal is streamed row by row, not expanded into Frame JSON. A source actor's acknowledged
pause/final journal flush precedes export. Destination verifies/stages every exact original,
restores accepted debt under Restoring and waits for authenticated Committed before native
activation and positive ACK. Tokens keep their immutable identity across the exact epoch
transition. Tokenless legacy retained/opaque rows remain distinct from signed publications.

For the existing concrete same-process move API, a serde-skipped capability holds the exact
source Inbox. Conditional source Records commit/abort binds that immutable manifest; first
commit also binds the concrete destination Inbox. A serialized unbound Saved cannot recreate
this authority. Token-bound rows always require the server's authenticated transfer protocol.
No cfg(test) behavior authorizes production ownership.

Failure rollback is an operation on the same select owner: send exact Abort, then wait for
Aborted or Committed. Only Aborted authorizes killing green and resuming blue. Committed and
uncertainty retain both control/native owners fenced. The single actual `roll_back` call is
behind this authorization. No timeout maps an unknown phase to refusal or success.

Required coherent restart closure, approved but held during the coordinator's current compile:

1. Persist exact Saved/Frozen recovery metadata in the existing private transfer spool before
   Adopt. Store its exact manifest, size and SHA256 in transfer Records. Verify it on recovery.
2. Persist explicit Aborting intent before posting Abort. Reconnect must replay Abort, rather
   than accidentally replaying Offer for an uncertain Source/Offered record.
3. Synchronous restore must fence a saved source/destination before killing an outlived native
   process or starting an actor. A private small manifest marker is only a fence, never phase
   authority. Exact async Records and authenticated server phases authorize later recovery.
4. Restart cannot adopt stale raw descriptor integers, synthesize provider consumption, infer
   an original from a plaintext ID or automatically resend opaque ProviderFlight. Preserve the
   complete retained snapshot/journal and visible uncertainty; no data is deleted to make room.
5. Destination committed header recovery and live SCM receiver must serialize on the existing
   per-session operation lock. Existing connection/phase events wake the operation; no polling.
6. Source FD guards survive until destination complete ACK/known commit. An Abort request whose
   reply was lost is not permission to resume. A rejected old feature is not quiet fallback.

Normal cold restart preserves `o.instance` (`--instance gid` in the supervisor arguments) and
the same `agent.pem`, as the coordinator confirmed in `upgrade.rs` and `agent.rs`. The existing
exact Principal checks therefore remain unchanged for that supported restart. A deliberately
fresh instance/certificate is not entitled to an earlier transfer. No new successor protocol is
added; that distinct case remains refused unless normal custody authorizes it.

The unreadable-inbox fixture now requires refusal and source/original preservation, clears the
injected fault, and then runs the original handover/repeat once assertions. The existing bluegreen
repeat fixture awaits actual async move_in. Both sources remain unrun here. Actual native SCM,
crash/restart/phase-loss and descriptor lifetime evidence are still owed. Full header/frame
allocation accounting also remains a coordinator/source audit gap; a package byte bound alone
is not RAM authority. No performance, portability or no-loss completion claim follows from
these parser checks or simulated fixture sources.

Cancellation while waiting for Offered is also part of the approved coherent closure. The one
existing Preparing owner must observe its cancellation event, persist Aborting and post the exact
Abort while retaining all native/Saved handles. A cancellation flag that is only examined after
Offered returns is insufficient when the destination has disappeared. Reconnect replays the
persisted intent; it does not turn a rejected/unknown phase into rollback permission.

## Coherent restart/cancel source checkpoint

`direct-native-source.json` now names eight parser-checked source files for the coherent
restart/cancel batch. Its two new fixture sources are written but unrun by the implementer.
The coordinator's compiled `1794383179335510000` precedes this batch: its actual focused run
was 544/546 passed. The real full-sink/cancellation and unreadable-source/recovery fixtures
passed there. The legacy unknown-digest transfer failed there and motivated the narrow source
change below. That run is not evidence for the later code.

The existing native import now owns Saved, optional Handed, restored debt, and its existing
per-session mutex through Commit and durable save. Source Saved also retains that same source
operation mutex through Moving/ACK; a terminal event cannot start restart recovery beside a live
rollback. Both mutex guards are serde-skipped. No second transfer manager or body queue exists.
The complete header is streamed into a private fsynced file, counted and hashed, with exact
manifest/size/SHA256 metadata in transfer Records. On an uncertain retry, the persisted header
is canonical; a later header cannot replace its Frozen/adapter/partial state. A different native
PID/kind refuses. A valid new SCM descriptor still supplies actual ownership of each native FD.

A small manifest marker is synced before its ownership request. Synchronous restore reads the
markers before reading survivor PIDs, archive intents, or starting actors. Unreadable markers
hold automatic restore. The marker itself never authorizes a phase. Existing Welcome/terminal
phase events drive async recovery, which rechecks exact Records under the existing mutex.
Actual phase transitions alone request a further replay; duplicate terminal echoes do not
form a replay loop. Recovery never adopts serialized descriptor integers. Committed/Aborted
recovery retains the full native snapshot, restores original journal debt, and starts in an
explicit manual-resume/uncertain state. ProviderFlight and opaque legacy original metadata do
not become synthetic consumed receipts. A notice identifies the uncertainty. Lane::Open keeps
Restoring until its ordinary current-body/file reconciliation completes.

Cancellation now signals the existing Preparing operation. While waiting for Offered it persists
Aborting and sends exact Abort without dropping the future or native ownership. Reconnect replays
Abort for Aborting; delayed Offered cannot regress it. Committed remains authoritative over a late
Abort. A cancellation known before any Offer can mark local Aborted and restore the source.

The old legacy handover fixture has a complete retained body but an older Kept receipt whose
digest was explicitly unknown (empty). Import preserves that receipt verbatim. Only
LegacyRetained + tokenNone + empty original receipt permits message-ID/body binding through the
exact transfer manifest without equality to a nonexistent receipt digest. Accepted and token-bound
rows retain strict digest equality. No receipt is fabricated and no once/repeat assertion removed.

Known resource-policy issue found during the frozen read-only review: the source header is
currently written before Offered, when `state.disk_floor` has not yet been negotiated. Its
`unwrap_or(0)` therefore does not establish the actual global floor. The destination has already
received Offered and uses its known Off/Reserved policy. The coordinator was informed immediately;
source policy ordering must be resolved before readiness. This is not waived by custody safety.
Existing full Frame/header decoded-allocation accounting also remains unproven: reserving encoded
header bytes is not a complete measurement or proof of decoded capacities. Actual native SCM,
restart/cancellation/phase-loss, FD lifetime and seven-target checks remain coordinator-owned.

## Policy and event-wake follow-up

The two source gaps above are closed in the next frozen source receipt; this statement describes
code, not executed behavior. Source writes only a fence before Offer. Full header and journal
spools are written after the actual Offered policy, using its known Off/Reserved floor. The header
helper refuses Prepared, so unknown policy cannot silently become Off. A crash before a source
header exists stays explicitly fenced/manual; it does not activate an invented snapshot.

Only a typed `Recovered::ResourceHeld` result arms the budget wake. It arises when the header
copy reservation refuses, before a new actor/debt has been retained. Backend/IO failures after
opening an actor remain on phase/connection recovery and do not arm a resource-driven retry.
Such a quiet backend repair may require the next real phase/connection event; it is not claimed
to recover spontaneously. This preserves the existing actor and accepted leases after write failure.

The existing one-operation MailReplay owns a pre-operation (used,bound) baseline and an optional
idle budget cursor. Its budget waiter is disabled while replay runs. Once all its temporary
representations have dropped, a real bound increase or net used decrease permits one attempt.
Its own reserve/drop pulse returning to the baseline permits none. While idle, an unfavourable
change advances only the notification cursor, keeping the failure baseline, so a transient pulse
back to that baseline also permits none. A foreign release during replay is checked afterward.
PhysicalBudget adds only `changed_since(counts)`, a subscribe-before-read event wait; its authority,
reserve/restore/release and per-input accounting are unchanged by this lane.

The bounded new fixture source uses the real Host pool to exercise own reserve/release, one idle
foreign release, and foreign release during replay. It is unrun here. The updated header fixture
also verifies that Prepared cannot write a full header under an unknown floor. No polling,
heartbeat, per-body task, new manager, cache allowance, signature change or cap increase is added.
The encoded-header reservation is a physical payload/copy check; it is explicitly not proof of
all decoded capacities or total process memory. Those audits and actual SCM/restart/FD proofs
remain coordinator-owned, as do compilation and execution of the new fixtures.
