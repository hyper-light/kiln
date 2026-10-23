-- Actions' tarballs in the store (docs/ci.md W07-R03, W08-R01): each a store object, so versions
-- of an action share their chunks. Keys: GitHub's actions (`gh/<owner>__<repo>/<sha>`), `kiln://`
-- actions (`kiln/<owner>__<repo>/<sha>`) and a repository's `$/` directories
-- (`own/<owner>__<repo>/<sha>-<path digest>`).
CREATE TABLE ci_action_tarballs (
  key TEXT PRIMARY KEY,
  object TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
