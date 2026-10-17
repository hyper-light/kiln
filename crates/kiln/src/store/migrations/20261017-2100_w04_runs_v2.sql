-- docs/ci.md §2.4, block 1: runs of the CI engine (v2) in the tables v1 runs use, so the run list
-- and run pages show both. A v2 run has `workflow_id`; a v2 leg has `job_id`. v1 code reads only
-- rows without them.
ALTER TABLE runs ADD COLUMN workflow_id TEXT;
ALTER TABLE runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
ALTER TABLE runs ADD COLUMN conclusion TEXT;
ALTER TABLE runs ADD COLUMN actor TEXT NOT NULL DEFAULT '';
ALTER TABLE runs ADD COLUMN trigger TEXT NOT NULL DEFAULT '{}';
ALTER TABLE runs ADD COLUMN run_name TEXT;
ALTER TABLE runs ADD COLUMN trust TEXT NOT NULL DEFAULT 'trusted';
ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 50;

-- The event payload a run saw ($GITHUB_EVENT_PATH).
CREATE TABLE IF NOT EXISTS ci_events (
  run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
  payload TEXT NOT NULL
);

-- What a run's plan is made of, and every report it took, in order: the plan is pure, so
-- replaying them rebuilds it exactly after a restart (W05-R25).
CREATE TABLE IF NOT EXISTS ci_plans (
  run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
  workflow TEXT NOT NULL,          -- the model (JSON)
  input TEXT NOT NULL,             -- github, inputs, vars, changed, trusted (JSON)
  done INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS ci_plan_log (
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,              -- leg | cancel
  leg TEXT,
  report TEXT,                     -- the leg's report (JSON)
  PRIMARY KEY (run_id, seq)
);

ALTER TABLE jobs ADD COLUMN job_id TEXT;              -- jobs.<id> in the workflow (jobs rows are legs)
ALTER TABLE jobs ADD COLUMN matrix TEXT;
ALTER TABLE jobs ADD COLUMN leg_index INTEGER;
ALTER TABLE jobs ADD COLUMN outputs TEXT;
ALTER TABLE jobs ADD COLUMN conclusion TEXT;
ALTER TABLE jobs ADD COLUMN requirements TEXT;
ALTER TABLE jobs ADD COLUMN routing TEXT;
ALTER TABLE jobs ADD COLUMN wait_reason TEXT;
ALTER TABLE jobs ADD COLUMN concurrency_group TEXT;
ALTER TABLE jobs ADD COLUMN priority INTEGER NOT NULL DEFAULT 50;
ALTER TABLE jobs ADD COLUMN input_hash TEXT;
ALTER TABLE jobs ADD COLUMN reused_from TEXT;
ALTER TABLE jobs ADD COLUMN environment TEXT;
ALTER TABLE jobs ADD COLUMN leg TEXT;                 -- the plan's leg (JSON)
CREATE INDEX IF NOT EXISTS jobs_v2 ON jobs(job_id) WHERE job_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS ci_attempts (
  id TEXT PRIMARY KEY,
  leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  status TEXT NOT NULL,
  conclusion TEXT,
  runner TEXT,
  executor TEXT,
  queued_at INTEGER,
  started_at INTEGER,
  finished_at INTEGER,
  exit_code INTEGER,
  error TEXT,
  failure_class TEXT,
  peak_rss INTEGER,
  cpu_seconds REAL,
  UNIQUE (leg_id, number)
);

ALTER TABLE steps ADD COLUMN kind TEXT NOT NULL DEFAULT 'run';
ALTER TABLE steps ADD COLUMN step_id TEXT;
ALTER TABLE steps ADD COLUMN action TEXT;
ALTER TABLE steps ADD COLUMN outcome TEXT;
ALTER TABLE steps ADD COLUMN conclusion TEXT;
ALTER TABLE steps ADD COLUMN outputs TEXT;
ALTER TABLE steps ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS ci_annotations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  step_idx INTEGER,
  level TEXT NOT NULL,
  title TEXT,
  message TEXT NOT NULL,
  file TEXT,
  line INTEGER,
  end_line INTEGER,
  col INTEGER,
  end_col INTEGER,
  log_offset INTEGER
);
CREATE INDEX IF NOT EXISTS ci_annotations_leg ON ci_annotations(leg_id);

CREATE TABLE IF NOT EXISTS ci_summaries (
  leg_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  step_idx INTEGER NOT NULL,
  markdown TEXT NOT NULL,
  PRIMARY KEY (leg_id, step_idx)
);
