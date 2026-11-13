//! The spec the runtime checked, handed to what starts the container's init (AC-T7.1.1, T-T7.1.4; found
//! 2026-11-04). The runtime reads and parses a bundle's `config.json` and checks it (`spec.rs`: among the rest, a
//! container in a user namespace holding CAP_SYS_ADMIN without a network namespace of its own is refused). A
//! create seed or the shim that then read `config.json` again would make the container from whatever the file held
//! by then: a spec nobody checked, swapped in between. So neither reads the bundle. The runtime puts the exact bytes
//! it parsed in a memfd and seals it against every change (`F_SEAL_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW`, then
//! `F_SEAL_SEAL`: memfd_create(2), fcntl(2) "File Sealing"), and the seed takes the descriptor over `SCM_RIGHTS`, the
//! shim by its `/proc/<pid>/fd` path as it takes its pipes. Each reads it from offset 0 with `pread` (the seed's
//! descriptor shares the runtime's file offset) and refuses one that isn't sealed so, or is over
//! [`MAX_CONFIG_BYTES`]: the bytes it parses are the bytes the runtime checked.
//!
//! The same holds after create. An exec takes its program's process settings (capabilities, seccomp, user,
//! rlimits) from the container's spec, and a start and a delete take their hooks from it, which the agent runs on
//! the host. So create keeps the bytes it checked with the container's state ([`keep`], `checked.json` in the
//! container's state directory, which the runtime alone writes), and each later reader takes the spec from there
//! ([`spec_of`]), as runc keeps the config it built in its state. A container made before its spec was kept has
//! its bundle's `config.json` read and checked again with the same checks (`Spec::parse`), never taken unchecked:
//! a spec they refuse refuses the exec, the start or the delete by type.

use std::io::Write;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{MemfdFlags, SealFlags};

use crate::linux::oci::OciError;
use crate::linux::oci::spec::{MAX_CONFIG_BYTES, Spec};

/// The checked spec's name in a container's state directory ([`keep`]).
pub const KEPT: &str = "checked.json";

/// The seals a checked spec's memfd carries: nothing can write, shrink or grow it, nor take a seal away.
const SEALS: SealFlags = SealFlags::WRITE
    .union(SealFlags::SHRINK)
    .union(SealFlags::GROW)
    .union(SealFlags::SEAL);

/// `bytes`, the `config.json` the runtime parsed and checked, in a sealed memfd, close-on-exec.
pub fn seal(bytes: &[u8]) -> Result<OwnedFd, OciError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(OciError::Config(format!(
            "config.json is {} bytes, over {MAX_CONFIG_BYTES}",
            bytes.len()
        )));
    }
    let fd = rustix::fs::memfd_create(
        "hex-checked-config",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|e| OciError::os("making the checked config's memfd", e))?;
    let mut at = 0usize;
    while let Some(rest) = bytes.get(at..).filter(|rest| !rest.is_empty()) {
        let n = rustix::io::write(&fd, rest)
            .map_err(|e| OciError::os("writing the checked config", e))?;
        if n == 0 {
            return Err(OciError::Broken(
                "the checked config's memfd took no bytes".to_owned(),
            ));
        }
        at = at
            .checked_add(n)
            .ok_or_else(|| OciError::Broken("the checked config's length overflowed".to_owned()))?;
    }
    rustix::fs::fcntl_add_seals(&fd, SEALS)
        .map_err(|e| OciError::os("sealing the checked config", e))?;
    Ok(fd)
}

/// The bytes of a checked spec's memfd ([`seal`]), read from offset 0; refused unless it carries every seal of
/// [`SEALS`] and is at most [`MAX_CONFIG_BYTES`].
pub fn read(fd: BorrowedFd<'_>) -> Result<Vec<u8>, OciError> {
    let seals = rustix::fs::fcntl_get_seals(fd)
        .map_err(|e| OciError::os("reading the checked config's seals", e))?;
    if !seals.contains(SEALS) {
        return Err(OciError::Config(
            "the spec handed over isn't sealed against change, so it may not be the one checked"
                .to_owned(),
        ));
    }
    let size = rustix::fs::fstat(fd)
        .map_err(|e| OciError::os("reading the checked config's size", e))?
        .st_size;
    let size = usize::try_from(size)
        .ok()
        .filter(|&n| n <= MAX_CONFIG_BYTES)
        .ok_or_else(|| {
            OciError::Config(format!(
                "the checked config is {size} bytes, over {MAX_CONFIG_BYTES}"
            ))
        })?;
    let mut bytes = vec![0u8; size];
    let mut at = 0usize;
    while at < size {
        let rest = bytes
            .get_mut(at..)
            .ok_or_else(|| OciError::Broken("the checked config's buffer is short".to_owned()))?;
        let offset = u64::try_from(at)
            .map_err(|_| OciError::Broken("the checked config's offset overflowed".to_owned()))?;
        let n = rustix::io::pread(fd, rest, offset)
            .map_err(|e| OciError::os("reading the checked config", e))?;
        if n == 0 {
            return Err(OciError::Broken(format!(
                "the checked config ended at {at} of its {size} bytes"
            )));
        }
        at = at
            .checked_add(n)
            .ok_or_else(|| OciError::Broken("the checked config's length overflowed".to_owned()))?;
    }
    Ok(bytes)
}

/// Keeps `bytes`, the `config.json` the runtime parsed and checked at create, in the container's state directory
/// `dir`: written once (O_EXCL), readable by the runtime alone (0600), and, as `state.json` is, never fsynced (the
/// state root is under /run, emptied at boot: decision 1).
pub fn keep(dir: &Path, bytes: &[u8]) -> Result<(), OciError> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(KEPT))
        .map_err(|e| OciError::os("keeping the checked spec", e))?;
    f.write_all(bytes)
        .map_err(|e| OciError::os("keeping the checked spec", e))
}

/// The spec container `dir` was made from: the one its create checked and kept ([`keep`]). With no state
/// directory named (a request from before it was), or none kept in it (a container made before it was), the
/// bundle's `config.json`, checked again with the same checks; `Ok(None)` only where neither exists, so there is
/// no spec to act on. Never a spec unchecked.
pub fn spec_of(dir: Option<&Path>, bundle: &Path) -> Result<Option<Spec>, OciError> {
    if let Some(dir) = dir {
        match std::fs::read(dir.join(KEPT)) {
            Ok(bytes) => return Spec::parse(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(OciError::os("reading the checked spec", e)),
        }
    }
    match std::fs::read(bundle.join("config.json")) {
        Ok(bytes) => Spec::parse(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OciError::os("reading config.json", e)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    /// The checked spec's bytes come back whole from a sealed memfd, whatever its file offset (CLAUDE.md §6: do X,
    /// expect Y). Do: seal a config, read it twice. Expect: the bytes each time.
    #[test]
    fn a_sealed_config_reads_back_its_bytes_from_offset_zero() {
        let bytes = br#"{"ociVersion":"1.3.0"}"#;
        let fd = seal(bytes).unwrap();
        assert_eq!(read(fd.as_fd()).unwrap(), bytes);
        assert_eq!(read(fd.as_fd()).unwrap(), bytes);
    }

    /// Nothing can change a sealed config (T-T7.1.4). Do: write to it, and truncate it. Expect: both refused.
    #[test]
    fn a_sealed_config_refuses_every_change() {
        let fd = seal(b"{}").unwrap();
        assert!(rustix::io::pwrite(&fd, b"x", 0).is_err(), "written");
        assert!(rustix::fs::ftruncate(&fd, 0).is_err(), "shrunk");
        assert!(rustix::fs::ftruncate(&fd, 64).is_err(), "grown");
    }

    /// A descriptor that isn't sealed isn't taken for the checked spec (T-T7.1.4). Do: read an unsealed memfd.
    /// Expect: refused by type, naming why.
    #[test]
    fn an_unsealed_descriptor_is_refused() {
        let fd = rustix::fs::memfd_create("unsealed", MemfdFlags::CLOEXEC).unwrap();
        rustix::io::write(&fd, b"{}").unwrap();
        assert!(matches!(read(fd.as_fd()), Err(OciError::Config(why)) if why.contains("sealed")));
    }

    /// A config over the runtime's bound is refused before it's sealed (CLAUDE.md §2).
    #[test]
    fn a_config_over_the_bound_is_refused() {
        let big = vec![b' '; MAX_CONFIG_BYTES + 1];
        assert!(matches!(seal(&big), Err(OciError::Config(_))));
    }

    const SPEC: &[u8] =
        br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"args":["/kept"]}}"#;
    const SWAPPED: &[u8] =
        br#"{"ociVersion":"1.3.0","root":{"path":"rootfs"},"process":{"args":["/swapped"]}}"#;

    fn args(spec: Option<Spec>) -> Vec<String> {
        spec.unwrap().process.unwrap().args
    }

    /// AC-T7.1.1: a container's later readers take the spec its create kept, whatever its bundle holds since. Do:
    /// keep a spec, then write another to the bundle. Expect: the kept one; and a second keep refused (written once).
    #[test]
    fn the_kept_spec_is_read_and_not_the_bundles() {
        let (dir, bundle) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        keep(dir.path(), SPEC).unwrap();
        std::fs::write(bundle.path().join("config.json"), SWAPPED).unwrap();
        assert_eq!(
            args(spec_of(Some(dir.path()), bundle.path()).unwrap()),
            ["/kept"]
        );
        assert!(keep(dir.path(), SWAPPED).is_err(), "kept once");
    }

    /// An older container (no spec kept) or an older request (no state directory named) has its bundle's spec
    /// checked again, never taken unchecked: a spec the checks refuse is refused by type, and with no spec at all
    /// there is nothing to act on.
    #[test]
    fn an_older_container_has_its_bundle_spec_checked_again() {
        let (dir, bundle) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert!(
            spec_of(Some(dir.path()), bundle.path()).unwrap().is_none(),
            "no spec at all"
        );
        std::fs::write(bundle.path().join("config.json"), SWAPPED).unwrap();
        assert_eq!(
            args(spec_of(Some(dir.path()), bundle.path()).unwrap()),
            ["/swapped"]
        );
        assert_eq!(args(spec_of(None, bundle.path()).unwrap()), ["/swapped"]);
        std::fs::write(bundle.path().join("config.json"), br#"{"ociVersion":""}"#).unwrap();
        assert!(matches!(
            spec_of(Some(dir.path()), bundle.path()),
            Err(OciError::Config(_))
        ));
    }
}
