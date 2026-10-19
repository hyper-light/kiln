-- Package registries' caching proxies (docs/ci.md W12-R04): each answer kept, by the proxy and what
-- was asked (its path, query and the variant asked for), its content in the content-addressed store.
CREATE TABLE proxy_cache (
    proxy TEXT NOT NULL,
    key TEXT NOT NULL,
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    immutable INTEGER NOT NULL DEFAULT 0,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (proxy, key)
);
CREATE INDEX proxy_cache_digest ON proxy_cache (digest);
