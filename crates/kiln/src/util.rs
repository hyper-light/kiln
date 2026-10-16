use std::path::Path;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

/// 32 random bytes, URL-safe base64. Used for tokens and secrets.
pub fn random_token() -> String {
    use base64::Engine as _;
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(data))
}

/// Constant-time comparison for secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write a file atomically with the given mode (temp file + rename; the mode is Unix-only).
pub fn write_secret(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = crate::os::with_mode(std::fs::OpenOptions::new().create(true).truncate(true).write(true), mode)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Read a secret file, or create it with a fresh random token. Never prompts.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        _ => {
            let t = random_token();
            write_secret(path, t.as_bytes(), 0o600)?;
            Ok(t)
        }
    }
}

pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

/// HMAC-SHA256 of `msg` under `key`, hex encoded.
pub fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            hex::encode(m.finalize().into_bytes())
        }
        Err(_) => String::new(),
    }
}
