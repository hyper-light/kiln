-- kiln:epoch 4
-- U05: an observed job view is not an attempt-bound producer delivery frontier.
-- Immutable bytes remain under configured Blobs keys; Amphora owns all publication state.
CREATE TABLE ci_log_accepted_views (
    job TEXT PRIMARY KEY,
    view TEXT NOT NULL UNIQUE,
    job_order INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision>=0),
    root TEXT NOT NULL CHECK(length(root)=64),
    bytes INTEGER NOT NULL DEFAULT 0 CHECK(bytes>=0),
    chunks INTEGER NOT NULL DEFAULT 0 CHECK(chunks>=0),
    operations INTEGER NOT NULL DEFAULT 0 CHECK(operations>=0),
    fenced INTEGER NOT NULL DEFAULT 0 CHECK(fenced IN (0,1)),
    qualification TEXT NOT NULL CHECK(qualification='historical_snapshot_then_legacy_observed_job_view'),
    retired INTEGER NOT NULL DEFAULT 0 CHECK(retired IN (0,1))
);

CREATE TABLE ci_log_accepted_retired (view TEXT PRIMARY KEY, after_key TEXT NOT NULL DEFAULT '');

-- The original operation survives unknown replies. A different append may not replace it.
CREATE TABLE ci_log_accepted_ops (
    operation TEXT PRIMARY KEY,
    job TEXT NOT NULL,
    view TEXT NOT NULL,
    expected_revision INTEGER NOT NULL,
    expected_root TEXT NOT NULL,
    staged_revision INTEGER NOT NULL,
    staged_root TEXT NOT NULL,
    staged_bytes INTEGER NOT NULL,
    staged_chunks INTEGER NOT NULL,
    step INTEGER NOT NULL CHECK(step>=0 AND step<=4294967295),
    input_key TEXT NOT NULL UNIQUE,
    input_hash TEXT NOT NULL CHECK(length(input_hash)=64),
    input_bytes INTEGER NOT NULL CHECK(input_bytes>=0),
    input_cursor INTEGER NOT NULL DEFAULT 0 CHECK(input_cursor>=0),
    origin TEXT NOT NULL CHECK(length(origin)<=256),
    baseline TEXT NOT NULL DEFAULT '' CHECK(length(CAST(baseline AS BLOB))<=1024),
    baseline_host TEXT NOT NULL CHECK(length(baseline_host)<=256),
    baseline_count INTEGER NOT NULL DEFAULT 0 CHECK(baseline_count>=0),
    baseline_bytes INTEGER NOT NULL CHECK(baseline_bytes>=0),
    baseline_state TEXT NOT NULL CHECK(baseline_state IN ('discovering','checking','staging','verifying','ready')),
    history_after INTEGER,
    baseline_file INTEGER NOT NULL DEFAULT 0,
    baseline_offset INTEGER NOT NULL DEFAULT 0,
    input_ready INTEGER NOT NULL DEFAULT 0 CHECK(input_ready IN (0,1)),
    input_staged INTEGER NOT NULL DEFAULT 0 CHECK(input_staged IN (0,1)),
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','accepted','no_bytes','refused')),
    refusal TEXT,
    receipt_revision INTEGER,
    receipt_root TEXT
);
CREATE UNIQUE INDEX ci_log_one_pending_append ON ci_log_accepted_ops(view) WHERE status='pending';
CREATE INDEX ci_log_accepted_ops_status ON ci_log_accepted_ops(status,operation);
CREATE INDEX ci_log_accepted_ops_view ON ci_log_accepted_ops(view,status,operation);
CREATE INDEX ci_log_accepted_ops_retire ON ci_log_accepted_ops(view,operation);
CREATE INDEX ci_log_accepted_ops_job ON ci_log_accepted_ops(job,status,operation);
-- Original refusal owns this recoverable fence. Cursors advance only with deletion of
-- fixed unpublished rows above the same checked published head. Pins/receipts survive.
CREATE TABLE ci_log_accepted_unpublished (
    view TEXT PRIMARY KEY, operation TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL CHECK(revision>=0), root TEXT NOT NULL CHECK(length(root)=64),
    phase INTEGER NOT NULL DEFAULT 0 CHECK(phase IN (0,1)),
    chunk_after INTEGER NOT NULL CHECK(chunk_after>=0),
    catalog_after INTEGER NOT NULL CHECK(catalog_after>=0)
);

-- A copied step uses the same operation and bounded raw chunk pipeline. An active
-- source lease is acquired atomically with source-not-retired, before any Blob read.
-- Descriptor remains after release for the original receipt/retry; active=0 is no lease.
CREATE TABLE ci_log_accepted_copy (
    operation TEXT PRIMARY KEY, request_hash TEXT NOT NULL CHECK(length(request_hash)=64),
    source_job TEXT NOT NULL, source_view TEXT,
    descriptor TEXT NOT NULL CHECK(length(CAST(descriptor AS BLOB))<=2048),
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
    target_attempt INTEGER NOT NULL, effect TEXT NOT NULL CHECK(length(effect)=64),
    result_hash TEXT NOT NULL CHECK(length(result_hash)=64),
    result_bytes INTEGER NOT NULL CHECK(result_bytes>=0 AND result_bytes<=16777216),
    result_ready INTEGER NOT NULL DEFAULT 0 CHECK(result_ready IN (0,1)),
    metadata_applied INTEGER NOT NULL DEFAULT 0 CHECK(metadata_applied IN (0,1)),
    output_scope TEXT, output_digest TEXT, output_bytes INTEGER,
    object_cursor INTEGER NOT NULL DEFAULT 0 CHECK(object_cursor>=0),
    objects_complete INTEGER NOT NULL DEFAULT 0 CHECK(objects_complete IN (0,1))
);
CREATE INDEX ci_log_copy_view ON ci_log_accepted_copy(source_view,active,operation);
CREATE INDEX ci_log_copy_job ON ci_log_accepted_copy(source_job,active,operation);

-- FIRST SavedHit metadata is staged invisibly in bounded pages, never one growing SQL
-- value. Its checked hash/length is fixed with the original source descriptor.
CREATE TABLE ci_log_copy_result_parts (
    operation TEXT NOT NULL, part INTEGER NOT NULL CHECK(part>=0),
    hash TEXT NOT NULL CHECK(length(hash)=64),
    body BLOB NOT NULL CHECK(length(body)<=65536), PRIMARY KEY(operation,part)
);
-- Flat transitive output custody is shared with the EXISTING CAS collector.
CREATE TABLE ci_log_copy_objects (
    operation TEXT NOT NULL, digest TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size>=0), PRIMARY KEY(operation,digest)
);
CREATE INDEX ci_log_copy_objects_digest ON ci_log_copy_objects(digest);
CREATE TABLE ci_log_deferred_delete(job TEXT PRIMARY KEY,host TEXT,capture TEXT CHECK(capture IS NULL OR length(CAST(capture AS BLOB))<=1024));
-- Job IDs are opaque identities. Idempotent UPDATEs remain legal, but retired IDs cannot
-- be reinserted/upserted while an old directory intent or authority tombstone exists.
CREATE TRIGGER ci_log_retired_job_id BEFORE INSERT ON jobs
WHEN EXISTS(SELECT 1 FROM ci_log_deferred_delete WHERE job=NEW.id)
  OR EXISTS(SELECT 1 FROM ci_log_accepted_views WHERE job=NEW.id AND retired=1)
BEGIN SELECT RAISE(ABORT,'retired log job identity cannot be reused'); END;

-- The unpublished historical capture is built in pages by its original operation.
-- A large legal old step catalog never needs to fit one value or one writer call.
CREATE TABLE ci_log_baseline_files (
    operation TEXT NOT NULL,
    step INTEGER NOT NULL CHECK(step>=0 AND step<=4294967295),
    capture TEXT NOT NULL CHECK(length(CAST(capture AS BLOB))<=1024),
    bytes INTEGER NOT NULL CHECK(bytes>=0),
    PRIMARY KEY(operation,step)
);

-- Old document order is prepared in the same bounded baseline owner, without a new
-- CREATE INDEX over all historical bodies. Verification retains one normalized step.
CREATE TABLE ci_log_baseline_docs (
    operation TEXT NOT NULL, step INTEGER NOT NULL, line INTEGER NOT NULL,
    doc INTEGER NOT NULL, version TEXT, verified INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(operation,step,line,doc)
);
CREATE INDEX ci_log_baseline_docs_pending ON ci_log_baseline_docs(operation,verified,step,line,doc);

-- A pin is durably reserved before the matching blob write starts. Neither an unknown
-- operation nor an unpublished baseline loses ownership to object-store collection.
CREATE TABLE ci_log_accepted_pins (
    key TEXT PRIMARY KEY,
    view TEXT NOT NULL,
    operation TEXT NOT NULL,
    hash TEXT NOT NULL CHECK(length(hash)=64),
    bytes INTEGER NOT NULL CHECK(bytes>=0 AND bytes<=4194304),
    put_started INTEGER NOT NULL DEFAULT 0 CHECK(put_started IN (0,1)),
    ready INTEGER NOT NULL DEFAULT 0 CHECK(ready IN (0,1)),
    ambiguous INTEGER NOT NULL DEFAULT 0 CHECK(ambiguous IN (0,1))
);
CREATE INDEX ci_log_accepted_pins_view ON ci_log_accepted_pins(view,key);
CREATE INDEX ci_log_accepted_pins_retirable ON ci_log_accepted_pins(view,key) WHERE ambiguous=0;

-- Ordered immutable chunks also carry a per-step checked prefix chain. Appending never
-- rewrites a growing object manifest. Staged rows above the published revision are invisible.
CREATE TABLE ci_log_accepted_chunks (
    view TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision>0),
    operation TEXT NOT NULL,
    step INTEGER NOT NULL,
    step_ordinal INTEGER NOT NULL CHECK(step_ordinal>0),
    step_bytes INTEGER NOT NULL CHECK(step_bytes>=0),
    key TEXT NOT NULL,
    hash TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes>=0 AND bytes<=4194304),
    previous_root TEXT NOT NULL,
    root TEXT NOT NULL,
    previous_step_root TEXT NOT NULL,
    step_root TEXT NOT NULL,
    historical INTEGER NOT NULL CHECK(historical IN (0,1)),
    PRIMARY KEY(view,revision),
    UNIQUE(view,step,step_ordinal)
);
CREATE INDEX ci_log_accepted_chunk_step ON ci_log_accepted_chunks(view,step,revision);
CREATE INDEX ci_log_accepted_chunk_projection ON ci_log_accepted_chunks(operation,historical,revision);
CREATE TABLE ci_log_accepted_steps (
    view TEXT NOT NULL,
    step INTEGER NOT NULL,
    first_revision INTEGER NOT NULL,
    PRIMARY KEY(view,step)
);

CREATE UNIQUE INDEX ci_log_accepted_steps_first ON ci_log_accepted_steps(view,first_revision);

-- NULL is the separate, explicitly unqualified historical-file source. No migration
-- relabels old documents or files as accepted bytes, and no producer-closed bit is created.
ALTER TABLE ci_log_index_progress ADD COLUMN source_view TEXT;
ALTER TABLE ci_log_index_progress ADD COLUMN source_revision INTEGER;
ALTER TABLE ci_log_index_progress ADD COLUMN source_root TEXT;
ALTER TABLE ci_log_indexed ADD COLUMN source_view TEXT;
ALTER TABLE ci_log_indexed ADD COLUMN source_revision INTEGER;
ALTER TABLE ci_log_indexed ADD COLUMN source_root TEXT;
ALTER TABLE ci_log_docs ADD COLUMN source_view TEXT;
ALTER TABLE ci_log_docs ADD COLUMN source_revision INTEGER;
ALTER TABLE ci_log_docs ADD COLUMN source_root TEXT;
