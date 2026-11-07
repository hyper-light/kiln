-- dbq ci (2026-11-06): CI's reads that scanned a table, sought on an index (medians from the DB query
-- audit's bench, at production's size and at 100x). land_queue_live: the land tick's live trunks every
-- 3 s and each diff's and pull's place in the queue (17 -> 4 us; 696 us -> 3.5 us at 100x).
-- review_ladders_pending: the ladder's tick every 3 s (227 -> 5 us at 100x). diffs_number,
-- diffs_author, diff_reviewers_reviewer, review_threads_open: a diff by number, the next number and
-- Review's inbox (diffs::find 930 -> 78 us at 100x). runs_lane, runs_repo_created (v2 runs alone): a
-- lane's last green, last good and runs before (4.3 ms -> 24 us at 100x) and a repository's runs
-- (31.6 -> 3.0 ms at 100x). ci_cache_events_ts: the roll of old cache events every minute (141 -> 3 us;
-- 5.7 ms -> 9 us at 100x). ci_cache_events_leg, ci_artifacts_leg: a leg's cache events and artifacts
-- (compare 287 -> 12 us, reuse 56 -> 5 us). Ship's triggers on land_queue look its diffs' commits up
-- by diff rather than joining every commit change to the diffs' list: the same rows, 351 -> 28 us
-- for each entry written. runs_lane holds each run's commit, attempt and id too: a verdict's lane is
-- read from it alone (3.3 -> 0.9 ms at 100x). runs_lane_finished: a lane's last run to finish, for
-- run.fixed (1.6-2.1 ms -> 25-34 us at 100x); its WHERE leaves out finished_at, which a run's end
-- sets (with it there, each run's end measured 28-36 us slower). github_checks_due and runs_attempts:
-- GitHub's checks caught up every minute, those under way and those of runs past their first attempt
-- rather than every check (0.4 -> 0.08 ms; 78 -> 27 ms at 100x). The four add 2-11 us to a v2 run's
-- row, its end and a rerun. jobs_queued_labels: the routing rules' preview of where the last week's
-- legs would go now, the week's jobs read on it rather than every job (1.3 -> 0.13 ms; 231 -> 2.0 ms at
-- 100x); it adds 2-3 us to a job's row. ci_log_pending and ci_log_cursor: the log indexer's work, the
-- legs not in this node's index yet (when each finished, if it has) and how far into the jobs it has
-- looked, with how many of the pending have finished (the search's count): a pass takes in the jobs
-- made since and indexes the pending that finished first, and a search reads the count, rather than
-- either reading every finished job against the index (a pass 4.9 -> 0.19 ms, 570 -> 0.29 ms at 100x;
-- the count 1.6 ms -> 5 us, 330 ms -> 29 us at 100x); filled here from the jobs as they are (7 ms with
-- nothing indexed, as with the engine off; 1.4 s at 100x). Its triggers keep them with the index
-- whoever writes it (a leg indexed leaves the pending, one let go of whose job is still here is pending
-- again; 3-6 us a leg) and the count with the pending. This node's own, as its index is: written past
-- the cluster group. runs.agent, ci_run_attempts.agent and review_comments.agent: the agent session
-- (its whole id) that asked for a run, reran an attempt or wrote a comment, kept as each is made, where
-- an agent's principal (`agent:<its id's first eight>@<runner>`) was matched by those eight, which
-- sessions begun within a minute of each other share: an attachment's budget counted another's runs.
-- Filled here where the principal names the session whole (an agent's own token, `agent:<its id>`), a
-- comment's signature names it, or the eight start the id of one session alone of those kiln has a
-- record of (whom it acts for, who opened it, a certificate of it: its identities' sessions, migration
-- dbq_custody); left empty where they start none
-- or several (11 ms; 0.1 s at 100x). runs_agent and ci_run_attempts_agent:
-- an attachment's agents' runner minutes today, sought on them rather than read from every finished
-- job for each agent (0.88 -> 0.025 ms an agent, 4.9 -> 0.07 ms for six; 158 -> 0.27 ms and 839 -> 0.70
-- ms at 100x), at no cost measured to a run's row (within 3 us of 28-32). An agent session's principal
-- names it whole now (`agent:<id>@<runner> for ...`) and review knows it by its whole id (`agent:<id>`):
-- what review and diffs kept of one by those eight, and read back as who it is (a diff's author, its
-- reviewers, reviews, comments' authors, threads' openers, files viewed, inboxes), is named whole where
-- the eight start one session alone of those kiln has a record of (a comment's own session first), and
-- left as it was where they don't. What's only shown or audited keeps its text.
CREATE INDEX IF NOT EXISTS land_queue_live ON land_queue (repo_id, trunk, state) WHERE state IN ('queued', 'testing', 'held', 'returned');
CREATE INDEX IF NOT EXISTS review_ladders_pending ON review_ladders (whole_tests) WHERE whole_tests = 'pending';
CREATE INDEX IF NOT EXISTS diffs_number ON diffs (number);
CREATE INDEX IF NOT EXISTS diffs_author ON diffs (author);
CREATE INDEX IF NOT EXISTS diff_reviewers_reviewer ON diff_reviewers (reviewer);
CREATE INDEX IF NOT EXISTS review_threads_open ON review_threads (created_at) WHERE state = 'open';
CREATE INDEX IF NOT EXISTS runs_lane ON runs (repo_id, workflow_id, branch, created_at, number, sha, attempt, id) WHERE workflow_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS runs_lane_finished ON runs (repo_id, workflow_id, branch, finished_at) WHERE workflow_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS runs_repo_created ON runs (repo_id, created_at) WHERE workflow_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS ci_cache_events_ts ON ci_cache_events (ts);
CREATE INDEX IF NOT EXISTS ci_cache_events_leg ON ci_cache_events (leg_id);
CREATE INDEX IF NOT EXISTS ci_artifacts_leg ON ci_artifacts (leg_id) WHERE leg_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS runs_attempts ON runs (id) WHERE attempt != 1;
CREATE INDEX IF NOT EXISTS github_checks_due ON github_checks (run_id) WHERE state != 'completed' OR attempt != 1;
CREATE INDEX IF NOT EXISTS jobs_queued_labels ON jobs (queued_at, labels);
CREATE TABLE IF NOT EXISTS ci_log_pending (leg TEXT PRIMARY KEY, finished_at INTEGER);
CREATE INDEX IF NOT EXISTS ci_log_pending_finished ON ci_log_pending (finished_at, leg) WHERE finished_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS ci_log_pending_open ON ci_log_pending (leg) WHERE finished_at IS NULL;
CREATE TABLE IF NOT EXISTS ci_log_cursor (id INTEGER PRIMARY KEY CHECK (id = 1), upto INTEGER NOT NULL, upto_id TEXT, waiting INTEGER NOT NULL, swept TEXT NOT NULL DEFAULT '');
INSERT INTO ci_log_pending (leg, finished_at) SELECT j.id, j.finished_at FROM jobs j WHERE NOT EXISTS (SELECT 1 FROM ci_log_indexed x WHERE x.leg = j.id) ON CONFLICT (leg) DO NOTHING;
INSERT OR IGNORE INTO ci_log_cursor (id, upto, upto_id, waiting)
    VALUES (1, COALESCE((SELECT MAX(rowid) FROM jobs), 0), (SELECT id FROM jobs ORDER BY rowid DESC LIMIT 1), (SELECT COUNT(*) FROM ci_log_pending WHERE finished_at IS NOT NULL));
CREATE TRIGGER IF NOT EXISTS ci_log_pending_ai AFTER INSERT ON ci_log_pending WHEN NEW.finished_at IS NOT NULL BEGIN UPDATE ci_log_cursor SET waiting = waiting + 1; END;
CREATE TRIGGER IF NOT EXISTS ci_log_pending_ad AFTER DELETE ON ci_log_pending WHEN OLD.finished_at IS NOT NULL BEGIN UPDATE ci_log_cursor SET waiting = waiting - 1; END;
CREATE TRIGGER IF NOT EXISTS ci_log_pending_au AFTER UPDATE OF finished_at ON ci_log_pending WHEN (OLD.finished_at IS NULL) != (NEW.finished_at IS NULL)
    BEGIN UPDATE ci_log_cursor SET waiting = waiting + (NEW.finished_at IS NOT NULL) - (OLD.finished_at IS NOT NULL); END;
CREATE TRIGGER IF NOT EXISTS ci_log_indexed_ai AFTER INSERT ON ci_log_indexed BEGIN DELETE FROM ci_log_pending WHERE leg = NEW.leg; END;
CREATE TRIGGER IF NOT EXISTS ci_log_indexed_ad AFTER DELETE ON ci_log_indexed
    BEGIN INSERT INTO ci_log_pending (leg, finished_at) SELECT id, finished_at FROM jobs WHERE id = OLD.leg ON CONFLICT (leg) DO UPDATE SET finished_at = excluded.finished_at; END;
DROP TRIGGER IF EXISTS ship_review_land_ai;
DROP TRIGGER IF EXISTS ship_review_land_au;
DROP TRIGGER IF EXISTS ship_review_land_ad;
CREATE TRIGGER ship_review_land_ai AFTER INSERT ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (SELECT value FROM json_each(CASE WHEN json_valid(NEW.diffs) THEN NEW.diffs ELSE '[]' END)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_land_au AFTER UPDATE ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (SELECT value FROM json_each(CASE WHEN json_valid(OLD.diffs) THEN OLD.diffs ELSE '[]' END)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (SELECT value FROM json_each(CASE WHEN json_valid(NEW.diffs) THEN NEW.diffs ELSE '[]' END)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_land_ad AFTER DELETE ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (SELECT value FROM json_each(CASE WHEN json_valid(OLD.diffs) THEN OLD.diffs ELSE '[]' END)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
ALTER TABLE runs ADD COLUMN agent TEXT;
ALTER TABLE ci_run_attempts ADD COLUMN agent TEXT;
ALTER TABLE review_comments ADD COLUMN agent TEXT;
UPDATE runs SET agent = substr(actor, 7, 32) WHERE actor LIKE 'agent:%' AND length(substr(actor, 7, 32)) = 32 AND substr(actor, 7, 32) NOT GLOB '*[^0-9a-f]*' AND substr(actor, 39, 1) IN ('', ' ');
UPDATE ci_run_attempts SET agent = substr(actor, 7, 32) WHERE actor LIKE 'agent:%' AND length(substr(actor, 7, 32)) = 32 AND substr(actor, 7, 32) NOT GLOB '*[^0-9a-f]*' AND substr(actor, 39, 1) IN ('', ' ');
UPDATE review_comments SET agent = substr(author, 7, 32) WHERE author LIKE 'agent:%' AND length(substr(author, 7, 32)) = 32 AND substr(author, 7, 32) NOT GLOB '*[^0-9a-f]*' AND substr(author, 39, 1) IN ('', ' ');
UPDATE runs SET agent = (SELECT CASE WHEN COUNT(*) = 1 THEN MIN(s) END FROM (
        SELECT substr(k, 15) AS s FROM kv WHERE k >= 'agents/opener/' || substr(runs.actor, 7, 8) AND k < 'agents/opener/' || substr(runs.actor, 7, 8) || 'g' AND length(k) = 46
        UNION SELECT substr(k, 18) FROM kv WHERE k >= 'agents/opened-by/' || substr(runs.actor, 7, 8) AND k < 'agents/opened-by/' || substr(runs.actor, 7, 8) || 'g' AND length(k) = 49
        UNION SELECT session FROM identities WHERE session >= substr(runs.actor, 7, 8) AND session < substr(runs.actor, 7, 8) || 'g' AND length(session) = 32))
    WHERE actor LIKE 'agent:%' AND substr(actor, 15, 1) IN ('@', ' ') AND length(substr(actor, 7, 8)) = 8 AND substr(actor, 7, 8) NOT GLOB '*[^0-9a-f]*';
UPDATE ci_run_attempts SET agent = (SELECT CASE WHEN COUNT(*) = 1 THEN MIN(s) END FROM (
        SELECT substr(k, 15) AS s FROM kv WHERE k >= 'agents/opener/' || substr(ci_run_attempts.actor, 7, 8) AND k < 'agents/opener/' || substr(ci_run_attempts.actor, 7, 8) || 'g' AND length(k) = 46
        UNION SELECT substr(k, 18) FROM kv WHERE k >= 'agents/opened-by/' || substr(ci_run_attempts.actor, 7, 8) AND k < 'agents/opened-by/' || substr(ci_run_attempts.actor, 7, 8) || 'g' AND length(k) = 49
        UNION SELECT session FROM identities WHERE session >= substr(ci_run_attempts.actor, 7, 8) AND session < substr(ci_run_attempts.actor, 7, 8) || 'g' AND length(session) = 32))
    WHERE actor LIKE 'agent:%' AND substr(actor, 15, 1) IN ('@', ' ') AND length(substr(actor, 7, 8)) = 8 AND substr(actor, 7, 8) NOT GLOB '*[^0-9a-f]*';
UPDATE review_comments SET agent = COALESCE(
        (SELECT p.value FROM review_signatures s, json_each('["' || replace(substr(s.signer, 8), '/', '","') || '"]') p
          WHERE s.kind = 'comment' AND s.id = review_comments.id AND substr(s.signer, 1, 7) = 'kiln://' AND substr(s.signer, 8) NOT GLOB '*[^A-Za-z0-9._/-]*' AND p.key = 4
            AND (SELECT value FROM json_each('["' || replace(substr(s.signer, 8), '/', '","') || '"]') WHERE key = 3) = 'agent' AND substr(p.value, 1, 8) = substr(review_comments.author, 7, 8)),
        (SELECT CASE WHEN COUNT(*) = 1 THEN MIN(s) END FROM (
        SELECT substr(k, 15) AS s FROM kv WHERE k >= 'agents/opener/' || substr(review_comments.author, 7, 8) AND k < 'agents/opener/' || substr(review_comments.author, 7, 8) || 'g' AND length(k) = 46
        UNION SELECT substr(k, 18) FROM kv WHERE k >= 'agents/opened-by/' || substr(review_comments.author, 7, 8) AND k < 'agents/opened-by/' || substr(review_comments.author, 7, 8) || 'g' AND length(k) = 49
        UNION SELECT session FROM identities WHERE session >= substr(review_comments.author, 7, 8) AND session < substr(review_comments.author, 7, 8) || 'g' AND length(session) = 32)))
    WHERE author LIKE 'agent:%' AND substr(author, 15, 1) IN ('@', ' ') AND length(substr(author, 7, 8)) = 8 AND substr(author, 7, 8) NOT GLOB '*[^0-9a-f]*';
CREATE TEMP TABLE dbq_principals (short TEXT PRIMARY KEY, session TEXT);
INSERT OR IGNORE INTO dbq_principals (short)
    SELECT author FROM diffs WHERE author LIKE 'agent:%' AND substr(author, 15, 1) IN ('@', '') AND length(substr(author, 7, 8)) = 8 AND substr(author, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT reviewer FROM diff_reviewers WHERE reviewer LIKE 'agent:%' AND substr(reviewer, 15, 1) IN ('@', '') AND length(substr(reviewer, 7, 8)) = 8 AND substr(reviewer, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT reviewer FROM reviews WHERE reviewer LIKE 'agent:%' AND substr(reviewer, 15, 1) IN ('@', '') AND length(substr(reviewer, 7, 8)) = 8 AND substr(reviewer, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT author FROM review_comments WHERE author LIKE 'agent:%' AND substr(author, 15, 1) IN ('@', '') AND length(substr(author, 7, 8)) = 8 AND substr(author, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT created_by FROM review_threads WHERE created_by LIKE 'agent:%' AND substr(created_by, 15, 1) IN ('@', '') AND length(substr(created_by, 7, 8)) = 8 AND substr(created_by, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT user FROM viewed_files WHERE user LIKE 'agent:%' AND substr(user, 15, 1) IN ('@', '') AND length(substr(user, 7, 8)) = 8 AND substr(user, 7, 8) NOT GLOB '*[^0-9a-f]*'
    UNION SELECT person FROM ci_inbox WHERE person LIKE 'agent:%' AND substr(person, 15, 1) IN ('@', '') AND length(substr(person, 7, 8)) = 8 AND substr(person, 7, 8) NOT GLOB '*[^0-9a-f]*';
UPDATE dbq_principals SET session = (SELECT CASE WHEN COUNT(*) = 1 THEN MIN(s) END FROM (
        SELECT substr(k, 15) AS s FROM kv WHERE k >= 'agents/opener/' || substr(short, 7, 8) AND k < 'agents/opener/' || substr(short, 7, 8) || 'g' AND length(k) = 46
        UNION SELECT substr(k, 18) FROM kv WHERE k >= 'agents/opened-by/' || substr(short, 7, 8) AND k < 'agents/opened-by/' || substr(short, 7, 8) || 'g' AND length(k) = 49
        UNION SELECT session FROM identities WHERE session >= substr(short, 7, 8) AND session < substr(short, 7, 8) || 'g' AND length(session) = 32));
UPDATE review_comments SET author = 'agent:' || agent WHERE author LIKE 'agent:%' AND substr(author, 15, 1) IN ('@', '') AND length(substr(author, 7, 8)) = 8 AND substr(author, 7, 8) NOT GLOB '*[^0-9a-f]*' AND agent IS NOT NULL AND substr(agent, 1, 8) = substr(author, 7, 8);
UPDATE review_comments SET author = 'agent:' || m.session FROM dbq_principals m WHERE review_comments.author = m.short AND m.session IS NOT NULL;
UPDATE diffs SET author = 'agent:' || m.session FROM dbq_principals m WHERE diffs.author = m.short AND m.session IS NOT NULL;
UPDATE OR IGNORE diff_reviewers SET reviewer = 'agent:' || m.session FROM dbq_principals m WHERE diff_reviewers.reviewer = m.short AND m.session IS NOT NULL;
DELETE FROM diff_reviewers WHERE reviewer IN (SELECT short FROM dbq_principals WHERE session IS NOT NULL);
UPDATE reviews SET reviewer = 'agent:' || m.session FROM dbq_principals m WHERE reviews.reviewer = m.short AND m.session IS NOT NULL;
UPDATE review_threads SET created_by = 'agent:' || m.session FROM dbq_principals m WHERE review_threads.created_by = m.short AND m.session IS NOT NULL;
UPDATE OR IGNORE viewed_files SET user = 'agent:' || m.session FROM dbq_principals m WHERE viewed_files.user = m.short AND m.session IS NOT NULL;
DELETE FROM viewed_files WHERE user IN (SELECT short FROM dbq_principals WHERE session IS NOT NULL);
UPDATE ci_inbox SET person = 'agent:' || m.session FROM dbq_principals m WHERE ci_inbox.person = m.short AND m.session IS NOT NULL;
DROP TABLE dbq_principals;
CREATE INDEX IF NOT EXISTS runs_agent ON runs (agent) WHERE agent IS NOT NULL;
CREATE INDEX IF NOT EXISTS ci_run_attempts_agent ON ci_run_attempts (agent) WHERE agent IS NOT NULL;
