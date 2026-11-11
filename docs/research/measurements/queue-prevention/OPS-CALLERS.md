# Durable receipt caller fixtures

POLICY_READ implementer: Shards origin/main
`3830327b6c8d7ac22452a5e32ffa32799263132c`; rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full Shards AGENTS/rules/evidence/CLAUDE, Kiln AGENTS/ci §0 and task ROOT read.
Checkout `/private/tmp/kiln-queue-prevention-20261111`, base
`f555736bd887a4196c8841564cb9b4d6c1ede95f`. Role implementer; root owns execution,
integration, audits, profiles and deployment. This chunk owns only ops.rs
`end_to_end`, port.rs test module, kiln-seams harness.rs c3/c5 custody assertions,
and this evidence file. Production receipt aliases and typed NoSession are root changes.

## Reproduction and decision

The coordinator's unchanged compiled source capture
`check-1794384968309236000` ran [11 caller cases](additional-caller-tests.log):
8 passed, 3 failed. The two keyed-send failures already returned identical durable
IDs/revisions with repeat false then true, but assertions expected legacy
`message`/`accepted` fields and immediate Inbox admission. The busy case never
saw its first native prompt. An [isolated retry](caller-receipt-red.log) instead
returned typed 503: current owner capacity pending/unsupported; no custody taken.
The [port contract run](port-contract-red.log) passed HARNESS-1 and failed 2–5.
These are retained executions by the coordinator, not implementer runs.

Source inspection identifies separate boundaries:

- Production server.rs starts Registry::run_local **and** mail_delivery::run;
  both old fixtures started only Registry. Committed server custody did not by
  itself drive the native subprocess in those fixtures.
- `recording` used cat, which neither answered initialize nor completed work.
  Waiting for a PID/features did not establish current same-epoch admission.
- The busy test expected six admissions without any successful native result.
  Bootstrap has two semantic roles, in service and refill, so that workload
  cannot prove progress under the new authority.
- The old port fixture had no provider and tried three consecutive sends before
  observing any completion. `Retained` is truthful custody; rewriting it to
  `Delivered` or increasing source capacity would conceal the contract change.
- Unknown/closed recipients incorrectly surfaced Unreachable before the root's
  separate typed NoSession correction. Committed exact retries must still keep
  their receipt after closure; the root fix performs receipt lookup first.

The chosen fixture exercises the existing production boundary through actual
subprocess stdin/stdout, adapter handling, source journal and SQLite MailFlow
receipt. It does not directly call consumption hooks, change capacity, mutate
source state or synthesize receipt rows. The protocol fixture has explicit Busy
and Complete modes. Busy logs exact UUIDs and never emits a successful result.
Complete records/fsyncs the actual input, computes its deterministic output and
emits a correlated successful native result. Its duration field is the rounded-up
measured fixture operation, not a performance observation of a real API/model.
The required native result fields are taken from current claude.rs::result:
completed terminal reason, no error, positive turns/API duration, exact original
user message UUID. [The official Claude SDK changelog pinned at
9fc7ea5ff6a2caa382fceabca2fde7e53064e5a9](https://github.com/anthropics/claude-agent-sdk-typescript/blob/9fc7ea5ff6a2caa382fceabca2fde7e53064e5a9/CHANGELOG.md)
and [ADMISSION.md](ADMISSION.md) record why lifecycle Started is insufficient.
This validates Kiln's protocol proof handling, not model cognition or actual
Claude availability.

Alternative retaining cat would require independent busy sessions for every
accepted input and still could not validate positive consumption. Directly
calling a source completion method, raising bootstrap authority, or treating
HTTP custody as completion was rejected because each skips the affected path.
No new production architecture, dependency, controller or runtime constants.

## State, waits and assertions

The fixtures now start the same durable delivery worker as the server. Source
waits attach the existing session feed and Store change notifier before checking
predicates. They read the same-epoch advertised capacity or exact recipient/ID
receipt and wait for real feed/commit events. The existing ANSWER_WAIT is a bound
on the complete wait, never a retry cadence. Deadline, missing source/feed, lost
feed or query failure fail the fixture; no timeout implies consumption/capacity.
No polling, synthetic traffic, retry timer or cache was added.

Each busy steer/default/own and explicit queue/person-default case gets its own
session with one in-service input and one real communication. It asserts exact
native UUID order once, peer provenance, absence of interrupt priority, exact
queued ID/body, and zero consumed rows. Cleanup closes the test session only
after these assertions. A queued message is not declared consumed.

The keyed test compares immutable receipt ID/revision and repeat false→true;
mutable attempt generation/disposition may advance asynchronously. It checks
retained/message aliases and absence of fabricated accepted, waits for real
correlated consumed receipts before distinct unkeyed sends, proves the complete
native sequence once, and retains changed-word key conflict checks. Moved sender
identity keeps the same publication and Inbox admission/native UUID once.

Port test settlement awaits each exact source Consumed receipt before returning
the original Sent unchanged. HARNESS-3 and -5 accept has_custody, then still demand
actual native one/order and recipient isolation from the subprocess log. Each
case must have a distinct temporary log directory. No Retained→Delivered mapping
is introduced. Simulator behavior and other seam assertions are unchanged.

## Checkpoint and owed validation

Implementer light checks: rustfmt parser (`--emit stdout`, no format writes) for
all three Rust files, scoped git diff --check, and Python ast.parse of the embedded
fixture: exit 0. No Cargo, native provider, test binary, service, benchmark,
profile or deployment was run by this implementer. These checks cannot establish
Rust type correctness or runtime progress. Root must compile this exact frozen
source and rerun the three failed caller tests, all five port clauses, the seam
suite/doc truth and affected caller selection. Guard-removal mutation and actual
provider/platform coverage remain coordinator obligations. Prior compiled pass
counts do not cover this chunk.

R01/R04/R06/R07/R13: only test code changes; fixture failures remain explicit and
bounded; existing production ownership/failure handling is untouched. Test
assert/expect is intentional. R02/R03/R05/R12: reused server worker, source
protocol boundary and event seams; no capacity tuning or new production literal.
R09/R10: retained red executions above; green/mutation results still owed.
R08/R14: no resource/latency/4–5× claim; no profiling performed. R11: provider
fixture is Unix/Python3 only, matching existing recording tests; Windows execution
of these fixture cases is unverified. R15/R16: no children/heavy jobs/cleanup or
merge; root owns remaining integration and source review. User data and private
recovery originals remain untouched. This is not prevention readiness or a
deployment/no-loss completion claim.

## Follow-up: compiler import, scoped lint cleanup and goal caller fixtures

Coordinator test compilation reported missing `kiln_seams::Bus` trait imports in
source_until, proving why the earlier parser-only checkpoint was insufficient.
The test helper now imports that trait locally. The coordinator's
[consumer execution](consumer-check.log) also exposed goal caller fixture
failures. Its execution was still ongoing when this lane was assigned; no final
consumer-suite count is inferred here. [The actual clippy log](final-clippy.log)
is a failing result, not a pass for the cleanup below.

Root expanded ownership narrowly to admission.rs at reported lines
482/606/610/627/680/701, inbox/mod.rs 444/448/455/458, session.rs 1023/1439,
host.rs 1796, the source_until trait import, and goals/ops.rs test fixtures.
Only suggested equivalent conditional collapses and two needless borrows were
changed in those production spans. Four `too_many_arguments` allowances explain
why independent authenticated identity, epoch, time, immutable body and lease
inputs remain explicit at the existing seam. No panic/resource guard or protocol
meaning was weakened; no context wrapper was introduced just for a lint.

The goal tests' `stopped_agent` had no provider and no mail worker. Worse,
`taken_within` manually QueueRemoved accepted inputs and called this what the
harness would have taken. That cannot establish consumption and cannot release
new admission credit truthfully. The real-send fixtures now start the actual
worker, reuse the Complete protocol fixture and await authoritative capacity.
Their simulated goal clock/advances remain unchanged. `taken_by` waits for the
expected count and **all** visible publications for that recipient to become
Consumed, including an unexpected extra. `taken_no_more` uses the same complete
settlement barrier before exact count/order comparison; it does not hide an
unexpected publication behind an immediate Inbox check or an arbitrary two-second
sample. Both compare native UUID log order/count with Inbox.taken and require an
empty source queue. No QueueRemove simulates success.

Before the existing clock-skew `in_turn(t)` injection, the helper also waits for
the actual owner's Idle/last_turn/turn_started/summary projection to catch up with
the local native completion; a receipt SQL commit can precede that state frame.
The original manual clock injection positions and goal lifecycle assertions are
unchanged. Any remaining native-turn/clock-fixture conflict must be reported,
not repaired with an invented turn or consumption event. Existing outer `within`
bounds the goal test by wall time even though its SimClock advances only when
that test says so. Source predicates themselves wake only on real events.

The plan-run fixture settles its first two actual inputs before another distinct
run, then settles all four, preserving immutable run, exact counts and one-agent
assertions. Completing the protocol fixture's deterministic task still proves
only adapter/journal/receipt handling, not cognition or model service rate.
The rest of the goals domain assertions were preserved.

Frozen source hashes at this lane's handoff (other agents/root may subsequently
change shared files; coordinator capture is authoritative):

| Path | SHA256 |
| --- | --- |
| harness/ops.rs | b44712ba09adf435d99e50052dc484052899d033cf7f47c3cc8ec5be5f190da8 |
| harness/admission.rs | b9d0d07c046077cdb1a96e0290748a655b60a547f18b15ca4c7417185f0e6c69 |
| harness/inbox/mod.rs | bc120a73aaf55a7a6029ae5c9b894eebf0e7eafd483a61722e0f53db6ccc3eae |
| harness/session.rs | a70d0daefc2f37dc19334ddcb9a5397cdbb81929783b9cc476e5346a1b06b24c |
| harness/host.rs | 98a28f7d0e403208d2c79e279f7758a0e2bcbb1542767d0e1e1b7f68c5d168f2 |
| goals/ops.rs | b8307595986f0a23821d320b3828acefc838005c6e4d7287e901ccdd9c42d7e0 |

Light validation only: all six files parsed with rustfmt emit-stdout, scoped
git diff --check exited 0. No Cargo, native subprocess, runtime test, audit or
profile was executed by this lane. Fresh type/clippy checks, migrated goal/ops
and port/caller executions and mutation/cleanup coverage remain coordinator
work. The previous 552-test compiled source does not cover this new checkpoint.

## Goal producer identity closure and immutable-status fixtures

The completed [consumer run](consumer-check.log) was 171 tests: 153 passed,
18 failed on the earlier compiled checkpoint. Five goals/ops fixture changes were
already written but unexecuted. Source inspection of the other goal failures
found two actual producer defects among obsolete mutation expectations:

1. `goal_message` still bound changed text to the previous waiting word's ID via
   send_id_through. `say_with` had moved to immutable send_status, so this attempted
   a changed body under an existing immutable ID. The red simulator history shows
   one ID first saying `/goal clear`, then `/goal tests pass`; the real ledger
   rejects changed bodies as typed Reused.
2. Unknown was collapsed to `(ID,false)`, including identical text. It could
   publish duplicate unknown work and could lose the distinction between uncertain
   presence and actual delivery.

Root approved the finite correction. WaitingWord now carries existing typed Now
presence separately from text equality. An identical Unknown retains its exact
ID, returns Retained and issues no publication. A changed desired state may
publish an explicitly replaceable new immutable status, preserving its prior
accepted original/history; that decision is not evidence that the predecessor
is Gone. Tracked source read failure leaves Unknown. Discovery of an untracked
prior word in a partial queue still requires authoritative source lookup.

Only identical waiting text uses send_id_through. Changed intent uses the
existing per-item persisted send_id. Canonical state transitions already rotate
this attempt: Row::Reworded clears it, pending Row::Next clears it conditionally
when text changes, Row::Back/requeue clears it, and custody success clears the
attempt while recording sent_id. Restore deliberately preserves an unanswered
pending attempt. The new regression exercises the actual canonical reword
transaction, a refused successor attempt, loss of SAID memory, and exact successor
ID retry. No new schema, ID hash, manager or retry timer was added. Full native
command-shape drift across a route/stopped-state change remains an un-reproduced
risk; this chunk does not claim that separate issue fixed.

The pure seam fixtures now use StatusSpy, which records only (ID,status key) and
delegates to ordinary SimHarness. It never implements coalescing. They assert
`internal-goal-stack`, immutable original/successor body history, distinct IDs
for changed states, no extra identical publication, and no publication after
exact successor handoff. The no-coalescing fallback preserves all admitted
originals; a one-element simulated queue is not claimed. Existing real SQLite
mail coalescing/history regressions remain the authority for pending successor
supersession. The registry reconnect/state-first handoff fixture now reports the
actual new successor ID and forbids a third publication, instead of fabricating
an edit of the predecessor. Real source restart tests retain native Inbox
idempotency proof; a simulator retry log is not itself a model handoff proof.

Three source restart/refusal cases and the failing stack lifecycle/review fixture
now use the shared Complete provider and real mail worker/readiness/receipt
observations. QueueRemove no longer stands in for source consumption. A committed
exact receipt remains valid while the owner is held; the retry test asserts the
saved receipt and unchanged source/native ID rather than demanding that committed
custody become pending again. The review API checks sent+retained truthfully and
preserves review, run and lifecycle counts.

A subsequent coordinator run of the earlier goal fixture build exposed 90-second
outer hangs. The public keyed-send Complete fixture separately passed in 0.49 s,
so that is evidence the provider fixture can run, not that all goal predicates
were correct. Source_until used hub.clock.sleep with a stopped SimClock, which
never expired. Its test-only deadline now uses rt::clock and the same ANSWER_WAIT.
Goal barriers now treat Waiting/Dispatched/Admitted as unsettled; Superseded and
Cancelled remain explicit terminal history and are never labelled Consumed.
Native Inbox/log count and order assertions are retained, with terminal history
in mismatch diagnostics. A valid terminal-status count difference must be assessed
from the actual run; it is not silently counted as native delivery.

Frozen hashes at handoff:

| Path | SHA256 |
| --- | --- |
| goals/mod.rs | 6a05df57a20ec1f78673ac7c17c7ebe57bf5be3134ae0df469fd1e2ee471345f |
| goals/ops.rs | 7b28598d0138ebd187bb9dd503dedcc4a892fce8bc77b6ae5a1c4c706f180521 |
| harness/ops.rs | 9a685ec119f30cf8e166703fd1701408d671f4a7a7341a8c5c034417b15663d4 |

Rustfmt parser and scoped diff checks exit 0. No Cargo or runtime execution by
this lane. Fresh compilation, the canonical regression, all migrated goal and
caller tests, cleanup/mutation checks and supported platform verification remain
coordinator obligations. The public fixture's earlier pass and the 153 prior
consumer passes do not cover these edits. No prevention/no-loss completion,
performance, readiness, merge or deployment claim is made.

## Bounded incident regression with a real result gate

The coordinator's old caller-core execution spent about 312 s and finished
27 passed, 5 failed; the old coordinator incident was the long-running obsolete
fixture. Its cat-only process never answered initialize or completed actual work,
yet it expected unlimited goal/news/peer admission and immediate agents.wait queue
removal. Those assertions conflict with bootstrap credit and the new durable
native tool-result proof boundary. This chunk does not relabel those old outcomes
as a successful handoff.

The obsolete test is replaced by
`a_busy_coordinator_bounds_custody_and_retries_the_exact_blocked_communication`.
The Complete protocol fixture gains one optional private FIFO result gate,
configured only by the test's wrapper environment. Both FIFO ends open before the
initial correlated native Started frame; its own writer prevents premature EOF.
The gate blocks only the first actual successful result. A nonblocking external
writer releases one byte, so the runtime does not retain a blocking pool job if
the provider disappeared. No synthetic message, consumption callback, capacity
override, timer/probe or production code was introduced.

The initial ordinary agents.send owns an actual durable lease; Spec.prompt does
not bypass its accounting. One actual queued goal fills the receiver's observed
bootstrap roles. A peer retains its exact key/body in the test caller and makes
three bounded attempts: each must return typed Backpressure and create no accepted
SQLite row. During the hold, source admission stays within its advertised target,
the two accepted originals remain journaled, and no additional native input is
claimed consumed. After actual FIFO release, the initial/goal correlated results
must be durably consumed. Reoffering the same blocked key/body must yield its
precomputed exact ID, and an exact retry must return the same publication revision
with one native delivery. Earlier accepted bodies remain byte-for-byte in SQLite.

After credit returns, two explicit news states retain both original publication
histories. The first may be consumed frozen work or an undispatched Superseded
revision; the latest must be Consumed once. Native counts distinguish those
outcomes instead of treating supersession as consumption. Advancing the goal
preserves the completed predecessor in the stack and gives the new desired goal
its own immutable ID and one correlated native result.

The old agents.wait immediate-clear assertions are removed from this incident
scope. Existing exact MailHandoff proof tests remain the authority for that
separate boundary; this test claims no native MCP tool-result handoff proof.
The old unused until300s sampling helper is removed. Other shared recording
callers are unchanged unless their own wrapper explicitly supplies the gate.

At the initial freeze harness/ops.rs was
`b88c1880e1a6cb0ba42163edf40e8e48125426d9111e9bba479785e555db60d2`.
Readback requested one deterministic predicate refinement: require the exact
native Started delivery state, not only Running plus the input log, before
releasing the FIFO. Coordinator capture/final hash supersedes this initial hash.
Rustfmt parser, embedded Python ast.parse and scoped diff check passed; the
implementer executed no native fixture, Cargo or runtime tests. A fresh compiled
run of the renamed case, error/cancel cleanup, gate/platform behavior and the
remaining caller suite are unverified until the coordinator executes them.
