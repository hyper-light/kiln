-- kiln's OCI registry (docs/ci.md W12-R01): its repositories, the blobs pushed (or mounted) to
-- each (their content is in the content-addressed store, `cas_objects`, by their sha256), the
-- manifests by digest (with what they're about, for the referrers API), tags, and uploads under way.
CREATE TABLE registry_repos (
    name TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
CREATE TABLE registry_blobs (
    repo TEXT NOT NULL,
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo, digest)
);
CREATE INDEX registry_blobs_digest ON registry_blobs (digest);
CREATE TABLE registry_manifests (
    repo TEXT NOT NULL,
    digest TEXT NOT NULL,
    media_type TEXT NOT NULL,
    artifact_type TEXT,
    subject TEXT,
    annotations TEXT,
    body BLOB NOT NULL,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo, digest)
);
CREATE INDEX registry_manifests_subject ON registry_manifests (repo, subject);
CREATE TABLE registry_tags (
    repo TEXT NOT NULL,
    tag TEXT NOT NULL,
    digest TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (repo, tag)
);
CREATE TABLE registry_uploads (
    id TEXT PRIMARY KEY,
    repo TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
