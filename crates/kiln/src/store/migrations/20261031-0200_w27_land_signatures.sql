-- Each land, signed as the queue that tested it (docs/ci.md W27-R07): the statement kiln signed as
-- its server when its land queue moved the trunk, by the commit it moved it to (`head`), with the
-- candidate it tested, that candidate's base, its digest, and when.
CREATE TABLE land_signatures (
  repo_id TEXT NOT NULL,
  trunk TEXT NOT NULL,
  head TEXT NOT NULL,
  candidate TEXT NOT NULL,
  base TEXT NOT NULL,
  digest TEXT NOT NULL,
  signer TEXT NOT NULL,
  signed_at INTEGER NOT NULL,
  bundle TEXT NOT NULL,
  PRIMARY KEY (repo_id, trunk, head)
);
