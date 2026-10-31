-- What a repository's host verified of each commit's signature, as kiln read it (docs/ci.md
-- W27-R03): GitHub's verdict on each of a pull request's commits, GitLab's on each commit's
-- signature (asked once a commit), and Bitbucket's none (`unknown`, saying it verifies none). A
-- verdict read again unchanged keeps when kiln first read it.
CREATE TABLE host_verifications (
  repo_id TEXT NOT NULL,
  sha TEXT NOT NULL,
  host TEXT NOT NULL,
  state TEXT NOT NULL,
  reason TEXT NOT NULL,
  signer TEXT,
  verified_at INTEGER,
  read_at INTEGER NOT NULL,
  PRIMARY KEY (repo_id, sha)
);
