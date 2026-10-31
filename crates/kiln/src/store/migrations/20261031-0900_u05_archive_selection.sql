-- kiln:epoch 6
-- Bounded, durable candidate sweeps belong to the existing archive owner. The
-- pending index contains no joined eligibility predicate before its page limit.
ALTER TABLE conversation_maintenance ADD COLUMN archive_scan TEXT;
CREATE INDEX transcript_archives_pending_due ON transcript_archives(status,due_ms,session);
