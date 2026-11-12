# Explicit status coalescing with retained history — proposal

Research checkpoint, 2026-11-11. Design approved by the coordinator; implementation checkpoint follows below. Source inspected: Kiln `f555736bd887a4196c8841564cb9b4d6c1ede95f`,
clone `/private/tmp/kiln-queue-prevention-20261111`. Role: implementer;
owned path at this checkpoint: this document only. Shards policy receipt:
`0a6d91b3de0f10a8397868aa3794adab1e09afdc`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS/rules/evidence, Kiln AGENTS/ci §0 and [ROOT](ROOT.md) read.

## Decision and alternatives

Recommend immutable publication records plus a latest-pending presentation in the
coordinator's **shared communication ledger**. Every accepted status body and its
original provenance remain readable. Coalescing changes what is pending for model
delivery; it does not delete an accepted communication. Plain sends use the same
durable admission and resource authority. This is a proposed Kiln design, not a
claim that the following systems implement Kiln's whole contract.

* Matrix v1.16 represents edits as separate replacement events, restricts them to
  the same sender/room/type, leaves original server content intact, and aggregates
  the latest valid edit. This supports separating immutable originals from their
  presentation. Kiln should use its authoritative commit order, rather than copy
  Matrix's timestamp ordering. [Matrix v1.16 event replacements](https://spec.matrix.org/v1.16/client-server-api/#event-replacements)
* NATS KV provides server-assigned revisions, compare-and-swap and configurable
  history. Its history depth can discard old revisions; it is a useful concurrency
  model, not an adequate all-originals archive for this task. Accessed 2026-11-11;
  this documentation URL is unversioned. [NATS history and revisions](https://docs.nats.io/learn/key-value/history-and-revisions)
* Kafka 4.1 compaction preserves latest keyed state by removing older same-key
  records. That alone violates the requested retained communication history.
  [Kafka 4.1 log compaction](https://kafka.apache.org/41/design/design/#log-compaction)

Reusing QueueEdit alone is also insufficient: a consumed item is no longer editable,
and the displayed event is overwritten. Appending every status as an ordinary send
preserves separate delivery but retains the backlog pressure. These are source
observations, not new live defect reproductions. [ACP QueueEdit](../../../../crates/kiln/src/harness/acp.rs)
(`485–491`), [Core::shown_queued](../../../../crates/kiln/src/harness/core.rs)
(`1385–1409`).

## Existing seams

* `ops::send` already checks lineage/permissions, derives sender-scoped immutable
  message IDs from the existing `key`, wraps peer identity and forwards signed
  relay provenance. Preserve those checks. A status key must be separate from
  that immutable request key. [ops.rs](../../../../crates/kiln/src/harness/ops.rs)
  (`216–300`, `1001–1014`).
* Inbox acceptance rejects changed content under one message ID as `Reused` and
  fences acceptance with the session epoch. It stores IDs/digests, not a permanent
  full-body revision archive. Do not turn its immutable key into mutable status.
  [inbox/mod.rs](../../../../crates/kiln/src/harness/inbox/mod.rs) (`378–421`).
* `Core::note_sent` is too early to establish actual consumption: it can precede
  correlated provider acceptance/completion. Inbox acceptance, stdin writes and
  this hook must not release capacity or the ledger. Its current `given` tracking
  covers Kiln's goal/news only. The admission lane owns actual provider evidence.
  [core.rs](../../../../crates/kiln/src/harness/core.rs) (`1098–1110`),
  [acp.rs](../../../../crates/kiln/src/harness/acp.rs) (`1027`).
* Existing news/goal `Kept` edits pending text and uses timing-based observations
  to handle races. New status delivery must use explicit owner events instead;
  do not inherit its `SHOW_MS`/`HEARD_MS` heuristics.
  [queued.rs](../../../../crates/kiln/src/harness/queued.rs) (`1–34`).
* Convo has no immutable revision row with the required retention contract:
  Event and Record writes are upserts; resets/pruning can remove them. A new
  revision ID avoids an upsert collision but does not establish retention.
  `put` is asynchronous and may lose rows if its writer stopped; `put_wait` waits
  for intake, not a per-publication durable result. Therefore neither is the
  acceptance receipt for this feature. Use the root-owned numbered-migration
  ledger and committed transaction result, not a synthetic harness record source.
  [convo/sql.rs](../../../../crates/kiln/src/convo/sql.rs) (`117–126`, `150–172`),
  [convo/store.rs](../../../../crates/kiln/src/convo/store.rs) (`471–516`).
* Existing `Store::changes` table notifications are emitted after committed writes.
  They provide the relay wake source; the ledger remains authoritative across a
  missed notification or restart. Register/listen before reading pending work,
  then recheck the commit revision before sleeping, to avoid a lost wake.
  [store.rs](../../../../crates/kiln/src/store.rs) (`1110–1177`, `1709`).

## Minimum shared API and storage contract

Extend the ordinary publish envelope with
`status: Option<StatusKey { task: Option<String>, key: String }>`. No field means an
ordinary distinct message. Sender and owning principal come from authentication;
recipient is the persistent session ID. Stream identity is
`(owner, sender, recipient, task, status.key)`. Task is an opaque sender-owned workflow label, not a source Task.id or routing
authority; absence explicitly means session scope.
Do not infer status from text, tools, sender names or duplicate content.

Keep `agents.send.key` as the immutable publication retry key. A new status update
uses a new publication key and the same explicit status key. Exact retries return
the original receipt; changed body/status scope under one retry key remains a
typed reuse conflict. Optional expected stream revision can fence a caller's
concurrent updates using its prior receipt; it must not require a read probe.

The shared ledger needs these operations, not a second status outbox:

1. `publish(envelope) -> DurableReceipt`: in one transaction, admit/reserve the
   physical bytes, insert the original body/provenance and idempotency identity,
   assign its revision, and advance the status stream's desired head when present.
   No accepted/parked success precedes commit. The receipt distinguishes durable
   pending custody from actual model handoff.
2. `pending(recipient, cursor, byte_budget) -> Page`: ordinary messages plus latest
   undelivered status heads, using bounded reads and the common admission policy.
   Do not materialize the entire archive or a second unbounded waiting queue.
3. `handed(delivery_receipt)`: record the exact publication revision, delivery
   generation and source epoch handed to the model. Clear/advance only that
   generation/revision, never a newer desired head. This is idempotent.
4. `history(stream, cursor, byte_budget) -> Page`: immutable original revisions and
   their superseded/handed dispositions, under the same owner checks. Oversized
   bodies require bounded body ranges or a typed too-large response; no clipping
   or oversized reassembly is allowed by a history page.

At minimum the schema must distinguish original publications, each stream's latest
desired revision, and exact handed receipts. The pending index may be rebuilt
from those records; it is not another authority. Existing body/provenance bytes
must never be updated when a status is superseded. Byte accounting includes
archive growth as well as pending work; bounded pending depth alone is not enough.
With finite storage, stop new admission before capacity is exhausted, preserving
already accepted revisions. No expiry, truncation or auto-deletion is authorized.

## Owner transitions and failure invariants

Approved simplification: coalesce only in the unified durable pending relay before
source admission. A claimed/dispatched revision is frozen permanently. Keep at
most one latest never-dispatched successor while it waits for correlated actual
provider consumption; do not edit the source queue. Superseded bodies stay in the
same ledger. This removes the source QueueEdit/consumed race from the design.

| Event | Required effect |
| --- | --- |
| First publication | Commit original; create one pending presentation. |
| New same-stream publication before claim | Commit original; supersede only a never-dispatched predecessor; retain its body in history. |
| New publication after claim | Keep the frozen publication; supersede only its never-dispatched successor. |
| Actual provider consumption | Record exactly the frozen revision/generation/epoch; release its latest successor for admission. |
| New publication after prior handoff | Create new pending work; the old message's idempotency receipt must not suppress it. |
| Retry or late delivery/ACK | Match immutable publication and exact generation; never replay a handed revision or clear a later head. |
| Restart/disconnect | Reconstruct from committed ledger on startup/connection event; unknown delivery stays explicit, not reported gone. |
| Move/restore | Fence source epoch and carry/reconcile pending identity plus exact handed receipts before admitting the new owner. The full archive stays in the shared ledger. |
| Capacity/store/error/cancel | Before commit: typed refusal, sender retains custody. After commit: retain original plus explicit pending/terminal disposition; do not silently drop. |

Move packages already carry State and inbox receipts/digests; a status feature
must extend those existing ownership semantics rather than create an independent
owner. [moving.rs](../../../../crates/kiln/src/harness/moving.rs) (`74–114`).
All original signed provenance remains attached to its own revision: a new body
must never reuse an earlier body's signature. The ordinary send relay checks are
the source seam, not permission to claim a different revision was authenticated.
[ops.rs](../../../../crates/kiln/src/harness/ops.rs) (`251–291`).

Drain from publication commit, authoritative handoff/ACK, connection and peer
lifecycle events. No debounce, periodic polling, synthetic probe or sender retry
timer. A protocol deadline may bound one outstanding request; expiry leaves
uncertainty and retained custody, not an inferred delivery result.

## Proposed patch ownership and validation

Approved ownership: this lane owns `harness/mail.rs`, its numbered store migration,
status request parsing/schema/description spans in `harness/ops.rs`, and this
document. Root owns shared model/wire capability, registry dispatch and event glue.
The admission lane owns actual provider acceptance/completion evidence as well as
owns bounded physical admission and learned pending depth. Shared signatures must
be agreed before concurrent edits.

Producer description: send a replaceable status only when progress changes what
the recipient can do; use one explicit stream key per task/status purpose. Send
instructions, requests for decisions and final results as ordinary distinct
communications. This guidance does not replace byte/depth authority.

Minimum focused fixtures: ordinary sends remain distinct; same stream has one
latest pending presentation and every original exact body in history; different
sender/recipient/task/key never coalesce; exact retry versus changed-key-body
conflict; both orders of publish/handoff race; late ACK cannot clear newer head;
publish after consumption works; store failure never claims acceptance; committed
publication survives reopen; disconnect/reconnect and moved epoch do not lose or
replay work; byte bounds hold for escaped/UTF-8 bodies and history page boundaries;
no wake lost between pending read and event wait. Integration must prove actual
model handoff, rather than treating an inbox ACK as consumption.

The proposal stage performed source inspection and primary-document research.
Implementation and actual checks are recorded below; no live action, build, benchmark
or allocation/performance claim. Root still owes design review, live reproduction/green/mutation evidence, integration,
resource accounting, security/move/restart validation and platform checks. This
checkpoint does not claim an implemented or ready fix.


## Implementation checkpoint (not an integrated passing result)

`harness/mail.rs` and migration `20261111-0100_agent_mail.sql` implement one
shared ledger. `publish(store, envelope, retained_bytes_authority,
pending_authority)` commits original body/provenance, scope, idempotency and
pre-dispatch supersession together. The transaction refuses new custody when
immutable byte accounting or total unresolved recipient count would exceed caller
physical/logical authority. Exact retries require no new authority. Metadata
accounting updates in the same transaction; it is not an additional outbox.

`claim_at(store, revision, epoch, source_generation, source_connection, budget)`
freezes before source dispatch and assigns
an attempt generation. `mark_admitted`, `mark_waiting`, `mark_consumed` and
`mark_disposed` preserve that token/epoch. Waiting never unfreezes the original;
late admission cannot regress a consumed/cancelled terminal. Only
correlated actual provider evidence may consume. Cancellation retains a typed terminal reason and every original. Transfer appends
an immutable old-token/from-epoch/to-epoch receipt, then requeues the same frozen
original at the destination epoch; it does not strand the body as terminal or call
it consumed. Old transfer ACKs remain idempotent after the destination claim. The
coordinator must prove actual destination custody before calling this transition;
a new capacity epoch alone is insufficient. Transfer history is available through
message_body path /$transfers, with generation cursors.

`pending`, `pending_for`, `recipients`, `peer_heads`, `attempts`, `attempt` and
`admitted_count` expose bounded relay/recovery metadata without exposing SQL.
Recovery returns the original uncertain token/epoch. A frozen Waiting attempt
requires a changed observed source capacity generation/epoch/actual connection to
claim again. replay_at keeps the same uncertain token and requires an actual
connection/startup identity change; unrelated StoreChanged events never authorize
a replay. Repeated waiting/admitted/consumed transitions perform no UPDATE. `history` pages immutable
originals with explicit terminal metadata; `body` inspects JSON-pointer nodes,
object key pages, array lengths and UTF-8 string ranges when one original cannot
fit beside history metadata. Page budgets include their encoded envelope/cursor.
They do not fragment and reassemble an oversized logical JSON snapshot.

`agents.send.status` is opt-in. Existing sender/recipient lineage and original
signed relay provenance are retained before the common `command_published` call.
Transmission fault injection is the relay lane's post-custody responsibility.
The response is a durable receipt, not a model-delivered claim. `agents.messages`
and `agents.message_body` check owner, sender and recipient lineage; another
sender's incoming history requires permission to see the recipient conversation.
The two corresponding GET routes are registered in the exact approved api.rs span.

Focused Rust fixture source covers coalescing/archive preservation, independent
ordinary messages/scopes, immutable retries, byte/logical refusal rollback,
frozen successors, stale attempts/epochs, consumption before admission ACK,
UTF-8 body ranges, exact page budgets and cancellation distinct from consumption.
Those Rust tests and all compilation/schema generation remain **unrun** by this
lane; root owns the combined compile and integration. No commit or service action.

Actual light checks at this checkpoint: `rustfmt --edition 2024 --emit stdout`
on owned mail.rs and ops.rs parsed successfully (exit 0); this did not compile
types or execute Rust tests. A Python SQLite 3.51.0 fixture executed the migration,
confirmed immutable request/disposition constraints and exact UTF-8 original body,
byte accounting and transfer fence after closing/reopening its scratch database.
The scratch directory was created under this isolated clone and removed by the
fixture. This validates SQL schema/storage behavior only, not the Rust transitions.
No cargo/build/provider actors/profiling/heavy checks were run.


## Internal news caller checkpoint (not readiness)

Told now explicitly publishes `internal-news` status through Registry::command_status,
owner/sender `server`. Changed snapshots receive fresh immutable IDs; no news revision
uses QueueEdit. A pending original holds its exact body/ID until custody is committed,
while current desired lines remain at the existing Told owner. One existing sender
per parent retains an unaccepted original and becomes Blocked until an actual source
capacity/owner event; no periodic retry task is introduced. Receipt progress checks
exact current message identity and generation. Source queue absence and old Info.said
observations no longer consume managed news. Frozen older revisions remain in the
ledger even after a newer desired snapshot exists.

Integration hooks are `told::mail_progress(hub,recipient,receipt,mail_scope)` for
committed internal sender progress and `told::wake(hub,recipient)` after an actual
registry capacity/owner change. Wake must occur after cache publication; an early
callback can otherwise observe stale capacity. Preaccept failure performs at most
one bodyless exact-ID reconciliation on that request-completion event; it does not
poll. The public agents.send schema/status option and tool description were already
wired through the common operation table, not merely implemented in the ledger.
Ordinary instructions/results remain separate publications.

Focused new Told fixture source exercises retained pending originals, fresh successor
IDs, old receipt fencing, and exact consumption versus queue/index observations.
Rust tests are unrun. Earlier QueueEdit/unheard-grace fixture expectations describe
the old contract and require deliberate migration; they are not reported passing.
Parser and scoped diff checks passed. Frozen Told SHA-256:
`8cae2a7718634e80c243a19a239bff80672515b56a1cc6d73d3e264eddd7ac4f`.

The preceding checkpoint exposed a durability gap: `due_runs` could persist `told`
before a generated publication committed, leaving its unaccepted original only
in memory. The finite persistence implementation below addresses that gap; its
new Rust fixtures are unrun and do not establish complete producer-to-model proof.

## Producer persistence implementation checkpoint (not readiness)

Policy receipt: implementer; Shards origin/main
`749c3ca0602c43dac74976e8db713102f5b15874`, full AGENTS/rules/evidence read;
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln base `f555736bd887a4196c8841564cb9b4d6c1ede95f`; owned clone
`/private/tmp/kiln-queue-prevention-20261111`, macOS arm64. Owned paths for this
chunk: Told and its in-file fixtures, this record, plus the separately requested
metadata-only `mail::waiting_peers` helper/fixture. No build, service, actor,
profiling, benchmark, audit, commit or further delegation was performed.

One canonical producer row per parent now uses the existing KV namespace
`told/news/<parent>`. Explicit Quiet/Pending/Retained state holds the immutable
original Command/id separately from current desired News lines. Every accepted
original remains in the unified mail ledger; this row is the existing producer's
owned working state, not another archive or outbox. A producer row uses the same
512-KiB bounded encoder as control data and refuses an oversized row before
acknowledgment. No body is clipped by persistence. Existing news formatting bounds
are unchanged. Closed/unknown parents retain managed originals rather than remove
them based on source queue absence.

`Told::capture` snapshots dirty raw rows and producer state under the existing
lock. `write_persist` writes producer records and captured `told_runs` changes in
one savepoint; success is acknowledged only after the outer `Store::call` commit.
An inner RELEASE alone is not durable. [SQLite savepoints](https://www.sqlite.org/lang_savepoint.html)
(primary documentation, accessed 2026-11-11),
[Store::call and KV](../../../../crates/kiln/src/store.rs).
The producer record explicitly records captured told-run versions; existing raw
RunNews stays in its actual schema, avoiding a producer-envelope/default-Going
compatibility trap. [told_runs schema](../../../../crates/kiln/src/store/migrations/20261107-0600_told_runs.sql),
[capture/write_persist/acknowledge](../../../../crates/kiln/src/harness/told.rs).

Dirty keys are retained until that commit, never drained and reconstructed from
old values on failure. Database producer versions fence stale transactions,
including run-row deletion/resurrection. Returned acknowledgments exclude stale
parents. Latest-captured versions also fence concurrent in-memory X→Y→X changes:
an equal older X save cannot clear dirty ownership while a newer Y save can still
commit. No wrapping version is issued; exhaustion is a typed refusal.

Replacing/clearing an existing original requires its exact bodyless committed
ledger receipt. Reusing its ID with a different command is refused. On startup,
the selected known parent's row is read once under the same byte bound and the
exact `(server,server,parent,id)` receipt is reconciled once. Pending restores
the exact saved ID/body; Retained without ledger proof fails closed. A successful
canonical read alone marks that parent loaded; malformed/failed reads do not
become empty state. No kv_prefix collection of all bodies, periodic retry,
synthetic probe, new manager or waiter per revision was introduced.

Actual serialized row/key payload bytes and mutated row count feed the shared
`Store::storage_reserve_tables` helper for `kv` and `told_runs`. The producer reads
actual volume free space and the existing configured operator `disk_free` floor.
Coordinator integration now uses the common `mail_delivery::storage_floor` rule:
absent admission configuration explicitly disables that floor (zero), while
SQLite scratch remains required. Unknown layout/free space, invalid configured
values, overflow, insufficient room and commit failure refuse success. This is
conservative preflight, not a physical reservation
against unrelated writers. `ran`/Reach Told now return typed `Refusal::Persistence`
and no successful ACK before the canonical commit; the caller retains its
unaccepted original. No optimality or performance claim is made.

New fixture source covers file close/reopen with exact UTF-8/escaped original and
separate newer desired state, atomic producer/run acknowledgment rollback,
unarchived original replacement refusal, stale deletion resurrection,
injected write failure retaining dirty keys, X→Y→X concurrent capture fencing,
and archive-proven advancement. The narrow `waiting_peers` fixture distinguishes
Waiting demand (including frozen Waiting) from Admitted-only peers and checks its
page byte refusal. `waiting_peers` reads no original bodies; the relay samples it
on actual ledger/source events. All these new Rust fixtures are **unrun**.

Actual checks: `rustfmt --edition 2024 --emit stdout` parsed the owned Told and
mail files with exit 0; scoped `git diff --check` is recorded at handoff. Parser
success is not a Rust type check or test result. Coordinator still owes compilation,
focused fixture execution, actual crash/commit/publication race tests, full
end-to-end no-loss/cleanup validation, all applicable R01–R16 review, seven-target
coverage, profiling and comparative measurements. No readiness claim is made.

### r64 compiler repair and fixture migration assessment

Coordinator r64 development check (`check-1794376774560192000/result.json`)
reported ten combined compile errors with unchanged captured source. This lane
fixed exactly its two Told errors: the SQLite read budget is now a checked
`i64::try_from(FRAME)` rather than a `usize` SQL parameter, and initial publication
marks its producer dirty before moving the parent ID into the output command.
No extra clone or protocol change was added. Parser and scoped diff checks exit 0;
repaired Told SHA-256 is
`a3f5b696919144a3cf0dfc8dc4bff7c81fd7b03531f3dbc5466804acbfc1c78a`.
The repaired Rust has not been compiled or executed by this lane.

Read-only assessment of existing Told fixtures found the smallest semantic
migration is explicit typed publication/source receipts, not changing all
`apply`/`take` helpers to pretend a queue observation means consumption:

- The long-turn, edit-too-late, edited/taken and two child-read fixtures must use
  fresh immutable successor IDs, keep dispatched originals frozen, and retire
  only their exact typed Consumed receipt. Preserve their body/line deduplication
  assertions; replace obsolete QueueEdit and grace-time authority assumptions.
- The unsent/away fixtures must keep exact pending identity/body across source
  absence and planning, then use actual publication/readiness receipts for
  advancement. Planning alone must not mint a retry or a new ID.
- The legacy-found fixture must distinguish the original preledger source message
  from a fresh managed revision. Its original QueueEdit expectation no longer
  describes production behavior; do not re-enable mutation just to pass it.
- The saw-lines, t08 start/end and seen-run fixtures must change desired lines
  independently of the frozen original. They cannot remove an accepted source
  original solely because a child/run was read. The mixed run/child fixture must
  inspect current desired `News::text` separately from `news_of`'s frozen body.
- The raw-row/hold `e1_runs_and_holds_outlive_a_server_restart` fixture can remain
  scoped to its original schema; it is not producer no-loss proof. New producer
  reopen/rollback/fence fixtures cover that separate authority seam.

No existing fixture was deleted, weakened, ignored or migrated in this assessment.
Their actual results remain unrun at this repaired source checkpoint.

### Eleven Told fixture migrations (fresh execution pending)

Root released the exact Told test span after real r67 cached red results:
four of four selected legacy fixtures failed (`told-legacy-red.log`: long-turn,
edit-too-late and the two taken/read cases). Root r69 separately reports 81/84
focused passes with three attachment-storage failures; those results do not
cover the newly migrated source below and are not reclassified here.

All eleven existing fixture names remain present, enabled and unchanged. Their
implementation now uses a test-only `NewsLedger` backed by actual
`Store::in_memory` and common mail APIs: explicit immutable publish, exact
revision claim, mark_admitted, mark_consumed and bounded history. The fixture's
two unresolved slots model the already-approved frozen original plus latest
desired successor; this is not a new runtime tuning constant. `apply` and `take`
still change only source observations. No callback infers consumption from an
empty queue, a timer or Info.said. Native provider proof is outside this driver;
the source lane's native-result fixtures must validate that production bridge.

The migrated cases are:

1. `a_parent_in_a_long_turn_has_one_message_of_news_waiting_each_child_s_latest_line_once`
2. `what_an_edit_too_late_said_goes_again_in_the_next_message`
3. `news_edited_and_taken_between_two_looks_waits_to_hear_it_was_handed`
4. `news_taken_then_a_child_read_before_the_next_look_is_told_once`
5. `news_taken_then_a_child_read_while_its_handing_is_awaited_is_told_once`
6. `news_that_didn_t_reach_its_parent_goes_at_the_next_look`
7. `news_that_came_while_its_parent_s_machine_was_away_goes_once_it_s_back`
8. `news_a_server_before_left_waiting_is_added_to_not_queued_again`
9. `what_a_parent_sees_itself_comes_out_of_its_news_and_the_message_with_the_last_line`
10. `t08_a_run_s_start_is_state_its_end_wakes`
11. `what_an_agent_saw_of_its_run_comes_out_of_its_news`

Assertions retain exact body/ID history and latest child/run content. Long-turn
revisions preserve a frozen admitted body, supersede only never-dispatched
statuses, keep one desired successor, and admit it only after exact predecessor
consumption. Delayed/old receipts cannot retire newer identities. Reads change
desired status without QueueEdit/QueueRemove against accepted originals. A read
before proof can create a fresh reduced status; that explicit revision remains
immutable even when an older source receipt arrives. A read after exact proven
consumption creates no needless successor. The legacy-found case preserves the
original legacy queue entry and moves new status into a distinct managed ID.
Real preaccept backpressure commits neither a body nor custody and leaves the
producer's exact pending original intact. Every fixture checks full originals in
actual bounded ledger history.

Only test spans and this evidence changed in this migration. Coordinator's
production `storage_floor` integration and prior-payload `FRAME × rows` scratch
bound remain intact. Parser (`rustfmt --edition 2024 --emit stdout`) and scoped
diff checks exit 0. Told SHA-256:
`a6038b488ab9de1379f342fdb2a1b5eb16bc3e1ca93394fe331c45666bd13fa3`.
The new fixture source is **uncompiled and unrun** by this lane; no own builds,
provider actors, services, full suite, audit, benchmark or profiling occurred.
Root owns the fresh compiled snapshot and scoped execution. The separate mixed
run/child fixture still reads frozen `news_of` after a newer desired addition;
its migration was outside the exact eleven-case release and remains a source
observation to check, not a claimed runtime failure.
