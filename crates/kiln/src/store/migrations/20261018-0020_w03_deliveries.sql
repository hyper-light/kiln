-- W03-R14: GitHub's webhook deliveries seen (X-GitHub-Delivery), so a redelivery does nothing for
-- 24 hours.
CREATE TABLE ci_deliveries (
  id TEXT PRIMARY KEY,
  event TEXT NOT NULL,
  received_at INTEGER NOT NULL
);
