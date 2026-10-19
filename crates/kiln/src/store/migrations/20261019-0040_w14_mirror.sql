-- `native + mirror` (docs/ci.md W14-R01): the upstream a native repository's trunk is pushed to
-- after each land.
ALTER TABLE repos ADD COLUMN mirror TEXT;
