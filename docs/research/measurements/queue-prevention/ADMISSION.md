# Adaptive admission: proposal for coordinator review

Status: original researched proposal, followed by coordinator-approved implementation.
This implementer runs parser checks only; coordinator owns compilation, execution and deployment.
Source baseline: `f555736bd887a4196c8841564cb9b4d6c1ede95f`, branch
`fix/adaptive-inbox-flow`, checkout `/private/tmp/kiln-queue-prevention-20261111`.
Implementer: `queue_index_consumers`; coordinator: root. Owned document: this file.
Production ownership awaits coordinator approval: a new `harness/admission.rs` plus
narrow `harness/host.rs`, `harness/session.rs` and `harness/inbox/mod.rs` integration.

POLICY_READ: Shards `origin/main` `0a6d91b3de0f10a8397868aa3794adab1e09afdc`;
full AGENTS, development-rules and development-evidence read. Rules SHA256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln AGENTS, ci section 0 and this task's ROOT.md read in full. No children.

## Decision proposed

Use receiver-owned, event-driven application credit with one lease for each admitted
communication. Keep durable sender custody until explicit receiver consumption or a
visible terminal disposition. A blocked offer remains the same durable communication,
with its identity, body, attachments, order and cause retained; it is not accepted
and then evicted. The coordinator owns the shared durable message store/outbox and
shared model/protocol changes. This lane must not create a competing spool.

Learn the useful pipeline depth from real consumption and natural credit-feedback
latency. Divide existing credit fairly among interacting peers; adding peers must
not multiply total authority. Physical byte authority remains an independent ceiling.
Publish capacity changes on actual events. No sampling timer, synthetic traffic,
minRTT phase, retry timer or repeated state query is proposed.

## Primary sources and alternatives

1. [RFC 9000, May 2021, sections 4.1–4.3](https://www.rfc-editor.org/rfc/rfc9000.html#name-increasing-flow-control-limi)
   permits credit autotuning using application consumption rate and RTT. It separates
   stream and connection byte limits, describes sending credit with existing traffic,
   and warns that waiting for a blocked notification can deadlock. The useful transfer
   here is consumption-driven credit plus aggregate authority; transport delivery is
   not evidence that a model consumed a communication. The RFC does not prescribe our
   estimator or a queue-length constant.
2. [AMQP 1.0 OASIS Standard, 29 October 2012, part 2, sections 2.6.7–2.6.10](https://docs.oasis-open.org/amqp/core/v1.0/os/amqp-core-transport-v1.0-os.html#doc-flow-control)
   gives the receiver ownership of per-link message credit and describes asynchronous
   flow updates. Sender availability is distinct from receiver credit. In-flight
   transfers remain possible after reducing credit. This supports explicit grants,
   cumulative accounting and preserving already admitted work during shrink. Its
   unsettled-delivery model is a useful custody comparison, not a reason to add a
   second messaging subsystem.
3. [Reactive Streams 1.0.4, publisher rule 1](https://github.com/reactive-streams/reactive-streams-jvm/blob/v1.0.4/README.md#1-publisher-code)
   bounds delivered elements by subscriber demand across asynchronous boundaries.
   It provides a sound demand invariant but does not supply durability, peer fairness
   or a learned buffer target. Adopt the invariant, not a JVM framework.
4. [Netflix concurrency-limits v0.5.3, Gradient2Limit.java](https://github.com/Netflix/concurrency-limits/blob/v0.5.3/concurrency-limits-core/src/main/java/com/netflix/concurrency/limits/limit/Gradient2Limit.java)
   updates a concurrency estimate from request latency, with configured smoothing,
   queue allowance, RTT tolerance and minimum/maximum limits. Useful comparison:
   distinguish completed service from arrivals and avoid treating application-limited
   samples as evidence for growth. Reject its supplied constants and rejection-first
   semantics for this contract. Its RPC concurrency estimate is not a proven agent
   queue-depth controller.
5. [Envoy v1.36.0 adaptive concurrency](https://www.envoyproxy.io/docs/envoy/v1.36.0/configuration/http/http_filters/adaptive_concurrency_filter)
   periodically reduces concurrency to measure minRTT and uses configurable margins.
   Reject that controller here: probe phases, periodic windows, load shedding and
   imported tuning parameters conflict with the user's event-only, no-loss direction.
6. [RabbitMQ 4.1 flow control](https://www.rabbitmq.com/docs/4.1/flow-control)
   propagates pressure back toward publishers rather than allowing unchecked growth.
   [RabbitMQ 4.4 confirms](https://www.rabbitmq.com/docs/confirms#are-publisher-confirms-related-to-consumer-delivery-acknowledgements)
   expressly separates publisher confirmation from consumer acknowledgement. This
   distinction applies directly to Kiln's durable inbox ACK versus provider handoff.
   [Queue overflow behavior](https://www.rabbitmq.com/docs/maxlength#queue-overflow-behaviour)
   includes dropping or publisher nacks; neither alone meets this task's durable
   visible waiting requirement.

These are architectural references, not live validation of Kiln or a claim that a
specific estimator beats their implementations. Sources were read on 2026-11-11.

## Exact existing seams and custody gaps

All source observations below refer to baseline `f555736b`; they are not claimed as
newly reproduced live defects. ROOT owns the private live backlog evidence. No raw
private communications are copied here.

| Existing owner/seam | Observed behavior and consequence |
| --- | --- |
| `harness/host.rs:671,1540,1565` | `LANE=1024` bounds only the host lane. A message can leave that lane and accumulate elsewhere, so lane occupancy is not end-to-end queue depth. |
| `harness/host.rs:2610` `take_message` | `Inbox::accept` commits first; then `Taker::hand` invokes `Session::command`. A failed hand attempts `unaccept`; failure of that rollback leaves the id taken without delivery. Reserving before this sequence closes only the capacity race, not crash durability. |
| `harness/inbox/mod.rs:378` `accept` | Durable `Kept` holds epoch, time, digest and order, not the communication body or pending delivery stage. An inbox ACK is deduplication/acceptance evidence. It is not consumption or a general replayable body outbox. |
| `harness/session.rs:1020,1082,1619,3569` | `ctl` is unbounded. Commands can then wait in local `carry`, `for_room`, `State.queue` or an adapter's pending flight. A lease must cover all these stages rather than being released when one queue pops. |
| `harness/host.rs:356,2506,2545` | The saved queue is copied from `State.queue`; ordinary saving is delayed/coalesced. The shown record does not atomically commit the inbox body with its acceptance, nor capture every ctl/carry item. |
| `harness/asked.rs:1,90` | Command-answer deduplication is explicitly memory-only. It stores state/answers, not a restart-safe general sender body outbox. |
| `ui/src/api/harness.ts:1316–1378` | `asked` and outgoing commands live in in-memory maps; retries use reconnects and `setTimeout`. No durable browser body outbox was found in this path. Existing retry timers must not become the new capacity mechanism. |
| `harness/ops.rs:245–305`, `plans/mod.rs:1216` | Generic agent sends/plans can return an error on refusal or disconnection. Their callers do not share a durable pending-body mechanism. Goal pending identities are narrower and do not solve general custody. |
| `harness/registry.rs` `dispatch_relayed` | Remote CAP_INBOX messages use direct HarnessSend. `wire::commands` parked/unsettled commands are in-memory; they must not be mistaken for the required durable body store. |
| `memory/queue.rs:245,335,440` | Existing Room notifications can wake waiters on resize/free. Pace learns from queue take and producer timing, not model consumption, and cannot simply be applied to ctl. Room also permits one oversize item into an empty queue: it is not by itself a strict physical admission authority. |
| `memory/room.rs` | Existing event-driven physical-share mechanism uses effective process capacity, resident memory, connection changes and OS pressure. Its current owner set covers runner-to-copy buffers, not all host inbox retention. Coordinate its authority/accounting instead of inventing a per-session byte constant. |

### Actual provider boundaries, and legacy uncertainty

`Core::note_sent` is too early: it is called after producing provider input, and
`Delivery::Started` is not uniformly a provider acknowledgement. Do not release
credit by observing those fields generically.

| Path | Candidate authoritative natural event | What must remain charged/unknown |
| --- | --- | --- |
| Claude `claude.rs:1592,3177` | Matching `command_lifecycle` `started` confirms the provider began that command. A matching completed lifecycle also proves it got beyond submission. | `core.write`, local `Sent`, provider `queued`, or an unrelated assistant event are insufficient. Older CLI versions without correlatable lifecycle stay explicitly unknown; no five-second fallback or inferred delivery. |
| Codex `codex.rs` | Exact native `userMessage` item lifecycle, with the supplied client id and original source user event, confirms dequeue into the model conversation history. The RPC response does not. | Missing client/original identity remains unknown. Model-history admission is not proof of successful sampling or cognitive completion. |
| ACP `acp.rs:1007,1277` | A matching successful `session/prompt` response proves processing completed, although later than desired. An earlier correlated provider update could be used only where that adapter protocol actually proves identity. | Current local Started is set at request submission. Generic session updates are not necessarily attributable to one of several pending prompts. Keep charged until a real matching boundary; conservative throughput is preferable to fabricated consumption. |
| Antigravity `antigravity.rs:488,952` | The adapter's result for its uniquely active turn can prove completion, after verifying the current turn identity. | Started at `core.write` is not acceptance. Exact early acceptance support is unverified; use explicit uncertainty/conservative completion rather than guessing. |
| `ops.rs:386` `agents.wait` mail | Needs an explicit typed consume/handoff action and a durable response/handoff receipt. | Current conditional `QueueRemove` removes before the tool result is delivered. It cannot distinguish user cancellation from consumption, and a disconnected tool response is not proof the model saw the words. |
| Nested/subagent messages | Match that adapter's actual recipient acceptance, with the original communication identity. | `subagent::show` also calls note_sent while merely presenting queued/sent state. Relaying to the parent is not proof the child received it. |

Core/model/adapter and wait-tool changes are coordinated dependencies, outside the
initial lane's production ownership. Legacy behavior must be exposed as unverified
and retain custody. A controller must not silently treat unsupported evidence as an
empty receiver or successful consumption.

### Codex boundary evidence, pinned 0.162.1

Source revision is `092d3acd6bec3e3a14bdc7e7a2810ab628ab759d` (release
`rust-v0.162.1`, matching this adapter's schema). The
[turn-input owner](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/session/turn_input.rs#L1-L5)
documents that its reply follows the routing decision before hooks, model history,
persistence and sampling. Therefore `turn/start`/`turn/steer` success cannot return
application-consumption credit.

The [natural turn loop](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/session/turn.rs#L403-L428)
drains pending input before constructing the next model request;
[record_pending_input](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/hook_runtime.rs#L681-L703)
carries the submitted client id into
[record_user_prompt_and_emit_turn_item](https://github.com/openai/codex/blob/092d3acd6bec3e3a14bdc7e7a2810ab628ab759d/codex-rs/core/src/session/mod.rs#L4717-L4776).
That function updates conversation history before emitting the corresponding user
item lifecycle. Sampling is later in the turn loop. The implemented boundary is
therefore dequeue into model history; it does not claim reasoning completed or
that a network sampling request succeeded. Unidentified items do not free a lease.
The app-server notifications expose individual item identities, not the native
drain-batch identity. No time-adjacent grouping is inferred. Explicit real batches
have a separate atomic receipt API whose quantum is their distinct live leases.

## Coherent proposed interface

Names are proposal vocabulary, not a new public framework. Implement pure decisions
in `admission.rs` and perform effects at the existing host/session owner.

```text
Capacity { epoch, generation, learning, admitted_count, admitted_bytes,
           logical_target, physical_bytes, eligible_peer }
Learning = Bootstrap | Observed { consumption, feedback }
WaitCause = Bootstrap | LogicalWindow | PeerTurn | PhysicalBytes
          | Fenced | Restoring | ConsumptionUnknown | SourceUnavailable
Admission = Reserved(LeaseKey) | Waiting { epoch, generation, cause }
          | Repeat(InboxAnswer) | Refused(InboxRefusal)
LeaseKey = existing communication identity + receiver epoch
Stage = Reserved | InboxAccepted | InCtl | Carried | Queued | ProviderFlight
Disposition = Consumed { boundary } | Cancelled { explicit_actor }
            | Transferred { destination_epoch } | Uncertain { cause }
```

`offer` receives trusted sender identity, the durable communication reference, exact
or conservatively charged resident bytes, and the relevant physical budget input.
It reserves before host lane enqueue or inbox commit, under the same authoritative
owner/fence. Repeat lookup is permitted without a fresh lease, including when full;
the existing epoch/digest checks still decide repeats. No duplicate-admission or
identity rewrite is proposed.

The lane/session envelope carries the lease identity, or refers to the owner's
ledger by existing message identity. Moving among ctl, carry, queue and provider
flight changes stage; it does not return credit. A dropped future/lease handle
does not imply consumption. Reservation cancellation before acceptance releases it;
after acceptance the durable communication disposition controls release. The
ledger is restored from unresolved durable communications and retained queue/flight
state, not reconstructed from a count of recent ACKs.

The root store needs the following single integration contract:

- Persist a full immutable communication, attachments/provenance and its sender
  sequence before reporting it retained. Expose `Waiting{cause,epoch,generation}`
  visibly. A resource/write failure before custody is acknowledged leaves the
  caller's original communication pending and returns a typed cause.
- Select ready communications fairly by authenticated peer within each receiver.
  Preserve each peer's order and causal dependencies; do not reorder existing
  accepted recipient history. Coalescing supplies explicit supersession/disposition
  under this same custody record, preserving originals in durable history.
- Subscribe to source capacity/owner/connection events before the first offer;
  compare generation after subscription so a credit-return race cannot lose a wake.
  Wake on consumption, explicit cancellation/removal, resize, peer lifecycle,
  reconnect or epoch change. Initial/reconnect state is pushed once from the owner.
  Duplicate/old events are idempotent. No periodic retry or probe is required.
- Acceptance, provider consumption and terminal cancellation are different durable
  transitions. An uncertain ACK or delivery keeps the body retriable under its
  original identity. Do not erase sender custody merely because Inbox accepted it.
- Batch resumed work only within current real grants and byte authority. Waiting
  metadata itself is bounded by stored communications and active peer lifecycle;
  avoid one new task/body copy per blocked attempt.

Existing session state-change notifications carry many of these events locally,
but an authoritative receiver-capacity wire event and general durable sender wake
subscription do not exist in the inspected path. Those are explicit shared changes,
not an existing mechanism this lane can claim to have reused.

## Learning and peer allocation proposed

Rate and peer count alone cannot determine queue depth: a rate needs a time horizon.
Use the natural feedback/refill interval rather than selecting an arbitrary queue
delay target. This is an application adaptation of the RFC's rate/RTT principle,
not a controller prescribed by the RFC.

On matching actual consumption events, measure messages and bytes over intervals
where work was available; exclude producer starvation, suspension and unsupported
delivery evidence. On real capacity-update/next-transfer events, measure credit
return/refill latency using an epoch-scoped correlated exchange on one monotonic
clock. Do not compare independent host clocks. Exclude generation time when the
sender had no pending communication. Arrival count and inbox ACK never add capacity.

Candidate useful in-flight target is one service quantum plus the work naturally
consumed while feedback/refill is in flight (`ceil(consumption_rate * feedback_time)`).
The extra quantum is a pipeline role, not a chosen queue-depth constant. The exact
estimators and batch quantum must be reviewed with the coordinator's observed traces
before production: an unlimited-lifetime mean can adapt too slowly; a selected EWMA
gain/window would need justification and cannot be copied from upstream defaults.
No estimator is silently chosen by this proposal.

Cold start is `Bootstrap`: grant the minimum real-message opportunity necessary to
obtain the first natural observations, constrained by bytes, while other work stays
durably waiting. The candidate minimum is one receiver-wide grant, allocated fairly;
do not grant one new bootstrap queue to every arriving peer. If that minimum cannot
identify busy service without starvation, remain explicitly unlearned and review the
required second pipeline role from traces; do not manufacture a sample or timer.

Let interacting peers be authenticated peers with unresolved custody or an active
relationship supplied by existing peer lifecycle events, not distinct arrival ids.
Allocate the total learned window among these peers in fair rounds; at most one peer
gets each indivisible residual grant before the round advances. Thus `P` affects each
peer's share (approximately total/P), not total authority. A quiet/new peer gets its
next fair opportunity; a noisy peer cannot acquire all future grants by flooding.
Byte fairness for unequal messages requires actual charged-byte service accounting;
the coordinator must choose count or byte service policy explicitly. Disconnect does
not delete that peer's retained communication or reset its fairness debt.

Physical reserve is checked on every new/expanded resident allocation. Shrink stops
future admission and preserves all accepted work, even if it is already over target.
A message that cannot fit remains durable at the sender; no oversize-alone exception
may exceed the physical authority. Existing accepted excess on restore is debt to
drain, never a reason to evict. Encoded provider buffers/copies must be charged too;
releasing the original Command's bytes while its encoded clone remains is incorrect.

## Required invariants and checks

1. Each accepted unresolved communication has durable body custody and exactly one
   receiver lease. A repeat acquires none. Every stage and transfer preserves it.
2. Count, bytes and peer service accounting change only at explicit owner events.
   Consumption credits never come from enqueue, inbox ACK, write completion alone,
   user QueueRemove, status changes or unknown provider delivery.
3. Lifetime spans lane, ctl, carry, for_room, queue, nested queue and provider flight.
   No mutation temporarily releases credit before another stage claims it.
4. Completion returns credit once. Explicit removal/cancel returns capacity with its
   own terminal reason and is excluded from consumption-rate learning. Failed edit
   growth preserves the old communication; a successful edit adjusts byte charge
   atomically. Reordering alone changes neither rate nor charge.
5. Shrink, move, restart, process loss, timeout and cancellation never delete accepted
   work. Epoch fencing rejects stale grants. Restoring unresolved flight is explicit
   uncertainty, not a fresh uncharged queue or invented success.
6. Capacity publication never waits for a blocked sender to request it. Subscribe/
   compare-generation tests cover lost-wake races. Event storms with unchanged
   capacity cause no resends; reconnect pushes one current owner view.
7. Blocked peers cannot use an unbounded in-memory wait queue. Durable store pressure
   propagates to the original sender before a custody acknowledgement. There is no
   finite-storage guarantee of accepting an infinite stream while a receiver stops.

Pure tests planned after approval: cold/observed transitions; sample eligibility;
peer churn/fair-round service; byte and count shrink without eviction; large/small
messages; edits/removal; repeat/fence interactions; arithmetic overflow; every lease
stage, failure and cancellation transition. Small host tests: reserve-before-accept,
blocked ctl/carry without a credit leak, inbox commit failure, hand failure and
restart restore. Coordinator integration: real provider boundaries, disconnected
wait tool response, move/restart/crash at every custody edge, backpressure wakeups,
one noisy sender plus quiet peers, disk/memory pressure, compatibility and actual
supported platform behavior. Mutations must remove each relevant release/wake guard
and fail for its forbidden effect. No tests have been run for this proposal.

## Scope and evidence status

The original proposal above is retained as its design context. Coordinator-approved
source implementation is now present; this is not a production-complete claim. Root
owns the combined build/test evidence in ROOT.md. This implementer has written
fixtures and run parser/scoped diff checks only. The current readback is Shards policy
`749c3ca0602c43dac74976e8db713102f5b15874`, unchanged rules SHA256 shown above,
implementer role; no delegated children, builds, services or commits.

R01/R04/R06/R07/R13: proposed typed ownership/transitions above; implementation proof
owed. R02/R03/R05/R12: primary comparisons and reuse gaps recorded; estimator and
physical-allocation decision remain open. R09/R10: root owns live reproduction and
retained aggregate evidence; source inspection here is not reported as reproduction.
R08/R14: no performance, allocation or comparative result claimed; coordinator work
owed. R11: shared Rust design proposed; seven-target and provider/version behavior
unverified. R15: implementer research only, no children/actors/builds/tests or merge.
R16: no runner space or live services changed. Research is ready for design review,
not production readiness or deployment.

## Current source checkpoint and remaining allocation proof

The source checkpoint frozen for root R67 separates ownership fencing from storage
readiness. A moving unfence cannot clear a failed journal read. Exact terminal
disposition is part of MailAck: an old nonterminal ACK cannot erase a newer terminal
receipt. Queue edits retain the accepted original and CAS an explicit current-body
revision before actor mutation; failed growth/persistence leaves the old body intact.
Attachments now materialize all-or-error before source acceptance; completed receiver
file ids are immutable and replay verifies ranges rather than truncating them.

Fairness population and waiting demand are separate. `Demand{peer,revision}` binds a
natural refill witness to an exact durable waiting publication. Waiting-to-Dispatched
withdrawal keeps that one witness valid for its matching Admit; another publication
cannot inherit an old timestamp. Holds/peer/readiness changes invalidate observations.
True idle ends the active busy timestamp. The last completed qualifying service
observation is retained until lifecycle invalidation; new matching feedback measures
only its own grant interval. Arrival counts and time passing supply no rate. The new
fixture covers Waiting, claim withdrawal, Admit, native handoffs and later idle/refill.
Fairness is established only among authenticated retained peers; strict fairness
between callers refused before custody is not proven.

The aggregate PhysicalBudget is strict for the bytes currently charged to it, and
`command_bytes` counts owned string capacities and attachment vector places. This
does **not** yet prove the physical peak of source input retention. In particular:

| Allocation seam | Missing lifetime/authority proof |
| --- | --- |
| Host `take_admitted`, Inbox `accept_inner` | Owned command/Pending clones coexist with the accepted body. Journal serialization and the Records transaction retain encoded buffers through the commit. Charging one Command does not charge those independent allocations. |
| Inbox get/scan/CAS | Records returns an owned encoded row before Pending parsing. Both representations coexist. A post-parse size adjustment is too late to bound the allocation peak; restore debt does not authorize arbitrary new temporary copies. |
| Session attachment materialization | Image read/decode/base64 buffers are allocated before the final materialized-command resize. Failure is now lossless, but transient allocation can exceed headroom before refusal. |
| Adapter input builders and Pending maps | `json!` and provider-specific attachment/text transforms allocate before `Core::write`. Their retained Pending values and native input arrays need explicit ownership. A Core-only after-the-fact counter cannot prove their peak. |
| Core writes and skills bridge | Provider Value, encoded String, `writes.clone`, bridge JSON/projection and final newline buffer can coexist. The lease must cover each real allocation until it drops or is transferred to the stdin queue. Catalogue snapshot storage is separate from each effective prompt copy. |
| Frozen/live-provider adoption | Encoded writes and provider Pending values may survive while original bodies are also retained. Destination debt must be restored before those writes can resume. |

The proposed correction is to move ownership wherever a copy is avoidable, then
reserve each remaining representation **before** allocating it. Borrow the immutable
command for journal serialization instead of cloning its body. Provider mail-input
encoding should serialize borrowed typed input directly using a counting pass and
then a single exactly sized owned buffer, rather than building a duplicate Value
tree first. A charge travels with that real buffer through bridge/stdin ownership;
its release cannot precede the allocation's destruction. The bridge should consume
the outgoing ownership, with typed failure retaining durable original custody.
Materialization knows file length and base64's exact encoded size before allocating;
decoded and encoded buffers require separate checked reservations. Journal reads
need reservation before the owned row and parsed representation are produced, not a
silent fixed copy multiplier. Unknown/read-size limits must return typed waiting.

This requires changes at the actual builders/readers; the existing JSON heap helper
is useful telemetry, not a proof of allocator-exact map capacity. No arbitrary copy
factor, per-session allowance, oversize exception or new polling manager is proposed.
The physical proof remains a release blocker until these lifetimes are implemented
and coordinator-validated. No allocation measurement or performance result is claimed.

## Refusal evidence and next source seam

The [ACP v1 schema at revision 4effcc11e117c67feb5ed505b17f75537932f5a6](https://github.com/agentclientprotocol/agent-client-protocol/blob/4effcc11e117c67feb5ed505b17f75537932f5a6/schema/v1/schema.json)
specifies that refusal removes the prompt and following material from the next model
prompt. A non-error `session/prompt` response is therefore insufficient by itself.
The frozen ACP callback currently treats any response other than error/cancelled as
ProviderCompleted; refusal must instead preserve its original journal and lease.
This source finding was reported to root during the freeze; correction is owed.

Codex exact RPC rejection and Claude refused/discarded lifecycle similarly need a
visible retained/retryable state, separate from user cancellation and consumption.
The minimal owner seam is a typed retry disposition/stage event: persist the original
as replayable, keep the same token, publish retained uncertainty, and retry only on
an actual provider-ready/reconnect/explicit user event. Do not automatically resubmit
on each adapter tick, grow the estimate from refusals, or relabel archival cancellation
as delivery. Where native identity is unavailable, the body remains retained unknown.

Claude's [official SDK changelog v0.3.206, commit 9fc7ea5ff6a2caa382fceabca2fde7e53064e5a9](https://github.com/anthropics/claude-agent-sdk-typescript/blob/9fc7ea5ff6a2caa382fceabca2fde7e53064e5a9/CHANGELOG.md)
records dead-turn corrections: input-builder failure occurs before the turn starts,
and exhausted API/setup turns report cancellation rather than completion. Therefore
this implementation must not treat lifecycle `started` alone as proven model
handoff. A conservative proposed boundary is a successful real result with exact
native original-message identities and actual model turns, treating merged native
identities as one explicit service batch. Zero-API results and refusal/error are
excluded. Missing identities remain unknown; no time-nearby association is inferred.
The [later official changelog](https://github.com/anthropics/claude-agent-sdk-typescript/blob/main/CHANGELOG.md)
documents exact message identity fields on results and the first reply; support must
be detected from actual supplied fields, rather than assumed for older emitters.


## Finite source checkpoint after R67 (not readiness)

POLICY_READ implementer: current Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`; full AGENTS, rules, evidence,
CLAUDE and Kiln ROOT reloaded. Rules SHA256 remains
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
No children, builds, test executions, actors, service actions or commits in this lane.
The earlier R67 compile/test evidence does not cover this source checkpoint.

The source now seeds retained main/task queue bodies into the existing Pending
journal before restore-complete, including old IDs already present in dedup history.
Imported journal originals/current revisions win over queue previews. Existing
accepted work restores as aggregate physical/logical debt, even above bootstrap;
new temporary clone authority remains strict and unavailable authority holds
reconciliation rather than allocating over the bound. Actual live-provider flight
identities come from each adapter's retained requests, never State.delivery guesses.
A missing complete original remains typed LegacyOpaque with command=None; actual
retained matching native buffers are preserved, never fabricated or auto-replayed.
Early exact consumption receipts remain pending until their legacy debt is seeded.
The original-body recovery from retained Core user events/digest evidence and full
native/metadata byte proof remain incomplete; opaque handling does not establish
that unknown original bytes are accounted.

Provider refusal is no longer archival success: native Codex RPC rejection, ACP
error/refusal and Claude refused/discarded lifecycle preserve retriable Carried
intent with the same original/token/lease. Source owner flush persists the stage and
publishes retained Waiting uncertainty. Actual new provider/thread readiness or
Host reconnect can replay; failure itself adds no retry loop or timer. Missing
source journal identity remains unknown. ACP refusal is excluded from consumption;
Claude lifecycle started/completed alone is excluded and only exact successful
real-work result identities provide a completion batch. Codex exact native
userMessage dequeue returns application credit but cannot train service rate from
per-item event spacing; missing real batch/service evidence remains Bootstrap.
No cognitive-processing or adaptive production liveness claim is made.

The source write barrier now includes ownership fencing and holds only the four
supported model-input envelopes. Native history/init/control/tool replies and UI
updates remain live during storage restoration. Journal stage/terminal failure
retains effects and marks source storage unavailable. Session::hand_over is now
fallible: it refuses unresolved effects before detach, drains trailing native
callbacks, commits their effects before positive Handed, and re-adopts the same
native descriptors on a late journal failure. The transfer caller owns authoritative
rollback, destination debt/attachment import and positive activation checks; these
are separate unvalidated integration obligations.

Source MailHandoff registration retains queue/body/lease and journals exact bounded
metadata. Only the configured native kiln agents_wait result with exact registered
canonical digest and matching successful main turn can stage WaitHandoff consumption.
HTTP/tool-return custody alone, altered/missing identity, attachments unsupported by
text proof, interrupted turns and failed source journal commit never release credit.
The shared canonical serializer belongs to the wait lane; no body archive or blind
QueueRemove-to-consumed inference is introduced. Metadata/proof/parse allocations
remain part of the physical lifetime blocker below.

BufferCharge is an aggregate RAII reservation with reserve/resize-before-allocation;
actual transfer encoding uses the same authority. Host attachment_ready listens
before checking exact finalized file size/hash and wakes only from file-completion
events; the transfer caller supplies cancellation/deadline. These primitives do not
close the journal/Records, materialization, provider JSON/Pending, bridge/catalogue,
stdin queue and handoff metadata/proof allocation gaps already itemized above.
Resource-event-only retry of a reconciliation held for temporary physical authority,
all retained-provider refusal variants, native legacy full-original recovery,
provider/version fixtures, end-to-end restart/move/error/cancel, and allocation
peak/cleanup measurements are still owed. The checkpoint is not shippable as a
complete physical/no-loss proof.

Light checks performed: rustfmt parser via --emit stdout on the ten owned source
files and scoped git diff --check, both exit0. No formatter writes occurred. Written
fixtures cover prompt/control gating, fenced/unavailable distinction, exact legacy
native identity, early proof retention, legacy old-dedup adoption and opaque
non-execution, exact terminal ACK, canonical wait proof and unchanged originals.
New Rust fixtures are unrun; the coordinator owns fresh combined compilation,
focused execution, live reproduction/mutations, seven targets, resource profiling
and R01–R16 evidence review. Exact source hashes accompany the parent handoff.


## Finite preaccept representation ownership chunk (source only)

POLICY_READ implementer remains Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The full policy/evidence/CLAUDE and current ROOT were reloaded across the runner
restart before these writes. Root R18 test-executable compilation was exit0 with
unchanged source at check-1794380497363412000; that result precedes this chunk.
Implementer ran parser checks and scoped diff checks only. No tests, builds,
dependency installs, actors, child agents, services or commits were executed here.

The Host now borrows an ordinary command through acceptance and then moves it to
the session; it no longer clones that body into a Pending just to encode it. When
attachment materialization requires retaining the original separately, Input first
reserves the planned copied string lengths and actual attachment element places,
then uses fallible exact reservations for the copy. Its BufferCharge stays beside
that copied original until the acceptance call has encoded and committed it. This
is not a claim about the separate attachment materialization allocations.

Inbox acceptance uses a private borrowed PendingWrite with the same field schema,
including immutable original/current metadata and acceptance stamp. A checked
serde writer counts the exact escaped UTF-8 JSON, refusing the existing Records
VALUE_MAX before encoding allocation. A second pass writes into one fallibly
preallocated bounded buffer and refuses a different size rather than growing it.
The acceptance transaction reserves its actual condition/write vector places,
family/key copies, each counted encoded value, and the largest one-row driver
binding representation before allocating these. The one-row driver term derives
from both actual backends executing one row at a time; it is not an unexplained
payload multiplier. The existing Records batch moves each Tx and does not make an
additional payload copy. CAS retries drop the prior transaction/guard before
constructing their next counted representation. The 512KiB harness wire limit is
unchanged and is a different authority from these physical payload reservations.

A new Records::commit_owned(tx, owner) transfers the existing BufferCharge into
the existing worker request. The additive default refuses before enqueue for an
implementation that cannot guarantee ownership. StoreRecords captures the owner
beside Tx in its existing Store writer closure (Tx drops first). Turso's existing
Job::Commit carries the owner through batching; its payload is explicitly dropped
before the owner/reply. Thus cancellation of the caller after enqueue cannot
release authority while the worker still retains the transaction. The synchronous
SimRecords implementation and Host's existing Stalling fixture implement this
same entry point. Other wrappers use the safe default until explicitly supported.
There is no detached request task or new request manager.

The pinned primary [Turso source revision 3e09a82750d8290ef96e3830896e7205b7c0b9e6](https://github.com/tursodatabase/turso/tree/3e09a82750d8290ef96e3830896e7205b7c0b9e6)
is recorded by vendor/turso/UPSTREAM.md. Its core/types.rs makes ValueBlob an owned
Vec; Value::from_slice allocates, and Value Clone copies that body. The prior
borrowed run_while cloned each supplied Value again into a cached binding. The
new PUT path moves its owned Value parameters into bind_at; every success/error
path resets the statement and clears bindings before the worker releases its
charge. This also prevents the cached statement retaining its last body after
commit. The existing [SQLite binding contract](https://www.sqlite.org/c3ref/bind_blob.html)
used by rusqlite's SQLITE_TRANSIENT requires one driver-owned input copy through
execute; the ordinary noncached statement drops it before the owned Store closure
returns. Database engine pages, record assembly, allocator metadata and replication
capture belong to their existing store resource accounting and have not been
proven by this input-buffer change.

Written, unrun fixtures: borrowed-vs-existing Pending wire equality with UTF-8 and
escaping; exact count and bounded writer; one-byte-short input authority returns
no built transaction/leaked charge; existing record value maximum rejects before
encoding; fallible original copy preserves every supported field and unused source
capacity is not charged as another copy; actual Turso worker gate retains its
owner after caller cancellation until it drops the Tx; cached PUT bindings clear
after successful and failed execution. The worker-gate fixture releases its gate
on unwind to avoid leaving a test worker blocked. Root owns combined compilation,
fixture execution and guard-removal/live cancellation/resource evidence.

Remaining blockers are specific: Records get/scan still allocates encoded rows
before parsing and those reads are not covered by this write-only reservation;
legacy/terminal replay, journal edit/stage/dispose serializers remain outside this
new acceptance path; fallible Vec/String requests reserve requested capacity but
allocator overhead/rounding and small Box/worker/database metadata are not a full
process physical proof. Attachment read/decode/base64, provider builders/Pending,
bridge catalogue/effective prompt/stdin queue, native adoption and transfer proof
retain their separately recorded obligations. Preaccept reserve refusal followed
by self-release/capacity notification needs integrated no-busy-loop validation
against the relay attempt fence; no liveness claim is inferred. Producer original
custody is retained on refusal, no accepted body is deleted, and this chunk does
not enlarge wire or physical authority. R01 allocation/driver error paths and
R09/R10/R14 reproduction, mutations and peak/cleanup measurements remain coordinator
gates; the source checkpoint is not a full no-loss/physical-bound readiness claim.


## Executed preaccept retry red and finite identity correction

POLICY_READ implementer: Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
full AGENTS/rules/evidence/CLAUDE and ROOT reread before the correction.

Root compiled the bounded Host+SQLite regression at
`check-1794383179335510000`, exit0 with source_unchanged=true. Root then executed
`retry-red.py`: one test, exit101 at the intended assertion that the failed source
reservation/release must not reauthorize the same publication. Retained records are
`retry-red.log` and `retry-red-result.json`. The test actually opened a disposable
Host with its ordinary Input/Inbox, gave the aggregate pool authority for the body
but not its counted journal, observed the exact source journal-reservation refusal
and absence of accepted custody, preserved the SQLite original, and called the
production SQLite claim gate using the returned source capacity. This is an executed
red result, not a trace inference. No production retry identity was changed before it.

The correction adds optional Capacity.retry_generation and a shared
retry_permission() helper. Source projections still use generation; metadata,
demand changes, reservations and BeforeAcceptance releases can change that
projection without creating another retry permission. The relay persists/compares
the retry permission in its existing source_generation column; no schema, timer,
probe, detached task, per-message retry cache or manager is introduced. Existing
feedback_generation remains the natural feedback estimator witness. A legacy
absent retry_generation retains its old projection fallback; the new guarantee
requires the updated source and relay, not an invented promise for old emitters.

Each existing Input now has one held-byte account shared by its Controller leases
and temporary BufferCharge values. Global and account updates occur under the same
PhysicalBudget mutex, including restore debt and Drop/resize/release. Snapshots
compare the actual bound and total held minus this Input's held amount. The owner
creates retry credit only on a bound increase or foreign held-byte decrease; its
own reserve/release/clone/JSON-buffer drop cannot create outside room. Projection
observation can acknowledge already-projected own bytes but leaves foreign/bound
changes pending for the existing owner event wait, so an interleaved own update
cannot erase another Input's release. The signal is the existing Changed event;
there is no watcher task, periodic sample or strong Host lifetime across a wait.
A host-level buffer not attributed to an Input remains foreign to those Inputs;
full transfer/native buffer attribution is a separate existing obligation.

Actual accepted consumption/disposal advances retry permission once. A
BeforeAcceptance rollback does not. Successful restore reconciliation, real native
provider-ready, true hold release and verified ownership unfence are separate
positive lifecycle events. Provider-ready publishes current capacity but never
clears unavailable/restoring storage. Actual Inbox head/pending read or commit
unavailability marks storage unavailable at that failing seam; a physical journal
reservation refusal does not impersonate a disk failure. A missing Inbox advertises
unavailable target0. Epoch/connection changes retain the existing ledger identity
rules and uncertain attempts keep their exact token.

The same actual Host+SQLite regression now obtains its retry key from the same
production helper as relay claim_at. It additionally performs real demand
withdrawal/reappearance, an owned temporary-buffer lifetime, and same-bound resource
remeasurement before asserting no quiet second claim. The positive control changes
the real aggregate bound, awaits its normal source capacity, and permits exactly
one new attempt with the same publication and durable source original. A separate
bounded Input/pool event fixture covers metadata/stage/BeforeAcceptance rollback,
own buffers, unchanged bound, a second Input's release and interleaved own activity.
A readiness fixture proves native-ready cannot reopen storage unknown. These new
versions/fixtures are written and unrun by the implementer; root owns fresh combined
compilation, actual green, quiet-drain/no-spin observation and guard-removal mutation.
Parser (--emit stdout) and scoped git diff checks pass; no builds/tests/actors/deps,
children or service actions were performed by this lane.

Unsupported notifications remain explicit: a quiet filesystem repair has no
storage-ready event; existing Open/Welcome/replay must successfully reconcile before
reopening. Older provider versions without an actual ready/context event retain
work until their existing reconnect/readiness event. Same-Input committed QueueEdit
shrink currently updates only projection/bytes, so it alone does not mint retry
permission for an older refused publication; other real resource, consumption or
readiness events still do. No new edit feature was added in this finite batch.
The previously listed full allocation/custody/transfer/provider/version bounds and
resource measurements remain separate release blockers. This correction addresses
the reproduced event-feedback loop without claiming complete physical or no-loss
readiness from parser success.


## Narrow committed QueueEdit shrink retry-credit closure (source only)

The same-Input edit-shrink notification gap named in the prior checkpoint is now
closed at the existing owner application seam. Accepted QueueEdit still journals
its explicit current-body revision first. Growth reserves before that CAS; a
failed CAS rolls growth back. Shrink now records its planned journal lease size
without releasing the old resident queue body's authority. The old size stays
charged through both successful CAS and any CAS refusal. Only after the owner
synchronously applies the journaled edit does Controller.applied_edit resize that
same accepted lease downward and advance retry permission once. Equal-sized or
growing edits do not create retry credit, and a repeated equal-sized edit cannot
repeat a previous release. The accepted original, token and body history remain.

The existing journal_edit helper accepts the owner's actual adapter apply closure
and consumes accepted edits at that one seam after journal success, including the
same AtState validation/reply handling. Its other commands and untracked legacy
edits continue through their existing adapter path. No pending-edit cache, manager,
wire field, new command or extra body queue is introduced. Callback ownership is
released before the outer command match uses the adapter again. An AtState failure
or missing item never invokes the apply closure and never grants shrink credit.

A bounded source fixture injects a real Records CAS-refusal result into the existing
Inbox edit transition, checks that bytes were not released even at that commit seam,
checks unchanged queue/current-body/counter after refusal, then applies the same
edit through the actual adapter callback. It checks the new current body is durable
while the old queue body and retry permission remain unchanged immediately before
application, checks one byte release/permission afterward, retains the accepted
original, and checks an equal-sized repeat creates no additional permission. The
existing growth fixture now checks the synchronous owner application after its
journal success. These fixtures are written and unrun. Parser and scoped diff
checks pass; root owns fresh compilation/execution/mutation evidence. This small
release-timing correction does not close the previously recorded whole-edit or
other provider/bridge/Records-read allocation audit limits.
