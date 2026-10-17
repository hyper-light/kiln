//! GitHub's cache rules (docs/ci.md W08-R06, W08-R14): who may restore and save, which entry a
//! restore gets, when a save is refused, and what eviction removes. The server's cache services
//! carry these out.

use serde::{Deserialize, Serialize};

/// What a leg may do with the cache (`cache-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Restore only.
    Read,
    /// Restore and save.
    Write,
    /// Save only.
    WriteOnly,
    /// Neither.
    None,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "read" => Some(Mode::Read),
            "write" => Some(Mode::Write),
            "write-only" => Some(Mode::WriteOnly),
            "none" => Some(Mode::None),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::Write => "write",
            Mode::WriteOnly => "write-only",
            Mode::None => "none",
        }
    }

    pub fn restores(self) -> bool {
        matches!(self, Mode::Read | Mode::Write)
    }

    pub fn saves(self) -> bool {
        matches!(self, Mode::Write | Mode::WriteOnly)
    }

    /// Whether this mode asks for nothing `cap` withholds: a called workflow's mode under its
    /// caller's (W08-R14).
    pub fn within(self, cap: Mode) -> bool {
        (!self.restores() || cap.restores()) && (!self.saves() || cap.saves())
    }
}

/// Events trusted to save caches when no mode is set: GitHub's, and kiln's `land` and `deploy`.
pub const TRUSTED_EVENTS: &[&str] = &["push", "workflow_dispatch", "repository_dispatch", "delete", "registry_package", "page_build", "schedule", "land", "deploy"];

/// A leg's mode (W08-R14). The job's mode wins over the workflow's. With neither set, trusted
/// events write. Any other event whose run is on the default branch only reads, so a low-trust
/// run can't poison the default branch's caches. Runs on any other ref, such as a pull request's
/// merge ref, write to their own scope.
pub fn effective(job: Option<Mode>, workflow: Option<Mode>, event: &str, on_default_branch: bool) -> Mode {
    if let Some(m) = job.or(workflow) {
        return m;
    }
    if TRUSTED_EVENTS.contains(&event) || !on_default_branch {
        Mode::Write
    } else {
        Mode::Read
    }
}

/// The scopes a run restores from, in order (W08-R06): its own ref, then a pull request's base
/// branch, then the default branch.
pub fn scopes(git_ref: &str, base_ref: Option<&str>, default_branch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let branch = |b: &str| if b.starts_with("refs/") { b.to_owned() } else { format!("refs/heads/{b}") };
    for s in [Some(git_ref.to_owned()), base_ref.filter(|b| !b.is_empty()).map(branch), Some(branch(default_branch))].into_iter().flatten() {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// A stored cache entry, as the rules see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub scope: String,
    pub key: String,
    pub version: String,
    pub size: u64,
    pub created_at: i64,
    pub last_used_at: i64,
    /// Finalized (not still uploading).
    pub ready: bool,
}

/// Why keys are refused, as the toolkit words it.
pub fn check_keys(keys: &[String]) -> Result<(), String> {
    if keys.is_empty() {
        return Err("Key Validation Error: at least one key is required.".into());
    }
    if keys.len() > 10 {
        return Err("Key Validation Error: Keys are limited to a maximum of 10.".into());
    }
    for k in keys {
        if k.chars().count() > 512 {
            return Err(format!("Key Validation Error: {k} cannot be larger than 512 characters."));
        }
        if k.contains(',') {
            return Err(format!("Key Validation Error: {k} cannot contain commas."));
        }
    }
    Ok(())
}

/// Keys compare without regard to case, as GitHub's service does (`actions/cache` compares the
/// key it gets back the same way).
fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// The entry a restore gets (W08-R06). For each scope in order, take the keys in order (the
/// primary key, then the restore keys). The first key that names a ready entry of `version`
/// exactly wins; failing that, the newest such entry the key is a prefix of. Whether the result
/// is an exact hit on the primary key is the caller's `cache-hit`.
pub fn lookup<'a>(entries: &'a [Entry], keys: &[String], version: &str, scopes: &[String]) -> Option<&'a Entry> {
    for scope in scopes {
        let here: Vec<&Entry> = entries.iter().filter(|e| e.ready && e.version == version && &e.scope == scope).collect();
        for k in keys {
            let k = fold(k);
            if let Some(e) = here.iter().filter(|e| fold(&e.key) == k).max_by_key(|e| (e.created_at, e.id)) {
                return Some(e);
            }
            if let Some(e) = here.iter().filter(|e| fold(&e.key).starts_with(&k)).max_by_key(|e| (e.created_at, e.id)) {
                return Some(e);
            }
        }
    }
    None
}

/// Whether a save may start (W08-R06): entries are immutable, so a key and version that exist in
/// the scope (ready or still uploading) refuse it.
pub fn may_reserve(entries: &[Entry], scope: &str, key: &str, version: &str) -> bool {
    let k = fold(key);
    !entries.iter().any(|e| e.scope == scope && e.version == version && fold(&e.key) == k)
}

/// The entries eviction removes (W08-R06): those unused for longer than `max_idle_secs`, then the
/// least recently used until the rest fit in `quota` bytes. Uploads in progress are left alone.
pub fn evict(entries: &[Entry], now: i64, max_idle_secs: i64, quota: u64) -> Vec<i64> {
    let mut gone: Vec<i64> = Vec::new();
    let mut kept: Vec<&Entry> = Vec::new();
    for e in entries.iter().filter(|e| e.ready) {
        let used = e.last_used_at.max(e.created_at);
        if now.saturating_sub(used) > max_idle_secs {
            gone.push(e.id);
        } else {
            kept.push(e);
        }
    }
    let mut total: u64 = kept.iter().map(|e| e.size).sum::<u64>() + entries.iter().filter(|e| !e.ready).map(|e| e.size).sum::<u64>();
    kept.sort_by_key(|e| (e.last_used_at.max(e.created_at), e.id));
    for e in kept {
        if total <= quota {
            break;
        }
        total = total.saturating_sub(e.size);
        gone.push(e.id);
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: i64, scope: &str, key: &str, created_at: i64) -> Entry {
        Entry { id, scope: scope.into(), key: key.into(), version: "v1".into(), size: 100, created_at, last_used_at: created_at, ready: true }
    }

    fn keys(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn w08_r06_exact_then_prefix_then_restore_keys_newest_first() {
        let main = "refs/heads/main";
        let all = vec![e(1, main, "npm-linux-aaa", 10), e(2, main, "npm-linux-bbb", 20), e(3, main, "npm-mac-ccc", 30), e(4, main, "npm-linux-aaa-old", 5)];
        let sc = vec![main.to_owned()];
        // The primary key exactly.
        assert_eq!(lookup(&all, &keys(&["npm-linux-aaa", "npm-"]), "v1", &sc).map(|e| e.id), Some(1));
        // The primary key as a prefix: the newest.
        assert_eq!(lookup(&all, &keys(&["npm-linux"]), "v1", &sc).map(|e| e.id), Some(2));
        // A restore key, exact first, else as a prefix.
        assert_eq!(lookup(&all, &keys(&["npm-linux-zzz", "npm-mac-ccc"]), "v1", &sc).map(|e| e.id), Some(3));
        assert_eq!(lookup(&all, &keys(&["npm-linux-zzz", "npm-"]), "v1", &sc).map(|e| e.id), Some(3));
        // Case doesn't matter.
        assert_eq!(lookup(&all, &keys(&["NPM-LINUX-AAA"]), "v1", &sc).map(|e| e.id), Some(1));
        // The version must match; unfinished entries don't count.
        assert_eq!(lookup(&all, &keys(&["npm-linux-aaa"]), "v2", &sc), None);
        let mut uploading = all.clone();
        uploading.iter_mut().for_each(|x| x.ready = false);
        assert_eq!(lookup(&uploading, &keys(&["npm-"]), "v1", &sc), None);
    }

    #[test]
    fn w08_r06_scopes_fall_back_from_the_ref_to_the_base_then_the_default_branch() {
        let sc = scopes("refs/pull/7/merge", Some("release"), "main");
        assert_eq!(sc, vec!["refs/pull/7/merge", "refs/heads/release", "refs/heads/main"]);
        assert_eq!(scopes("refs/heads/main", None, "main"), vec!["refs/heads/main"]);
        let all = vec![e(1, "refs/heads/main", "k-1", 10), e(2, "refs/heads/release", "k-2", 5), e(3, "refs/heads/other", "k-3", 50)];
        // The base branch's entry wins over the default branch's, whatever their ages; another
        // branch's is never seen.
        assert_eq!(lookup(&all, &keys(&["k-"]), "v1", &sc).map(|e| e.id), Some(2));
        let feature = scopes("refs/heads/feature", None, "main");
        assert_eq!(lookup(&all, &keys(&["k-"]), "v1", &feature).map(|e| e.id), Some(1));
        // A scope is searched in full (every key) before the next.
        let both = vec![e(1, "refs/heads/feature", "other-key", 1), e(2, "refs/heads/main", "exact", 99)];
        assert_eq!(lookup(&both, &keys(&["exact", "other"]), "v1", &feature).map(|e| e.id), Some(1));
    }

    #[test]
    fn w08_r06_entries_are_immutable_and_keys_validated() {
        let all = vec![e(1, "refs/heads/main", "k", 1)];
        assert!(!may_reserve(&all, "refs/heads/main", "K", "v1"));
        assert!(may_reserve(&all, "refs/heads/main", "k", "v2"));
        assert!(may_reserve(&all, "refs/heads/dev", "k", "v1"));
        assert!(check_keys(&keys(&["a", "b"])).is_ok());
        assert!(check_keys(&keys(&["a,b"])).is_err());
        assert!(check_keys(&keys(&["k"; 11])).is_err());
        assert!(check_keys(&[String::from("x").repeat(513)]).is_err());
        assert!(check_keys(&[]).is_err());
    }

    #[test]
    fn w08_ac07_eviction_by_idleness_then_least_recently_used() {
        let day = 86_400;
        let mut all = vec![e(1, "s", "a", 0), e(2, "s", "b", 0), e(3, "s", "c", 0), e(4, "s", "d", 0)];
        all.iter_mut().zip([10 * day, 2 * day, 3 * day, 4 * day]).for_each(|(x, t)| x.last_used_at = t);
        // Entry 1 was used 0.5 days before "now"; the others longer. With 7 days of idleness
        // allowed, none expire; over a 250-byte quota the two least recently used go.
        let now = 10 * day + day / 2;
        assert_eq!(evict(&all, now, 7 * day, 250), vec![2, 3]);
        // Seven days later everything but entry 1 has expired.
        assert_eq!(evict(&all, 4 * day + 7 * day + 1, 7 * day, u64::MAX), vec![2, 3, 4]);
        // An upload in progress is never evicted, but counts against the quota.
        all.push(Entry { ready: false, size: 1000, ..e(5, "s", "e", now) });
        assert_eq!(evict(&all, now, 7 * day, 1100), vec![2, 3, 4]);
        assert_eq!(evict(&all, now, 7 * day, 1050), vec![2, 3, 4, 1]);
    }

    #[test]
    fn w08_r14_modes_by_event_and_caps() {
        assert_eq!(effective(None, None, "push", true), Mode::Write);
        assert_eq!(effective(None, None, "schedule", true), Mode::Write);
        assert_eq!(effective(None, None, "pull_request_target", true), Mode::Read);
        assert_eq!(effective(None, None, "issue_comment", true), Mode::Read);
        assert_eq!(effective(None, None, "workflow_run", true), Mode::Read);
        // pull_request runs write to their merge ref.
        assert_eq!(effective(None, None, "pull_request", false), Mode::Write);
        // kiln's own events.
        assert_eq!(effective(None, None, "land", true), Mode::Write);
        assert_eq!(effective(None, None, "agent", true), Mode::Read);
        // An explicit mode wins; the job's over the workflow's.
        assert_eq!(effective(Some(Mode::None), Some(Mode::Write), "push", true), Mode::None);
        assert_eq!(effective(None, Some(Mode::Write), "issue_comment", true), Mode::Write);
        // Caps for called workflows.
        assert!(Mode::Read.within(Mode::Write));
        assert!(!Mode::Write.within(Mode::Read));
        assert!(!Mode::Read.within(Mode::WriteOnly));
        assert!(Mode::None.within(Mode::None));
        assert!(Mode::WriteOnly.within(Mode::Write));
        for m in [Mode::Read, Mode::Write, Mode::WriteOnly, Mode::None] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
    }
}
