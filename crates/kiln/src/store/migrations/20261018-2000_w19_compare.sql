-- Comparing runs (docs/ci.md W19-R03): what each attempt ran on, as its leg reported it
-- (machine, runner version, executor, images, environment with secrets masked; JSON).
ALTER TABLE ci_attempts ADD COLUMN facts TEXT;
