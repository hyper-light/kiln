//! The sign-in password: an optional passphrase the owner chooses (in Settings, with
//! `kiln passwd`, or `KILN_ADMIN_PASSWORD` at install) so any device can sign in from the login
//! page. The admin token stays the root credential; a password only ever yields a UI session.
//!
//! Stored as an Argon2id PHC string in `<data>/admin.password` (mode 0600, replaced atomically).
//! Wrong guesses are throttled process-wide: a handful are free, then each further attempt must
//! wait an exponentially growing interval after the last failure. Clients behind the LAN
//! forwarder or a Kubernetes Service all share one source address, so the throttle is global
//! rather than per client; the admin token always works regardless.
//!
//! Everything here is synchronous: hashing costs tens of milliseconds and must run on a
//! blocking thread, never on an async worker.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use parking_lot::Mutex;

pub const MIN_LEN: usize = 8;
pub const MAX_LEN: usize = 1024;

/// Where the hash lives for a server with this data dir.
pub fn path(cfg: &crate::config::Config) -> PathBuf {
    cfg.data("admin.password")
}

/// Why a new password is refused, in words for the UI.
pub fn validate(password: &str) -> Result<(), String> {
    let n = password.chars().count();
    if n < MIN_LEN {
        return Err(format!("use at least {MIN_LEN} characters"));
    }
    if password.len() > MAX_LEN {
        return Err(format!("use at most {MAX_LEN} bytes"));
    }
    if password.trim() != password {
        return Err("remove leading or trailing spaces".to_owned());
    }
    Ok(())
}

/// An Argon2id PHC string (random salt, default parameters).
pub fn hash(password: &str) -> anyhow::Result<String> {
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))
}

/// Whether `password` matches the stored PHC string (constant-time inside argon2).
pub fn verify(password: &str, phc: &str) -> bool {
    if password.len() > MAX_LEN {
        return false;
    }
    PasswordHash::new(phc).is_ok_and(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

/// The stored hash, if a password is set.
pub fn load(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Replace the stored hash atomically (write a 0600 temp file, fsync, rename).
pub fn store(path: &Path, phc: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("password path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = crate::os::with_mode(std::fs::OpenOptions::new().write(true).create(true).truncate(true), 0o600)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(phc.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Turn password sign-in off.
pub fn clear(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Seed the password from `KILN_ADMIN_PASSWORD` when none is set yet (unattended installs that
/// want one). An existing password is never overwritten by the environment.
pub fn seed_from_env(path: &Path) {
    let Ok(pw) = std::env::var("KILN_ADMIN_PASSWORD") else { return };
    if pw.is_empty() || load(path).is_some() {
        return;
    }
    if let Err(why) = validate(&pw) {
        tracing::warn!(reason = %why, "KILN_ADMIN_PASSWORD ignored");
        return;
    }
    match hash(&pw).and_then(|h| store(path, &h)) {
        Ok(()) => tracing::info!("sign-in password set from KILN_ADMIN_PASSWORD"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "cannot store the sign-in password"),
    }
}

/// Failed attempts allowed before throttling starts.
const FREE_FAILURES: u32 = 5;
/// The longest an attempt ever has to wait.
const MAX_WAIT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct ThrottleState {
    failures: u32,
    last_failure: Option<Instant>,
}

/// Process-wide throttle on wrong passwords.
#[derive(Default)]
pub struct Throttle {
    state: Mutex<ThrottleState>,
}

impl Throttle {
    fn required_wait(failures: u32) -> Duration {
        if failures < FREE_FAILURES {
            return Duration::ZERO;
        }
        let exp = (failures - FREE_FAILURES).min(16);
        Duration::from_secs(1u64 << exp).min(MAX_WAIT)
    }

    /// `Err(remaining)` while attempts are paused.
    pub fn check(&self) -> Result<(), Duration> {
        let st = self.state.lock();
        let wait = Self::required_wait(st.failures);
        match st.last_failure {
            Some(at) if at.elapsed() < wait => Err(wait.saturating_sub(at.elapsed())),
            _ => Ok(()),
        }
    }

    pub fn failed(&self) {
        let mut st = self.state.lock();
        st.failures = st.failures.saturating_add(1);
        st.last_failure = Some(Instant::now());
    }

    pub fn succeeded(&self) {
        *self.state.lock() = ThrottleState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_verify_store_clear() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("admin.password");
        assert!(load(&p).is_none());
        let h = hash("correct horse battery")?;
        assert!(h.starts_with("$argon2id$"));
        store(&p, &h)?;
        let loaded = load(&p).ok_or_else(|| anyhow::anyhow!("not stored"))?;
        assert!(verify("correct horse battery", &loaded));
        assert!(!verify("correct horse batter", &loaded));
        assert!(!verify("", &loaded));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        }
        clear(&p)?;
        clear(&p)?;
        assert!(load(&p).is_none());
        Ok(())
    }

    #[test]
    fn validation() {
        assert!(validate("short").is_err());
        assert!(validate(" padded password ").is_err());
        assert!(validate("long enough").is_ok());
        assert!(validate(&"x".repeat(MAX_LEN + 1)).is_err());
    }

    #[test]
    fn throttle_backs_off_then_resets() {
        let t = Throttle::default();
        for _ in 0..FREE_FAILURES {
            assert!(t.check().is_ok());
            t.failed();
        }
        // The sixth wrong guess has to wait.
        assert!(t.check().is_err());
        t.succeeded();
        assert!(t.check().is_ok());
        assert_eq!(Throttle::required_wait(FREE_FAILURES + 30), MAX_WAIT);
    }
}
