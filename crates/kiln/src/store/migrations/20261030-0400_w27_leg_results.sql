-- A leg's signed records (docs/ci.md W27-R05), each as kiln took it: the statements its runner sent
-- at the leg's end, signed as the leg (its results, and its custody records: a receipt of each thing
-- it received, then its hand-on of what it made), and kiln's own receipt of that hand-on, signed as
-- the server. Each row holds:
--   - the bundle as it came (a Sigstore bundle of kiln's statement), its digest and payload type;
--   - its signer as kiln checked it when it took it (`signer` in words, `uri` its identity);
--   - when kiln took it (unix seconds: the time it's checked at again), or why it was refused.
-- By attempt: a job retried in place keeps its id, and each attempt is a leg of its own. A record
-- that isn't one is kept under `bundle:` and its text's SHA-256.
CREATE TABLE ci_leg_records (
    job_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    digest TEXT NOT NULL,
    payload_type TEXT NOT NULL DEFAULT '',
    bundle TEXT NOT NULL,
    signer TEXT NOT NULL DEFAULT '',
    uri TEXT NOT NULL DEFAULT '',
    taken_at INTEGER NOT NULL,
    refused TEXT,
    PRIMARY KEY (job_id, attempt, digest)
);

-- The server's delegation of each leg it certifies (W27-R05, R06): a statement it signs as itself
-- (application/vnd.kiln.delegation+json), its delegate the leg's identity, by its digest (SHA-256
-- over its PAE), with when kiln took it, which is when it signed it. A leg's records name it in
-- `under`, and are checked through these rows. `signs`: the leg's runner said, as it asked for the
-- leg's identity, that the leg signs (its records, and provenance when asked).
CREATE TABLE ci_leg_delegations (
    digest TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    bundle TEXT NOT NULL,
    taken_at INTEGER NOT NULL,
    signs INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ci_leg_delegations_job ON ci_leg_delegations (job_id);

-- The commit's tarball kiln served each leg (its facade's `GET …/tarball/{ref}` under the leg's
-- token), by the SHA-256 of the bytes it sent: what the leg's receipt of its checkout is held to.
CREATE TABLE ci_leg_served (
    job_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    commit_sha TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    served_at INTEGER NOT NULL,
    PRIMARY KEY (job_id, attempt, sha256)
);

-- What kiln handed each leg as its assignment, by the digest its receipt of it names
-- (`kiln_ci::attest::leg::assignment`), as kiln sent it: its delegation of the leg names it.
CREATE TABLE ci_leg_assignments (
    job_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (job_id, attempt)
);
