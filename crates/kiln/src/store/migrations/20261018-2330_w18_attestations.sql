-- Attestations (docs/ci.md W18-R01, R02): the Sigstore bundles of the statements the server made
-- and signed for a leg's attest steps (`origin` 'kiln'), or that a job uploaded through the
-- facade's attestations API ('job'), and the subject digests (`sha256:<hex>`) they're about.
CREATE TABLE ci_attestations (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL,
    leg_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    nonce TEXT NOT NULL DEFAULT '',
    origin TEXT NOT NULL DEFAULT 'kiln',
    predicate_type TEXT NOT NULL,
    subjects TEXT NOT NULL,
    bundle TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX ci_attestations_run ON ci_attestations (run_id);
CREATE INDEX ci_attestations_leg ON ci_attestations (leg_id, nonce);
CREATE TABLE ci_attestation_subjects (
    attestation_id INTEGER NOT NULL REFERENCES ci_attestations (id) ON DELETE CASCADE,
    digest TEXT NOT NULL
);
CREATE INDEX ci_attestation_subjects_digest ON ci_attestation_subjects (digest);
-- An SBOM a runner uploaded for its attest step, until the step's report signs it.
CREATE TABLE ci_attestation_uploads (
    leg_id TEXT NOT NULL,
    nonce TEXT NOT NULL,
    doc TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (leg_id, nonce)
);
