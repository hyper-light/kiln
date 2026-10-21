-- CP-T10.1 (docs/blobs.md): kiln's bytes behind kiln-seams' Blobs. The local store keeps each key's
-- object in the content-addressed store (cas_objects, cas_chunks), its data where it was.

-- A key's object: the content-addressed object `digest` (its SHA-256), or, for a file kept before
-- the store had keys (sync's contents, the upgrade binaries), the whole file at `file`.
CREATE TABLE blob_keys (
  key TEXT PRIMARY KEY,
  digest TEXT NOT NULL,
  size INTEGER NOT NULL,
  modified_ms INTEGER NOT NULL,
  file TEXT
);
CREATE INDEX blob_keys_digest ON blob_keys (digest);

-- Multipart uploads under way, and their parts: each part cut into chunks as it arrives, its
-- chunks in the content-addressed store, listed here ([digest, size] in order, JSON).
CREATE TABLE blob_uploads (
  id TEXT PRIMARY KEY,
  key TEXT NOT NULL,
  started_ms INTEGER NOT NULL
);
CREATE TABLE blob_parts (
  upload TEXT NOT NULL,
  part INTEGER NOT NULL,
  etag TEXT NOT NULL,
  size INTEGER NOT NULL,
  chunks TEXT NOT NULL,
  PRIMARY KEY (upload, part)
);

-- When the local store let go of an object (Unix seconds; NULL while a key holds it, and for what an
-- older kiln kept): what an older kiln run on this store again (a rollback) kept or used is told
-- from what this one let go, and taken in when this one starts again (LocalBlobs::reconcile). An
-- older kiln names the columns it writes, so it leaves this one alone.
ALTER TABLE cas_objects ADD COLUMN released_at INTEGER;

-- What the content-addressed store held before is the local store's, each object at
-- `cas/<digest>`: the key kiln's CAS names it by (ci/services/cas.rs).
INSERT OR IGNORE INTO blob_keys (key, digest, size, modified_ms, file)
  SELECT 'cas/' || digest, digest, size, created_at * 1000, NULL FROM cas_objects;

-- kiln's CAS's own record of the objects it keeps, in whichever store: what refers to them is found
-- when it collects, and `last_used_at` keeps one just put or found from being collected under the
-- upload about to refer to it.
CREATE TABLE cas_index (
  digest TEXT PRIMARY KEY,
  size INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER NOT NULL
);
INSERT OR IGNORE INTO cas_index (digest, size, created_at, last_used_at)
  SELECT digest, size, created_at, last_used_at FROM cas_objects;

-- Closed conversations whose record (the harness's transcript) is archived in kiln's store of
-- bytes (crate::harness::archive): when it closed, due once its last lines are in, done once
-- written. Until it's done, the conversation store keeps the records of every session that ended
-- since it closed (convo::store::Store::hold_prune).
CREATE TABLE transcript_archives (
  session TEXT PRIMARY KEY,
  closed_ms INTEGER NOT NULL,
  due_ms INTEGER NOT NULL,
  done_ms INTEGER
);
CREATE INDEX transcript_archives_due ON transcript_archives (done_ms, due_ms);

-- The archived pieces of a record: lines `first` to `last` of a source, an object at `key` written
-- once and never replaced, in `codec`: `bytes` as kept, `raw` unpacked. A session that closes again
-- adds pieces of the lines since.
CREATE TABLE transcript_segments (
  session TEXT NOT NULL,
  source TEXT NOT NULL,
  first INTEGER NOT NULL,
  last INTEGER NOT NULL,
  key TEXT NOT NULL,
  codec TEXT NOT NULL,
  lines INTEGER NOT NULL,
  bytes INTEGER NOT NULL,
  raw INTEGER NOT NULL,
  written_ms INTEGER NOT NULL,
  PRIMARY KEY (session, source, first)
);
