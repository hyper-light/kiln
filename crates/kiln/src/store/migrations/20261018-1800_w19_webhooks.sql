-- Outgoing webhooks (docs/ci.md W19-R07): every delivery to a subscriber, with its body, so it
-- can be sent again.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    integration TEXT NOT NULL,
    event TEXT NOT NULL,
    delivery TEXT NOT NULL,
    status TEXT NOT NULL,
    code INTEGER,
    attempts INTEGER NOT NULL,
    error TEXT,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS webhook_deliveries_integration ON webhook_deliveries (integration, id);
