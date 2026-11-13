//! A capacity holder's scope as the host that keeps the machine's view reads it (joint plan step 13,
//! `docs/design/capacity-view.md` CV12): proved its holder's user's, then what the work in it holds of the memory the
//! host measures free, for the room's credit. Each OS reads the kind it accounts: a cgroup v2 directory on Linux, a
//! process group on macOS, a job object on Windows; any other kind, or a scope not proved the holder's, earns no
//! credit and is refused, typed.

use hex_api::capacity::{Amounts, Read, Scope, ScopeRefused};

/// A holder's user as the person declared it in the host's configuration: a uid on Linux and macOS, its account's
/// SID (`S-1-...`) on Windows. Nothing the holder says can name it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct User(pub String);

impl User {
    /// Whether this OS can take it as a user, refused, typed, if not: a uid on Linux and macOS, a SID on Windows.
    pub fn check(&self) -> Result<(), hex_api::Refusal> {
        #[cfg(unix)]
        let ok = self.0.parse::<u32>().is_ok();
        #[cfg(windows)]
        let ok = sid_bytes(&self.0).is_some();
        #[cfg(not(any(unix, windows)))]
        let ok = false;
        if ok {
            Ok(())
        } else {
            Err(hex_api::Refusal::Invalid {
                what: "capacity holder's user",
                why: format!(
                    "{:?} is no uid (Linux, macOS) or SID (Windows) of this OS",
                    self.0
                ),
            })
        }
    }
}

/// What the work in `scope` holds now, once the scope is proved `user`'s: none declared, no credit.
pub fn used(scope: &Scope, user: Option<&User>) -> Result<Amounts, ScopeRefused> {
    let user = user.ok_or(ScopeRefused::NoUser)?;
    here::used(scope, user).map(|mem_bytes| Amounts {
        mem_bytes,
        ..Amounts::ZERO
    })
}

/// Each holder's scope, by its link, read on both sides of `between` (the host's read of its free memory), the
/// smaller kept: a use that moved while free memory was read is credited at its least (CV12).
pub fn around<T>(
    holders: &[(u64, Scope, Option<User>)],
    between: impl FnOnce() -> T,
) -> (T, Vec<(u64, Read)>) {
    let before: Vec<_> = holders
        .iter()
        .map(|(_, scope, user)| used(scope, user.as_ref()))
        .collect();
    let read = between();
    let sample = holders
        .iter()
        .zip(before)
        .map(|((link, scope, user), before)| {
            let after = used(scope, user.as_ref());
            (*link, before.and_then(|b| after.map(|a| b.min(a))))
        })
        .collect();
    (read, sample)
}

/// The cgroup v2 directory `path` under the unified hierarchy at `root`, proved `uid`'s and read: its
/// `memory.current`. Proved where some directory from it up to the root, the root not counted, is owned by `uid`:
/// a tree the user owns, or one delegated to it (the kernel's cgroup-v2 "Delegation": the delegatee is given the
/// subtree's directory). A path that is empty or leaves the hierarchy is invalid.
#[cfg(all(unix, any(target_os = "linux", test)))]
pub(crate) fn cgroup(root: &std::path::Path, path: &str, uid: u32) -> Result<u64, ScopeRefused> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};

    let relative = Path::new(path);
    if path.is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(ScopeRefused::Invalid);
    }
    let dir = root.join(relative);
    let mut at = dir.clone();
    // Bounded by the path's own components: each step pops one.
    let owned = loop {
        if at == root || !at.starts_with(root) {
            break false;
        }
        let meta = std::fs::symlink_metadata(&at).map_err(|_| ScopeRefused::Unreadable)?;
        if !meta.is_dir() {
            return Err(ScopeRefused::Invalid);
        }
        if meta.uid() == uid {
            break true;
        }
        if !at.pop() {
            break false;
        }
    };
    if !owned {
        return Err(ScopeRefused::NotOwned);
    }
    // A u64 in decimal and its newline: the longest `memory.current` the kernel writes.
    let most = u64::from(u64::MAX.ilog10()).saturating_add(2);
    let mut text = String::new();
    std::fs::File::open(dir.join("memory.current"))
        .and_then(|file| file.take(most).read_to_string(&mut text))
        .map_err(|_| ScopeRefused::Unreadable)?;
    text.trim().parse().map_err(|_| ScopeRefused::Unreadable)
}

/// Whether `owner`, a SID's binary form, is the SID `declared` names in its string form (`S-1-<authority>-<sub>...`,
/// MS-DTYP 2.4.2.1): compared byte for byte, so a job an elevated holder made, which BUILTIN\Administrators owns
/// (S-1-5-32-544), is not its account's, and earns no credit.
#[cfg(any(windows, test))]
pub(crate) fn sid_is(owner: &[u8], declared: &str) -> bool {
    sid_bytes(declared).is_some_and(|sid| sid == owner)
}

/// A SID's binary form from its string form (MS-DTYP 2.4.2.1 and 2.4.2.2): revision 1, the count of
/// sub-authorities (at most 15, SID_MAX_SUB_AUTHORITIES), the identifier authority in six bytes big-endian (decimal
/// below 2^32, else `0x` and twelve hex digits), then each sub-authority as four bytes little-endian.
#[cfg(any(windows, test))]
fn sid_bytes(text: &str) -> Option<Vec<u8>> {
    const MOST_SUB_AUTHORITIES: usize = 15;
    let rest = text
        .strip_prefix("S-1-")
        .or_else(|| text.strip_prefix("s-1-"))?;
    let mut parts = rest.split('-');
    let authority = parts.next()?;
    let authority: u64 = match authority
        .strip_prefix("0x")
        .or_else(|| authority.strip_prefix("0X"))
    {
        Some(hex) if hex.len() == 12 => u64::from_str_radix(hex, 16).ok()?,
        Some(_) => return None,
        None => u64::from(authority.parse::<u32>().ok()?),
    };
    let subs: Vec<u32> = parts.map(|p| p.parse().ok()).collect::<Option<_>>()?;
    if subs.is_empty() || subs.len() > MOST_SUB_AUTHORITIES {
        return None;
    }
    let mut sid = vec![1u8, u8::try_from(subs.len()).ok()?];
    sid.extend_from_slice(authority.to_be_bytes().get(2..)?);
    for sub in subs {
        sid.extend_from_slice(&sub.to_le_bytes());
    }
    Some(sid)
}

#[cfg(target_os = "linux")]
mod here {
    use hex_api::capacity::{Scope, ScopeRefused};

    use super::User;

    /// The unified hierarchy's mount, where a cgroup v2 path is read.
    const ROOT: &str = "/sys/fs/cgroup";

    pub(super) fn used(scope: &Scope, user: &User) -> Result<u64, ScopeRefused> {
        let Scope::Cgroup(path) = scope else {
            return Err(ScopeRefused::NotThisPlatform);
        };
        let uid = user.0.parse().map_err(|_| ScopeRefused::NoUser)?;
        super::cgroup(std::path::Path::new(ROOT), path, uid)
    }
}

#[cfg(target_os = "macos")]
mod here {
    use hex_api::capacity::{Scope, ScopeRefused};
    use hex_runtime::macos::sys;

    use super::User;

    /// The members of process group `pgid`, each proved to run as `uid`, and their footprints summed (the memory
    /// XNU charges each, which the host's free memory lacks). Listed with room for every process the kernel holds
    /// (`kern.maxproc`), so the listing is whole. A member that ended since the listing holds nothing.
    pub(super) fn used(scope: &Scope, user: &User) -> Result<u64, ScopeRefused> {
        let Scope::Group(pgid) = scope else {
            return Err(ScopeRefused::NotThisPlatform);
        };
        if *pgid <= 0 {
            return Err(ScopeRefused::Invalid);
        }
        let uid: u32 = user.0.parse().map_err(|_| ScopeRefused::NoUser)?;
        let mut pids = sys::listing().map_err(|_| ScopeRefused::Unreadable)?;
        sys::group(*pgid, &mut pids).map_err(|_| ScopeRefused::Unreadable)?;
        if pids.is_empty() {
            return Err(ScopeRefused::Unreadable);
        }
        let mut sum = 0u64;
        for pid in pids {
            match sys::user_of(pid) {
                Ok(Some(runs_as)) if runs_as == uid => {}
                Ok(Some(_)) => return Err(ScopeRefused::NotOwned),
                Ok(None) => continue,
                Err(_) => return Err(ScopeRefused::Unreadable),
            }
            match sys::rusage(pid) {
                Ok(Some(rusage)) => {
                    sum = sum
                        .checked_add(rusage.footprint)
                        .ok_or(ScopeRefused::Unreadable)?;
                }
                Ok(None) => continue,
                Err(_) => return Err(ScopeRefused::Unreadable),
            }
        }
        Ok(sum)
    }
}

#[cfg(windows)]
mod here {
    use hex_api::capacity::{Scope, ScopeRefused};
    use hex_runtime::windows::sys::Job;

    use super::User;

    /// The job named `name`, proved owned by the holder's account (its descriptor's owner is the declared SID), and
    /// its processes' private resident memory summed.
    pub(super) fn used(scope: &Scope, user: &User) -> Result<u64, ScopeRefused> {
        let Scope::Job(name) = scope else {
            return Err(ScopeRefused::NotThisPlatform);
        };
        if super::sid_bytes(&user.0).is_none() {
            return Err(ScopeRefused::NoUser);
        }
        let job = match Job::open_to_read(name) {
            Ok(Some(job)) => job,
            Ok(None) => return Err(ScopeRefused::Unreadable),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                return Err(ScopeRefused::Invalid);
            }
            Err(_) => return Err(ScopeRefused::Unreadable),
        };
        let owner = job.owner().map_err(|_| ScopeRefused::Unreadable)?;
        if !super::sid_is(&owner, &user.0) {
            return Err(ScopeRefused::NotOwned);
        }
        job.private_resident().map_err(|_| ScopeRefused::Unreadable)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod here {
    use hex_api::capacity::{Scope, ScopeRefused};

    use super::User;

    pub(super) fn used(_: &Scope, _: &User) -> Result<u64, ScopeRefused> {
        Err(ScopeRefused::NotThisPlatform)
    }
}

#[cfg(test)]
mod tests;
