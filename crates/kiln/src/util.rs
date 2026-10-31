use std::path::Path;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// An OS file lock owned by this Rust value, even while another thread spawns a child.
/// Closing alone can leave an inherited descriptor holding the lock until that child execs.
/// Keep shared ownership in an `Arc<FileLock>`; only its final owner unlocks.
#[derive(Debug)]
pub(crate) struct FileLock(std::fs::File);

impl FileLock {
    pub(crate) fn lock(file: std::fs::File) -> std::io::Result<Self> {
        file.lock()?;
        Ok(Self(file))
    }

    pub(crate) fn try_lock(file: std::fs::File) -> Result<Self, std::fs::TryLockError> {
        file.try_lock()?;
        Ok(Self(file))
    }

    #[cfg(test)]
    pub(crate) fn try_clone(&self) -> std::io::Result<std::fs::File> {
        self.0.try_clone()
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // No work retains this owner now. Explicit unlock also releases inherited duplicates.
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "an owned file lock could not be released");
        }
    }
}

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

/// An id made from `parts` (a namespace, then what it names), the same every time they're the same
/// and as unlikely to meet another as a random one (a UUIDv8 of their SHA-256): how a retry finds
/// the id its first try made, from the key its sender gave (CP-T3.2, an idempotency key).
pub fn derive_id(parts: &[&str]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for p in parts {
        // Each part's length first, so no two lists of parts read the same.
        h.update(u64::try_from(p.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    for (b, d) in bytes.iter_mut().zip(digest.iter()) {
        *b = *d;
    }
    uuid::Builder::from_custom_bytes(bytes).into_uuid().simple().to_string()
}

/// 32 random bytes, URL-safe base64. Used for tokens and secrets.
/// 32 random bytes, base64url: never beginning with `-`, which a command line would take for a
/// flag (`kiln agent --token -Hq3…`).
pub fn random_token() -> String {
    use base64::Engine as _;
    loop {
        let bytes: [u8; 32] = rand::random();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        if !token.starts_with('-') {
            return token;
        }
    }
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
    write_private(path, contents, mode, true)
}

/// As [`write_secret`], without waiting for the disk: scratch files a crash may lose (a test's
/// throwaway keys; headroom's note of what it paced, written while the disk may be stalled).
pub fn write_secret_unsynced(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    write_private(path, contents, mode, false)
}

fn write_private(path: &Path, contents: &[u8], mode: u32, sync: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = crate::os::with_mode(std::fs::OpenOptions::new().create(true).truncate(true).write(true), mode)
            .open(&tmp)?;
        f.write_all(contents)?;
        if sync {
            f.sync_all()?;
        }
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

#[cfg(test)]
mod tests {
    /// U07/U17/U19: ownership ends when its final Rust owner leaves, even if a spawned child still
    /// holds an inherited handle. A duplicated handle exercises that OS rule without forking.
    #[test]
    fn a_file_lock_releases_after_its_final_owner_with_a_duplicate_handle_alive() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("owner.lock");
        let open = || std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path);
        let owner = std::sync::Arc::new(super::FileLock::lock(open()?)?);
        let shared = std::sync::Arc::clone(&owner);
        let duplicate = owner.0.try_clone()?;
        drop(owner);
        assert!(matches!(super::FileLock::try_lock(open()?), Err(std::fs::TryLockError::WouldBlock)), "a shared owner still excludes a successor");
        drop(shared);
        let _successor = super::FileLock::try_lock(open()?)?;
        drop(duplicate);
        Ok(())
    }

    /// CP-T3.2: an id made from a key is the same each time it's made from it, a UUID like any
    /// other id, and another for other parts (however they're split).
    #[test]
    fn an_id_made_from_a_key_is_the_same_each_time_and_apart_from_others() {
        let a = super::derive_id(&["agents.send", "caller", "agent", "key-1"]);
        assert_eq!(a, super::derive_id(&["agents.send", "caller", "agent", "key-1"]));
        assert_eq!(a.len(), 32);
        assert!(uuid::Uuid::parse_str(&a).is_ok_and(|u| u.get_version_num() == 8));
        assert_ne!(a, super::derive_id(&["agents.send", "caller", "agent", "key-2"]));
        assert_ne!(super::derive_id(&["ab", "c"]), super::derive_id(&["a", "bc"]), "split differently, different");
        assert_ne!(super::derive_id(&["a"]), super::new_id());
    }

    #[test]
    fn a_token_never_begins_with_a_dash() {
        // One in 64 would, without the care.
        assert!((0..10_000).map(|_| super::random_token()).all(|t| !t.starts_with('-') && t.len() == 43));
    }
}
