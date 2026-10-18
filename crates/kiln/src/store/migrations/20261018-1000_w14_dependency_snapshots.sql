-- W14-R11 (docs/ci.md): GitHub's dependency submission API (`POST /repos/{o}/{r}/dependency-graph/
-- snapshots`), which Gradle's, Maven's and sbt's submission actions call after a build. The
-- snapshots are kept for W18's SBOMs and provenance.
CREATE TABLE ci_dependency_snapshots (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL,
  sha TEXT NOT NULL,
  git_ref TEXT NOT NULL,
  job TEXT NOT NULL,                    -- the snapshot's `job.correlator`
  detector TEXT NOT NULL,               -- `detector.name`
  manifests INTEGER NOT NULL,           -- how many manifests it holds
  snapshot TEXT NOT NULL,               -- the snapshot as sent (JSON)
  created_at INTEGER NOT NULL
);
CREATE INDEX ci_dependency_snapshots_repo ON ci_dependency_snapshots (repo_id, sha);
