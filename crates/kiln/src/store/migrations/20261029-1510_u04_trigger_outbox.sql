-- U04/W03-R10: an occurrence and its pending command commit together in Amphora.
-- Quiver carries wakeups; this table is the canonical recovery state.
CREATE TABLE ci_trigger_outbox (
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'materialized', 'done', 'failed')),
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at INTEGER NOT NULL DEFAULT 0,
  diagnostic TEXT,
  receipt TEXT,
  consumer_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX ci_trigger_outbox_pending ON ci_trigger_outbox(available_at, updated_at, id) WHERE state IN ('pending', 'materialized');
CREATE INDEX ci_trigger_outbox_consumer ON ci_trigger_outbox(kind, state, consumer_id);
