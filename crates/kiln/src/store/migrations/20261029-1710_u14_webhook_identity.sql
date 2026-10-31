-- U14/W19-R07: accepted delivery recipients keep their immutable integration identity.
-- NULL is explicit compatibility for existing name-only deliveries, never new staging.
ALTER TABLE webhook_deliveries ADD COLUMN integration_id TEXT;
CREATE INDEX IF NOT EXISTS webhook_deliveries_delivery ON webhook_deliveries (delivery);
CREATE INDEX IF NOT EXISTS webhook_deliveries_pending ON webhook_deliveries (status, integration_id, integration, id);
CREATE UNIQUE INDEX IF NOT EXISTS webhook_deliveries_identity ON webhook_deliveries (delivery) WHERE integration_id IS NOT NULL;
