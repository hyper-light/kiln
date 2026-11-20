-- kiln's Slates fleet's one-time bootstrap (A-734; goal 91, main's ruling of 2026-11-20): the runner that makes the
-- fleet's root group, claimed once by the first runner of the fleet to ask, bound to its machine, and marked done
-- once it says so. A claim is never taken again: a restart, a second runner or a server restart never bootstraps a
-- fleet a second time (Slates' docs/deploy.md: re-running bootstrap does not recover a lost group). `member` is the
-- Slates member id the bootstrap named; no key material is kept here.
CREATE TABLE IF NOT EXISTS slates_fleet_bootstrap (
    fleet TEXT PRIMARY KEY,
    machine TEXT NOT NULL,
    member INTEGER NOT NULL,
    claimed_at INTEGER NOT NULL,
    done_at INTEGER
);
