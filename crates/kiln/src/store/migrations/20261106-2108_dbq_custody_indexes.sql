-- The custody area's indexes over its existing tables, built once kiln serves (`crate::store::indexes`:
-- an index migration, so a first start doesn't wait the seconds a cold 3.4 GB custody table takes;
-- queries read without them meanwhile, only slower). Medians on bench databases at production's size,
-- 795k custody records (the DB query audit, 2026-11-05 and -06, ~/.cache/dbq/custody, custody2 and
-- r4relays).
-- The records under a delegation, looked for as each delegation is taken, on the writer's thread
-- (`custody::under_on`): a scan of every record, 644 ms; 50 us on custody_under, which holds only
-- the few records that act under one.
-- A thing's newest chain head by its name (`custody::head_named`: the custody page's `name`, a
-- diff's verification, a runner's replant): a scan of every thing, 86-136 ms; 0.3 ms on
-- custody_things_name, NOCASE as a name's letters' case is set aside: a name, or a prefix's range.
-- The newest breaks and refusals (`custody::breaks`, the custody page; `custody::refusals`,
-- `kiln transfers refused`): every one read and sorted, 2.3 and 2.7 ms; 0.4 and 0.44 ms read in
-- their order on custody_breaks_at and transfer_refusals_at.
CREATE INDEX IF NOT EXISTS custody_under ON custody (under) WHERE under IS NOT NULL;
CREATE INDEX IF NOT EXISTS custody_things_name ON custody_things (name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS custody_breaks_at ON custody_breaks (at);
CREATE INDEX IF NOT EXISTS transfer_refusals_at ON transfer_refusals (at);
