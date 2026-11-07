-- dbq trail (2026-11-06): an export's revocations (`verify::export`, verify.rs), read off their own
-- index rather than the whole trail: a scan of every entry for the revocations under a checkpoint,
-- 122 ms at production's size (809k entries), became 2.2 ms on this index, which holds only the
-- revocations (thousands), each with its digest so no entry is read. Appending a revocation costs
-- 3.1 -> 3.5 us; any other entry nothing, outside the index's WHERE.
CREATE INDEX IF NOT EXISTS trail_revocations ON trail (idx, statement) WHERE payload_type = 'application/vnd.kiln.revocation+json';
