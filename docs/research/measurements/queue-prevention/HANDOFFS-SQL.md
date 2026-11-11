# Verified handoff SQL checkpoint (not readiness)

POLICY_READ implementer: Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`; full AGENTS, rules, evidence and
CLAUDE read. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln AGENTS/ci section 0 and ROOT reread. Source base
`f555736bd887a4196c8841564cb9b4d6c1ede95f`; isolated clone
`/private/tmp/kiln-queue-prevention-20261111`, macOS arm64. Owned: new
`harness/mail/handoffs.rs`, its declaration, transfer-history helpers and terminal
projection in mail.rs, transfer/handoff portions of the fresh **undeployed**
20261111-0100 migration, focused fixtures and evidence. Root owns authentication,
current owner/capability/instance gates, physical preflight and Hub routing;
transport lane owns inactive destination import/activation. Told/wait stay frozen.

## Authority and transitions

The existing Store is the only transaction owner. A handoff record stores an
exact TransferManifest and authenticated source/destination Principal, not another
copy of message bodies. Root derives principals from authenticated connection
context; caller-supplied principal text is not authority. Internal APIs are:

```text
Principal { peer, machine, instance? }
Record { manifest, source, destination, phase }
Phase = Offered | Prepared | Committed | Aborted
offer(Store, manifest, source, destination) -> Result<Record, TransferRefusal>
prepare(Store, manifest, actorDestination) -> Result<Record, TransferRefusal>
adopt(Store, manifest, actorDestination) -> Result<Record, TransferRefusal>
abort(Store, manifest, actorSource) -> Result<Record, TransferRefusal>
get(Store, attempt) -> Result<Option<Record>, TransferRefusal>
latest(Store, session) -> Result<Option<Record>, TransferRefusal>
affected(Store, session, sourceEpoch) -> Result<u64, TransferRefusal>
mail::terminal_projection(Store, recipient, Token) -> Result<Option<Projection>, mail::Error>
```

Offer commits before its receipt can be acknowledged. Duplicate Offer returns the
stored current phase and never regresses it. Exact destination Ready changes
Offered to Prepared; repeated Prepared or a late Ready after Committed returns
the authoritative phase. Only Prepared can adopt to Committed. Exact source can
abort Offered/Prepared; Aborted retries are idempotent and Committed cannot abort.
Wrong actor/instance, manifest or attempt binding refuses without ownership
changes. Prepared is a natural event boundary: root persists it before source
Offered ACK, rather than awaiting destination wire ACK inside the shared Control
loop. No timer, waiter task, manager, cache or probe was introduced.

One unique partial index covers both Offered and Prepared for a session. SQLite
supports uniqueness over just the rows selected by the index predicate.
[SQLite partial indexes](https://www.sqlite.org/partialindex.html) (primary docs,
accessed 2026-11-11). A checked persisted ordinal gives latest(session) ordering
without a wall clock; exhaustion refuses rather than wraps.

Adoption uses one savepoint to append audit entries, rebind unresolved ledger
epochs and commit its phase. Awaited Store::call completion includes the outer
commit: an inner savepoint release alone is not durable.
[SQLite savepoints](https://www.sqlite.org/lang_savepoint.html) (primary docs,
accessed 2026-11-11), [Store::call](../../../../crates/kiln/src/store.rs).

The exact mutation predicate is recipient=session, source epoch, generation>0,
and disposition Waiting/Dispatched/Admitted. Adoption changes only epoch and
source_connection; generation, original envelope, status, disposition, terminal
field, frozen flag and source-generation metadata remain intact. Accepted or
uncertain provider work therefore keeps its same Token across A→B→C. Affirmative
preaccept Waiting can follow its existing retry path; adoption does not invent a
new generation. All historical Consumed/Cancelled rows remain untouched, avoiding
a transaction proportional to lifetime history. `affected` counts precisely this
unresolved predicate for root's physical preflight; manifest.count is trusted
export metadata and is not assumed to equal ledger rows.

An imported terminal journal receipt uses bounded, bodyless terminal_projection:
exact recipient/current token generation, only already-committed Consumed or
Cancelled, repeat=true and immutable sender scope. Stored historic epoch is not
rewritten. Root must authenticate current owner and current event epoch before
using that result; it can publish current capacity and ACK the exact terminal
copy, but grants no new consumption credit or guessed status transition.

Transfer audit uniqueness is (revision,generation,from_epoch). Its independent
AUTOINCREMENT cursor permits multiple moves for one unchanged generation and
opaque keyset pagination. Returned Transfer fields are unchanged. SQLite's
AUTOINCREMENT prevents cursor reuse and produces increasing values, with gaps
permitted; it adds storage/I/O overhead, so it is used only for this explicit
cursor identity requirement. [SQLite AUTOINCREMENT](https://www.sqlite.org/autoinc.html)
(primary docs, accessed 2026-11-11). The old per-token preaccept transfer helper
now matches exact from_epoch; its distinct affirmative no-admission semantics
were retained at coordinator direction.

## Bounds, errors and physical custody

Metadata uses the unchanged 512-KiB FRAME encoder and matching SQL byte CHECK
524288 (512×1024). Offer also encodes its longest future phase, Committed, before
acceptance; no guessed reserve or semantic clipping is used. SHA-256's 256 bits
derive the 256/8×2 hexadecimal length check. Numeric SQLite conversions and the
ordinal increment are checked. Reads bound the stored record before parsing.
Raw SQL, serialization and storage errors become typed refusals, never SQL text
or private original bodies. SQLite full maps to Capacity when available;
uncertain storage failure maps to Unavailable. Failed commit produces no ACK.

Root's handler owns actual configured storage floor plus SQLite/WAL reservation,
using handoffs::TABLES and actual affected rows while Offered/Prepared fences new
claims. Manifest count cannot size that reservation. This lane does not claim a
preflight reservation protects against unrelated writers; outer commit failure
still leaves caller/source custody. Destination imports remain inactive until
the authenticated Committed event; that transport activation is not implemented
or proven by SQL alone. The migration was edited only because root confirmed it
is undeployed; an already-migrated database would require a new numbered upgrade.

## Actual light checks

Before changing the schema, an in-memory SQLite fixture inserted a transfer for
Token(1,1), epoch1→2, then attempted the same token epoch2→3. The old primary key
(revision,generation) raised IntegrityError. Captured result:
`RED: current transfer primary key rejects A->B->C for unchanged token`.

Executed after implementation:

```sh
python3 docs/research/measurements/queue-prevention/handoffs-sql.py
```

Exit 0, SQLite 3.51.0. The script extracts and executes the exact adopt/audit/phase
SQL statements from handoffs.rs against the migration. It proves same-token
unresolved A→B→C; terminal history/body/state preservation; Offered and Prepared
active-index fences; repeated-generation opaque cursor; and rollback of audit
and epoch after injected phase-commit failure. It uses only in-memory SQLite and
closes its connection. This is SQL/schema proof, not Rust execution, connection
authentication or crash-durability proof.

A bounded in-memory mutation replaced the active predicate with Offered alone.
The same fixture failed exactly `Prepared did not retain its active session
fence`; the outer mutation runner required that failure. No production file or
database was mutated. Rust parser checks (`rustfmt --edition 2024 --emit stdout`)
for mail and handoffs exit 0; scoped tracked diff checks exit 0.

Five Rust fixture sources remain **uncompiled/unrun**: multi-epoch same-token
replay with untouched terminal rows and exact terminal projection; actor/instance/
manifest/phase fencing and Prepared abort; injected atomic rollback with typed
storage error; exact longest-phase metadata byte cap; and bounded cursor pages
for repeated generations. These do not replace native provider proof, actual
authenticated owner move/restart, resource accounting, or cleanup validation.

Frozen source SHA-256:

- handoffs.rs: `79c2c1618751d58df27979b3ef128faee60294477387cb39477ab39186ba30f3`
- mail.rs: `1f9ffe647c0fc15b22c74ebce1a84b74a9a1e8cad0a696114314d0f2d790238a`
- migration: `149ae1792af98f147b3467c96ac6daa345560d14525a19c4f477d1e0cfcaed86`
- SQL fixture: `45bafc6899956cf2609461b4a21463d44aac16c7a5d8b7cee950813a23ddedaa`

No own cargo/build, native/provider actor, service, full suite, child delegation,
commit, audit, benchmark or profiling occurred. R01/R04/R06/R07/R13 have explicit
guards/state/source and light checks above; full code review remains root's work.
R02/R03/R05/R12 primary sources, derivations, one-owner/no-cache design and shared
SQLite implementation are recorded here. R09/R10 have the SQL red/green/mutation
only, with native integration and actual crash/fault proof still owed. R08/R14
comparative/resource performance is unmeasured, with no achieved claim. R11's
seven-target and host coverage remains unrun. R15/R16: scoped implementer only,
no actors or cache cleanup, no user/source data removed; coordinator owns all
integration, evidence review, final gates and merges.
