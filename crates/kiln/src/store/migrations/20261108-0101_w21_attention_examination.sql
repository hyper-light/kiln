-- Plan 01a199e1b177752e84dad5dc990e6a15: future paired position evidence only.
-- Historical choices retain unknown positions/examination; no retrospective correction.
ALTER TABLE attention_choices ADD COLUMN exposure TEXT;
ALTER TABLE attention_choices ADD COLUMN position INTEGER;
ALTER TABLE attention_choices ADD COLUMN correction TEXT;
CREATE TABLE attention_visits (
    principal TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY(principal,key)
);
CREATE TABLE attention_rendered (
    principal TEXT NOT NULL, exposure TEXT NOT NULL, revision INTEGER NOT NULL,
    received_at INTEGER NOT NULL, value TEXT NOT NULL, valid INTEGER NOT NULL,
    PRIMARY KEY(principal,exposure,revision),
    FOREIGN KEY(principal,exposure) REFERENCES attention_visits(principal,key)
);
CREATE TABLE attention_visit_feedback (
    principal TEXT NOT NULL, exposure TEXT NOT NULL, final_revision INTEGER NOT NULL,
    closed_at INTEGER NOT NULL, chosen TEXT, choice_at INTEGER, position INTEGER,
    choice_key TEXT, source TEXT, prior_report TEXT,
    PRIMARY KEY(principal,exposure),
    FOREIGN KEY(principal,exposure) REFERENCES attention_visits(principal,key)
);
CREATE INDEX attention_visit_feedback_closed ON attention_visit_feedback(principal,closed_at);

-- Optional provenance is a real composite FK; command and direct choices have no such row.
CREATE TABLE attention_choice_exposures (
    principal TEXT NOT NULL, choice_key TEXT NOT NULL, exposure TEXT NOT NULL, position INTEGER NOT NULL,
    PRIMARY KEY(principal,choice_key),
    FOREIGN KEY(principal,choice_key) REFERENCES attention_choices(principal,key),
    FOREIGN KEY(principal,exposure) REFERENCES attention_visits(principal,key)
);
