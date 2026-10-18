-- W10-R06: the OIDC issuer's signing keys. The private key (PKCS#8) is sealed by the master key
-- (as W09's data keys are); the public half is published as a JWK.
CREATE TABLE oidc_keys (
  kid TEXT PRIMARY KEY,
  alg TEXT NOT NULL,
  sealed BLOB NOT NULL,
  nonce BLOB NOT NULL,
  n TEXT NOT NULL,                      -- base64url modulus
  e TEXT NOT NULL,                      -- base64url exponent
  created_at INTEGER NOT NULL,          -- published from
  sign_from INTEGER NOT NULL
);
