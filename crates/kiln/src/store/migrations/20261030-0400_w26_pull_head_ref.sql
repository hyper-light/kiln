-- A host's pull request's own branch (docs/ci.md W26-R16): GitHub's head.ref, GitLab's
-- source_branch, Bitbucket's source branch. A pull request whose base is another open one's branch
-- sits on it in a stack, and kiln replays it onto that one's replay as its base moves. Nullable: an
-- older kiln never names it, and the mirror fills it on its next read of each pull request.
ALTER TABLE host_pulls ADD COLUMN head_ref TEXT;
