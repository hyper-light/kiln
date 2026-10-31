//! U09/U15: bind an out-of-band bootstrap identity, never distribute or rotate bootstrap keys.
use std::io::Read as _;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::sql::{Connection, OptionalExtension as _, params};
use crate::store::Store;

use super::Master;

const VERIFIER_KEY: &str = "secrets/bootstrap-master";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verifier {
    version: u32,
    sha256: String,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// U05/U09: the pin is out of band so introducing it need not add TOML fields which an
/// older build cannot parse. It does not make a SQLite-era binary a safe data rollback.
fn trusted_pin(cfg: &Config) -> anyhow::Result<Option<String>> {
    let path = cfg.data("secrets").join("master.sha256");
    let from_file = match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            // A dangling/unreadable provisioned path is an error, never an absent pin.
            // Refuse nonregular inputs before opening a FIFO/device which could block startup.
            anyhow::ensure!(std::fs::metadata(&path)?.is_file(), "U09 trusted master pin must be a regular file: {}", path.display());
            let file = std::fs::File::open(&path).with_context(|| format!("read trusted master pin {}", path.display()))?;
            anyhow::ensure!(file.metadata()?.is_file(), "U09 trusted master pin must be a regular file: {}", path.display());
            let mut bytes = Vec::new();
            file.take(67).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 66, "U09 trusted master pin exceeds 64 hexadecimal digits plus a line ending: {}", path.display());
            let text = std::str::from_utf8(&bytes).context("U09 trusted master pin must be UTF-8 hexadecimal digits")?;
            let text = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(text);
            anyhow::ensure!(valid_digest(text), "U09 trusted master pin must contain exactly 64 lowercase hexadecimal SHA256 digits: {}", path.display());
            Some(text.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read trusted master pin {}", path.display())),
    };
    let configured = cfg.ci.secrets.trusted_master_sha256.as_deref();
    if let Some(pin) = configured { anyhow::ensure!(valid_digest(pin), "[ci.secrets] trusted_master_sha256 must be exactly 64 lowercase hexadecimal SHA256 digits"); }
    if let (Some(configured), Some(provisioned)) = (configured, from_file.as_deref()) {
        anyhow::ensure!(crate::util::ct_eq(configured.as_bytes(), provisioned.as_bytes()), "U09 trusted master pin file and configured pin disagree; retain the original trusted identity");
    }
    Ok(configured.map(str::to_owned).or(from_file))
}

fn verifier(c: &Connection<'_>) -> anyhow::Result<Option<Verifier>> {
    let value: Option<String> = c.query_row("SELECT v FROM kv WHERE k = ?1", [VERIFIER_KEY], |r| r.get(0)).optional()?;
    value.map(|value| {
        anyhow::ensure!(value.len() <= 256, "U09/U15 bootstrap master verifier exceeds its bound");
        let value: Verifier = serde_json::from_str(&value).context("U09/U15 unreadable bootstrap master verifier")?;
        anyhow::ensure!(value.version == 1 && valid_digest(&value.sha256), "U09/U15 unsupported or invalid bootstrap master verifier; refusing to replace it");
        Ok(value)
    }).transpose()
}

/// No ciphertext is read or rewritten here. Without a verifier, these records may depend on
/// the original master, including HMAC-derived hooks whose matching key cannot be recovered.
/// Initial legacy binding therefore requires an explicit pin obtained from the trusted source.
fn legacy_protected(c: &Connection<'_>) -> anyhow::Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM secret_keys) OR EXISTS(SELECT 1 FROM ci_keys)
         OR EXISTS(SELECT 1 FROM secret_versions WHERE ciphertext IS NOT NULL)
         OR EXISTS(SELECT 1 FROM ci_secrets WHERE ciphertext IS NOT NULL)
         OR EXISTS(SELECT 1 FROM oidc_keys) OR EXISTS(SELECT 1 FROM github_app)
         OR EXISTS(SELECT 1 FROM ci_secret_outputs)
         OR EXISTS(SELECT 1 FROM user_identities WHERE token_sealed IS NOT NULL)
         OR EXISTS(SELECT 1 FROM ci_hooks)
         OR EXISTS(SELECT 1 FROM ci_integrations WHERE kind IN ('slack', 'discord', 'telegram', 'teams'))",
        [], |r| r.get(0),
    )?)
}

pub(crate) fn initialize(cfg: &Config, store: &Store) -> anyhow::Result<Master> {
    // SDK clients must inspect the product owner too: absence of a local replica handle/config
    // does not mean the shared Database is standalone. Refused status never becomes local mode.
    let status = store.database().replica_status(crate::store::deadline()).context("U09/U15 cannot determine bootstrap master replication mode")?;
    let replicated = cfg.cluster.replica.is_some() || status.is_some();
    let pin = trusted_pin(cfg)?;
    let pin = pin.as_deref();
    anyhow::ensure!(!replicated || pin.is_some(), "U09/U15 replicated serving Hub requires secrets/master.sha256 or [ci.secrets] trusted_master_sha256 from the trusted source before readiness");
    let trusted = pin.is_some();
    let bound = store.now(move |c| {
        let bound = verifier(c)?.is_some();
        anyhow::ensure!(bound || trusted || !legacy_protected(c)?, "U09/U15 initial legacy master binding requires secrets/master.sha256 or [ci.secrets] trusted_master_sha256 from the original trusted source; protected state is retained");
        Ok(bound)
    })?;
    // Only a fresh standalone service may create a key. Once bound, a missing file must never
    // be replaced; replicas and explicit legacy pins always require preprovisioned bytes.
    let key = super::master_at_mode(&super::master_path(cfg), !replicated && !trusted && !bound)?;
    let digest = crate::util::sha256_hex(&key);
    if let Some(pin) = pin {
        anyhow::ensure!(crate::util::ct_eq(pin.as_bytes(), digest.as_bytes()), "U09/U15 bootstrap master does not match the trusted master pin; provision the original trusted master, not a new key");
    }
    // One product transaction verifies/initializes the immutable record. Failed or ambiguous
    // commit acknowledgement returns no Master and exposes no Hub; retry reconciles the record.
    store.now(move |c| {
        let tx = crate::store::tx(c)?;
        match verifier(&tx)? {
            Some(value) => anyhow::ensure!(crate::util::ct_eq(value.sha256.as_bytes(), digest.as_bytes()), "U09/U15 bootstrap master changed: this key disagrees with the canonical verifier; refusing rebind"),
            None => {
                anyhow::ensure!(trusted || !legacy_protected(&tx)?, "U09/U15 protected state appeared before master binding; an explicit trusted source pin is required");
                let value = serde_json::to_string(&Verifier { version: 1, sha256: digest })?;
                tx.execute("INSERT INTO kv (k, v) VALUES (?1, ?2)", params![VERIFIER_KEY, value])?;
            }
        }
        tx.commit()?;
        Ok(())
    }).context("U09/U15 bootstrap master verifier was not acknowledged; Hub is unavailable")?;
    Ok(Master(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u09_u15_derived_only_legacy_binding_requires_trusted_pin_and_refuses_rebind() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        // Exercise the documented override rather than the default secrets/master.key path.
        let path = dir.path().join("provisioned-master");
        cfg.ci.secrets.master_key_file = Some(path.clone());
        let key = [37u8; 32];
        std::fs::write(&path, key)?;
        store.now(|c| {
            c.execute("INSERT INTO ci_hooks (id,repo_id,name,created_by,created_at) VALUES ('legacy-hook','repo','hook','operator',1)", [])?;
            Ok(())
        })?;
        assert!(initialize(&cfg, &store).is_err(), "a local file alone must not claim existing derived-only state");
        assert!(store.now(|c| verifier(c))?.is_none());
        let pin_path = cfg.data("secrets").join("master.sha256");
        std::fs::create_dir_all(pin_path.parent().context("pin parent")?)?;
        for invalid in [String::new(), "g".repeat(64), "a".repeat(67)] {
            std::fs::write(&pin_path, invalid)?;
            assert!(initialize(&cfg, &store).is_err());
            assert!(store.now(|c| verifier(c))?.is_none());
        }
        let pin = crate::util::sha256_hex(&key);
        std::fs::write(&pin_path, format!("{pin}\r\n"))?;
        cfg.ci.secrets.trusted_master_sha256 = Some("f".repeat(64));
        assert!(initialize(&cfg, &store).is_err(), "conflicting operator inputs never choose a silent precedence");
        cfg.ci.secrets.trusted_master_sha256 = None;
        let cached = initialize(&cfg, &store)?;
        cfg.ci.secrets.trusted_master_sha256 = Some(pin);
        initialize(&cfg, &store)?;
        let before = store.now(|c| Ok(verifier(c)?.map(|v| v.sha256)))?;
        // An otherwise well-formed replacement and a matching *new* operator pin cannot rebind.
        let changed = [38u8; 32];
        std::fs::write(&path, changed)?;
        let changed_pin = crate::util::sha256_hex(&changed);
        cfg.ci.secrets.trusted_master_sha256 = Some(changed_pin.clone());
        std::fs::write(&pin_path, changed_pin)?;
        assert!(initialize(&cfg, &store).is_err());
        assert_eq!(store.now(|c| Ok(verifier(c)?.map(|v| v.sha256)))?, before);
        assert!(crate::util::ct_eq(&cached.bytes(), &key), "the accepted master remains cached");
        std::fs::remove_file(&path)?;
        assert!(initialize(&cfg, &store).is_err());
        assert!(!path.exists(), "a required missing master must not be regenerated");
        Ok(())
    }
}
