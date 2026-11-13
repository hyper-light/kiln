//! A passthrough function that no reset clears between VMs ([`super::vfio::once_per_boot`]) goes to one VM per host
//! boot (AC-T7.4.1; docs/design/firecracker-gpu-host.md): the next VM given it would boot into the last one's state, as
//! GPU run r940's did ("PSP create ring failed"). Its mark names the boot session a VM was given it on, in the node's
//! runtime directory, the one every node of its user on the host shares ([`MarksAt`]): written durably before that
//! VM's VMM opens it, it refuses the function to every later VM of that boot, any node's, and a later boot's session
//! admits it again. Only a node whose runtime directory is that shared one, and whose user owns the function's VFIO
//! group, gives it out ([`Spent::Unshared`], [`Spent::Foreign`]): so every node that can give it out reads one
//! directory. What clears a function at a VM's end, vfio-pci's reset or nothing, is the kernel's alone to know: it
//! tells no process (pci.c logs a reset's outcome at debug level only).

use super::native::path;
use super::{exhausted, invalid};
use hex_api::{BackingKey, Refusal};
use hex_core::record::BootSession;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The marks' directory in the node's runtime directory, a name no container's or record directory takes there (theirs
/// are `<key>-<id>` and `<key>.records`).
pub(super) const MARKS: &str = "vfio-single-use";

/// Where a node keeps its single-use marks ([`Marks`]): its runtime directory, `dir`, and, where the other nodes on the
/// host don't read that directory, why (`unshared`): a rootless node with no `$XDG_RUNTIME_DIR` keeps it in its own
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarksAt {
    pub dir: PathBuf,
    pub unshared: Option<String>,
}

/// Why a function that no reset clears can't go to a VM now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Spent {
    /// A VM was given it on this boot.
    ThisBoot,
    /// This boot's session can't be read, so no mark can be told from this boot's.
    BootUnknown,
    /// The node's mark would lie where the host's other nodes don't read it: its runtime directory and why.
    Unshared { dir: PathBuf, why: String },
    /// Its VFIO group's interface belongs to another user, whose nodes mark it in their own runtime directory.
    Foreign { owner: u32, uid: u32 },
}

/// The core: whether a function no reset clears, last given to a VM on boot `mark` (`None`: never, as its marks
/// go), may go to a VM on boot `now`.
pub(super) fn admit(mark: Option<&BootSession>, now: Option<&BootSession>) -> Result<(), Spent> {
    match (now, mark) {
        (None, _) => Err(Spent::BootUnknown),
        (Some(now), Some(mark)) if now == mark => Err(Spent::ThisBoot),
        (Some(_), _) => Ok(()),
    }
}

/// The marks in a node's runtime directory on boot `now`: the shell around [`admit`].
pub(super) struct Marks<'a> {
    dir: PathBuf,
    now: Option<&'a BootSession>,
}

impl<'a> Marks<'a> {
    pub(super) fn of(runtime: &Path, now: Option<&'a BootSession>) -> Result<Self, Refusal> {
        Ok(Self {
            dir: path(runtime, MARKS)?,
            now,
        })
    }

    /// The first of `functions` (SBDFs) that can't go to a VM now, and why.
    pub(super) fn check<'f>(
        &self,
        functions: &[&'f str],
    ) -> Result<Option<(&'f str, Spent)>, Refusal> {
        for &function in functions {
            if let Err(spent) = admit(self.read(function)?.as_ref(), self.now) {
                return Ok(Some((function, spent)));
            }
        }
        Ok(None)
    }

    /// Marks `functions` given to a VM on this boot, durably, before the VMM that opens them starts: a crash past
    /// here forgets none.
    pub(super) fn spend(&self, functions: &[&str]) -> Result<(), Refusal> {
        if functions.is_empty() {
            return Ok(());
        }
        let now = self
            .now
            .ok_or_else(|| invalid("a single-use function's mark needs the host's boot session"))?;
        match fs::create_dir(&self.dir) {
            Ok(()) => {
                fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                    .map_err(|e| Refusal::os("making the single-use marks' directory", e))?;
                if let Some(runtime) = self.dir.parent() {
                    sync(runtime)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Refusal::os("making the single-use marks' directory", e)),
        }
        for function in functions {
            let temporary = path(&self.dir, &temporary(function)?)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)
                .map_err(|e| Refusal::os("writing a single-use function's mark", e))?;
            file.write_all(now.as_str().as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|e| Refusal::os("writing a single-use function's mark", e))?;
            fs::rename(&temporary, path(&self.dir, function)?)
                .map_err(|e| Refusal::os("publishing a single-use function's mark", e))?;
        }
        sync(&self.dir)
    }

    fn read(&self, function: &str) -> Result<Option<BootSession>, Refusal> {
        let file = match File::open(path(&self.dir, function)?) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Refusal::os("reading a single-use function's mark", e)),
        };
        // A session's bytes, its newline, and one more to tell a longer file.
        let bound = BootSession::MAX_BYTES
            .checked_add(2)
            .ok_or_else(|| invalid("mark bound overflows"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(bound)
            .map_err(|_| exhausted("single-use mark bytes", bound))?;
        let limit = u64::try_from(bound).map_err(|_| invalid("mark bound overflows"))?;
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(|e| Refusal::os("reading a single-use function's mark", e))?;
        let session = std::str::from_utf8(&bytes)
            .ok()
            .filter(|_| bytes.len() < bound)
            .and_then(|text| BootSession::new(text).ok());
        match session {
            Some(session) => Ok(Some(session)),
            None => Err(Refusal::Broken(format!(
                "{}: a single-use function's mark that names no boot session; once the host has rebooted, \
                 removing it admits the function again",
                path(&self.dir, function)?.display()
            ))),
        }
    }
}

/// `function`'s mark's name while it is written: `.` first, which no SBDF has, and `.tmp` last.
fn temporary(function: &str) -> Result<String, Refusal> {
    let room = function
        .len()
        .checked_add(".".len())
        .and_then(|room| room.checked_add(".tmp".len()))
        .ok_or_else(|| invalid("mark name overflows"))?;
    let mut name = String::new();
    name.try_reserve_exact(room)
        .map_err(|_| exhausted("single-use mark name bytes", room))?;
    name.push('.');
    name.push_str(function);
    name.push_str(".tmp");
    Ok(name)
}

fn sync(dir: &Path) -> Result<(), Refusal> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| Refusal::os("flushing the single-use marks", e))
}

/// Why `function`, which no reset clears, can't go to a VM now.
pub(super) fn reason(function: &str, spent: &Spent, now: Option<&BootSession>) -> String {
    match (spent, now) {
        (Spent::ThisBoot, Some(now)) => format!(
            "{function} has no reset method that clears it between VMs, and a VM had it on this boot ({}); the host \
             must reboot before another VM may have it",
            now.as_str()
        ),
        (Spent::Unshared { dir, why }, _) => format!(
            "{function} has no reset method that clears it between VMs, and this node's runtime directory ({}), where \
             it marks one given out, is its own: {why}; it gives such a function to no VM",
            dir.display()
        ),
        (Spent::Foreign { owner, uid }, _) => format!(
            "{function} has no reset method that clears it between VMs, and its VFIO group belongs to user {owner}, \
             not this node's ({uid}): that user's nodes mark it given out where this one doesn't read"
        ),
        (Spent::ThisBoot | Spent::BootUnknown, _) => format!(
            "{function} has no reset method that clears it between VMs, and the host's boot session can't be read to \
             show that no VM had it on this boot"
        ),
    }
}

/// A VM's refusal of `function`, which no reset clears, for `spent`.
pub(super) fn refused(function: &str, spent: &Spent, now: Option<&BootSession>) -> Refusal {
    Refusal::Platform {
        what: "a VM given a device whole (VFIO)",
        platform: "linux",
        why: reason(function, spent, now),
    }
}

/// Pool `key`'s refusal of `function`, which no reset clears, for `spent`: `what` is the pool's step, its observer's
/// start or its handoff to a box, which a person reads as why the pool can't go on.
pub(super) fn pool_refused(
    what: &'static str,
    key: &BackingKey,
    function: &str,
    spent: &Spent,
    now: Option<&BootSession>,
) -> Refusal {
    let pool: String = key.pool.iter().map(|byte| format!("{byte:02x}")).collect();
    Refusal::Platform {
        what,
        platform: "linux",
        why: format!("pool {pool}: {}", reason(function, spent, now)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(name: &str) -> BootSession {
        BootSession::new(name).unwrap()
    }

    /// AC-T7.4.1: do ask for a function never marked, marked on this boot, marked on another, and on a boot whose
    /// session is unknown; expect the first and third admitted, the second spent this boot, the last refused whatever
    /// its mark.
    #[test]
    fn a_function_is_admitted_on_a_boot_that_marked_it_never() {
        let (now, then) = (boot("now"), boot("then"));
        assert_eq!(admit(None, Some(&now)), Ok(()));
        assert_eq!(admit(Some(&now), Some(&now)), Err(Spent::ThisBoot));
        assert_eq!(admit(Some(&then), Some(&now)), Ok(()));
        for mark in [None, Some(&now)] {
            assert_eq!(admit(mark, None), Err(Spent::BootUnknown));
        }
    }

    /// AC-T7.4.1: do spend a function in a runtime directory, read its mark back on the same boot and a later one,
    /// then garble the mark; expect the mark durable in `vfio-single-use` (no temporary left), spent on the same boot,
    /// admitted on the later one, and a garbled mark refused by type, naming its file, never read as none.
    #[test]
    fn a_spent_functions_mark_outlives_its_writer_until_another_boot() {
        let state = tempfile::tempdir().unwrap();
        let (now, later) = (boot("6d1c0e2a-7b4f"), boot("0f9e8d7c-6b5a"));
        let function = "0000:79:00.0";
        Marks::of(state.path(), Some(&now))
            .unwrap()
            .spend(&[function])
            .unwrap();
        let dir = state.path().join(MARKS);
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [function]);
        assert_eq!(
            Marks::of(state.path(), Some(&now))
                .unwrap()
                .check(&[function])
                .unwrap(),
            Some((function, Spent::ThisBoot))
        );
        assert_eq!(
            Marks::of(state.path(), Some(&later))
                .unwrap()
                .check(&[function])
                .unwrap(),
            None
        );
        fs::write(dir.join(function), "not a boot!\n").unwrap();
        let refused = Marks::of(state.path(), Some(&later))
            .unwrap()
            .check(&[function])
            .unwrap_err();
        assert!(
            matches!(&refused, Refusal::Broken(why) if why.contains(function)),
            "{refused:?}"
        );
    }
}
