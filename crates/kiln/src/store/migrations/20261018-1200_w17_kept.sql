-- W17-R02 (docs/ci.md): a failed leg kept to look into, until when (Unix seconds).
ALTER TABLE jobs ADD COLUMN kept_until INTEGER;
