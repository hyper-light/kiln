-- The audit log, v2 (docs/ci.md W19-R05): who (and for whom), with which token, in what scope,
-- did what to what, how it ended, from where.
CREATE TABLE IF NOT EXISTS audit2 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    principal TEXT NOT NULL,
    on_behalf_of TEXT,
    token TEXT,
    scope TEXT,
    action TEXT NOT NULL,
    target TEXT,
    result TEXT NOT NULL,
    ip TEXT,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit2_ts ON audit2 (ts);
CREATE INDEX IF NOT EXISTS audit2_principal ON audit2 (principal, id);
CREATE INDEX IF NOT EXISTS audit2_action ON audit2 (action, id);
-- Today's rows move in: an actor `x for y` as principal and on-behalf-of.
INSERT INTO audit2 (ts, principal, on_behalf_of, action, result, detail)
SELECT ts,
       CASE WHEN instr(actor, ' for ') > 0 THEN substr(actor, 1, instr(actor, ' for ') - 1) ELSE actor END,
       CASE WHEN instr(actor, ' for ') > 0 THEN substr(actor, instr(actor, ' for ') + 5) END,
       action, 'ok', detail
FROM audit ORDER BY id;
DELETE FROM audit;
