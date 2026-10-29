-- A change's identity (docs/ci.md W26-R03): kiln's own, carried in commits as `Kiln-Change:`.
-- `changes`: each change by identity, on its repository's branch, with what it was split from
-- or absorbed into (a squash). `change_aliases`: what points to a change without defining it:
-- Gerrit's `Change-Id` trailer (kind `change-id`; kiln wrote them before W26) and the
-- `change-id` header jj and GitButler write (kind `header`). `change_numbers`: the numbers a
-- change has on each host (`kiln`: D41, CR4; `github`: #482; `gitlab`: !91; `bitbucket`: #7).
-- A diff's versions stay in diff_versions, each tied to its change through its diff.
CREATE TABLE IF NOT EXISTS changes (
    repo_id TEXT NOT NULL,
    id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    split_from TEXT,
    absorbed_into TEXT,
    PRIMARY KEY (repo_id, id)
);

CREATE TABLE IF NOT EXISTS change_aliases (
    repo_id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    kind TEXT NOT NULL,
    alias TEXT NOT NULL,
    change TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, trunk, kind, alias)
);
CREATE INDEX IF NOT EXISTS change_aliases_change ON change_aliases (repo_id, change);

CREATE TABLE IF NOT EXISTS change_numbers (
    repo_id TEXT NOT NULL,
    change TEXT NOT NULL,
    host TEXT NOT NULL,
    number TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, change, host, number)
);

ALTER TABLE diffs ADD COLUMN kiln_change TEXT;
CREATE INDEX IF NOT EXISTS diffs_kiln_change ON diffs (repo_id, kiln_change);

-- Every diff and change request there is gets an identity; its Change-Id becomes an alias, so
-- kiln's history keeps finding its diffs; its number is recorded.
UPDATE diffs SET kiln_change = 'kc-' || lower(hex(randomblob(8))) WHERE kiln_change IS NULL;
INSERT OR IGNORE INTO changes (repo_id, id, trunk, created_at) SELECT repo_id, kiln_change, trunk, created_at FROM diffs;
INSERT OR IGNORE INTO change_aliases (repo_id, trunk, kind, alias, change, created_at)
    SELECT repo_id, trunk, 'change-id', change_id, kiln_change, created_at FROM diffs WHERE change_id IS NOT NULL AND change_id != '' ORDER BY number DESC;
INSERT OR IGNORE INTO change_numbers (repo_id, change, host, number, created_at)
    SELECT repo_id, kiln_change, 'kiln', CASE kind WHEN 'change_request' THEN 'CR' ELSE 'D' END || number, created_at FROM diffs;
