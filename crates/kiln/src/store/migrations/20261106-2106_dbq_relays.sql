-- The relays' records no receipt answered yet (`relays::unanswered`, every minute, and the relays'
-- page; the DB query audit, 2026-11-06): it walked every record a relay ever passed on, and looked
-- for each one's followers, so it grew with every pass relayed. These are the passes still open,
-- kept by the same writes that open and answer them: a record of kind passed-on on a stream that
-- waits for a receipt (not one a runner from before relays were checked, which says so), while no
-- record follows it and no break is recorded at it. The query reads them alone. Each write that
-- could change one's standing changes it in its own transaction, whichever build makes it, and the
-- passes open before are found from the records kept.
CREATE TABLE IF NOT EXISTS custody_open_passes (record TEXT PRIMARY KEY) WITHOUT ROWID;

CREATE TRIGGER IF NOT EXISTS custody_open_passes_custody_ai AFTER INSERT ON custody
WHEN NEW.kind = 'passed-on' AND NEW.stream IS NOT NULL AND instr(NEW.stream, ' (unanswered: a runner from before)') = 0 BEGIN
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT NEW.digest
    WHERE NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = NEW.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = NEW.digest);
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_custody_au AFTER UPDATE OF digest, kind, stream ON custody BEGIN
    DELETE FROM custody_open_passes WHERE record = OLD.digest;
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT NEW.digest
    WHERE NEW.kind = 'passed-on' AND NEW.stream IS NOT NULL AND instr(NEW.stream, ' (unanswered: a runner from before)') = 0
      AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = NEW.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = NEW.digest);
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_custody_ad AFTER DELETE ON custody WHEN OLD.kind = 'passed-on' BEGIN
    DELETE FROM custody_open_passes WHERE record = OLD.digest;
END;

-- A record following a pass answers it; one following it no more (its row gone or changed) leaves
-- it open again if nothing else answers it.
CREATE TRIGGER IF NOT EXISTS custody_open_passes_follows_ai AFTER INSERT ON custody_follows BEGIN
    DELETE FROM custody_open_passes WHERE record = NEW.follows;
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_follows_au AFTER UPDATE OF follows ON custody_follows BEGIN
    DELETE FROM custody_open_passes WHERE record = NEW.follows;
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT c.digest FROM custody c
    WHERE c.digest = OLD.follows AND c.kind = 'passed-on' AND c.stream IS NOT NULL AND instr(c.stream, ' (unanswered: a runner from before)') = 0
      AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = c.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = c.digest);
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_follows_ad AFTER DELETE ON custody_follows BEGIN
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT c.digest FROM custody c
    WHERE c.digest = OLD.follows AND c.kind = 'passed-on' AND c.stream IS NOT NULL AND instr(c.stream, ' (unanswered: a runner from before)') = 0
      AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = c.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = c.digest);
END;

-- A break recorded at a pass closes it, as answered ones are; one no longer at it reopens it.
CREATE TRIGGER IF NOT EXISTS custody_open_passes_breaks_ai AFTER INSERT ON custody_breaks BEGIN
    DELETE FROM custody_open_passes WHERE record = NEW.record;
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_breaks_au AFTER UPDATE OF record ON custody_breaks BEGIN
    DELETE FROM custody_open_passes WHERE record = NEW.record;
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT c.digest FROM custody c
    WHERE c.digest = OLD.record AND c.kind = 'passed-on' AND c.stream IS NOT NULL AND instr(c.stream, ' (unanswered: a runner from before)') = 0
      AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = c.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = c.digest);
END;
CREATE TRIGGER IF NOT EXISTS custody_open_passes_breaks_ad AFTER DELETE ON custody_breaks BEGIN
    INSERT OR IGNORE INTO custody_open_passes (record) SELECT c.digest FROM custody c
    WHERE c.digest = OLD.record AND c.kind = 'passed-on' AND c.stream IS NOT NULL AND instr(c.stream, ' (unanswered: a runner from before)') = 0
      AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = c.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = c.digest);
END;

-- Each relay's stream as the relays' page lists it (`relays::view`): how many of its records kiln
-- has, its first and last numbers, and when kiln took the newest, kept as each pass is kept, changed
-- or gone, where every pass kiln ever took was read and grouped (the DB query audit, 2026-11-06).
CREATE TABLE IF NOT EXISTS custody_relay_streams (
    custodian TEXT NOT NULL,
    stream TEXT NOT NULL,
    records INTEGER NOT NULL,
    first INTEGER,
    last INTEGER,
    newest INTEGER NOT NULL,
    PRIMARY KEY (custodian, stream)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS custody_relay_streams_custody_ai AFTER INSERT ON custody WHEN NEW.kind = 'passed-on' AND NEW.stream IS NOT NULL BEGIN
    INSERT INTO custody_relay_streams (custodian, stream, records, first, last, newest) VALUES (NEW.custodian, NEW.stream, 1, NEW.seq, NEW.seq, NEW.taken_at)
    ON CONFLICT (custodian, stream) DO UPDATE SET records = records + 1,
        first = CASE WHEN excluded.first IS NULL THEN first WHEN first IS NULL THEN excluded.first ELSE min(first, excluded.first) END,
        last = CASE WHEN excluded.last IS NULL THEN last WHEN last IS NULL THEN excluded.last ELSE max(last, excluded.last) END,
        newest = max(newest, excluded.newest);
END;
-- A pass gone or changed: its stream, and the one it's on now, counted again from their records.
CREATE TRIGGER IF NOT EXISTS custody_relay_streams_custody_ad AFTER DELETE ON custody WHEN OLD.kind = 'passed-on' AND OLD.stream IS NOT NULL BEGIN
    DELETE FROM custody_relay_streams WHERE custodian = OLD.custodian AND stream = OLD.stream;
    INSERT INTO custody_relay_streams (custodian, stream, records, first, last, newest) SELECT custodian, stream, count(*), min(seq), max(seq), max(taken_at)
    FROM custody WHERE custodian = OLD.custodian AND stream = OLD.stream AND kind = 'passed-on' GROUP BY custodian, stream;
END;
CREATE TRIGGER IF NOT EXISTS custody_relay_streams_custody_au AFTER UPDATE OF kind, custodian, stream, seq, taken_at ON custody
WHEN (OLD.kind = 'passed-on' AND OLD.stream IS NOT NULL) OR (NEW.kind = 'passed-on' AND NEW.stream IS NOT NULL) BEGIN
    DELETE FROM custody_relay_streams WHERE (custodian = OLD.custodian AND stream = OLD.stream) OR (custodian = NEW.custodian AND stream = NEW.stream);
    INSERT INTO custody_relay_streams (custodian, stream, records, first, last, newest) SELECT custodian, stream, count(*), min(seq), max(seq), max(taken_at)
    FROM custody WHERE ((custodian = OLD.custodian AND stream = OLD.stream) OR (custodian = NEW.custodian AND stream = NEW.stream)) AND kind = 'passed-on' GROUP BY custodian, stream;
END;

-- The passes open now and each stream's records, from the records kept before: the passes read
-- once, for both, sorted by digest, so the open ones are kept in their table's order and each
-- pass's followers and breaks are sought in their indexes'.
CREATE TEMP TABLE dbq_relays_passes AS SELECT digest, custodian, stream, seq, taken_at FROM custody WHERE kind = 'passed-on' AND stream IS NOT NULL ORDER BY digest;
INSERT OR IGNORE INTO custody_open_passes (record) SELECT p.digest FROM dbq_relays_passes p
WHERE instr(p.stream, ' (unanswered: a runner from before)') = 0
  AND NOT EXISTS (SELECT 1 FROM custody_follows WHERE follows = p.digest) AND NOT EXISTS (SELECT 1 FROM custody_breaks WHERE record = p.digest);
INSERT OR REPLACE INTO custody_relay_streams (custodian, stream, records, first, last, newest) SELECT custodian, stream, count(*), min(seq), max(seq), max(taken_at) FROM dbq_relays_passes GROUP BY custodian, stream;
DROP TABLE dbq_relays_passes;
CREATE INDEX IF NOT EXISTS custody_relay_streams_newest ON custody_relay_streams (newest);
