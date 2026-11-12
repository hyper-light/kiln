# Program notice and plan stack fixtures

Implementer POLICY_READ: Shards origin/main
3830327b6c8d7ac22452a5e32ffa32799263132c; rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Kiln worktree base f555736bd887a4196c8841564cb9b4d6c1ede95f plus shared uncommitted
prevention changes. This batch changes test fixtures only; root owns execution.

The actual pre-change run in [consumer-check.log](consumer-check.log) executed 171
tests: 153 passed, 18 failed. The two scoped failures were:

- Program settings notice had an authoritative Waiting receipt and `delivered=false`;
  the old fixture asserted delivery although it had not started the mail relay.
- Plan stack step 1 returned `state="nothing"`, empty paths and no SHA. Source inspection
  places that outcome before stack Git creation when the reported changes are empty.
  A delayed source initialization clearing the injected report is a hypothesis, not an
  independently traced production defect; no production Reset behavior changed.

The program fixture now observes Waiting before starting the real relay, then waits
for correlated source admission and the settings projection. Legacy recovery clears
the simulated legacy marker's publication explicitly and calls `resume_notices`;
the assertions require the same publication attempt and one identical stored original.
All prior settings, ownership, goals, weights and usage checks remain.

The Unix plan fixture uses the existing protocol stand-in that durably records actual
native inputs and emits correlated completion events. Real relay and goal owners run.
Before reporting file work, it waits for the latest server goal-stack publication to
be Consumed and for both source and registry to be Idle. It also checks that the
actual `paths_of` projection includes the file. Goal completion remains the fixture's
explicit store update, as before. Existing Git author, parent, SHA, restack, branch
and index checks remain. Cleanup stops only this disposable hub's sessions.

Three existing platform-independent source wait helpers are available to the
cross-platform program test; the Python/shell provider helper remains Unix-only.
Their existing protocol deadline bounds hangs; it does not poll or synthesize
capacity, admission or consumption.

Actual light checks: `rustfmt --edition 2024 --emit stdout <path>` passed for each
of the three files, and named `git diff --check` passed. No Cargo command, fixture,
provider, production service, live SCM run, benchmark or full gate was executed by
this implementer. Compilation and actual regression outcomes remain unverified for
this batch until the coordinator's captured run.

Frozen SHA256:

- `crates/kiln/src/harness/ops.rs`: 9a685ec119f30cf8e166703fd1701408d671f4a7a7341a8c5c034417b15663d4
- `crates/kiln/src/programs/tests.rs`: 7d4b97be072f55dffffe61500c4578ac5f3f2808997d3272c2cc8b546243a4a5
- `crates/kiln/src/plans/stack.rs`: f3d6d2b43fc51848a11f3fad381894601f139fb12cd729c3ec489769955e08f5

Root execution selectors (reuse the captured test executable, one at a time):

```text
--exact programs::tests::a_program_shows_each_track_s_worker_goal_pass_and_slot_and_holds_the_usage_rules --nocapture
--exact plans::stack::end_to_end::p_ac1_a_plan_s_steps_end_as_a_stack_of_diffs_each_by_its_own_agent --nocapture
```

## Three keyed plan-run fixtures

The coordinator reported a later actual caller run with 27 passes and 5 failures;
three scoped keyed plan-run fixtures still used a missing provider without source
readiness and received a typed owner-capacity-pending refusal. This implementer
did not execute that run. Only these three test bodies and their Unix attributes
changed in the follow-up:

- `a_keyed_plan_run_retried_after_its_agent_moved_is_the_same_run`
- `a_key_given_to_another_plan_run_is_refused`
- `a_key_given_to_a_plan_run_in_one_mode_is_refused_in_another`

They use the existing Complete provider and event-based readiness. Direct plan-run
messages are identified from the actual returned run ID and their production ID
derivation, then wait for correlated Consumed receipts before retry or another run.
Step agents use a separate recording file and the real goal owner, then wait for
their actual input consumption too. Assertions keep the original key/sender-move
and changed-request/mode 409 behavior, stable run/session identity, total sent plus
retained count, and exact-once Inbox/native ID vectors. No queue is cleared and no
receipt is invented. Cleanup stops only each fixture's own hub sessions.

The three native fixtures are now explicitly Unix-only because their existing
provider helper uses a shell wrapper. Windows native coverage is unverified; pure
key-scope/refusal coverage remains cross-platform. Parser and named diff checks
passed; compilation and execution remain coordinator work. No Cargo/tests were
run by this implementer. Final follow-up `ops.rs` SHA256:
`cf77fcbf7c4d686c6249a49c86be0671ce13c3cc8cd7482e83406362777deb97`.
