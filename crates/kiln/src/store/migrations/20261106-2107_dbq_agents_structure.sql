-- dbq agents, structure (2026-11-06): what the plans' lists, the programs and the goal driver read by
-- going through words, kept where it's read.
-- plans, made again with its columns in the order its lists read them: what's small first, its words
-- and items last (body, items: read by the plan alone, a search, and a list in full), so a list in
-- brief, and the programs' reads of tags and titles, don't go through each plan's words to the columns
-- kept after them. Between them, its steps' count (steps, as of its version steps_of: what a brief
-- says), written with the plan by plans::create_in and update. One without it (each kept before now,
-- and any a kiln from before writes after a rollback) is counted from its words where it's read, and
-- kept counted by plans::count_steps as kiln starts. No column of plans is renamed or dropped, and
-- kiln names them all in what it writes: no epoch.
-- Measured on kiln's SQLite (median, at production's size and at 10x): the plans' list kept in memory
-- made again 20 -> 1.1 ms and 96 -> 27 ms; their briefs from the store 10.8 -> 0.86 ms and 131 -> 21.5
-- ms; an agent's proposed plan (its Plan tab, every 5 s) 4.0 -> 0.15 ms and 91 -> 3.3 ms. A list in full,
-- and a search, still read every plan's words (3.9 and 90 ms).
-- plan_tags: each plan's tags, a row each, made from plans.tags (the plan's own record of them, in the
-- order given, as the API and the UI show them) by the triggers below, in the transaction of every
-- write of a plan, whoever makes it (an older kiln's after a rollback too). programs::masters (the
-- driver's, for each running stack each tick), programs::track_plans and plans::list by tag seek a
-- tag on it rather than matching the JSON with LIKE (re-planned on every call for a bound pattern).
-- Measured (median us, at production's size and at 10x): track_plans 53 -> 10 and 2707 -> 92; plans::list
-- by a tag 23 ms -> 89 us and 303 -> 8.7 ms; masters 1.2 -> 2.0 and 1.2 -> 2.1 (round 2's index of the
-- JSON's words found it sorted; the sort of the master plans is the 1 us). A plan's write costs 4 to 5 us
-- more, 9 with its tags changed: the triggers, compiled into each statement that can fire them.
-- paused_for: why kiln paused a stack that goes on by itself once that's over: `usage`, its program's
-- usage rule (docs/programs.md), as of the stack's change `paused_for_at` (its updated_at then). It
-- holds while that's still the stack's updated_at: goals::touch carries it through a change that
-- leaves the state as it is, and any other change, or any change a kiln from before it makes (after a
-- rollback), leaves it behind, so a pause made since is never taken for usage's. The reason people
-- read is as it was. goals::paused_for_usage (each tick) and resume_lifted seek it rather than the
-- reason's first words; backfilled from the words the rule writes.
-- Measured: paused_for_usage 5.8 -> 0.8 us, 30 -> 1.9 us at 10x; each change of a stack (touch) 0.5 to
-- 0.7 us more, for its stamp. The migration in all: 85 to 89 ms at production's size with the tables
-- cached (120 ms cold), 0.35 to 0.66 s at 10x (to 2.1 s cold): the plans' copy, then its commit.
CREATE TABLE plans_by_list (id TEXT PRIMARY KEY, title TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]', source TEXT, share TEXT UNIQUE, version INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, phase TEXT NOT NULL DEFAULT 'ready', steps INTEGER, steps_of INTEGER, body TEXT NOT NULL, items TEXT NOT NULL DEFAULT '[]');
INSERT INTO plans_by_list (id, title, tags, source, share, version, created_at, updated_at, phase, body, items) SELECT id, title, tags, source, share, version, created_at, updated_at, phase, body, items FROM plans ORDER BY rowid;
DROP TABLE plans;
ALTER TABLE plans_by_list RENAME TO plans;
CREATE TABLE plan_tags (tag TEXT NOT NULL, plan_id TEXT NOT NULL, PRIMARY KEY (tag, plan_id)) WITHOUT ROWID;
INSERT INTO plan_tags (tag, plan_id) SELECT DISTINCT j.value, p.id FROM plans p, json_each(CASE WHEN json_valid(p.tags) THEN p.tags ELSE '[]' END) j WHERE j.type = 'text';
CREATE TRIGGER plan_tags_ai AFTER INSERT ON plans BEGIN INSERT INTO plan_tags (tag, plan_id) SELECT DISTINCT value, NEW.id FROM json_each(CASE WHEN json_valid(NEW.tags) THEN NEW.tags ELSE '[]' END) WHERE type = 'text'; END;
CREATE TRIGGER plan_tags_au AFTER UPDATE OF id, tags ON plans WHEN OLD.id IS NOT NEW.id OR OLD.tags IS NOT NEW.tags BEGIN DELETE FROM plan_tags WHERE plan_id = OLD.id AND tag IN (SELECT value FROM json_each(CASE WHEN json_valid(OLD.tags) THEN OLD.tags ELSE '[]' END) WHERE type = 'text'); INSERT INTO plan_tags (tag, plan_id) SELECT DISTINCT value, NEW.id FROM json_each(CASE WHEN json_valid(NEW.tags) THEN NEW.tags ELSE '[]' END) WHERE type = 'text'; END;
CREATE TRIGGER plan_tags_ad AFTER DELETE ON plans BEGIN DELETE FROM plan_tags WHERE plan_id = OLD.id AND tag IN (SELECT value FROM json_each(CASE WHEN json_valid(OLD.tags) THEN OLD.tags ELSE '[]' END) WHERE type = 'text'); END;
ALTER TABLE goal_stacks ADD COLUMN paused_for TEXT;
ALTER TABLE goal_stacks ADD COLUMN paused_for_at INTEGER;
UPDATE goal_stacks SET paused_for = 'usage', paused_for_at = updated_at WHERE state = 'paused' AND substr(reason, 1, 16) = 'Paused for usage';
CREATE INDEX goal_stacks_paused_for ON goal_stacks (paused_for) WHERE paused_for IS NOT NULL;
