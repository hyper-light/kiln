-- CP-T2.2 step 8 (AC-T2.2.1, T-T2.2.1): deliveries made from kiln.events (docs/bus.md, "kiln's
-- events"). Each reader of the topic keeps its position here, moved in the transaction that logs
-- the deliveries an event makes; each delivery log is its targets' queue, sent oldest first.

-- Where each reader of kiln.events is: the offset it reads next, and the number of the last event
-- it took (an event published again after a kill comes with a number it has passed).
CREATE TABLE event_positions (
  reader TEXT PRIMARY KEY,
  next INTEGER NOT NULL,
  passed_row INTEGER NOT NULL,
  passed_index INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

-- A delivery waits as `pending` until it's sent or given up: when it was logged, and when it may
-- be tried next (unix seconds).
ALTER TABLE webhook_deliveries ADD COLUMN first_at INTEGER;
ALTER TABLE webhook_deliveries ADD COLUMN next_at INTEGER;
CREATE INDEX webhook_deliveries_pending ON webhook_deliveries (status, integration, id);

-- A rule's delivery keeps what its message is made from, and where it goes, until it's sent.
ALTER TABLE notify_deliveries ADD COLUMN delivery TEXT;
ALTER TABLE notify_deliveries ADD COLUMN repo_id TEXT;
ALTER TABLE notify_deliveries ADD COLUMN message TEXT;
ALTER TABLE notify_deliveries ADD COLUMN first_at INTEGER;
ALTER TABLE notify_deliveries ADD COLUMN next_at INTEGER;
CREATE INDEX notify_deliveries_pending ON notify_deliveries (status, channel, id);
