-- CP-T2.2 step 8 (AC-T2.2.1): kiln's events recorded in the transaction that changes what they
-- report (the transactional outbox), then relayed in order to the bus's persisted topic
-- `kiln.events` (docs/bus.md, "kiln's events"; crate::events).
--
-- A row's number orders every event kiln makes and names it on the topic, so it never comes back
-- once used: AUTOINCREMENT, as the relay lets relayed rows go.
CREATE TABLE event_outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,        -- unix ms, as the change committed
  kind TEXT NOT NULL,         -- 'run', 'job' (the triggers below), 'run.finished', 'deployed', ...
  fact TEXT NOT NULL          -- JSON: what it's about
);

-- How far the relay got: every row up to `relayed` is on the topic.
CREATE TABLE event_relay (
  one INTEGER PRIMARY KEY CHECK (one = 1),
  relayed INTEGER NOT NULL
);
INSERT INTO event_relay (one, relayed) VALUES (1, 0);

-- A run or a job made, or its status or conclusion changed, or a job's step, whatever code changed
-- it: the `run` and `job` events outgoing webhooks take (docs/ci.md W19-R07). The cluster group's
-- replay writes these rows as the write made them, with triggers off (crate::store::group).
CREATE TRIGGER event_run_made AFTER INSERT ON runs BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'run', json_object('run', NEW.id, 'status', NEW.status, 'conclusion', NEW.conclusion));
END;
CREATE TRIGGER event_run_changed AFTER UPDATE OF status, conclusion ON runs
WHEN OLD.status IS NOT NEW.status OR OLD.conclusion IS NOT NEW.conclusion BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'run', json_object('run', NEW.id, 'status', NEW.status, 'conclusion', NEW.conclusion));
END;
CREATE TRIGGER event_job_made AFTER INSERT ON jobs BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'job', json_object('job', NEW.id, 'run', NEW.run_id, 'status', NEW.status, 'conclusion', NEW.conclusion));
END;
CREATE TRIGGER event_job_changed AFTER UPDATE OF status, conclusion ON jobs
WHEN OLD.status IS NOT NEW.status OR OLD.conclusion IS NOT NEW.conclusion BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'job', json_object('job', NEW.id, 'run', NEW.run_id, 'status', NEW.status, 'conclusion', NEW.conclusion));
END;
-- A job moving through its steps: a `job` event too, the job's own status unchanged by it.
CREATE TRIGGER event_step_made AFTER INSERT ON steps BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'job', json_object('job', NEW.job_id, 'step', NEW.idx, 'step_status', NEW.status));
END;
CREATE TRIGGER event_step_changed AFTER UPDATE OF status ON steps WHEN OLD.status IS NOT NEW.status BEGIN
  INSERT INTO event_outbox (at, kind, fact)
  VALUES (CAST(unixepoch('subsec') * 1000 AS INTEGER), 'job', json_object('job', NEW.job_id, 'step', NEW.idx, 'step_status', NEW.status));
END;
