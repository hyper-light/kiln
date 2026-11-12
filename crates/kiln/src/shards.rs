//! Shards as kiln carries it (vendor/shards, kiln's one Shards revision; docs/shards.md; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 16). kiln is the one file there is to install, and it carries
//! Shards' binary, `shards`, built to be carried (`SHARDS_HELPERS=beside`, Shards' D237), and its VM and
//! network processes, which Shards keeps processes of their own binaries (its D36: every VM maps the
//! binary it runs, and on macOS the VM process alone runs in App Sandbox, granted by entitlements that
//! are its binary's signature). ci/shards/pack.sh builds the three from vendor/shards for the platform
//! kiln is built for, and build.rs embeds them (`cfg(kiln_shards)`); a kiln built without them says so
//! when they're asked for.
//!
//! [`write_out`] writes them into `<base>/shards/<id>/`, beside each other, as the carried `shards`
//! looks for its helpers: the folder named by what's carried (and, on a Mac, by who signs it), written
//! whole (beside its place, signed, marked ready, then renamed into it), so a process never runs half a
//! folder or another build's. On a Mac kiln signs them as it signs its own builds (`os::macsign`): the VM
//! process with Shards' entitlements (App Sandbox, the hypervisor), the network process with its own (App
//! Sandbox, the network's client and server), both under the Hardened Runtime, as Shards' build signs
//! them; with kiln's identity where `kiln signing setup` made one, so macOS knows each as the program it
//! was across kiln's upgrades, else ad hoc. A process holds the folder it uses for as long as it keeps
//! the [`Held`] (a shared lock on the folder's `lock`), and the process that writes a folder out removes
//! every other one no process holds, as Shards collects its own (its D182).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};

use crate::os::macsign::{self, Signer};

#[cfg(kiln_shards)]
const SHARDS: &[u8] = include_bytes!(env!("KILN_SHARDS_BIN"));
#[cfg(kiln_shards)]
const VM: &[u8] = include_bytes!(env!("KILN_SHARDS_VM"));
#[cfg(kiln_shards)]
const NET: &[u8] = include_bytes!(env!("KILN_SHARDS_NET"));
#[cfg(not(kiln_shards))]
const SHARDS: &[u8] = &[];
#[cfg(not(kiln_shards))]
const VM: &[u8] = &[];
#[cfg(not(kiln_shards))]
const NET: &[u8] = &[];

/// Shards' entitlements, as its build signs its VM and network processes with them (its
/// `resources/vm.entitlements` and `net.entitlements`).
const VM_ENTITLEMENTS: &[u8] = include_bytes!("../../../vendor/shards/resources/vm.entitlements");
const NET_ENTITLEMENTS: &[u8] = include_bytes!("../../../vendor/shards/resources/net.entitlements");

/// Written last into a folder written out: its id, so a folder without it is never run.
const READY: &str = ".ready";
/// The lock file of the folders' root, and of each folder.
const LOCK: &str = "lock";

/// One executable carried: its name (the platform's suffix added), its bytes, and the entitlements it is
/// signed with on a Mac.
#[derive(Clone, Copy)]
struct Carried {
    name: &'static str,
    bytes: &'static [u8],
    entitlements: Option<&'static [u8]>,
}

fn carried_binaries() -> [Carried; 3] {
    [
        Carried { name: "shards", bytes: SHARDS, entitlements: None },
        Carried { name: "shards-vm", bytes: VM, entitlements: Some(VM_ENTITLEMENTS) },
        Carried { name: "shards-net", bytes: NET, entitlements: Some(NET_ENTITLEMENTS) },
    ]
}

/// Whether this kiln carries Shards.
pub fn carried() -> bool {
    carried_binaries().iter().all(|c| !c.bytes.is_empty())
}

/// Shards written out: its binary and its VM and network processes beside it, which no other process
/// removes while this is kept.
#[derive(Debug)]
pub struct Held {
    pub dir: PathBuf,
    /// The `shards` to run: shards-api's `binary` (`GuestSpec`, `ImageSpec`), Hex's `SHARDS_BIN`.
    pub shards: PathBuf,
    pub vm: PathBuf,
    pub net: PathBuf,
    /// Who signed them.
    pub signer: Signer,
    _lock: Locked,
}

/// The carried Shards, written out under `base` if it isn't there yet, and held while the answer is kept.
pub fn write_out(base: &Path) -> anyhow::Result<Held> {
    if !carried() {
        bail!("this kiln is built without Shards: ci/shards/pack.sh builds it for kiln's build to carry (docs/shards.md)");
    }
    write_out_of(base, &carried_binaries(), &macsign::signer())
}

fn write_out_of(base: &Path, carried: &[Carried], signer: &Signer) -> anyhow::Result<Held> {
    let root = base.join("shards");
    private_dir(&root)?;
    let id = format!("{}{}", hash_of(carried), signer.tag());
    let dir = root.join(&id);
    // Writing and holding take the root's lock shared, a collection takes it alone: no folder is
    // collected between being placed and being held.
    let writing = open_lock(&root.join(LOCK))?;
    writing.lock_shared().with_context(|| format!("locking {}", root.display()))?;
    let placed = !is_ready(&dir, &id) && place(&root, &id, carried, signer).with_context(|| format!("writing Shards out into {}", dir.display()))?;
    let lock = open_lock(&dir.join(LOCK))?;
    lock.lock_shared().with_context(|| format!("holding {}", dir.display()))?;
    let held = Held {
        shards: dir.join(format!("shards{}", std::env::consts::EXE_SUFFIX)),
        vm: dir.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX)),
        net: dir.join(format!("shards-net{}", std::env::consts::EXE_SUFFIX)),
        dir,
        signer: signer.clone(),
        _lock: Locked(lock),
    };
    drop(writing);
    if placed {
        collect(&root, &id);
    }
    Ok(held)
}

/// What's carried, named: the SHA-256 of each executable's length and bytes, its first 12 hex digits.
fn hash_of(carried: &[Carried]) -> String {
    let mut h = Sha256::new();
    for c in carried {
        h.update((c.bytes.len() as u64).to_le_bytes());
        h.update(c.bytes);
    }
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn is_ready(dir: &Path, id: &str) -> bool {
    fs::read_to_string(dir.join(READY)).is_ok_and(|r| r.trim() == id)
}

/// Writes the executables into a folder of this process's beside `root/id`, each synced and signed, its
/// lock file and ready mark last, then renames it into place; false where another process placed it
/// first, whose copy serves. A folder there that isn't ready is replaced.
fn place(root: &Path, id: &str, carried: &[Carried], signer: &Signer) -> anyhow::Result<bool> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = root.join(format!(".{id}.{}.{nanos}", std::process::id()));
    let written = fill(&tmp, id, carried, signer);
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let dir = root.join(id);
    if fs::rename(&tmp, &dir).is_ok() {
        return Ok(true);
    }
    if is_ready(&dir, id) {
        let _ = fs::remove_dir_all(&tmp);
        return Ok(false);
    }
    let _ = fs::remove_dir_all(&dir);
    let moved = fs::rename(&tmp, &dir);
    if moved.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    moved.with_context(|| format!("moving {} into place", dir.display()))?;
    Ok(true)
}

fn fill(tmp: &Path, id: &str, carried: &[Carried], signer: &Signer) -> anyhow::Result<()> {
    private_dir(tmp)?;
    for c in carried {
        let path = tmp.join(format!("{}{}", c.name, std::env::consts::EXE_SUFFIX));
        let mut file = executable(&path).with_context(|| path.display().to_string())?;
        file.write_all(c.bytes)?;
        file.sync_all()?;
        drop(file);
        if *signer == Signer::NotNeeded {
            continue;
        }
        match c.entitlements {
            Some(plist) => {
                let entitlements = tmp.join(format!(".{}.entitlements", c.name));
                fs::write(&entitlements, plist)?;
                macsign::sign_carried(&path, Some(&entitlements), signer)?;
                fs::remove_file(&entitlements)?;
            }
            None => macsign::sign_carried(&path, None, signer)?,
        }
    }
    fs::File::create(tmp.join(LOCK))?;
    let mut ready = fs::File::create(tmp.join(READY))?;
    ready.write_all(id.as_bytes())?;
    ready.sync_all()?;
    Ok(())
}

/// Removes every other folder under `root` that no process holds, and what a process that ended while
/// writing one left, holding the root's lock alone so none is being placed meanwhile; where another
/// process holds the root's lock, the next collection does it. A folder whose lock this takes alone is
/// moved aside, then removed: Windows moves none with a file open in it, and that one waits for the next.
fn collect(root: &Path, keep: &str) {
    let Ok(alone) = open_lock(&root.join(LOCK)) else { return };
    if alone.try_lock().is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else { return };
    for e in entries.filter_map(Result::ok) {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == keep || name == LOCK || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = e.path();
        if name.starts_with('.') {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        let Ok(lock) = open_lock(&path.join(LOCK)) else { continue };
        if lock.try_lock().is_err() {
            continue;
        }
        let aside = root.join(format!(".{name}-{}", std::process::id()));
        let moved = fs::rename(&path, &aside).is_ok();
        drop(lock);
        if moved {
            let _ = fs::remove_dir_all(&aside);
        }
    }
    drop(alone);
}

/// A lock file, its lock let go before it closes: a lock is its open file's, which a program another
/// thread starts meanwhile shares until its exec closes it (Shards' PM M244).
#[derive(Debug)]
struct Locked(fs::File);

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn open_lock(path: &Path) -> anyhow::Result<fs::File> {
    fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).with_context(|| path.display().to_string())
}

/// A new file at `path`, only this user's to read, write and run.
fn executable(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o700);
    options.open(path)
}

/// `path`, made if it isn't there, only this user's: what's in it runs as kiln.
fn private_dir(path: &Path) -> anyhow::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(path).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(bytes: [&'static [u8]; 3]) -> [Carried; 3] {
        let [s, v, n] = bytes;
        [
            Carried { name: "shards", bytes: s, entitlements: None },
            Carried { name: "shards-vm", bytes: v, entitlements: Some(VM_ENTITLEMENTS) },
            Carried { name: "shards-net", bytes: n, entitlements: Some(NET_ENTITLEMENTS) },
        ]
    }

    /// Unsigned scripts stand in for the binaries: a Mac signs whatever it writes out, which these tests
    /// leave to the real ones (`tests/shards_carried.rs`).
    fn unsigned() -> Signer {
        Signer::NotNeeded
    }

    fn names(root: &Path) -> anyhow::Result<Vec<String>> {
        let mut out: Vec<String> = fs::read_dir(root)?.filter_map(Result::ok).filter_map(|e| e.file_name().into_string().ok()).collect();
        out.sort();
        Ok(out)
    }

    #[test]
    fn step16_written_out_whole_once_beside_each_other_and_found_after() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let carried = fake([b"#!shards", b"#!vm", b"#!net"]);
        let held = write_out_of(base.path(), &carried, &unsigned())?;
        assert_eq!(held.shards.parent(), Some(held.dir.as_path()));
        assert_eq!((fs::read(&held.shards)?, fs::read(&held.vm)?, fs::read(&held.net)?), (b"#!shards".to_vec(), b"#!vm".to_vec(), b"#!net".to_vec()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&held.vm)?.permissions().mode() & 0o777, 0o700);
            assert_eq!(fs::metadata(base.path().join("shards"))?.permissions().mode() & 0o777, 0o700);
        }
        // Again, while held: the same folder, as it was, and no copy of this process's left beside it.
        fs::write(held.dir.join("marker"), b"x")?;
        let again = write_out_of(base.path(), &carried, &unsigned())?;
        assert_eq!(again.dir, held.dir);
        assert!(held.dir.join("marker").is_file());
        let id = hash_of(&carried);
        assert_eq!(names(&base.path().join("shards"))?, [id.clone(), LOCK.to_owned()]);
        // One that isn't ready (cut short, by hand) is written again.
        drop((held, again));
        fs::remove_file(base.path().join("shards").join(&id).join(READY))?;
        let rewritten = write_out_of(base.path(), &carried, &unsigned())?;
        assert!(is_ready(&rewritten.dir, &id) && !rewritten.dir.join("marker").exists());
        Ok(())
    }

    #[test]
    fn step16_other_builds_folders_go_once_no_process_holds_them() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let root = base.path().join("shards");
        let old = write_out_of(base.path(), &fake([b"old shards", b"old vm", b"old net"]), &unsigned())?;
        let gone = write_out_of(base.path(), &fake([b"gone shards", b"gone vm", b"gone net"]), &unsigned())?;
        let gone_dir = gone.dir.clone();
        drop(gone);
        // What a process that ended mid-write left.
        fs::create_dir_all(root.join(".cut.1.2"))?;
        let new = write_out_of(base.path(), &fake([b"new shards", b"new vm", b"new net"]), &unsigned())?;
        // The held one stays; the one no process holds and the cut copy go.
        assert!(old.dir.is_dir() && new.dir.is_dir());
        assert!(!gone_dir.exists());
        let mut expect = vec![LOCK.to_owned(), old.dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned(), new.dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned()];
        expect.sort();
        assert_eq!(names(&root)?, expect);
        // Let go, it goes with the next build's write-out.
        let old_dir = old.dir.clone();
        drop(old);
        let _next = write_out_of(base.path(), &fake([b"next shards", b"next vm", b"next net"]), &unsigned())?;
        assert!(!old_dir.exists() && new.dir.is_dir());
        Ok(())
    }

    #[test]
    fn step16_each_signer_has_a_folder_of_its_own() {
        let carried = fake([b"s", b"v", b"n"]);
        let id = hash_of(&carried);
        assert_eq!(format!("{id}{}", Signer::NotNeeded.tag()), id);
        assert_eq!(format!("{id}{}", Signer::AdHoc.tag()), format!("{id}-a"));
        assert_eq!(Signer::Kiln("0123456789ABCDEF".into()).tag(), "-k01234567");
        assert_ne!(hash_of(&fake([b"s", b"v", b"n2"])), id);
    }

    #[test]
    fn step16_a_kiln_without_shards_says_so() {
        if carried() {
            return;
        }
        let Ok(base) = tempfile::tempdir() else { return };
        let said = write_out(base.path()).map(|h| h.dir).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(said.contains("built without Shards"), "{said}");
        assert!(!base.path().join("shards").exists());
    }
}
