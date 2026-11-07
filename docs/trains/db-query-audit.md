# Train notes: the DB query audit

Ada's ask (2026-11-05): "Audit our DB queries. Determine optimizations and benchmark and profile them.
Aim for a 10x improvement." Every SQL statement kiln runs on kiln.db was inventoried, planned, and
measured on bench databases at production's size and at 10x and 100x of it (production's database
was never read). Each change was A/B'd old against new for the same rows, or tested against its
specified behavior where it fixed a bug. The benches, specs and results are under `~/.cache/dbq/`
on adacachy.

**Where it stands (2026-11-06, when written):**
- **Merged:** onto origin/main on Ada's "Merge it and ship it!".
- **Checked on the merged tree:**
  - the kiln library's tests, 3,526;
  - kiln-ci's, 800;
  - the integration tests `agents_review`, `store_indexes`, and the five that now start kiln
    through `common::kiln()`;
  - clippy on Linux (all targets), Windows and macOS;
  - the UI's typecheck, lint, tests (484) and build;
  - `replan.py --check` (no statement re-planned per call).
- **Deployed:** by replacing the bare-metal server's binary (docs/resilience.md); see below.

**What it delivers** (medians, production's size unless said):
- **Custody:**
  - the records under a delegation: 644 ms → 50 µs;
  - a hand-on's waiting receipt: 601 ms → 167 µs;
  - a session's unanswered work: 5.7 s → 0.13 ms;
  - work taken from a session: 617 ms → 16 µs;
  - a signed act's transfer receipt: 236 ms → 9 µs, and now the transfer's own;
  - identity lookups: 1.26 ms → 16 µs;
  - `relays::unanswered`: 448x, from a table of open passes kept by triggers.
- **Trail:** `verify export` 55x, its tile reads 426,596 → 223; proofs and checkpoints off the writer.
- **Store:**
  - a first start's index migrations are built after kiln serves (`store/indexes.rs`): open
    6.7-32 s → 0.67 s;
  - the overview's figures 10-40x (158-419x at 10x);
  - the scheduler's tick in one read;
  - the audit log's `q` search 14-782x;
  - the writer's and the readers' statement caches 16 → 64.
- **CI:**
  - the log indexer incremental: 42x, 3,382x at 100x;
  - the routing preview: 10x, 118x at 100x;
  - the lane's verdict book kept between refreshes: 5.5x, 12x at 100x;
  - land-queue writes: 6-12x;
  - lane lookups: 49-86x at 100x.
- **Agents:**
  - `programs::masters`: 43x;
  - the plan list: 18x;
  - goal reads: 5-87x;
  - the goal driver's writer time: 9.9 → 0.2 ms a tick.
- **Records:** `StoreRecords::scan` 164x on keys with long histories.
- **Bugs fixed:**
  - over-budget CI time counted by an id's first 8 characters;
  - review agents started close together merged into one reviewer;
  - `work_taken_from` blind past its newest 256 takes;
  - a holder's transfer receipt that was a work receipt;
  - a session named `%` seeing every agent's unanswered work;
  - the audit search's unescaped backslash.

**Deploying it:**
- Migrations `20261106-2100` to `-2108`; none raises the epoch.
- **At first start:**
  - `2100_dbq_converge` drops the indexes, triggers and tables the audit's drafts made (a no-op
    where none applied);
  - the side tables, columns and backfills apply as the store opens: about 0.65 s warm and 3.3 s
    cold at production's size;
  - the index migrations (`2101`, `2105`, `2108`) are built by the server's `store-indexes` child
    once it serves, one index per writer turn: watch `kiln_store_indexes_left` reach 0 on
    `/metrics`.
- The audit log's search index fills in the background (`audit-search`).

**Rollback:**
- The release before runs on the migrated database. Every change is additive or keeps old
  readers' columns (`plans` is rebuilt with the same columns, reordered), and the side tables'
  triggers keep them right under an older build's writes.
- main's `custody_counterpart_session` stays this release because the release before's
  `custody::unreceived` needs it. Drop it in the release after the next.

**Risks:**
- An index build holds the writer for its turn, 0.6-4.5 s measured; `/readyz` stays ready
  (unready only after 30 s with no finished call).
- If a draft of the CI migration ever applied to production's database (a test's servers opened it
  on 2026-11-06 and couldn't take its lock), `2104`'s `ALTER TABLE runs ADD COLUMN agent` fails on
  start. To check before deploying: `SELECT name FROM migrations WHERE name LIKE '20261106-%'`.
