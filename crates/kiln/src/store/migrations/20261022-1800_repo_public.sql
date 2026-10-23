-- A native repository anyone may read over git, as anyone may clone a public GitHub repository
-- (docs/ci.md W14-R01). Pushes still need a token.
ALTER TABLE repos ADD COLUMN public INTEGER NOT NULL DEFAULT 0;
