-- Accepted communications keep their original full envelope. Pending status replacement changes
-- disposition only; no accepted original is deleted or updated. Older builds do not name this table.
CREATE TABLE agent_mail (
    revision INTEGER PRIMARY KEY AUTOINCREMENT,
    owner TEXT NOT NULL,
    sender TEXT NOT NULL,
    recipient TEXT NOT NULL,
    id TEXT NOT NULL,
    body TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    status_task TEXT,
    status_key TEXT,
    disposition TEXT NOT NULL CHECK (disposition IN ('waiting', 'dispatched', 'admitted', 'consumed', 'superseded', 'cancelled', 'transferred')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    epoch INTEGER,
    frozen INTEGER NOT NULL DEFAULT 0 CHECK (frozen IN (0, 1)),
    terminal TEXT,
    source_generation INTEGER,
    source_connection TEXT,
    UNIQUE (owner, sender, recipient, id)
);
CREATE INDEX agent_mail_pending ON agent_mail (recipient, disposition, revision);
CREATE INDEX agent_mail_stream ON agent_mail (owner, sender, recipient, status_task, status_key, frozen, disposition);
CREATE TABLE agent_mail_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    bytes INTEGER NOT NULL CHECK (bytes >= 0)
);
INSERT INTO agent_mail_usage (singleton, bytes) VALUES (1, 0);
-- A completed custody transfer fences its old attempt without changing the original body.
-- Keeping its receipt makes old-owner terminal ACKs repeatable after a destination claim.
CREATE TABLE agent_mail_transfers (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    revision INTEGER NOT NULL REFERENCES agent_mail(revision),
    generation INTEGER NOT NULL,
    from_epoch INTEGER NOT NULL,
    to_epoch INTEGER NOT NULL,
    terminal TEXT NOT NULL,
    UNIQUE (revision, generation, from_epoch)
);

-- One authenticated, immutable custody binding per attempt. A live offer fences the session
-- until its destination commits or its exact source aborts it; no original body is copied here.
CREATE TABLE agent_mail_handoffs (
    attempt TEXT PRIMARY KEY,
    ordinal INTEGER NOT NULL UNIQUE CHECK (ordinal > 0),
    session TEXT NOT NULL,
    source_epoch INTEGER NOT NULL CHECK (source_epoch >= 0),
    destination_epoch INTEGER NOT NULL CHECK (destination_epoch > source_epoch),
    phase TEXT NOT NULL CHECK (phase IN ('offered', 'prepared', 'committed', 'aborted')),
    record TEXT NOT NULL CHECK (length(CAST(record AS BLOB)) <= 524288 AND json_valid(record)),
    CHECK (json_extract(record, '$.manifest.attempt') = attempt),
    CHECK (json_extract(record, '$.manifest.session') = session),
    CHECK (json_extract(record, '$.manifest.source_epoch') = source_epoch),
    CHECK (json_extract(record, '$.manifest.destination_epoch') = destination_epoch),
    CHECK (json_extract(record, '$.phase') = phase)
);
CREATE UNIQUE INDEX agent_mail_handoff_offered ON agent_mail_handoffs(session) WHERE phase IN ('offered', 'prepared');
CREATE INDEX agent_mail_handoff_latest ON agent_mail_handoffs(session, ordinal);
