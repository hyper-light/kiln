# First plan communication uses durable custody

POLICY_READ implementer: Shards origin/main06ae067c0beda71c98a8c6f9633df9feb71fc11b;
rules SHA256df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Full current AGENTS/rules/evidence/CLAUDE reread; Kiln AGENTS/ci section0 and
prevention ROOT loaded. Root owns execution, builds, profiling and live actions.
No children/Cargo/native runtime test executed by this implementer.

Root's unchanged snapshot1794391410322496000 readiness-consumers.log executed172
cases:171passed/1failed. The remaining goals::ops::tests::a_plan_run_again_under_its_key_is_the_same_run
returned typed503: legacy delivery has no custody receipt. Exact source path:
run_claimed(Target::New) split off the first message into Lineage Ask.prompt,
counted it sent without a mail receipt, then recorded the recipient. On keyed
made_again, claim required canonical derive_id([plan-run,run,0]); Spec.prompt used
its own initial input identity and never made that server receipt. Missing
receipt remained a truthful error; converting it to success would conceal the
ownership defect.

Approved finite correction:
- New recipient starts with no initial prompt.
- Its exact session binds in existing plan_runs before waiting/publishing.
  Conditional SQL cannot replace a different bound recipient.
- Existing session attachment/bus listens before capacity check; actual owner
  capacity and epoch are read through mail_delivery::capacity. ANSWER_WAIT on the
  real runtime bounds the whole wait; no polling/probe, new timer cadence, manager
  or second outbox.
- Every communication, including index0, follows the same Registry durable path
  and derived immutable ID as existing-target runs. Initial sent count is zero;
  actual Sent/custody outcomes determine sent/retained.
- No-session/feed loss/refusal/deadline is an error, never inferred custody.
  Cancellation leaves the created recipient/run recorded and cannot invite a
  fresh agent under the same uncertain key.
- Genuine legacy/uncertain missing receipts still refuse. No new pending marker
  or recovery protocol was added. Incomplete new runs can therefore remain
  conservatively held after readiness/publication failure. The original plan and
  version remain durable; this limitation is explicit, not an automated-retry or
  full no-loss completion claim.

The narrow goals/ops regression requires the new target's canonical index0
receipt, exact revision and run/session identity, one created agent, one native
input and actual Consumed proof. Sent/retained are mutable status on replay: each
response must sum to one custody result, while immutable effect identity remains
exact. It never maps merely retained work to model delivery.

Adjacent grouped-round mismatch was source-confirmed and reported immediately:
run_claimed emits picked_rounds.len() communications, but claim Steps still counts
selected steps. A parallel two-step round produces one receipt; old replay asks
for nonexistent index1. Root requires actual red execution before this extra
count correction. Added distinct test
`goals::ops::tests::a_keyed_parallel_plan_round_replays_its_one_immutable_publication`:
real Complete provider consumes exact index0, then keyed replay must keep one
receipt/native input and no index1 publication. The existing bad claim count is
INTENTIONALLY UNCHANGED for this root red run. No green or runtime proof is
claimed until root executes the captured binary.

Frozen hashes:
plans/mod.rs80a277da6585a85806c7001dc95e24ceaa4589e736987cbfd02edb57a0e4ff02
goals/ops.rs158c3322bdf9702d4551fbd0bb2097d5363ca5299093f0f2affeeb027c3e29e8
Rustfmt emit-stdout parse and scoped git diff --check exit0. No format writes,
builds, service/actor actions or dependency changes. Root owes exact new-target
and grouped-red execution, typed failure/cancel cleanup and mutation coverage,
combined compile/clippy, all relevant caller regressions and platform checks.
No performance/resource-optimality/deployment/completion claim. Private source,
recovery backups and accepted originals were preserved.

## Executed grouped-round red and scoped correction

Coordinator execution82595 used captured source
`check-1794392067933345000`. The [result](plan-round-count-red.json) records
exit101 with expected_group_count_failure=true. The [raw log](plan-round-count-red.log)
ran three tests in1.09s: new-recipient plan identity and native connection Ready
both passed; the exact grouped-round test failed at keyed replay with typed503
missing custody receipt. Its first actual native round had already been consumed,
so this reproduces the excess receipt lookup rather than a provider/startup fault.
This is coordinator runtime evidence; the implementer did not execute the tests.

After that red, root released exactly one production hunk. claim Mode::Steps
now counts `picked_rounds(&p.items,n,&pick.of(n)).len()`, with the same
`n=min(p.steps.len(),STEPS)` as run_claimed publication. It no longer invents an
index-one receipt for a single grouped communication. All other claim modes,
source identities, legacy missing-receipt refusals and incomplete-run conservative
hold behavior remain as documented above. No additional Rust edit was made.

Frozen plans/mod.rs SHA256:
`ec87cd7c0a9b7cdfa9fdec948989f5718c03b9987e4fb4c16645b6a3e2115d4c`.
Rustfmt emit-stdout parser and scoped diff check exit0. The new-target passing
execution precedes this final count hunk; exact grouped green and final combined
checks remain coordinator obligations until their actual results are recorded.
No release/deployment or broad completion claim follows from this red alone.
