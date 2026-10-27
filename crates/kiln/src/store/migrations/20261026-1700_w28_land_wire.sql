-- The land queue by component (docs/ci.md W28-R100 to R109). Each entry's place in the conflict
-- graph: its component (by its head entry's id), its position there (from 1), its component's
-- AIMD window, a bisection's bound on the component's next candidate (kept on its head), and its
-- decision against each entry ahead of it (`verdict`, a JSON list of sentences). The revisions it
-- lands (`pinned`: a JSON list, kiln_ci::land::queue::Pin for each diff: version, commit, parent,
-- patch-id). Who holds it (`hold`: `gate` while Review's gate holds it, `review` when Review did;
-- `reason` says why). When it last joined the queue (`queued_at`; its order, before `created_at`).
-- Which queue lands it (`route`: `kiln`, or the name of its host's queue) and, on a host's, its
-- changes there (`host`: JSON, the repository's name and each change's number, head and ETag).
-- The files it conflicts with the trunk in, while it's back with Review (`conflicts`, JSON).
-- New states beside queued, testing, landed, ejected and cancelled: `held`, `returned` (it
-- conflicts with the trunk: back to Review's conflict ladder) and `withdrawn` (by Review).
ALTER TABLE land_queue ADD COLUMN component TEXT;
ALTER TABLE land_queue ADD COLUMN position INTEGER;
ALTER TABLE land_queue ADD COLUMN window_size INTEGER;
ALTER TABLE land_queue ADD COLUMN bisect INTEGER;
ALTER TABLE land_queue ADD COLUMN verdict TEXT;
ALTER TABLE land_queue ADD COLUMN pinned TEXT;
ALTER TABLE land_queue ADD COLUMN hold TEXT;
ALTER TABLE land_queue ADD COLUMN queued_at INTEGER;
ALTER TABLE land_queue ADD COLUMN route TEXT;
ALTER TABLE land_queue ADD COLUMN host TEXT;
ALTER TABLE land_queue ADD COLUMN conflicts TEXT;

-- What each land keeps (W28-R108), one row per entry landed: its changes and their kind, the
-- revisions approved with their patch-ids, the candidate tested (and the trunk it was built on),
-- its checks' runs, whether it landed as tested onto a trunk that moved since (`rebased`), the
-- trunk's move, the commit carrying each change, the trunk's policy it landed under, who asked, and
-- which queue landed it (`via`: `kiln`, or its host's).
CREATE TABLE IF NOT EXISTS land_records (
    entry_id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    changes TEXT NOT NULL,
    kind TEXT NOT NULL,
    strategy TEXT NOT NULL,
    revisions TEXT NOT NULL,
    candidate TEXT,
    base TEXT,
    runs TEXT NOT NULL,
    rebased INTEGER NOT NULL DEFAULT 0,
    moved_from TEXT NOT NULL,
    moved_to TEXT NOT NULL,
    landed TEXT NOT NULL,
    policy TEXT NOT NULL,
    landed_by TEXT NOT NULL,
    via TEXT NOT NULL,
    landed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS land_records_trunk ON land_records (repo_id, trunk, landed_at);
