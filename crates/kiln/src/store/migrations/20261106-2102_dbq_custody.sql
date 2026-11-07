-- The custody area's side tables (the DB query audit, 2026-11-05 and -06: medians on bench databases at
-- production's size, 795k custody records, ~/.cache/dbq/custody, custody2 and r4relays). Its indexes over
-- custody's existing tables are migration 20261106-2108_dbq_custody_indexes, built once kiln serves.
-- A record's things of one algorithm (the work a take or a hand-on is about, the timeline a
-- holder's transfer receipt is of) are found by custody_things_record (migration
-- w27_custody_by_session).
-- The agent session and epoch an agent's identity names, each a column of its own: what
-- `kiln_ci::identity::chain::Identity::parse` reads in its subject, kiln://<org>/person/<person>/
-- agent/<session>/epoch/<epoch>, with /machine/<machine>/instance/<instance> after it when it names
-- the runner process holding the session's key; each part 1 to 128 of A-Z a-z 0-9 . _ - (never . or
-- ..), the epoch digits with no leading zero. Both are read from one split of the subject by its
-- parts' places. Triggers fill them as a certificate is kept, in its transaction, whichever build
-- keeps it (one rolled back to as well), and again should its kind or subject change; the
-- certificates kept before are filled here. A session's identities (`identities::epochs_of`,
-- `holder_at`, `agent_uri`, `newest_holder`; each signed act asks for its holder) are read by them,
-- where a pattern was matched against every agent's subject (0.5-0.8 ms at production's size, 5 ms
-- at ten times its agents).
ALTER TABLE identities ADD COLUMN session TEXT;
ALTER TABLE identities ADD COLUMN epoch INTEGER;
CREATE TRIGGER IF NOT EXISTS identities_named_ai AFTER INSERT ON identities WHEN NEW.kind = 'agent' BEGIN
    UPDATE identities SET (session, epoch) = (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END, CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN CAST(max(CASE WHEN key = 6 THEN value END) AS INTEGER) END FROM json_each('["' || replace(substr(NEW.subject, 8), '/', '","') || '"]') WHERE substr(NEW.subject, 1, 7) = 'kiln://' AND substr(NEW.subject, 8) NOT GLOB '*[^A-Za-z0-9._/-]*') WHERE rowid = NEW.rowid;
END;
CREATE TRIGGER IF NOT EXISTS identities_named_au AFTER UPDATE OF kind, subject ON identities BEGIN
    UPDATE identities SET (session, epoch) = (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END, CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN CAST(max(CASE WHEN key = 6 THEN value END) AS INTEGER) END FROM json_each('["' || replace(substr(NEW.subject, 8), '/', '","') || '"]') WHERE NEW.kind = 'agent' AND substr(NEW.subject, 1, 7) = 'kiln://' AND substr(NEW.subject, 8) NOT GLOB '*[^A-Za-z0-9._/-]*') WHERE rowid = NEW.rowid;
END;
UPDATE identities SET (session, epoch) = (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END, CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN CAST(max(CASE WHEN key = 6 THEN value END) AS INTEGER) END FROM json_each('["' || replace(substr(identities.subject, 8), '/', '","') || '"]') WHERE substr(identities.subject, 1, 7) = 'kiln://' AND substr(identities.subject, 8) NOT GLOB '*[^A-Za-z0-9._/-]*') WHERE kind = 'agent';
CREATE INDEX IF NOT EXISTS identities_by_session ON identities (session, epoch, issued_at, subject) WHERE session IS NOT NULL;

-- The agent session each custody record leaves its things with, read from that side's identity (its
-- taker) as an identity's session is read above: the one a hand-on, a take or a pass gives them to
-- (its counterpart), the one a receipt is signed by (its custodian). custody_taken_by reads it;
-- custody_takers keeps it for each record whose side is an agent's identity (a few hundredths of
-- them), triggers keeping it as records are kept, changed or gone. Whom a session's work was handed
-- to and no receipt answers (`custody::unreceived`, each agent session opened, each session of every
-- runner's inventory) is read by it, where every hand-on to an agent was read and a pattern matched
-- against its counterpart (1.3 ms at production's size, 15 ms at ten times its agents' hand-ons);
-- and a holder's transfer, its receipt of its session's timeline (`custody::transfer_receipt`, each
-- signed act, through `held_now`), where every receipt kiln keeps was in an index of its own (half
-- of every record) and the holder's newest receipt of anything was read. A record whose side names
-- no agent session has no row: its session is null, which the table refuses and each insert skips.
-- It takes the place of custody_counterpart_session (migration w27_custody_by_session), which keys
-- every record by the text between its counterpart's first `/agent/` and `/epoch/`, a person or an
-- organization named agent or epoch read wrong. Nothing here reads that index; it stays for a build
-- from before this one, should one run here again (rolled back to): its `unreceived` reads by it,
-- and without it would read every hand-on for each session of each runner that connects.
CREATE VIEW IF NOT EXISTS custody_taken_by AS SELECT c.digest AS record, (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END FROM json_each('["' || replace(substr(CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END, 8), '/', '","') || '"]') WHERE substr(CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END, 1, 7) = 'kiln://' AND substr(CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END, 8) NOT GLOB '*[^A-Za-z0-9._/-]*') AS session, c.kind AS kind, CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END AS taker, c.taken_at AS taken_at FROM custody c WHERE instr(CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END, '/agent/') > 0;
CREATE TABLE IF NOT EXISTS custody_takers (record TEXT PRIMARY KEY, session TEXT NOT NULL, kind TEXT NOT NULL, taker TEXT NOT NULL, taken_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS custody_takers_ai AFTER INSERT ON custody WHEN instr(CASE WHEN NEW.kind = 'received' THEN NEW.custodian WHEN NEW.kind IN ('handed-on', 'taken', 'passed-on') THEN NEW.counterpart END, '/agent/') > 0 BEGIN
    INSERT OR IGNORE INTO custody_takers (record, session, kind, taker, taken_at) SELECT record, session, kind, taker, taken_at FROM custody_taken_by WHERE record = NEW.digest;
END;
CREATE TRIGGER IF NOT EXISTS custody_takers_au AFTER UPDATE OF digest, kind, custodian, counterpart, taken_at ON custody BEGIN
    DELETE FROM custody_takers WHERE record = OLD.digest;
    INSERT OR IGNORE INTO custody_takers (record, session, kind, taker, taken_at) SELECT record, session, kind, taker, taken_at FROM custody_taken_by WHERE record = NEW.digest;
END;
CREATE TRIGGER IF NOT EXISTS custody_takers_ad AFTER DELETE ON custody BEGIN
    DELETE FROM custody_takers WHERE record = OLD.digest;
END;

-- The agent session each take of kiln's is taken for (its body's `taken_for`, read from the statement
-- the trail keeps decoded, in the same transaction as the record), so a session's work taken from
-- it (`custody::work_taken_from`, each call of an agent session's) is sought by it, where the newest
-- 256 takes of work were read and each one's statement decoded to match its giver as text, which
-- missed any take of a session's work older than those. custody_takes_for reads it; triggers keep
-- custody_taken_for as takes and their trail entries are kept, changed or gone.
CREATE VIEW IF NOT EXISTS custody_takes_for AS SELECT c.digest AS record, (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END FROM json_each('["' || replace(substr(json_extract(CAST(t.payload AS TEXT), '$.body.taken_for'), 8), '/', '","') || '"]') WHERE substr(json_extract(CAST(t.payload AS TEXT), '$.body.taken_for'), 1, 7) = 'kiln://' AND substr(json_extract(CAST(t.payload AS TEXT), '$.body.taken_for'), 8) NOT GLOB '*[^A-Za-z0-9._/-]*') AS session, c.taken_at AS taken_at FROM custody c CROSS JOIN trail t ON t.statement = c.digest WHERE c.kind = 'taken' AND json_valid(CAST(t.payload AS TEXT));
CREATE TABLE IF NOT EXISTS custody_taken_for (record TEXT PRIMARY KEY, session TEXT NOT NULL, taken_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS custody_taken_for_trail_ai AFTER INSERT ON trail WHEN NEW.payload_type = 'application/vnd.kiln.custody+json' BEGIN
    INSERT OR IGNORE INTO custody_taken_for (record, session, taken_at) SELECT record, session, taken_at FROM custody_takes_for WHERE record = NEW.statement;
END;
CREATE TRIGGER IF NOT EXISTS custody_taken_for_ai AFTER INSERT ON custody WHEN NEW.kind = 'taken' BEGIN
    INSERT OR IGNORE INTO custody_taken_for (record, session, taken_at) SELECT record, session, taken_at FROM custody_takes_for WHERE record = NEW.digest;
END;
CREATE TRIGGER IF NOT EXISTS custody_taken_for_au AFTER UPDATE OF digest, kind, taken_at ON custody BEGIN
    DELETE FROM custody_taken_for WHERE record = OLD.digest;
    INSERT OR IGNORE INTO custody_taken_for (record, session, taken_at) SELECT record, session, taken_at FROM custody_takes_for WHERE record = NEW.digest;
END;
CREATE TRIGGER IF NOT EXISTS custody_taken_for_ad AFTER DELETE ON custody WHEN OLD.kind = 'taken' BEGIN
    DELETE FROM custody_taken_for WHERE record = OLD.digest;
END;

-- The records kept before, read for both tables in one scan of custody, in its own order (not by an
-- index, which read the table in its digests' order, a page at a time from all over it): each whose
-- side may be an agent's identity, and each of kiln's takes, with its envelope. Sorted by digest,
-- they fill each table in its keys' order, and seek each take's trail entry in its index's.
CREATE TEMP TABLE dbq_custody_kept AS SELECT c.digest AS record, c.kind AS kind, CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END AS taker, c.taken_at AS taken_at, CASE WHEN c.kind = 'taken' THEN c.envelope END AS envelope FROM custody c NOT INDEXED WHERE instr(CASE WHEN c.kind = 'received' THEN c.custodian WHEN c.kind IN ('handed-on', 'taken', 'passed-on') THEN c.counterpart END, '/agent/') > 0 OR c.kind = 'taken' ORDER BY c.digest;
INSERT OR IGNORE INTO custody_takers (record, session, kind, taker, taken_at) SELECT k.record, (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END FROM json_each('["' || replace(substr(k.taker, 8), '/', '","') || '"]') WHERE substr(k.taker, 1, 7) = 'kiln://' AND substr(k.taker, 8) NOT GLOB '*[^A-Za-z0-9._/-]*'), k.kind, k.taker, k.taken_at FROM dbq_custody_kept k WHERE instr(k.taker, '/agent/') > 0;
-- Each take's statement, from its trail entry, or decoded from its envelope for a take kept before
-- the trail was (DSSE's payload, base64: each four letters three bytes).
WITH RECURSIVE takes(record, taken_at, payload, b) AS MATERIALIZED (SELECT k.record, k.taken_at, CAST(t.payload AS TEXT),
            CASE WHEN t.statement IS NULL AND json_valid(k.envelope) THEN rtrim(json_extract(k.envelope, '$.dsseEnvelope.payload'), '=') END
        FROM dbq_custody_kept k LEFT JOIN trail t ON t.statement = k.record WHERE k.kind = 'taken'),
    q(record, taken_at, b, i) AS (SELECT record, taken_at, b, 0 FROM takes WHERE b IS NOT NULL UNION ALL SELECT record, taken_at, b, i + 4 FROM q WHERE i + 4 < length(b)),
    bytes(record, taken_at, i, h) AS (SELECT q.record, q.taken_at, q.i, printf('%02X', ((instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 1, 1)) - 1) << 2) | ((instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 2, 1)) - 1) >> 4)) || CASE WHEN length(q.b) >= q.i + 3 THEN printf('%02X', (((instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 2, 1)) - 1) & 15) << 4) | ((instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 3, 1)) - 1) >> 2)) ELSE '' END || CASE WHEN length(q.b) >= q.i + 4 THEN printf('%02X', (((instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 3, 1)) - 1) & 3) << 6) | (instr('ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', substr(q.b, q.i + 4, 1)) - 1)) ELSE '' END FROM q),
    stated(record, taken_at, payload) AS (SELECT record, taken_at, payload FROM takes WHERE payload IS NOT NULL
        UNION ALL SELECT record, taken_at, CAST(unhex(group_concat(h, '' ORDER BY i)) AS TEXT) FROM bytes GROUP BY record, taken_at)
INSERT OR IGNORE INTO custody_taken_for (record, session, taken_at) SELECT s.record, (SELECT CASE WHEN count(*) IN (7, 11) AND max(key = 1 AND value = 'person') AND max(key = 3 AND value = 'agent') AND max(key = 5 AND value = 'epoch') AND (count(*) = 7 OR (max(key = 7 AND value = 'machine') AND max(key = 9 AND value = 'instance'))) AND min(length(value) BETWEEN 1 AND 128 AND value NOT IN ('.', '..')) AND max(key = 6 AND value NOT GLOB '*[^0-9]*' AND (value = '0' OR value GLOB '[1-9]*')) THEN max(CASE WHEN key = 4 THEN value END) END FROM json_each('["' || replace(substr(json_extract(s.payload, '$.body.taken_for'), 8), '/', '","') || '"]') WHERE substr(json_extract(s.payload, '$.body.taken_for'), 1, 7) = 'kiln://' AND substr(json_extract(s.payload, '$.body.taken_for'), 8) NOT GLOB '*[^A-Za-z0-9._/-]*'), s.taken_at FROM stated s WHERE json_valid(s.payload) ORDER BY s.record;
DROP TABLE dbq_custody_kept;
CREATE INDEX IF NOT EXISTS custody_takers_session ON custody_takers (session, kind, taker, taken_at);
CREATE INDEX IF NOT EXISTS custody_taken_for_session ON custody_taken_for (session, taken_at);
