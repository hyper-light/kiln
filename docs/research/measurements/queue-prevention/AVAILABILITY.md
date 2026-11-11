# Owner Open lifecycle reconciliation

POLICY_READ implementer: Shards origin/main `0a6d91b3de0f10a8397868aa3794adab1e09afdc`,
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln base `f555736bd887a4196c8841564cb9b4d6c1ede95f`, shared clone
`/private/tmp/kiln-queue-prevention-20261111`. Root owns integration, live evidence,
audits, profiling, resource admission and deployment. This lane has no actors or builds.

## Decision and source evidence

The prior live failure and bounded-state fix are retained in the earlier state-view ROOT.
This lane identifies an additional source-level gap; it has not independently reproduced
it against production. A fresh owner `Update::Open` replaces Mirror.info while retaining
Mirror.state, including the server's previous synthetic `disconnected` status.
An Open with `since: Some` currently sends no viewer update. A later attach therefore can
receive fresh Info followed by stale State, and an existing viewer can receive neither
fresh lifecycle value. `SessionStore.begin` also seeds cached State; its `gather` then lets
State overwrite Info.status. Composer and sendHeld use that State.status as an offline gate.

Sources inspected: registry.rs::apply Open and viewer commit, disconnected, inventory;
copies.rs::View::parts and Mirror::state_frame; session.rs::Info; model.rs::Status;
agent.rs::push_update/harness_frames and first STATE_PAGES Features refresh;
ui/api/harness.ts::begin/gather/take and views/harness/composer.tsx::sendHeld/sendText.
Registry dispatch uses current location, instance/inbox capability and epoch, rather than
the cached lifecycle label. A viewer socket being open only establishes browser-to-server
transport, so it alone cannot prove the owning runner or model is available.

Primary sources read 2026-11-11:

- [WHATWG WebSockets, 2026-10-19](https://websockets.spec.whatwg.org/#dom-websocket-readystate):
  OPEN describes the WebSocket connection; queuing a send does not establish application custody.
- [gRPC health checking, revision 571730d](https://grpc.io/docs/guides/health-checking/):
  service readiness is distinct from physical connection state; streaming Watch delivers changes.
- [Kubernetes Node Status](https://kubernetes.io/docs/reference/node/node-status/):
  availability conditions distinguish positive, negative and unknown knowledge; detailed state
  and lightweight availability need not be coupled. Its periodic leases are not adopted here.

Inference: reconcile the already authoritative owner Open into the existing lifecycle header,
and publish that header independently from pending detail/history. Do not derive Running from
transport connection, backlog, an acknowledgement or elapsed time. No new wire protocol,
availability manager, polling, probes, heartbeat, deadline or cache is needed for this gap.
Raising frame caps does not fix event authority; accepting arbitrary roster events as fresher
than a session stream would introduce a second ordering problem. Both alternatives are rejected.

## Transitions and invariants

- Owner Open accepted by existing owner/epoch routing: Info.status becomes current and an existing
  State gets only that lifecycle status reconciled; no default State is invented when none exists.
  Its queue, deferred descriptors, counts, field revisions and body values remain untouched.
- Open/resumed on the current browser socket: publish its lifecycle immediately while snapshot
  detail/history continues through the existing snapshot generation. A same-snapshot stale status
  cannot revert the Open header. A subsequent live State remains authoritative.
- Resumed-since source Open: publish a normal existing state frame/patch when lifecycle changed,
  retaining the event suffix and avoiding an unnecessary timeline reset.
- A header-only resumed Open uses existing Resumed when lifecycle changed, without inventing State.
  A full source Open that changes cached lifecycle uses existing RESYNC, so older viewers attach
  to coherent Info/State. Two frames at the same revision would be filtered by the viewer and
  cannot reliably deliver both Open and a new State.
- Starting, Idle, Running, Waiting and Exited remain exactly what the owner says. Pause/frozen
  semantics are retained; connected is never substituted for Running.
- Machine/session link loss still emits Disconnected through the existing path. Browser socket
  loss still holds messages locally. A source Disconnected stays offline until an owner event
  actually changes it; no guessed liveness or freshness timeout clears it.
- Composer eligibility reads the reconciled lifecycle separately from browser command-channel
  state. Local held messages keep existing ordering, upload waits, immutable ids, retries and ACK
  custody semantics; no local/live delivery is fabricated and no accepted message is dropped.
- Old socket generations are already ignored. Existing source owner/epoch fencing remains in
  force; the change must not authorize an old owner Open to revive a moved session.

## Owned change and validation plan

Root reserved ui/src/api/harness.ts; a small pure session availability helper; composer.tsx;
agent.tsx status selection only; matching UI tests; registry.rs Open block and its viewer
emission only, plus adjacent focused regression. Dispatch/mail integration belongs to root.
No generated wire edits, feature expansion, production service actions or commits in this lane.

Write regressions before production edits: cached Disconnected then current Open/resumed Idle,
first attach with inconsistent Info/State, immediate lifecycle before pending history, subsequent
real Disconnected, Starting/Paused/Exited preservation, no-State Info, and old socket exclusion.
Backend fixture must inspect both copy Info/State and an already attached viewer after resumed
Open while preserving event history. UI fixtures use actual SessionStore frame ingestion and
the exact pure composer eligibility selector, not rendering-only assertions.

Targeted pure Node tests may use the existing dependency installation; no npm install or UI/Rust
build is authorized here. Record actual red/green commands and results below. Rust regression,
full required UI checks, live original-symptom validation, mutation checks and all R14/R15
audits/profiles remain coordinator-owned and unrun until explicitly recorded.

All new literals must come from existing enum/protocol semantics. No tuning constants are added.
No comparative performance, live recovery or deployment claim follows from this source proposal.

## Results

2026-11-11 implementer source batch, no commit:

- New pure `ui/src/api/session-availability.ts` separates browser transport eligibility from owner
  lifecycle and reconciles only status. SessionStore publishes owner Open lifecycle immediately;
  snapshot State cannot undo its header, while a State with a later existing source revision is
  committed as live immediately. No State is created from Info. Composer/sendHeld and the agent's
  two status selectors use this source control authority. Existing queue bodies, counts, field
  revisions, local holding, message ids, ACKs and retries are unchanged.
- Registry changes are confined to the reserved Open block, viewer-frame emission and one focused
  test. Existing Hub::judge_runner_update rejects an older owner epoch before apply; no new
  authority bypass is added. No command dispatch/mail integration or generated wire edits.
- Exact initial red command, before production edits:
  `TSX_TSCONFIG_PATH=tsconfig.app.json node --import tsx --test --test-concurrency=1 --test-name-pattern='W20: owner Open|W20: first attach' tests/session-store.test.ts`
  from ui/. Exit1, 2 executed tests/2 failed: owner lifecycle remained `disconnected` instead of
  `idle`; first-attach Info stayed absent instead of `starting` while details waited.
- An additional ordering regression failed against the intermediate fix: a live State revision21
  was incorrectly held at the resumed header's Idle revision20. The same command with pattern
  `W20: a newer live lifecycle` exited1, 1 executed/1 failed. The final code uses existing frame
  revisions, not a timeout or freshness probe, to distinguish later live lifecycle.
- Final targeted command after formatting:
  `TSX_TSCONFIG_PATH=tsconfig.app.json node --import tsx --test --test-concurrency=1 tests/session-store.test.ts tests/session-availability.test.ts`
  from ui/, exit0, **29 executed/29 passed/0 failed**. Raw retained output:
  [availability-ui-tests.log](availability-ui-tests.log). This covers prior reconnect, cache,
  speculative budget, queue-page revision and outgoing-custody fixtures as well as new regressions.
- Root approved an ignored `ui/node_modules` symlink to the prior clone's existing installation;
  created once and left in place for root validation. No linked dependency was changed, no npm
  install/build run. The earlier external-loader/NODE_PATH attempt could not resolve React and
  executed no assertions; that infrastructure failure was not counted as a bug reproduction.
- Prettier200 executed only on the six changed named UI source/test files, exit0. No Rust format,
  Rust compile/test, full UI typecheck/lint/build, browser campaign, live action, performance check
  or deployment executed by this lane. Backend regression is written and **unrun**; root owes it
  and the original live symptom/mutation validation. No readiness/performance/deployment claim.

Compatibility limit: source revisions identify later live frames on current peers. A truly legacy
peer that omits revision markers cannot prove that a frame arriving during its unfinished snapshot
is newer than that snapshot's header; the existing snapshot completion boundary is retained there.
Disconnected without a new owner event is not optimistically cleared. Pause/exit/detail fields
are retained as existing lifecycle data; this lane does not guess their freshness or change them.
First attach publishes Info early, but the existing Agent useSessionReady boundary still waits
for its initial history snapshot before mounting the full workspace. Its loading composer keeps
local input meanwhile. This batch does not change that readiness/Suspense architecture; immediate
messaging eligibility while details wait is validated for an already mounted/cached session.

R01/R04/R06/R07/R13: no new panic path, typed lifecycle/eligibility and explicit event transitions;
source inspection and focused UI fixtures above, coordinator final audit/Rust check owed.
R02/R03/R05/R12: cited event-authority decision, existing revisions and frames only, no new tuning
constant/cache/wait/probe/platform adapter. R08/R14: no comparative or resource claim; not profiled.
R09/R10: exact UI regression red/green above; production reproduction belongs to retained prior
evidence and original live green remains owed. R11: platform-neutral TS/Rust edits, native Rust and
Chromium/WebKit validation unrun. R15/R16: scoped source and light Node tests only, no build, actor,
commit, cleanup of user data, service or release-checkout edit; root owns remaining integration.
