-- Code hosting (docs/ci.md W14-R01, R13): whether kiln hosts a repository (`native`, a bare
-- repository in the data directory) or follows one upstream, and a native one's trunk.
ALTER TABLE repos ADD COLUMN mode TEXT NOT NULL DEFAULT 'upstream';
ALTER TABLE repos ADD COLUMN default_branch TEXT;
