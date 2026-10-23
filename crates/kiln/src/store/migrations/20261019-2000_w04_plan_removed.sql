-- docs/ci.md W04-R29: the combinations a run's plan removed (`exclude`, `where`), by job, with
-- why (JSON), as the run's page and a comparison with its preview show them.
ALTER TABLE ci_plans ADD COLUMN removed TEXT;
