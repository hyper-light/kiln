-- W29-R25/R26: a person's exact review authorizes only this consumer's recorded pin change.
-- Global source/ref history remains unchanged, including for all other consumers.
CREATE TABLE ci_lock_approvals (
    id TEXT PRIMARY KEY,
    proposal_id TEXT NOT NULL REFERENCES ci_lock_reviews(id) ON DELETE CASCADE,
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    diff_id TEXT REFERENCES diffs(id) ON DELETE CASCADE,
    version INTEGER,
    commit_sha TEXT NOT NULL,
    tree_sha TEXT NOT NULL,
    human_review_id TEXT REFERENCES reviews(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('dependency', 'review')),
    principal_kind TEXT NOT NULL CHECK(principal_kind IN ('owner', 'user')),
    user_id TEXT,
    approved_by TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(proposal_id, human_review_id),
    CHECK(kind = 'dependency' OR (diff_id IS NOT NULL AND version IS NOT NULL AND human_review_id IS NOT NULL))
);
CREATE UNIQUE INDEX ci_lock_direct_approval ON ci_lock_approvals(proposal_id, approved_by) WHERE kind = 'dependency';
CREATE TABLE ci_lock_approved_pins (
    approval_id TEXT NOT NULL REFERENCES ci_lock_approvals(id) ON DELETE CASCADE,
    uses_key TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    git_ref TEXT NOT NULL,
    before_commit TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    PRIMARY KEY(approval_id, uses_key)
);
CREATE TABLE ci_lock_candidate_pins (
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    candidate_sha TEXT NOT NULL,
    approval_id TEXT NOT NULL,
    uses_key TEXT NOT NULL,
    PRIMARY KEY(consumer_repo_id, candidate_sha, approval_id, uses_key),
    FOREIGN KEY(approval_id, uses_key) REFERENCES ci_lock_approved_pins(approval_id, uses_key) ON DELETE CASCADE
);
CREATE TABLE ci_lock_acceptances (
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    approval_id TEXT NOT NULL,
    uses_key TEXT NOT NULL,
    landed_commit TEXT NOT NULL,
    landed_at INTEGER NOT NULL,
    PRIMARY KEY(consumer_repo_id, approval_id, uses_key),
    FOREIGN KEY(approval_id, uses_key) REFERENCES ci_lock_approved_pins(approval_id, uses_key) ON DELETE CASCADE
);
CREATE INDEX ci_lock_approved_source ON ci_lock_approved_pins(source_identity, git_ref, commit_sha);
