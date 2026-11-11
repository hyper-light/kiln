# Shards false offline: bounded Kiln state transport (in progress)

Coordinator owns integration, audits, profiling, evidence and deployment. Source base is
`812e968e2a182e11075e355a437183f8491530b1`, isolated checkout `/private/tmp/kiln-offline-fix-20261110`.
Product changes are uncommitted and **not deployed**. The original dirty Kiln checkout is untouched.
Policy readback is in POLICY_READ.json; current Shards rules SHA256 is
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

## Reproduction and decision

Live diagnosis `/private/tmp/shards-agent-offline-diagnosis-20261110.json`: runner
`e30189ea29182644fe5a4b2e06c7dac2` connected, Shards agent `01a148884a4770fdaf0111f3f792a248`
Info idle/564 queued, stale State disconnected/458 queued. Runner log repeatedly reports
962187-byte state dropped even after trimming. This is a transport failure, not proof the
agent process is offline. No duplicate queue ID was established.

D01: Keep the existing 512 KiB control cap. Its value predates this change; the new path reserves
the actual postcard Harness envelope and maximal existing taken_us stamp from it. A fitting State
keeps its exact prior JSON. Oversized State sends status, epoch, counts, bounded compact queue index
and explicit deferred descriptors. Full source bodies remain in the owner and canonical convo row.
Metadata is reserved before optional bodies. Large strings are read as UTF-8 ranges; collections
as bounded pages. Never assemble an entire unbounded logical State at the receiver.

Alternatives rejected: raising cap leaves growth unbounded; dropping Info.queue breaks existing
goal/news/handoff presence; arbitrary entry-count truncation does not bound encoded bytes; clipping
message text changes authoritative edit inputs; whole-state chunk reassembly moves the same memory
problem to receivers. Deferred queue absence is Unknown unless the source confirms it at the requested
revision. Do not add speculative Ambiguous states or unrelated queue admission changes.

D02: Existing request/reply and source session loops own reads/actions. Field SHA256 revisions include
epoch; unrelated status/usage does not stale queue pages. Source checks queue revision and current ID
under the same lock as its existing adapter action. This applies the precondition pattern in
[RFC 9110 §13.1.1](https://www.rfc-editor.org/rfc/rfc9110.html#name-if-match), June 2022.
Consistent pagination is an established requirement in
[Kubernetes API chunking](https://kubernetes.io/docs/reference/using-api/api-concepts/#retrieving-large-results-sets-in-chunks)
(docs read 2026-11-11); our source refuses a changed revision rather than retaining old snapshots.
Named state-pages negotiation gates new frames; the first post-Welcome feature reply refreshes quiet sessions.
No postcard variant indices or capability bits change.

D03: Use serde_json 1.0.154's existing borrowed
[to_writer](https://docs.rs/serde_json/1.0.154/serde_json/fn.to_writer.html) seam with a bounded writer.
Queue receipt digest remains u64 but uses existing SHA256 prefix semantics. Rust 1.99.1 documents that
[DefaultHasher's algorithm is unspecified across releases](https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html);
it cannot be a cross-platform wire digest. This is not an authorization hash; full field SHA256 fences actions.

## Actual results so far

- Native Mac ARM64/macOS26.4.1 RED: frozen base812 plus regression only,
  `cargo test --locked -p kiln --lib state_delivery_survives_a_queue_larger_than_a_frame -- --exact agent::tests::state_delivery_survives_a_queue_larger_than_a_frame`.
  One executed test fails because state is dropped. Raw baseline-macos-red.log; frozen source
  `/private/tmp/kiln-offline-red-20261110`, patch `/private/tmp/kiln-offline-baseline-red.patch`.
- First combined compile failed with16 errors before tests; first-combined-compile.log retained.
- Corrected source: state_view::tests16 passed, no failures; state-view-tests-macos.log. Compiler
  linker warned compact unwind section too large; vendored focal-memory has pre-existing deprecation warnings.
  /usr/bin/time peak RSS22,027,403,264 bytes exceeded requested16GiB; subsequent compile admission requests24GiB.
- Cached same product binary focused integration/schema:20 passed,2 failed. Offline regression,
  source adapter stale/current mutation, final reply envelope, retained bounded index and typed absence passed.
  Contract failed only on AtState doc label; schema naming check failed on new generic type collisions.
  first-regression-schema-tests-macos.log. Both source corrections written, retest owed.
- UI typecheck and lint exit0. Full UI tests initially683pass/3 EPERM creating fixtures under ~/.cache;
  normal cache access run686pass/0failed, raw ui-tests-with-cache-access.log (27.4s).
  Later generated-type alias changes will need revalidation.
- Current ROOT Reach r13 compiles coordinator-owned state_view_measure allocation fixture and requests
  one sample per case; no measurement result yet. Pure view source hash before compile
  8d80a40ef5e4b0e50f3f0a88af8dce68216575e2e181148f240dfd8290cf67af. Wait implementation edits were
  concurrent, so this run cannot certify final combined wait behavior. No performance claim.

## Wait/cache and state obligations

Info.queue remains local/serde-skipped and bounded by encoded metadata bytes. Partial index carries
revision/cursor and cannot prove absence. Existing news/goal/handoff consumers query owner through
the same live feed, one frame at a time, bounded by existing ANSWER_WAIT; failures preserve uncertainty.
Handoff keeps existing count/text budgets and claims; lost removal acknowledgments remain uncertain.
Wait early wake must see suffix mail; a per-call queue-revision probe is being implemented with the
remaining original wait deadline, no new timer/global cache. Existing RECHECK fallback is unchanged.
UI retains one replaceable page per field; field revision/generation, reconnect and disposal fence replies.
Incomplete nodes are not editable. Fitting approval/task controls remain inline; complete deferred items
reuse existing controls. No full-state hydration or accumulating page cache.

## R01–R16 account and remaining gates

| Rules | Evidence/status |
|---|---|
| R01/R04/R06/R07/R13 | Typed budget/stale/path/read errors and explicit partial states written; root panic/complexity audit and clippy pending. |
| R02/R03/R05/R12 | Decisions above; existing cap/deadlines reused, metadata bytes derived from actual encoding; wait/cache integration pending. No platform fork. |
| R08/R14 | Allocation fixture written; actual distributions, faults/parent costs pending. No achieved 4–5× or leak-free claim. Unbounded serialization baseline is not equivalent capped behavior. |
| R09/R10 | Exact live mismatch and executed native red; partial green above. Final original live green and guard-removal mutation still owed. |
| R11 | MacARM64 test results only. Linux/Windows lint/build and four Kiln release artifacts pending; no IntelMac support. Shards seven-target policy does not imply Kiln ships seven artifacts. |
| R15 | Three retained implementers read current policy; scoped source/light parser checks only. Root owns all executed tests/profiles/audits. Signed source commit and final checks pending. |
| R16 | Mac free space60–62GiB observed; private owned Cargo target. No user data/evidence/source removed. Keep at least30GiB. |

Deployment is authorized by the user, but not done. Normal primary-server admin.token authentication
returns401. Automatic approval review rejected reading service process environment for another admin
credential; do not retry indirectly. Existing authenticated browser tab was discovered, but documented
DOM/screenshot operations fail before providing state. A supported deployment credential/config path
has been requested, with no secret to be printed. Build/validation work proceeds independently.
Use tested, signed identical-source cross-platform artifacts via Reach and established blue/green update
machinery; inventory sessions/terminals before any runner promotion. No force push, signing disable,
service restart, runner reinstall or unrelated peer messages.

## Latest steering and continuation (2026-11-11 03:40 UTC)

User explicitly instructed: use Reach directly, skip full gate, build/deploy in background, and try
to shake Shards loose meanwhile. The API credential question is superseded: do not make it a blocker.
Direct host deployment through Reach is authorized. No further full gate/profile/browser campaign.
Root optional allocation build r13 was stopped with no completed measurements; its source fixture
was moved into this measurement directory and is not a Cargo example. r14 was stopped waiting on
the shared owned target lock. r15 currently compiles/runs focused regression checks plus schema
generation (KILN_WRITE_TS=1), CARGO_NET_OFFLINE=true, 2CPU/24GiB/8GiB. r17 syncs this isolated
source checkout to a Linux runner and prints source/tool metadata only. Both tell on completion.
git repack -a made clone objects self-contained (84MiB); no original checkout edits.

Shards wake-up was explicitly authorized and sent once using idempotency key
`shards-wake-20261111-01`; accepted seq9582, message
`from-agent-01a1c860cb617c389d6385f44f554a4d-fd3db32d43538f65adf5795505de2274`.
API Info starting/609 queued is ALSO stale: local saved queue716 then718. Local Codex rollout
metadata for source thread01a1c87d-b08c-77e3-b8cf-20f37437a7c5 shows active tool calls and completed
results at03:37:29–30 UTC, file4,755,104B; not a startup hang. No restart/interrupt/model change or
queue deletion performed. Later file mtime/size check independently observes ongoing writes.

Automatic review rejected a command that included full Saved.ran process records; a safer bounded
metadata read succeeded (counts, integer PIDs, boolean interrupted flag only). No conversation,
process command contents or environment credentials exported. Do not repeat the rejected read.

Pending finite implementer writes: engine owns ONLY convo/resume.rs guard refusing an explicitly
deferred saved State during failover (default CAP_CONVO ships raw source separately; a projected
snapshot must never be silently resumed as complete). UI owns ONLY StateMetadata/StateRead/
StateStep/StateDescriptor generated import/re-exports plus immutable row keys. Both reread current
b6af4ec8/df2 policies as implementers; no builds/actors/audits allowed. Wait lane is frozen; latest
wait.rs SHA4e553f3da016347f4f0ea142eb6357da7eb3fe11fa1c71026bd47e4b143c7388,
state_reads.rs SHA8566876b2aad70a48a8909971b5a89a52ddcb072f34ee1f4bf2ac6bf177b2c1d.

Next ROOT: collect r15/schema result and finite source batches; run required UI build (it typechecks),
sign exact source commit with /Users/adalundhe/.kiln-run/bin/gits without pushing; release scripts
via Reach for MacARM64, Linuxamd64/ARM64musl, Windowsamd64. Keep source/artifact hashes identical.
User skipped full gate, report unrun checks plainly. Direct server deploy can stage a validated
binary, back up current executable and atomically replace it as updates::replace_binary does;
existing supervisor takes it up. Inventory runner sessions/terminals before promotions; use
existing blue/green update path for active Mac sessions, do not force or kill them. Source code
shows plain Mac install --agent --upgrade may wait idle/10min and resume active processes; prefer
server-issued established blue/green once server staged. Manual file placement avoids admin token
only for server; assess correct host-local supported path for runner promotion from source.
No release artifacts have yet been built or deployed; no signed commit exists yet.

Final focused checks: r15 executed26passed/1failed, the sole failure being UI duplicate wire
interfaces while their approved import pass was pending. After the UI froze, the same binary's
wire::ts6passed/0failed (schema-final.log), and UI page/session31passed/0failed
(ui-page-final.log). UI production build passed TypeScript and asset compilation, then failed
binding its temporary localhost prerender listener under the sandbox; normal-loopback-access
build is now running. convo/resume.rs final guard/test source was written after r15 compilation;
that one added test is unrun, release compilation will check its production guard.
