-- SSH keys people sign commits with, each bound to its person's identity (docs/ci.md W27-R03):
-- a push's commit signed with one verifies as that person. A key is bound to one person; one
-- removed (or whose person is disabled) signs for no one from then.
CREATE TABLE ssh_signers (
  key TEXT PRIMARY KEY,
  person TEXT NOT NULL,
  kind TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  comment TEXT NOT NULL DEFAULT '',
  added_at INTEGER NOT NULL,
  removed_at INTEGER
);
CREATE INDEX ssh_signers_by_person ON ssh_signers (person);
