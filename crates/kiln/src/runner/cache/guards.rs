//! U07 / W08-R09: exact operation liveness, independent of authorization/history.
//!
//! All lock inodes are permanent. Separate headers remain readable on Windows while the
//! ownership file is locked. Only the root mutation owner allocates or reads these slots.
//! The limit is 64 simultaneous protected operations; saturation is terminal GuardCapacity.
//! Clones retain OS ownership through dispatched IO, including caller cancellation. Death
//! releases ownership; stale headers never establish liveness.

use std::collections::{BTreeSet, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::util::FileLock;

const SLOTS: usize = 64;
// More than the CDC chunk count of a 16 GiB object. Oversized operations decline explicitly.
const MAX_DIGESTS: usize = 65_536;
const HEADER: &str = "kiln-u07-active-v1\n";
const NATIVE_HEADER: &str = "kiln-u07-active-v2\n";
const CDC_HEADER: &str = "kiln-u07-active-v3\n";
const MAX_HEADER: usize = NATIVE_HEADER.len() + 40 + MAX_DIGESTS * 65;

#[derive(Clone, Debug)]
pub(super) struct Mutation {
    root: PathBuf,
    _file: Arc<FileLock>,
}

impl Mutation {
    pub(super) fn acquire(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let file = open_lock(&root.join(".mutation.lock"))?;
        match FileLock::try_lock(file) {
            Ok(file) => Ok(Self { root: root.to_path_buf(), _file: Arc::new(file) }),
            Err(TryLockError::WouldBlock) => Err(busy()),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    fn check(&self, root: &Path) -> io::Result<()> {
        if self.root == root { Ok(()) }
        else { Err(io::Error::new(io::ErrorKind::InvalidInput, "U07 mutation belongs to another physical root")) }
    }
}

#[derive(Clone, Debug)]
pub struct Guard(Arc<Held>);

#[derive(Debug)]
struct Held {
    root: PathBuf,
    digests: HashSet<String>,
    intent: Option<String>,
    slot: usize,
    _file: FileLock,
}

impl Guard {
    /// Nested reads reuse protection rather than allocating another operation slot.
    pub(crate) fn covers(&self, root: &Path, digest: &str) -> bool {
        self.0.root == root && self.0.digests.contains(digest)
    }

    pub(super) fn owns_intent(&self, root: &Path, intent: &str) -> bool {
        self.0.root == root && self.0.intent.as_deref() == Some(intent)
    }
}

#[derive(Default)]
pub(super) struct Activity {
    pub(super) digests: HashSet<String>,
    pub(super) intents: HashSet<String>,
}

#[derive(Debug)]
pub(crate) struct GuardCapacity;

impl std::fmt::Display for GuardCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("U07 physical cache protection capacity exhausted (64 active operations)")
    }
}

impl std::error::Error for GuardCapacity {}

fn busy() -> io::Error {
    // Keep the synthesized (no raw OS code) classification used by physical Busy retry.
    io::Error::new(io::ErrorKind::WouldBlock, "U07 physical cache operation is busy")
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)
}

fn valid(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn protect(root: &Path, mutation: &Mutation, digests: &[String]) -> io::Result<Guard> {
    protect_plan(root, mutation, digests, None, HEADER)
}

pub(super) fn protect_intent(root: &Path, mutation: &Mutation, digest: &str, intent: &str) -> io::Result<Guard> {
    if !intent_valid(intent) { return Err(malformed()); }
    protect_plan(root, mutation, &[digest.to_owned()], Some(intent), NATIVE_HEADER)
}

/// U07/ctl-10: CDC uses the same exact OS slot through verification, I/O and SQL.
pub(super) fn protect_cdc(root: &Path, mutation: &Mutation, digests: &[String], intent: &str) -> io::Result<Guard> {
    if !intent_valid(intent) { return Err(malformed()); }
    protect_plan(root, mutation, digests, Some(intent), CDC_HEADER)
}

pub(super) fn intent_valid(intent: &str) -> bool {
    intent.len() == 32 && intent.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn protect_plan(root: &Path, mutation: &Mutation, digests: &[String], intent: Option<&str>, prefix: &str) -> io::Result<Guard> {
    mutation.check(root)?;
    if digests.len() > MAX_DIGESTS || digests.iter().any(|digest| !valid(digest)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "U07 invalid or excessive physical protection set"));
    }
    let unique: BTreeSet<_> = digests.iter().collect();
    let mut encoded = String::with_capacity(HEADER.len() + unique.len() * 65);
    encoded.push_str(prefix);
    if let Some(intent) = intent { encoded.push_str("intent:"); encoded.push_str(intent); encoded.push('\n'); }
    for digest in unique {
        encoded.push_str(digest);
        encoded.push('\n');
    }
    let dir = root.join(".activity");
    std::fs::create_dir_all(&dir)?;
    for slot in 0..SLOTS {
        let file = match FileLock::try_lock(open_lock(&dir.join(format!("{slot}.lock")))?) {
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error),
            Ok(file) => file,
        };
        // Mutation remains held until the complete header is visible. Failure drops slot
        // ownership before any consumer receives a guard; stale partial data is ignored.
        let mut header = OpenOptions::new().create(true).truncate(true).write(true).open(dir.join(format!("{slot}.header")))?;
        header.write_all(encoded.as_bytes())?;
        header.flush()?;
        return Ok(Guard(Arc::new(Held {
            root: root.to_path_buf(), digests: digests.iter().cloned().collect(), intent: intent.map(str::to_owned), slot, _file: file,
        })));
    }
    Err(io::Error::other(GuardCapacity))
}

pub(super) fn protected(root: &Path, mutation: &Mutation) -> io::Result<HashSet<String>> {
    Ok(activity(root, mutation, None)?.digests)
}

pub(super) fn activity(root: &Path, mutation: &Mutation, except: Option<&Guard>) -> io::Result<Activity> {
    mutation.check(root)?;
    if except.is_some_and(|guard| guard.0.root != root) { return Err(malformed()); }
    let dir = root.join(".activity");
    std::fs::create_dir_all(&dir)?;
    let mut all = Activity::default();
    for slot in 0..SLOTS {
        if except.is_some_and(|guard| guard.0.slot == slot) { continue; }
        let file = open_lock(&dir.join(format!("{slot}.lock")))?;
        match FileLock::try_lock(file) {
            Ok(_lock) => continue, // Exclusive ownership proves any stale header inactive.
            Err(TryLockError::Error(error)) => return Err(error),
            Err(TryLockError::WouldBlock) => {}
        }
        let mut bytes = Vec::new();
        File::open(dir.join(format!("{slot}.header")))?.take((MAX_HEADER + 1) as u64).read_to_end(&mut bytes)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
        let intent_body = text.strip_prefix(CDC_HEADER).map(|body| (body, false))
            .or_else(|| text.strip_prefix(NATIVE_HEADER).map(|body| (body, true)));
        let body = if let Some((body, native)) = intent_body {
            let (intent, body) = body.split_once('\n').ok_or_else(malformed)?;
            let intent = intent.strip_prefix("intent:").filter(|intent| intent_valid(intent)).ok_or_else(malformed)?;
            if native && body.lines().count() != 1 { return Err(malformed()); }
            all.intents.insert(intent.to_owned());
            body
        } else { text.strip_prefix(HEADER).ok_or_else(malformed)? };
        if bytes.len() > MAX_HEADER || (!body.is_empty() && !body.ends_with('\n')) { return Err(malformed()); }
        for digest in body.lines() {
            if !valid(digest) { return Err(malformed()); }
            all.digests.insert(digest.to_owned());
        }
    }
    Ok(all)
}

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "U07 active physical protection header is malformed; eviction refused")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Root(PathBuf);
    impl Root {
        fn new() -> io::Result<Self> {
            let path = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
                .ok_or_else(|| io::Error::other("home unavailable"))?)
                .join(".cache/kiln-u07-guards").join(crate::util::new_id());
            std::fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
    }
    impl Drop for Root {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn u07_distinct_handles_exact_liveness_and_clone_release() -> io::Result<()> {
        let root = Root::new()?;
        let digest = "a".repeat(64);
        let mutation = Mutation::acquire(&root.0)?;
        let guard = protect(&root.0, &mutation, &[digest.clone(), digest.clone()])?;
        let clone = guard.clone();
        assert_eq!(protected(&root.0, &mutation)?, HashSet::from([digest.clone()]));
        assert!(Mutation::acquire(&root.0).is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock && error.raw_os_error().is_none()));
        drop(mutation);
        drop(guard);
        let mutation = Mutation::acquire(&root.0)?;
        assert!(protected(&root.0, &mutation)?.contains(&digest));
        drop(clone);
        assert!(protected(&root.0, &mutation)?.is_empty());
        let intent = crate::util::new_id();
        let native = protect_intent(&root.0, &mutation, &digest, &intent)?;
        assert!(native.owns_intent(&root.0, &intent));
        assert_eq!(activity(&root.0, &mutation, None)?.intents, HashSet::from([intent]));
        assert!(activity(&root.0, &mutation, Some(&native))?.digests.is_empty());
        drop(native);
        assert!(activity(&root.0, &mutation, None)?.intents.is_empty());
        Ok(())
    }

    #[test]
    fn u07_active_malformed_header_refuses_eviction_stale_header_ignored() -> io::Result<()> {
        let root = Root::new()?;
        let mutation = Mutation::acquire(&root.0)?;
        let guard = protect(&root.0, &mutation, &["b".repeat(64)])?;
        std::fs::write(root.0.join(".activity/0.header"), "partial")?;
        assert!(protected(&root.0, &mutation).is_err_and(|error| error.kind() == io::ErrorKind::InvalidData));
        drop(guard);
        assert!(protected(&root.0, &mutation)?.is_empty());
        Ok(())
    }

    #[test]
    fn u07_slot_capacity_terminal_and_clones_reuse_exact_protection() -> io::Result<()> {
        let root = Root::new()?;
        let mutation = Mutation::acquire(&root.0)?;
        let digest = "d".repeat(64);
        let mut guards = Vec::new();
        for _ in 0..SLOTS { guards.push(protect(&root.0, &mutation, std::slice::from_ref(&digest))?); }
        let original = guards.pop().ok_or_else(|| io::Error::other("guard missing"))?;
        let clone = original.clone();
        assert!(clone.covers(&root.0, &digest));
        assert!(!clone.covers(&root.0, &"e".repeat(64)));
        assert!(!clone.covers(&root.0.join("other"), &digest));
        drop(original);
        let error = protect(&root.0, &mutation, std::slice::from_ref(&digest)).err()
            .ok_or_else(|| io::Error::other("saturated slots admitted operation"))?;
        assert_ne!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.get_ref().is_some_and(|source| source.is::<GuardCapacity>()));
        drop(clone);
        let _replacement = protect(&root.0, &mutation, &[digest])?;
        Ok(())
    }

    #[test]
    fn u07_crossprocess_header_readability_and_crash_release() -> io::Result<()> {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};
        let root = Root::new()?;
        let mut child = Command::new(std::env::current_exe()?)
            .args(["u07_guard_child", "--nocapture"])
            .env("KILN_U07_GUARD_CHILD_ROOT", &root.0)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let result = (|| {
            let stdout = child.stdout.take().ok_or_else(|| io::Error::other("child stdout absent"))?;
            let mut ready = false;
            for line in io::BufReader::new(stdout).lines() {
                if line?.contains("U07-GUARD-READY") { ready = true; break; }
            }
            if !ready { return Err(io::Error::other("child did not acquire guard")); }
            let mutation = Mutation::acquire(&root.0)?;
            assert_eq!(protected(&root.0, &mutation)?, HashSet::from(["c".repeat(64)]));
            Ok(())
        })();
        // Kill only the child started above: OS death, rather than Rust drop, releases ownership.
        let killed = child.kill();
        let waited = child.wait();
        result?;
        killed?;
        waited?;
        let mutation = Mutation::acquire(&root.0)?;
        assert!(protected(&root.0, &mutation)?.is_empty());
        Ok(())
    }

    #[test]
    fn u07_guard_child() -> io::Result<()> {
        let Some(root) = std::env::var_os("KILN_U07_GUARD_CHILD_ROOT") else { return Ok(()) };
        let root = PathBuf::from(root);
        let mutation = Mutation::acquire(&root)?;
        let _guard = protect(&root, &mutation, &["c".repeat(64)])?;
        drop(mutation);
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"U07-GUARD-READY\n")?;
        stdout.flush()?;
        let mut finish = [0_u8; 1];
        let _ = io::stdin().read(&mut finish)?;
        Ok(())
    }
}
