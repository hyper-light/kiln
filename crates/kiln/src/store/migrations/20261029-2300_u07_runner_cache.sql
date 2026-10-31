-- U07 / W08-R09: one canonical inventory per physical cache root. Holds are authorization,
-- never retention. Every idle chunk/derived manifest is evictable; reservations cover write peaks.
CREATE TABLE runner_cache_tiers (
    root TEXT PRIMARY KEY,
    identity TEXT NOT NULL UNIQUE,
    max_bytes INTEGER NOT NULL CHECK(max_bytes >= 0),
    used_bytes INTEGER NOT NULL DEFAULT 0 CHECK(used_bytes >= 0),
    reserved_bytes INTEGER NOT NULL DEFAULT 0 CHECK(reserved_bytes >= 0),
    import_prefix INTEGER NOT NULL DEFAULT 0,
    evict_time INTEGER,
    evict_digest TEXT,
    ready INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE runner_cache_chunks (
    root TEXT NOT NULL,
    -- A chunk SHA-256 or manifest:<object SHA-256>; never a business object identity.
    digest TEXT NOT NULL,
    size INTEGER NOT NULL CHECK(size >= 0),
    last_used INTEGER NOT NULL,
    deleting INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(root,digest)
);
CREATE INDEX runner_cache_lru ON runner_cache_chunks(root,deleting,last_used,digest);
CREATE TABLE runner_cache_intents (
    root TEXT NOT NULL,
    id TEXT NOT NULL,
    reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
    chunks TEXT NOT NULL,
    PRIMARY KEY(root,id)
);
