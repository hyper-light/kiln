-- Groups (docs/deploy-api.md §2, §3.1, §15): teams of kiln's users, which own apps. A group from an
-- identity provider (`github:acme/platform`) needs no row: sign-in records the person's groups
-- there (`user_identities.groups`).
CREATE TABLE IF NOT EXISTS user_groups (
    name TEXT PRIMARY KEY COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS user_group_members (
    group_name TEXT NOT NULL COLLATE NOCASE,
    user_id TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    added_by TEXT NOT NULL,
    PRIMARY KEY (group_name, user_id)
);
CREATE INDEX IF NOT EXISTS user_group_members_user ON user_group_members (user_id);
