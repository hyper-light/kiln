# Installed-skill catalogue disclosure — implementation checkpoint

Source base: Kiln `f555736bd887a4196c8841564cb9b4d6c1ede95f`, isolated clone
`/private/tmp/kiln-queue-prevention-20261111`. Implementer policy readback: full
Shards origin/main AGENTS/rules/evidence at
`7a897def2ac3bb041da584d1d6854fa066ad7fc2`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln AGENTS, ci §0, ROOT and diagnosing-bugs skill read. The previous ledger and
ops batch remains frozen. No builds, actors, live service actions, profiles or
children are authorized for this lane.

## Symptom and traced seam

The coordinator supplied the user's live screenshot symptom: literal
`<kiln_installed_skills>` instructions precede a human message beginning “Please
ensure all lanes …” in the conversation. This lane has not independently driven
the live UI or retrieved private conversation history.

The actual source chain is:

1. `skills/bridge.rs::metadata` builds behavioural guidance even for an empty
   discovered catalogue. `adapt` prepends it to every ordinary prompt.
2. `harness/session.rs::flush` transforms outgoing native writes after collecting
   original Core updates. It currently calls a stateless `bridge::deliver`.
3. Codex `item_in("userMessage")` joins native echoed text, then upserts it under
   the original client ID as a human message. Its text can therefore be the
   effective provider prompt rather than the original display text. The UI's
   `timeline.tsx` faithfully displays/copies/edits that stored text.

These are code observations supporting the supplied symptom, not independently
executed provider/UI reproduction. Relevant source:
[bridge.rs](../../../../crates/kiln/src/skills/bridge.rs),
[session.rs](../../../../crates/kiln/src/harness/session.rs),
[codex.rs](../../../../crates/kiln/src/harness/codex.rs),
[timeline.tsx](../../../../ui/src/views/harness/timeline.tsx).
The bridge itself creates no extra request/turn; repeated metadata is inserted
into existing writes. No claim that it independently caused the reported total
message rate has been established.

## Regression and decision

Added `empty_existing_skill_folder_does_not_prepend_internal_guidance` **before**
changing production logic. It calls the existing real `deliver` seam with an
existing empty folder and a Codex human request. It requires unchanged bytes;
the original `metadata([])`/`adapt` path prepends the literal block. The test is
red-capable by construction, but **not executed** in this no-build lane. The
coordinator must execute it against the old source and fixed source; do not call
this a measured red/green result yet.

Focused command for the coordinator's admitted test runner:

```sh
cargo test --locked -p kiln skills::bridge::tests:: --lib
```

The official Agent Skills implementation guide recommends progressive disclosure,
omitting catalogue and behavioural instructions when no skills are available,
filtering skills excluded from model invocation, and keeping actual activation
available. It describes metadata in system instructions/tool descriptions, rather
than presenting infrastructure guidance as a human-authored message.
[Agent Skills integration guide](https://agentskills.io/client-implementation/adding-skills-support)
(unversioned primary documentation, accessed 2026-11-11).

Keep existing native transport shapes: Codex's already-used `clientUserMessageId`
and `clientId` identify submitted/echoed messages; no guessed native metadata field
is added. ACP's existing RPC request identity must correlate its streamed echo.
The local adapter/schema usage is the protocol evidence. A UI tag regex was
rejected: a person can legitimately type or paste that exact text.

## Scoped implementation

`skills::bridge::Session` retains one loaded catalogue snapshot and the revision
advertised to one provider context. `changed()` invalidates the snapshot on actual
installation/reload events. Snapshot revision hashes deterministic skill metadata,
body, path and automatic-invocation flag. An unchanged reload does not re-advertise.
`provider_ready()` resets advertisement only for a real new provider/thread
context; an ordinary skills-list refresh is not a new context.

An initially empty/manual-only catalogue adds no guidance. A previously advertised
nonempty catalogue changing to empty emits one short revocation on the next real
prompt. Explicit manual selection still loads its instructions and arguments;
native Codex skill items, images, resources, control messages and native slash
commands retain their existing behaviour. No synthetic user message, turn, steer,
timer, debounce or periodic probe is added.

`Delivered` returns the same real write stream plus `Projection` provenance only
for prompts this bridge changed. Projection contains request/client identities
and the SHA-256 of the exact effective text. `matches` requires the exact client
identity and digest. It never recognizes internal content from tag spelling.
The provider/Core hook must use that provenance to preserve the original
Body::User text, origin, attachments and reply identity. Match raw native text
before Codex's attachment display suffix is removed. Missing client IDs require
actual existing request/turn correlation; text-only hash fallback is unsafe.

The bridge does not keep a second archive of human text. Its acknowledgement can
be saved as `Session::advertised()` and restored with `Session::restore()` when
adopting the same live provider across a Kiln upgrade; file bodies are not carried
in Frozen. Projection is serializable for the corresponding pending-request
provenance. A real new provider context clears the old acknowledgement.

## Cache/wait/resource obligations

Authority is the existing scoped session installation linked by `skills::place`;
key/lifetime is one provider context. Invalidations are existing `SkillsReload`
installation events and actual provider lifecycle changes, not elapsed time.
No measured performance/retained-memory benefit is claimed. Existing installation
limits still govern files; allocation/clone charges and all source integration
are coordinator/admission-lane validation obligations.

The source admission agent integrated Core/session/provider call sites and froze
them for the coordinator compile. Core retains Session, advertised receipt and
pending exact projections across same-process adoption; an explicit refresh enum
defers catalogue/provider invalidation to the blocking delivery pool. No core event
handler waits on the filesystem mutex. Session registers projection before stdin
emission. Codex checks exact client ID/raw effective digest before its attachment
suffix projection and returns the entire original Body::User, including peer
origin, files and reply identity. The helper alone is not the whole screenshot fix.
Correct source projection fixes copy/edit/search and stored events together; a
UI-only hide would leave those contaminated. No UI tag stripping was added.

Known limit: requestless ACP streaming chunks and old Codex echoes lacking a
reliably correlated client ID remain unverified. The source lane deliberately did
not guess from tags or text. Existing persisted historical rows that lack generated
provenance are not rewritten. Actual provider fixtures/live UI evidence are still
required; parser checks do not establish a complete multi-provider rendering fix.

New fixture source covers initial empty folders, repeated unchanged catalogue,
unchanged reload, one-time revocation, provider-context reset, explicit manual
activation, and literal catalogue-looking human text with a different identity.
Original human/peer text is never stripped or rewritten by tag matching.

Actual checks so far: owned bridge parses with
`rustfmt --edition 2024 --emit stdout` (exit 0); scoped `git diff --check` (exit 0).
Rust tests, compilation, native echo fixtures, UI rendering in Chromium/WebKit,
cross-platform checks, live red/green/mutation proof and deployment are **unrun**
by this lane. No completion or readiness claim; coordinator owns those results.


Frozen owned bridge SHA-256:
`b295279ba08e15e0f05c1121216fb4def50feaaf4cda907d6f63e4156680a5ed`.
Source-agent hooks were observed in Core skill_refresh/skill_projections,
Session::flush and Codex item_in; the source agent reports its targeted parser and
regression-source additions, with Rust execution still unrun at this checkpoint.
