//! CP-T3.2. A closed SQLite file family changes hands without copying its data. A durable
//! intent names every original inode before publication. Both sides can finish it after a crash.
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use super::beside;
use crate::blobs::chunks::sync_dir;

const FAMILY: [&str; 3] = ["-wal", "-shm", ""];
const INTENT: &str = ".handoff.json";
const DEPARTED: &str = ".handed-to.json";
type Identity = (u64, u64);

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
struct Move {
    from: PathBuf,
    to: PathBuf,
    generation: String,
    files: Vec<Option<Identity>>,
    #[serde(default)]
    remember_source: bool,
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("conversation store has no parent directory")
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    Ok(parent(&path)?.canonicalize()?.join(path.file_name().context("missing store filename")?))
}

fn guard(path: &Path, exclusive: bool) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(beside(path, ".handoff.lock"))?;
    let lock = if exclusive { file.try_lock() } else { file.try_lock_shared() };
    lock.with_context(|| format!("conversation store {} is in use", path.display()))?;
    Ok(file)
}

fn identity(path: &Path) -> Result<Option<Identity>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(Some((meta.dev(), meta.ino())))
    }
    #[cfg(windows)]
    {
        Ok(Some(crate::os::file_identity(path)?))
    }
}

fn intent(path: &Path) -> Result<Option<Move>> {
    read_notice(path, INTENT)
}

fn read_notice(path: &Path, suffix: &str) -> Result<Option<Move>> {
    match File::open(beside(path, suffix)) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn publish_intent(path: &Path, expected: &Move) -> Result<()> {
    publish_notice(path, INTENT, expected)
}

fn publish_notice(path: &Path, suffix: &str, expected: &Move) -> Result<()> {
    if let Some(existing) = read_notice(path, suffix)? {
        ensure!(&existing == expected, "another handoff already names {}", path.display());
    } else {
        let mut tmp = tempfile::NamedTempFile::new_in(parent(path)?)?;
        serde_json::to_writer(&mut tmp, expected)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(beside(path, suffix))?;
    }
    sync_dir(parent(path)?)?;
    Ok(())
}

/// A completed handover reserves its empty source for the same store's return. Follow
/// additional handovers in case the receiving build changed its own storage directory.
fn allow_return(to: &Path, from: &Path) -> Result<()> {
    let Some(mut previous) = read_notice(to, DEPARTED)? else {
        return Ok(());
    };
    let returning = identity(from)?.context("returning store is missing")?;
    let mut visited = std::collections::HashSet::new();
    let mut here = to.to_owned();
    loop {
        ensure!(previous.from == here && visited.insert(here), "invalid completed store handoff path");
        // A receiving build can set a damaged original aside beside its destination.
        // The preserved inode still proves that it is the store that left here.
        if previous.to == from || previous.files.last().copied().flatten() == Some(returning) {
            return Ok(());
        }
        here = previous.to;
        previous = read_notice(&here, DEPARTED)?.context("destination is reserved for a different conversation store")?;
    }
}

impl Move {
    fn prepare(from: &Path, to: &Path, remember_source: bool) -> Result<Self> {
        ensure!(read_notice(from, DEPARTED)?.is_none(), "the source store was already handed over");
        allow_return(to, from)?;
        let mut files = Vec::with_capacity(FAMILY.len());
        for suffix in FAMILY {
            let target = beside(to, suffix);
            ensure!(identity(&target)?.is_none(), "{} is there already", target.display());
            files.push(identity(&beside(from, suffix))?);
        }
        ensure!(files.last().copied().flatten().is_some(), "original store {} is missing", from.display());
        let destination = identity(&beside(to, ".handoff.lock"))?.context("destination lock is missing")?;
        ensure!(
            files.iter().flatten().all(|(volume, _)| *volume == destination.0),
            "database handoff requires the same filesystem"
        );
        Ok(Self {
            from: from.to_owned(),
            to: to.to_owned(),
            generation: crate::util::new_id(),
            files,
            remember_source,
        })
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.files.len() == FAMILY.len() && self.files.last().copied().flatten().is_some(),
            "invalid conversation handoff manifest"
        );
        // Publish the destination intent first so an interrupted preparation cannot open a
        // newly created empty database there. No data moves until both intents are durable.
        publish_intent(&self.to, self)?;
        publish_intent(&self.from, self)?;
        for (suffix, expected) in FAMILY.iter().zip(&self.files) {
            let (source, target) = (beside(&self.from, suffix), beside(&self.to, suffix));
            let (local, remote) = (identity(&source)?, identity(&target)?);
            ensure!(
                local.is_none() || local.as_ref() == expected.as_ref(),
                "{} was replaced during handoff",
                source.display()
            );
            ensure!(
                remote.is_none() || remote.as_ref() == expected.as_ref(),
                "{} is occupied by another file",
                target.display()
            );
            if expected.is_some() && remote.is_none() {
                ensure!(local.is_some(), "original file {} is missing at both endpoints", source.display());
                fs::hard_link(&source, &target).with_context(|| format!("publish {} without replacing it", target.display()))?;
            }
        }
        sync_dir(parent(&self.to)?)?;
        // The complete family is durable at the destination before any original name is lost.
        for (suffix, expected) in FAMILY.iter().zip(&self.files).rev() {
            let source = beside(&self.from, suffix);
            if let Some(id) = identity(&source)? {
                ensure!(Some(id) == *expected, "{} was replaced during handoff", source.display());
                fs::remove_file(source)?;
            }
        }
        sync_dir(parent(&self.from)?)?;
        if self.remember_source {
            // Even after completing the handover, an old process that starts at the
            // vacated path must not silently create a new history there.
            publish_notice(&self.from, DEPARTED, self)?;
        }
        match fs::remove_file(beside(&self.to, DEPARTED)) {
            Ok(()) => sync_dir(parent(&self.to)?)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for endpoint in [&self.from, &self.to] {
            fs::remove_file(beside(endpoint, INTENT))?;
            sync_dir(parent(endpoint)?)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Recover,
    Handover,
    SetAside,
}

fn transfer(from: &Path, to: &Path, operation: Operation) -> Result<bool> {
    let (from, to) = (absolute(from)?, absolute(to)?);
    ensure!(from != to, "a store cannot be handed to itself");
    // Both opens are nonblocking. Opposing handoffs acquire in the same order.
    let _first = guard(std::cmp::min(&from, &to), true)?;
    let _second = guard(std::cmp::max(&from, &to), true)?;
    let (source, target) = (intent(&from)?, intent(&to)?);
    if let (Some(a), Some(b)) = (&source, &target) {
        ensure!(a == b, "conflicting conversation store handoffs");
    }
    if let Some(pending) = source.or(target) {
        let forward = pending.from == from && pending.to == to;
        ensure!(forward || (pending.from == to && pending.to == from), "an unrelated handoff holds this store");
        pending.finish()?;
        if forward || operation == Operation::Recover {
            return Ok(true);
        }
    } else if operation == Operation::Recover {
        return Ok(false);
    }
    Move::prepare(&from, &to, operation == Operation::Handover)?.finish()?;
    Ok(true)
}

/// Move a closed conversation store and its WAL and SHM files, refusing occupied destinations.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(parent(&std::path::absolute(to)?)?)?;
    transfer(from, to, Operation::Handover).map(|_| ())
}

pub(super) fn move_aside(from: &Path, to: &Path) -> Result<()> {
    transfer(from, to, Operation::SetAside).map(|_| ())
}

/// Reconcile only a previously recorded move for this pair before checking either filename.
pub fn recover_file_move(from: &Path, to: &Path) -> Result<bool> {
    if intent(from)?.is_none() && intent(to)?.is_none() {
        return Ok(false);
    }
    transfer(from, to, Operation::Recover)
}

/// Hold shared custody for the engine lifetime. Resolve an interrupted handoff before SQLite
/// can create a main file or open a WAL that belongs to another generation.
pub(super) fn open_guard(path: &Path) -> Result<File> {
    let hold = guard(path, false)?;
    let Some(pending) = intent(path)? else {
        if let Some(done) = read_notice(path, DEPARTED)? {
            anyhow::bail!("conversation store moved to {}", done.to.display());
        }
        return Ok(hold);
    };
    drop(hold);
    let path = absolute(path)?;
    ensure!(pending.from == path || pending.to == path, "handoff names another store");
    recover_file_move(&pending.from, &pending.to)?;
    ensure!(pending.to == path, "conversation store moved to {}", pending.to.display());
    let hold = guard(&path, false)?;
    ensure!(intent(&path)?.is_none() && path.is_file(), "conversation store changed hands while opening");
    Ok(hold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf, Move)> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        fs::create_dir(&old)?;
        fs::create_dir(&new)?;
        let (old, new) = (absolute(&old.join("c.db"))?, absolute(&new.join("c.db"))?);
        for suffix in FAMILY {
            fs::write(beside(&old, suffix), format!("contents {suffix}"))?;
        }
        let _held = guard(&new, true)?;
        let journal = Move::prepare(&old, &new, true)?;
        Ok((dir, old, new, journal))
    }

    #[test]
    fn cp_t3_2_interrupted_handoff_reconciles_every_half_published_family() -> Result<()> {
        for made in 0..=FAMILY.len() {
            let (_dir, old, new, journal) = fixture()?;
            publish_intent(&new, &journal)?;
            if made > 0 {
                publish_intent(&old, &journal)?;
            }
            for suffix in FAMILY.iter().take(made) {
                fs::hard_link(beside(&old, suffix), beside(&new, suffix))?;
            }
            if made == FAMILY.len() {
                fs::remove_file(&old)?;
            }
            let hold = open_guard(&new)?;
            for suffix in FAMILY {
                assert_eq!(fs::read_to_string(beside(&new, suffix))?, format!("contents {suffix}"));
                assert!(!beside(&old, suffix).exists());
            }
            assert!(move_file(&new, &old).is_err(), "a live store is fenced");
            drop(hold);
            let unrelated = parent(&new)?.join("unrelated.db");
            fs::write(&unrelated, "another database")?;
            assert!(move_file(&unrelated, &old).is_err());
            assert_eq!(fs::read_to_string(&unrelated)?, "another database");
            assert!(open_guard(&old).is_err(), "a cold predecessor must not create an empty database in place");
            move_file(&new, &old)?;
            assert!(!new.exists());
            assert_eq!(fs::read_to_string(&old)?, "contents ");
        }
        Ok(())
    }

    #[test]
    fn cp_t3_2_return_can_reclaim_the_original_set_aside_by_the_receiver() -> Result<()> {
        let (_dir, old, new, _) = fixture()?;
        move_file(&old, &new)?;
        let aside = beside(&new, ".unopened");
        move_aside(&new, &aside)?;
        {
            let _hold = open_guard(&new)?;
            fs::write(&new, "replacement")?;
        }
        move_file(&aside, &old)?;
        assert_eq!(fs::read_to_string(&old)?, "contents ");
        assert_eq!(fs::read_to_string(&new)?, "replacement");
        Ok(())
    }

    #[test]
    fn cp_t3_2_interrupted_handoff_preserves_both_original_and_unrelated_collision() -> Result<()> {
        let (_dir, old, new, journal) = fixture()?;
        publish_intent(&new, &journal)?;
        publish_intent(&old, &journal)?;
        fs::hard_link(beside(&old, "-wal"), beside(&new, "-wal"))?;
        fs::write(&new, "unrelated")?;
        assert!(recover_file_move(&old, &new).is_err());
        assert_eq!(fs::read_to_string(&new)?, "unrelated");
        for suffix in FAMILY {
            assert_eq!(fs::read_to_string(beside(&old, suffix))?, format!("contents {suffix}"));
        }
        Ok(())
    }
}
