-- W29-R25/R26: code evidence belongs to an exact consumer draft revision. Recording a
-- proposal never accepts its pins, changes global tag history, or authorizes execution.
CREATE TABLE ci_lock_reviews (
    id TEXT PRIMARY KEY,
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    draft_id TEXT NOT NULL REFERENCES ci_pipeline_drafts(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    evidence TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL,
    proposed_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (draft_id, commit_sha)
);
CREATE INDEX ci_lock_reviews_commit ON ci_lock_reviews(consumer_repo_id, commit_sha);

CREATE TABLE ci_lock_review_pins (
    review_id TEXT NOT NULL REFERENCES ci_lock_reviews(id) ON DELETE CASCADE,
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    uses_key TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    PRIMARY KEY (review_id, uses_key)
);
CREATE INDEX ci_lock_review_pins_consumer ON ci_lock_review_pins(consumer_repo_id, source_identity, commit_sha);
