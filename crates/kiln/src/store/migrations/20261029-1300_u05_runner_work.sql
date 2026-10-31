-- U05/U07: the runner's work ledger, scoped holdings and restartable import cursor share the
-- canonical host Amphora database. The old index.log and _cas/ledger.db are read-only inputs.
CREATE TABLE IF NOT EXISTS runner_work_holds (
    owner TEXT NOT NULL,
    repo TEXT NOT NULL,
    domain TEXT NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (owner,repo,domain,digest)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS runner_work_imports (
    source TEXT PRIMARY KEY,
    identity TEXT NOT NULL,
    cursor BLOB NOT NULL,
    done INTEGER NOT NULL
) WITHOUT ROWID;
