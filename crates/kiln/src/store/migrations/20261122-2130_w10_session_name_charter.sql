-- CP-T3.2. Positive historic reservations and the single immutable founding electorate.
CREATE TABLE session_name_authority (kind INTEGER PRIMARY KEY CHECK(kind IN (0,1)), certificate BLOB NOT NULL);
CREATE TABLE session_name_plan (id INTEGER PRIMARY KEY CHECK(id=1), voters TEXT NOT NULL);
CREATE TABLE session_name_reservations (name TEXT PRIMARY KEY NOT NULL);
-- An older node may have accepted local work before anyone else saw its name. Every prior
-- persistent, non-outpost execution host must inventory its names before the registrar delegates.
CREATE TABLE session_name_legacy_holders (name TEXT PRIMARY KEY, cursor TEXT NOT NULL DEFAULT '', complete INTEGER NOT NULL DEFAULT 0);
CREATE INDEX session_name_legacy_pending ON session_name_legacy_holders(name) WHERE complete=0;
CREATE TABLE session_name_legacy_rows (name TEXT PRIMARY KEY NOT NULL);
INSERT INTO session_name_legacy_holders(name)
SELECT a.name FROM agents a WHERE a.ephemeral=0 AND a.name<>'' AND NOT EXISTS
(SELECT 1 FROM json_each(CASE WHEN json_valid(a.labels) THEN a.labels ELSE '[]' END) WHERE value='outpost');
CREATE TABLE session_name_bootstrap_inventory (id INTEGER PRIMARY KEY CHECK(id=1));
