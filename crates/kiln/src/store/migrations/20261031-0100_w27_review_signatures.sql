-- Each verdict's and comment's signature, as kiln took it (docs/ci.md W27-R04): the statement its
-- giver signed over its revision (the diff's version, by its commit and verbatim patch-id), its
-- digest, its signer, and when kiln took it. A comment edited keeps the signature of its words now.
CREATE TABLE review_signatures (
  kind TEXT NOT NULL,
  id TEXT NOT NULL,
  diff_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  digest TEXT NOT NULL,
  signer TEXT NOT NULL,
  taken_at INTEGER NOT NULL,
  bundle TEXT NOT NULL,
  PRIMARY KEY (kind, id)
);
CREATE INDEX review_signatures_by_diff ON review_signatures (diff_id);
