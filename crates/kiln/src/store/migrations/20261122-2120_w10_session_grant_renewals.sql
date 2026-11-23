-- W10. The original grant has one renewal even when the issuer's response is lost.
CREATE TABLE session_grant_renewals (
    prior TEXT PRIMARY KEY,
    issued TEXT NOT NULL REFERENCES identities(serial)
);
CREATE INDEX revocations_by_serial_at ON revocations(serial, at) WHERE serial IS NOT NULL;
