-- The Remote Execution API's blobs (docs/ci.md W24-R26): which repository has put which blob into
-- kiln's content store through it. A repository is told of, and reads, only the blobs it has put,
-- so no one probes for another's content through deduplication (as W24-R08 has it for the runner's
-- cache). The bytes are the content store's (`cas_index`); a row outlives them harmlessly, since a
-- blob is held only while the store keeps it too.
CREATE TABLE IF NOT EXISTS reapi_blobs (
    repo TEXT NOT NULL,
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (repo, digest)
) WITHOUT ROWID;
