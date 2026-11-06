-- W27-R08's custody, found by the agent session it was handed to (2026-11-05: production's store
-- froze). `custody::unreceived` runs for every session a runner holds each time it connects, and
-- asked by `counterpart LIKE '%/agent/<session>/epoch/%'`, which no index serves: each call read
-- every custody record (755k, ~400 MB of pages), and a restart's reconnects queued hundreds of
-- them on the store's one connection. This index keys each record by the agent session its
-- counterpart names (the text between its first `/agent/` and `/epoch/`; none when it names no
-- agent session), the expression `custody::COUNTERPART_SESSION` repeats so the query uses it.
-- And each record's things by the record, which `unreceived`'s EXISTS looks up.
CREATE INDEX IF NOT EXISTS custody_counterpart_session ON custody (
    (CASE WHEN instr(counterpart, '/agent/') > 0 AND instr(counterpart, '/epoch/') > instr(counterpart, '/agent/') + 7 THEN substr(counterpart, instr(counterpart, '/agent/') + 7, instr(counterpart, '/epoch/') - instr(counterpart, '/agent/') - 7) END),
    taken_at
);
CREATE INDEX IF NOT EXISTS custody_things_record ON custody_things (record, algorithm);
