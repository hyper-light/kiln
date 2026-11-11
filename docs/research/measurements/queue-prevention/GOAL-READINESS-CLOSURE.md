# Goal caller native-readiness fixture correction

POLICY_READ implementer: Shards origin/main 06ae067c0beda71c98a8c6f9633df9feb71fc11b;
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Full AGENTS/rules/evidence/CLAUDE and Kiln AGENTS/ci section0/current prevention
ROOT loaded. Checkout signed checkpoint0ff33887757bc0584e7a69abfa0d2759ec7d850d.
Implementer owns goals/mod.rs and goals/ops.rs fixtures only unless a real goal
logic bug is established. Root owns all runtime execution/builds/profiling/live
operations. No children or runtime test executed by this lane.

Root's checkpoint-consumers.log: 172 executed, 158 passed, 14 failed. There are
12 goal failures (8 ops, 1 stack, 3 source restart), plus two other-lane failures.
The log's positive goal observers time out with both local/projected Idle,
no last_turn, no queue. Restart rows remain pending with send_id None: the first
push never attempted publication. The plan caller already receives sent0 rather
than the old sent2 field, independently of source delivery.

Source-backed diagnosis: source_ready checks only owner Capacity.target>0.
Bootstrap Capacity can be published before native initialization finishes.
Goal decide_on explicitly does not push while Starting. The fixtures run
Registry and the delivery pump, but no background goal driver; a push made while
Starting will not automatically be revisited. The prior missing-provider fixture
waited for Exited; its Complete-provider migration lost this readiness condition.
settled_native also short-circuited on an empty ledger regardless of source status,
which permitted the cleared-stack test to run control(Start) too early.
This is a fixture precondition repair, not a production Starting policy change.
The actual failing logs support the absent-publication symptom; direct mutation
proof of the exact startup ordering remains a coordinator obligation.

The finite private patch:
- goals/ops stopped awaits authoritative capacity, then actual local AND registry
  Idle through existing source_until events/real protocol deadline. The plan test
  uses this helper. Existing native two/four input assertions remain.
- Plan sent+retained must equal two, preserving truthful custody semantics rather
  than fabricating receiver delivery from the initial response.
- goals/mod settled_native requires local/projected Idle even with zero rows;
  last_turn may be None only for that initial zero-row state.
- Three source restart fixtures await this full readiness before their first push.
- Production WaitingWord, Unknown, body/ID binding, canonical edits and all
  existing once/order/restart/lifecycle assertions are unchanged.

No new timer, polling, cache, protocol variant, controller, schema or allowance.
The existing delivery/Store bus remains the wake source. Missing source, lost feed,
initialization failure or deadline returns a fixture error, never success inferred
from absent publication. No accepted body is removed, edited or counted consumed
by the readiness wait. This patch changes no supported-platform production logic;
these Complete-provider fixtures remain Unix/Python3 only.

Prepared outside the checkout while the root Mac release reads the signed tree.
Private paths: readiness.patch; manifest.json; ops-candidate.rs; mod-candidate.rs.
Both candidates parsed with rustfmt emit-stdout (mod used skip_children=true);
git apply --check against the live checkout exited0. No tracked source write.
Candidate hashes are in manifest.json. No Cargo or actual native actor was run.
Root must release/apply, compile exact source, rerun the 12 goal failures and
related caller tests, and retain meaningful mutation/error/cleanup evidence.
No readiness/deployment/performance/no-loss completion claim.
