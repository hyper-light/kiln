-- W05-R28 (docs/ci.md): where a person's retry asked a job to run, as JSON: `{"runner": <name>}`
-- (only there) or `{"avoid": <name>}` (anywhere but there). A `.kiln/` pipeline's job retried in
-- place keeps it for its new attempt; a leg of the CI engine's rerun has it from the rerun.
ALTER TABLE jobs ADD COLUMN placement TEXT;
