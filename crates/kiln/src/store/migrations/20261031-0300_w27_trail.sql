-- The trail (docs/ci.md W27-R09): every signed statement kiln takes, in the order it takes them,
-- appended in the transaction that takes it and never pruned (kiln_ci::identity::trail). Each
-- entry names its statement by digest (`statement`: SHA-256 over its PAE), its kind, its subjects
-- (a JSON list, each `<algorithm>:<hex>`), when kiln took it, its index from 0 and the hash of
-- the entry before it (`prev`, hex; zeros for the first). `hash` is its own: the leaf of the tree
-- kiln's checkpoints sign. It keeps its statement: the payload type and payload.
CREATE TABLE trail (
    idx INTEGER PRIMARY KEY,
    prev TEXT NOT NULL,
    statement TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    subjects TEXT NOT NULL,
    taken_at INTEGER NOT NULL,
    hash TEXT NOT NULL,
    payload_type TEXT NOT NULL,
    payload BLOB NOT NULL
);
-- Each signature kiln took of a statement in the trail, as it came (a Sigstore bundle), by the
-- SHA-256 of the signature, with when kiln took it: a statement signed by more than one (the
-- provenance kiln writes and the leg that built it signs too) has each. kiln's own words, for
-- what it checked that isn't a statement itself, have none: the checkpoints vouch for them.
CREATE TABLE trail_signatures (
    statement TEXT NOT NULL,
    signature TEXT NOT NULL,
    bundle TEXT NOT NULL,
    taken_at INTEGER NOT NULL,
    PRIMARY KEY (statement, signature)
);
-- Each whole subtree of 256^level leaves (level from 1), by its index on that level, its hash
-- kept as its last leaf came: the hashes of the trail's tiles above level 0 (C2SP tlog-tiles), and
-- what its root and proofs are hashed up from.
CREATE TABLE trail_nodes (
    level INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (level, idx)
);
-- Where each checkpoint is anchored (W27-R09): in the Rekor `[ci.attest] rekor_url` named (`log`),
-- at its entry `idx` (a hashedrekord of the note's text), with the note and certificate as signed
-- and Rekor's answer as it came. Kept apart from trail_checkpoints, so the trail is held to what
-- Rekor saw however its own checkpoints change.
CREATE TABLE trail_anchors (
    size INTEGER NOT NULL,
    log TEXT NOT NULL,
    idx INTEGER NOT NULL,
    uuid TEXT NOT NULL,
    note TEXT NOT NULL,
    certificate TEXT NOT NULL,
    answer TEXT NOT NULL,
    anchored_at INTEGER NOT NULL,
    PRIMARY KEY (log, idx)
);
-- Each entry by each of its subjects: what the trail says of a commit, an artifact or a key.
CREATE TABLE trail_subjects (
    subject TEXT NOT NULL,
    idx INTEGER NOT NULL,
    PRIMARY KEY (subject, idx)
);
-- kiln's checkpoints of the trail: a C2SP signed note of its size and root, signed by the server
-- (kiln_ci::identity::trail::signed) under `certificate` (DER, base64) at `signed_at`. Each
-- extends the one before; kiln signs none that doesn't.
CREATE TABLE trail_checkpoints (
    size INTEGER PRIMARY KEY,
    root TEXT NOT NULL,
    note TEXT NOT NULL,
    certificate TEXT NOT NULL,
    signed_at INTEGER NOT NULL
);
