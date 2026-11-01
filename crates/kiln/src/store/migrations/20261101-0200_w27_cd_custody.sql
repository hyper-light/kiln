-- Custody to production (docs/ci.md W27-R08; the Transfers plan's step 14, kiln plan
-- 01a17856b1b477eda14a574ae312a123): each KilnCD sync that handed a cluster its manifests, with CD's
-- hand-on and kiln's receipt for the cluster (their digests, in `custody`), the SHA-256 of what CD
-- sent and of what the API server held of it, and the containers the manifests name; and each pod's
-- container as kiln looked at it, by the image its status reported: what its deploy handed on for its
-- node's platform, what it runs, the pod's receipt, and whether it follows.
CREATE TABLE IF NOT EXISTS cd_custody_syncs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app TEXT NOT NULL,
    cluster TEXT NOT NULL,
    revision TEXT NOT NULL DEFAULT '',
    op INTEGER NOT NULL,
    hand_on TEXT NOT NULL,
    receipt TEXT NOT NULL,
    -- SHA-256 (hex) of the objects as CD sent them, and of what the API server's answers held of them.
    manifests TEXT NOT NULL,
    held TEXT NOT NULL,
    objects INTEGER NOT NULL,
    -- JSON: each container the manifests name, [{workload, container, image}].
    containers TEXT NOT NULL DEFAULT '[]',
    -- Where the cluster holds other than CD sent, if it does.
    why TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS cd_custody_syncs_app ON cd_custody_syncs (app, id);

CREATE TABLE IF NOT EXISTS cd_custody_pods (
    app TEXT NOT NULL,
    -- The sync (cd_custody_syncs.id) whose receipt its custody follows.
    sync INTEGER NOT NULL,
    cluster TEXT NOT NULL,
    namespace TEXT NOT NULL,
    pod TEXT NOT NULL,
    pod_uid TEXT NOT NULL,
    node TEXT NOT NULL,
    platform TEXT NOT NULL,
    container TEXT NOT NULL,
    -- Its image as the pod's spec named it, and its image ID as its status reported it.
    image TEXT NOT NULL,
    image_id TEXT NOT NULL,
    -- SHA-256 (hex): the manifest its deploy's image resolves to on the node's platform, and what it runs.
    wants TEXT NOT NULL,
    runs TEXT NOT NULL,
    -- The pod's receipt, in `custody`.
    record TEXT NOT NULL,
    follows INTEGER NOT NULL,
    why TEXT NOT NULL DEFAULT '',
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (pod_uid, container, image_id)
);
CREATE INDEX IF NOT EXISTS cd_custody_pods_app ON cd_custody_pods (app, sync);

-- A pod's image resolved by its digest, wherever it was pulled from.
CREATE INDEX IF NOT EXISTS registry_manifests_digest ON registry_manifests (digest);
