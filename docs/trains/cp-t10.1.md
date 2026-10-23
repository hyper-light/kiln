# Train notes: CP-T10.1, kiln's bytes behind `Blobs`

CP-T10.1's part of merge train 1 (the critical path's CP-M4; its master plan's §6): what it
delivers, what deploying it does and how to watch it, its risks and its rollback, and what's still
open: the change to events Kiln Metrics asked for, and the decisions that are Ada's.

**Where it stands (2026-10-22):**
- **Merged** on main at 25159ff, landed at 03:17Z on Ada's word as a fast-forward from 40cd59b.
- **Not deployed:** production runs kiln:6063b42.
- **Gated** on fedora, on 25159ff exactly, from 03:05 to 03:16:49Z, with every step passing:
  - clippy;
  - the tests: 1,550 passed, none failed;
  - the macOS and Windows cross-checks;
  - the UI's checks and the vendor checks;
  - the S3 store against MinIO: 163 of 163 clauses, and 8 of 8 labs.
- The landing commit's message has the change request's evidence.

## What it delivers

kiln's server keeps its bytes in one store, kiln-seams' `Blobs` (docs/blobs.md). There are two:
- `local`, the default: today's content-addressed store, its data unchanged on disk;
- `s3`: any S3-compatible store.

Behind it are:
- CI's caches, artifacts, registry blobs and release assets;
- CD's artifacts;
- sync's contents;
- recordings;
- transcripts, archived when a conversation closes;
- runner upgrade binaries.

## Deploying it

**Config: nothing to set.** Production's `kiln.toml`, as the chart renders it today, has neither
`[blobs]` nor `[conversations]` (KilnCD's manifests of `default/kiln-production`, read
2026-10-22). So production:
- stays on the local store, its data where it is;
- keeps archives 365 days, the default of `keep_archived_days` (Ada's choice).

**At its first start:**
- **The migration** `20261021-1200_t10_blobs` adds tables, a column on `cas_objects` that may be
  empty (`released_at`), and a key for each object the CAS holds. It writes rows only: no bytes
  move.
- **Files are taken in where they lie:** sync's contents and runner binaries kept as files become
  rows, and a file whose content the store holds already is removed. `<data>/sync/incoming` is
  cleared.
- **Old conversations are archived:**
  - The transcript archiver notes every conversation closed in the last 60 days:
    `keep_closed_days`, 30 there, plus the 30 days held past it.
  - It archives at most 8 a minute (480 an hour), packed with brotli at quality 5. A backlog of a
    few thousand takes hours, and adds compression work to adacachy's load meanwhile.
  - The conversation store stops pruning the records of sessions that ended after the oldest
    close not yet archived, until that one is archived.

**Disk:** on the local store, the archives are a second, compressed copy of the transcripts the
conversation store keeps, on the same volume (`<data>/cas/chunks`). Before deploying, check that
the data volume has at least as much free space as the conversation store's file takes.

## Watching it

On production's `/metrics` (public there), its logs and its dashboard:
- **Requests:** `kiln_blobs_requests_total` with `outcome="unavailable"` stays at 0. `refused` is a
  typed answer (not found, a failed precondition), so a small share is expected.
- **Archives:** `kiln_blobs_archives_given_up_total` stays at 0.
- **Collection:** `kiln_blobs_collected_total` moves only as kiln's collection lets go of what
  nothing refers to.
- **Events** on target `kiln.blobs`:
  - at start, `adopt` notes the conversations to archive and the files taken in;
  - no warnings: an unavailable store warns with `event = "refused"`, and a conversation not
    archived warns with `event = "put"`.
- **The dashboard:** the "Blobs" row of the Kiln overview dashboard
  (deploy/grafana/dashboards/kiln-overview.json).

## Metric names, agreed with Kiln Metrics

The names went to Kiln Metrics (Observe's owner, agent 01a14bf7c0ce7451a5fd7c6f0a9764d0) on
2026-10-21 at 08:53Z. Updates followed at 21:19Z and at 01:28Z on 2026-10-22, and a reminder at
03:28Z.

Kiln Metrics answered on 2026-10-23, in two messages:
- **Agreed:** all five families below suit Observe's conventions and stay as they are on main.
  Nothing is dropped or added.
- **Why they suit:** their cardinality is bounded, and no label key collides with the tag Observe
  adds (`host`) or the ones it reserves (service, source, env, version).
- **`outcome` stays,** although most of kiln's registry says `result`.
- **The fifth,** `kiln_blobs_archives_given_up_total`, needs no label: there's only one cause. A
  monitor can alert on the count rising above 0, or on the `given_up` event.

Once deployed, production exports them under these names:

| Family | Type | Labels |
|---|---|---|
| `kiln_blobs_requests_total` | counter | `store` (local, s3), `op` (put, get, head, delete, list, start_upload, upload_part, complete_upload, abort_upload, presign), `outcome` (ok, refused, unavailable) |
| `kiln_blobs_request_seconds` | histogram | `store`, `op` |
| `kiln_blobs_bytes_total` | counter | `store`, `direction` (written, read) |
| `kiln_blobs_collected_total` | counter | `kind` (cas, sync, recordings, transcripts) |
| `kiln_blobs_archives_given_up_total` | counter | none |

Events go on target `kiln.blobs`, where `event` is put, get, list, adopt, collect, refused or
given_up. A rename Kiln Metrics asks for is one change: crates/kiln/src/metrics.rs, docs/blobs.md
("Watching it") and the dashboard's Blobs row. Nothing else reads the names.

**Two lines change in T10's next goal,** which needs a gate. When observe merges, Observe's span
bridge makes an Observe event of each `kiln.blobs` line at INFO and up that has an `event` field,
so routine puts, gets and lists stay below INFO (Kiln Metrics, 2026-10-23):
- the per-request warning for an unavailable store says `event = "unavailable"`, not `refused`;
- the line for each archived piece of a transcript (`event = "put"`, at info) moves to debug.

Nothing else changes:
- per-request refusals and the CAS's "kept" are at debug already;
- the adopt and collect summaries are at info;
- given_up, and failures that need a look, are at warn.

Observe's built-in Blobs dashboard (`builtin:blobs`, on the observe branch) lands with observe.

## Risks

- **No capability gate.** §6 puts every migration behind a capability, with read fallback for one
  release. This migration has none: it only adds, and the release before runs on it (below). The
  coordinator and Ada may want one regardless.
- **The first start's backlog:** the CPU, disk and pruning effects under "Deploying it".
- **Keys:** a key's segments are 1 to 254 bytes, and none ends in a space (MinIO refuses or
  mislists those). kiln's own keys never had such segments. A key that breaks these rules is
  refused as invalid.
- **Known, and unchanged from main:**
  - a chunk sweep can race a put that touches the same chunk again, in a window of microseconds
    (the grace is an hour);
  - the list of recordings is kept in memory;
  - a collection holds every chunk's name in memory.

## Rolling back

docs/blobs.md, "Rolling back", has all of it. For production:

- **Back to kiln:6063b42** through KilnCD. The deploy's guard does this itself when health fails.
  There's no config to take out, since production's `kiln.toml` has neither `[blobs]` nor
  `keep_archived_days`. A server whose `kiln.toml` has either must lose both first: every section
  refuses settings it doesn't know, and `kiln install` writes both into a `kiln.toml` it makes.
- **The data stays.**
  - 6063b42 applies only the migrations it knows that aren't recorded yet, skipping one it
    doesn't know (crates/kiln/src/store/migrations.rs at 6063b42).
  - It names the columns it writes, so it runs on the migrated store.
  - Its CAS upkeep (crates/kiln/src/ci/services/cas.rs) is unchanged from 64b69db, and the local
    store's tests replay that upkeep, word for word, on what this release wrote.
- **What the older kiln does with what this release kept:**
  - caches, artifacts, registry blobs and release assets are its own, read and let go as before;
  - sync's contents, runner binaries, recordings and transcript archives are pinned, so it never
    lets them go. It doesn't know their keys, so sync's runners send contents again, and a binary
    is fetched or uploaded again;
  - one case loses a key. When the older kiln reads an object as CI's, or keeps the same content
    again as CI's (a release artifact that's also a runner binary), it unpins it. An hour later it
    lets it go if nothing of CI refers to it. Coming back, this release drops the keys that held
    it, and their content comes back as above.
- **Transcripts while it runs:** the older kiln archives nothing, and doesn't hold back pruning. A
  conversation that closes under it is archived when this release starts again, unless the older
  kiln has let its record go by then (`keep_closed_days`, 30, after its close).
- **Coming back to this release:** before it serves, it takes in what the older kiln did
  (`LocalBlobs::reconcile`):
  - objects the older kiln kept become keys again;
  - keys whose bytes it let go are dropped, with a warning.
- **Still to do (§6):** drill the rollback in the lab with the real binaries: 6063b42 on a store
  25159ff wrote, then 25159ff again. The tests replay the older kiln's SQL and sweep, not its
  binary.
- **A server moved to S3 doesn't go back:** an older kiln can't read S3, and nothing copies its
  objects back. Production isn't on S3.

## Moving a server to S3

This isn't production's plan: its bytes go to Mantle next (CP-T10.2). For any server, the steps
are in docs/blobs.md, "Configuring it" and "Moving a server to S3":
1. Write `[blobs.s3]`, with the secret key in a file (`secret_access_key_file`), never in
   `kiln.toml`.
2. Stop the server and run `kiln blobs copy`.
3. Run `kiln config set blobs.store s3`, then start the server.

## Ada's open decisions

1. **Deploying 25159ff to production.** The coordinator is raising it.
2. **The capability gate** for this migration (under "Risks"): accept it without one, or ask for
   one first.
3. **Production's bytes:** the plan's course is to stay on the local store until Mantle
   (CP-T10.2). The other choice is moving to an S3 sooner: which S3, with its credentials in a
   file.
4. **Mantle's nine** (docs/mantle.md, "Decisions for Ada"), for CP-T10.2 and after:
   1. whether Mantle exports an API to embed a node in a host process, and in what form (R1);
   2. the thread budget a host grants it (R1);
   3. a whole object's SHA-256 as `x-amz-checksum-sha256` with `FULL_OBJECT`: on Mantle's native
      protocol, and/or on HTTP multipart at the cost of a read after completion (R2);
   4. how a client certificate maps to an S3 principal, and whether SigV4 goes inside the native
      protocol (R3);
   5. adding `x86_64-pc-windows-gnu` to Mantle's targets (R3);
   6. a host-supplied trait for the root key's wrap and unwrap, so Veil holds Mantle's keys (R5);
   7. short-lived credentials from Veil with session policies, checked offline (R5);
   8. kiln's CA as a cell's authority (R6);
   9. aws-lc-rs: Mantle moves its patched copy to 1.18.2, or kiln requires 1.18.1.

Ada has decided:
- landing on main ("Just merge it to main");
- keeping archives 365 days.

## Open with others

- **Mantle's agent** tells T10 when its HTTP/1.1 listener serves. Then kiln's `Blobs` suite runs
  against it, and failing clauses are Mantle's to fix (docs/mantle.md, "Agreed next").
- **The measured mix:** once production exports `kiln_blobs_*`, kiln sends Mantle's agent the
  request mix by operation, size and latency, for its listener's benchmark.
