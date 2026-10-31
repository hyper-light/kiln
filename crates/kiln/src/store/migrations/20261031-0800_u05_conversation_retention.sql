-- kiln:epoch 6
-- U05: old pruning statements do not check an archive/content/owner receipt. The
-- reader floor is necessary, but does not drain an already-open old client.
CREATE TABLE conversation_maintenance (
    id INTEGER PRIMARY KEY CHECK(id=1),
    owner TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation>0),
    policy_version INTEGER NOT NULL CHECK(policy_version>0),
    keep_closed_ms INTEGER,
    keep_archived_ms INTEGER,
    hold_extra_ms INTEGER NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('initializing','ready')),
    backfill_after TEXT,
    backfill_horizon TEXT,
    collect_after TEXT,
    collect_horizon TEXT,
    legacy_after TEXT,
    prune_after TEXT,
    prune_horizon TEXT,
    prune_again INTEGER NOT NULL DEFAULT 0
);
-- State exists independently of the session projection: a Record or an empty
-- source reset may arrive before that projection. Maintenance deletes leave it.
CREATE TABLE convo_retention (
    session TEXT PRIMARY KEY,
    close_rev INTEGER,
    close_epoch INTEGER NOT NULL DEFAULT 0,
    close_at INTEGER,
    observed_ms INTEGER,
    origin TEXT,
    accepted_rev INTEGER NOT NULL DEFAULT 0 CHECK(accepted_rev>=0),
    last_complete_capture TEXT,
    history_rev INTEGER NOT NULL DEFAULT 0 CHECK(history_rev>=0)
);
CREATE INDEX convo_retention_close_revision ON convo_retention(close_rev);
CREATE INDEX convo_retention_accepted_revision ON convo_retention(accepted_rev);
CREATE INDEX convo_retention_observed ON convo_retention(observed_ms,session);
CREATE TABLE convo_record_sources (
    session TEXT NOT NULL,
    source TEXT NOT NULL,
    incarnation INTEGER NOT NULL CHECK(incarnation>0),
    accepted_rev INTEGER NOT NULL CHECK(accepted_rev>0),
    base_capture TEXT,
    dirty_from INTEGER,
    frontier INTEGER,
    PRIMARY KEY(session,source)
);
ALTER TABLE transcript_archives ADD COLUMN close_rev INTEGER;
ALTER TABLE transcript_archives ADD COLUMN accepted_rev INTEGER;
ALTER TABLE transcript_archives ADD COLUMN manifest TEXT;
ALTER TABLE transcript_archives ADD COLUMN status TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE transcript_archives ADD COLUMN owner_generation INTEGER;
ALTER TABLE transcript_archives ADD COLUMN policy_version INTEGER;
ALTER TABLE transcript_archives ADD COLUMN failure_ms INTEGER;
CREATE INDEX transcript_archives_identity ON transcript_archives(close_rev,accepted_rev);
CREATE TABLE transcript_manifests (
    capture TEXT PRIMARY KEY,
    session TEXT NOT NULL,
    close_rev INTEGER NOT NULL,
    close_epoch INTEGER NOT NULL,
    accepted_rev INTEGER NOT NULL,
    owner_generation INTEGER NOT NULL,
    policy_version INTEGER NOT NULL,
    history_rev INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','complete','collecting','deleted')),
    catalog_complete INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL,
    completed_ms INTEGER,
    collection TEXT,
    collection_after_source TEXT,
    collection_after_first INTEGER,
    CHECK(state<>'collecting' OR collection IS NOT NULL)
);
CREATE INDEX transcript_manifests_session ON transcript_manifests(session,close_rev,accepted_rev);
CREATE INDEX transcript_manifests_collect ON transcript_manifests(state,completed_ms,capture);
CREATE TABLE transcript_manifest_sources (
    capture TEXT NOT NULL,
    source TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    accepted_rev INTEGER NOT NULL,
    frontier INTEGER,
    rows INTEGER NOT NULL DEFAULT 0,
    progress_last INTEGER,
    base_capture TEXT,
    dirty_from INTEGER,
    complete INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(capture,source)
);
CREATE INDEX transcript_manifest_sources_completion ON transcript_manifest_sources(capture,complete);
-- The legacy numerical range key cannot represent a reset or changed old n.
-- Preserve its objects for reads, but never promote legacy metadata to a proof.
ALTER TABLE transcript_segments RENAME TO transcript_segments_before_receipts;
CREATE TABLE transcript_segments (
    key TEXT PRIMARY KEY,
    session TEXT NOT NULL,
    source TEXT NOT NULL,
    first INTEGER NOT NULL,
    last INTEGER NOT NULL,
    codec TEXT NOT NULL,
    lines INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    raw INTEGER NOT NULL,
    written_ms INTEGER NOT NULL,
    capture TEXT,
    incarnation INTEGER,
    sha256 TEXT,
    indices BLOB,
    state TEXT NOT NULL CHECK(state IN ('legacy','putting','ready','collecting','deleted')),
    collection TEXT,
    CHECK(state<>'collecting' OR collection IS NOT NULL)
);
INSERT INTO transcript_segments(key,session,source,first,last,codec,lines,bytes,raw,written_ms,state)
    SELECT key,session,source,first,last,codec,lines,bytes,raw,written_ms,'legacy'
    FROM transcript_segments_before_receipts;
DROP TABLE transcript_segments_before_receipts;
CREATE INDEX transcript_segments_session ON transcript_segments(session,source,incarnation,first);
CREATE INDEX transcript_segments_capture_state ON transcript_segments(capture,state,key);
CREATE INDEX transcript_segments_capture_key ON transcript_segments(capture,key);
CREATE TABLE transcript_manifest_pieces (
    capture TEXT NOT NULL,
    key TEXT NOT NULL,
    source TEXT NOT NULL,
    first INTEGER NOT NULL,
    PRIMARY KEY(capture,source,first)
);
CREATE INDEX transcript_manifest_pieces_key ON transcript_manifest_pieces(key,capture);
