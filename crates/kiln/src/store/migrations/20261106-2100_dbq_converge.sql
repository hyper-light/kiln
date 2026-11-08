-- The DB query audit's start (2026-11-06). Its migrations carry names none of their drafts had, so a
-- database a draft was ever applied to still applies each as it is now: on 2026-11-06 a test's servers
-- opened production's kiln.db and couldn't take its lock, and whether a draft got one in is unknown. The
-- indexes and tables the drafts made go first, so this build's make their own; each is a no-op where it
-- isn't there (every database a draft never reached).
DROP INDEX IF EXISTS custody_received;
DROP INDEX IF EXISTS custody_to_agents;
DROP INDEX IF EXISTS identities_sessions;
DROP INDEX IF EXISTS identities_agents;
DROP INDEX IF EXISTS custody_things_work;
DROP INDEX IF EXISTS plans_masters;
DROP INDEX IF EXISTS land_queue_live;
DROP INDEX IF EXISTS review_ladders_pending;
DROP INDEX IF EXISTS diffs_number;
DROP INDEX IF EXISTS diffs_author;
DROP INDEX IF EXISTS diff_reviewers_reviewer;
DROP INDEX IF EXISTS review_threads_open;
DROP INDEX IF EXISTS runs_lane;
DROP INDEX IF EXISTS runs_repo_created;
DROP INDEX IF EXISTS ci_cache_events_ts;
DROP INDEX IF EXISTS ci_cache_events_leg;
DROP INDEX IF EXISTS ci_artifacts_leg;
DROP INDEX IF EXISTS jobs_queued_labels;
DROP INDEX IF EXISTS runs_lane_finished;
DROP INDEX IF EXISTS runs_attempts;
DROP INDEX IF EXISTS github_checks_due;
DROP TRIGGER IF EXISTS ci_log_indexed_ai;
DROP TRIGGER IF EXISTS ci_log_indexed_ad;
DROP TRIGGER IF EXISTS ci_log_pending_ai;
DROP TRIGGER IF EXISTS ci_log_pending_au;
DROP TRIGGER IF EXISTS ci_log_pending_ad;
DROP TABLE IF EXISTS ci_log_pending;
DROP TABLE IF EXISTS ci_log_cursor;
