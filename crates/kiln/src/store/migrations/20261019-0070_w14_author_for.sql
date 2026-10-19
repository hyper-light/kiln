-- Who a diff's or change request's author acts for (docs/ci.md W14): an agent's person
-- (`agent:review` for `user:ada`), so what reviewers and checks say reaches the agent and its person.
ALTER TABLE diffs ADD COLUMN author_for TEXT;
