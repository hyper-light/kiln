-- The collision radar (docs/ci.md W26-R16): two open changes of a repository that change the same
-- lines of a file, on the trunk's lines, as kiln_ci::fresh found them. `a` and `b` name the changes
-- as kiln does (`D44`, `pull:github:488`), `a` before `b`; the lines are the first both change. A
-- pair and file is told once (its authors, through `change.collision`); `gone_at` says when it
-- stopped being so (one landed, closed or moved off those lines). A new table only: an older kiln
-- never names it.
CREATE TABLE review_collisions (
    repo_id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    a TEXT NOT NULL,
    b TEXT NOT NULL,
    file TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    told_at INTEGER NOT NULL,
    gone_at INTEGER,
    PRIMARY KEY (repo_id, a, b, file)
);
CREATE INDEX review_collisions_b ON review_collisions (repo_id, b);
