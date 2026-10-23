-- W08-R02 (docs/ci.md): with [ci.store.s3], when each chunk was last sent to the bucket or found
-- there by an upload that uses it; NULL while only the data directory holds it.
ALTER TABLE cas_chunks ADD COLUMN remote_at INTEGER;
CREATE INDEX cas_chunks_remote ON cas_chunks (remote_at);
