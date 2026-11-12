# Browser original custody and exact relay receipts

POLICY_READ implementer: Shards origin/main 749c3ca0602c43dac74976e8db713102f5b15874,
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Read full AGENTS/rules/evidence/CLAUDE, current ROOT and Kiln AGENTS/ci section0.
Scope only the approved existing browser Held/SessionStore/composer and actual send callers,
plus finite relay/proof projection chunk. Coordinator owns real-browser/integration/audit/build/deploy.
No children, full gate, production actor, commit, merge or deployment from this lane.

The source still owns lifecycle, receiver capacity and model handoff. A server Published receipt
means durable server custody, not delivery. Browser Offered/Waiting/Retained bodies stay in the
existing Held owner until positive source evidence; Pending never releases original custody.
Current authenticated opaque mail_scope binds early source progress to this producer even when
it precedes the publisher ACK. Wrong scope, epoch or publication cannot settle another original.

## Decision and primary source

[IndexedDB 3 draft 2026-10-30](https://www.w3.org/TR/2026/WD-IndexedDB-3-20261030/),
sections 2.7.1–2.7.2 and 4.4: overlapping readwrite scopes serialize, aborts roll back, and transaction
completion follows commit. Strict durability is a requested browser hint, not a measured
power-loss guarantee. File/Blob values use storage structured cloning. This removes the
localStorage read/modify/write race without a second outbox or a secure-context WebLocks dependency.

One Held IndexedDB record and its existing Zustand projection replace localStorage writes.
Every ownership-changing operation waits for its transaction to complete with strict durability;
unknown storage, unsupported strict mode, quota or abort returns false with a visible reason.
Composer retains its draft and Taken.back restores its files on refusal. Pending File/Blob bytes,
complete inline attachments and upload refs are stored; upload failure keeps the original blob
pending, rather than sending without that file. Explicit Retry files and removal remain available.
Multi-recipient sends and initial file messages use one all-or-none holdMany transaction.
Legacy raw localStorage is copied to a backup store during migration and is never deleted or
rewritten. Valid unbound records remain quarantined; invalid raw originals remain in that backup.
Opaque account scope is captured before asynchronous writes, and originals are never rebound to
another login. A failed start with files/uncertain custody cannot use the old text-only Back action
that erased its held originals; it stays retryable with an explicit message.

Bodyless BroadcastChannel commit notifications and existing visibility/source events refresh the
projection. They do not grant transmission. Atomic attempt records the actual socket identity and
source epoch/generation, so unchanged event storms do not resend. A delayed Waiting metadata write
cannot overwrite a newer attempt stamp. Retained work is offered again only to reconcile a real
new connection; server/source replay keeps the immutable original and publication identity.
No capacity retry timer or periodic storage/source probe was added.

## Actual light checks

`TSX_TSCONFIG_PATH=tsconfig.app.json node --import tsx --test --test-concurrency=1
 tests/held.test.ts tests/session-store.test.ts tests/held-uploads.test.ts tests/sending.test.ts
 tests/session-availability.test.ts`: 52 pass / 0 fail / 0 skip, log `producer-ui-node.log`.
Official fake-indexeddb 6.2.7 simulates transactions only. These tests do not establish actual browser
persistence, multi-process behavior or power-loss durability. Fixtures cover atomic competing
writers/batches, changed-ID refusal, abort/quota, Blob bytes, legacy invalid backup/unbound scope,
account boundaries, source-before-Pending, reconnect, unchanged capacity storm, pending detail
availability and positive partial-index evidence. Actual pre-fix failures included a stale
attempt-stamp and asynchronous restored-label race; the listed final command is green.

TypeScript syntax parser: 0 diagnostics in changed producer sources. Named UI files formatted with
Prettier print-width 200. `npx --no-install tsc -p tsconfig.app.json --noEmit` exit 0 and ESLint on the exact
changed named UI paths exit 0. Logs `producer-ui-types.log` and `producer-ui-lint.log`.
Full production build remains coordinator-owned and was not run by this implementer.

Native fixture source written, NOT run by this implementer:
`HELD_BROWSER=chromium node docs/research/measurements/queue-prevention/held-browser.mjs`
and `HELD_BROWSER=webkit ...`. It bundles the actual Held module in-memory, opens a disposable
127.0.0.1 random-port origin, uses two real tab connections and checks native concurrent writes,
reload+Blob, abort/quota, scope and legacy originals. It closes its browser/context/server.
No production server, credentials, persistent profile, live session or dependency install is used.

## Relay/proof chunk

Exact immutable receipt reconciliation precedes new source/storage preflight. Authenticated source
flow commits to the ledger, resolves exact recipient/token/epoch projection, updates program
progress and publishes opaque-scope receipt metadata before MailAck. Failure leaves source journal
unacknowledged. Duplicate receipts project capacity rather than a second consumption event.
Every signed retry verifies the full chain again. The existing bounded seen-proof record binds
recipient, publication revision, original message ID and exact inner command digest; only that
same publication can repeat across attempt generations. Naked/legacy replay remains refused.
Signing fixture assertions were written; root owns actual Rust execution. Root reported its later
focused compiled integration green, tracked separately in ROOT.

## Remaining coordinator proof

Real Chromium/WebKit fixture and actual browser UI behavior; production build; source/store/network
fault and reload cleanup proof; seven-target integration; resource/profile/comparative claims under
R08/R14 and all remaining R01–R16 review remain unclaimed here. The IndexedDB projection currently
reads all browser-held originals; no measured allocation or storage-optimality claim is made.
Browser retention must still be reviewed with actual pending population and file sizes. Browser
storage deletion is outside a transaction durability hint. Source attachment CAS, provider
acceptance/journal/move recovery and internal producer restart custody are separately owned lanes,
not established by these browser tests.
