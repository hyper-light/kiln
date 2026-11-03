-- W27-R08's relays (docs/ci.md W27; the Transfers plan's step 13, kiln plan
-- 01a17856b1b477eda14a574ae312a123): a break found in what a relay passed on names the relay (its
-- identity's URI), so kiln says which intermediary dropped, swapped or altered what it carried; and
-- the relays' records no receipt answers are found by their kind and when kiln took them.
ALTER TABLE custody_breaks ADD COLUMN relay TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS custody_kind_taken ON custody (kind, taken_at);
