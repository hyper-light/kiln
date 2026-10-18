-- ID tokens' subjects per organization or repository (docs/ci.md W10-R09, GitHub's "customize
-- subject claims"): `server`, `org:<owner>` or `repo:<owner/name>`.
CREATE TABLE IF NOT EXISTS ci_oidc_subjects (
    scope TEXT PRIMARY KEY,
    -- A repository's `use_default`: GitHub's default subject, whatever its organization says.
    use_default INTEGER NOT NULL DEFAULT 0,
    -- `include_claim_keys`, a JSON array.
    claim_keys TEXT NOT NULL DEFAULT '[]',
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL
);
