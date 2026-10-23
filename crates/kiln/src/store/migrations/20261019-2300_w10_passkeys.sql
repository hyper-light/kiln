-- W10 (docs/ci.md): passkeys (WebAuthn) for people and the owner, and people's own passwords.
CREATE TABLE passkeys (
  id TEXT PRIMARY KEY,                  -- the credential's id, base64url
  principal TEXT NOT NULL,              -- owner, or user:<id>
  name TEXT NOT NULL,                   -- what its person calls it
  rp_id TEXT NOT NULL,                  -- the domain it was made for
  alg INTEGER NOT NULL,                 -- its COSE algorithm: -7 (ES256), -8 (EdDSA), -257 (RS256)
  public_key BLOB NOT NULL,             -- its COSE key
  sign_count INTEGER NOT NULL DEFAULT 0,
  backed_up INTEGER NOT NULL DEFAULT 0, -- synced by its provider (a phone's, a password manager's)
  transports TEXT,                      -- JSON array: how the browser reaches it
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
CREATE INDEX passkeys_principal ON passkeys (principal);

-- A person's own password (Argon2id, PHC), beside their sign-in links, SSO and passkeys.
ALTER TABLE users ADD COLUMN password_hash TEXT;
