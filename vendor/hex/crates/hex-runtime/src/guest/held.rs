//! The guest a node holds, recorded in its state so the node after a crash takes it back
//! (`shards_api::Guests::attach`) rather than boot another beside it (native plan step 8, T-T7.1.2): its
//! VM's pid, that process's start time, and its vsock socket ([`shards_api::Identity`]).
//!
//! One small text file, written whole to a temporary file beside it, synced, renamed over it (rename(2),
//! MoveFileEx on Windows, replaces it atomically) and, on Unix, its folder synced, so a crash leaves the
//! last record or the next, never a torn one; a temporary left by a crash is overwritten by the next write. Read back, anything but a
//! record this writes is refused by type, naming the file, and so is a file past [`MAX_BYTES`].

use std::io::Write;
use std::path::{Path, PathBuf};

use hex_api::Refusal;

/// The record's first line: its format, so a later one is told apart.
const HEADER: &str = "hex-guest 1";
/// The most a record's file holds: its four lines with a vsock path of `PATH_MAX` (4,096 bytes on
/// Linux, 1,024 on macOS) and room to spare; a larger file is no record of this.
pub const MAX_BYTES: u64 = 8 << 10;

fn broken(path: &Path, why: impl std::fmt::Display) -> Refusal {
    Refusal::Broken(format!("the node's guest record {} {why}", path.display()))
}

/// Records `identity` at `path`, replacing what was there.
pub fn write(path: &Path, identity: &shards_api::Identity) -> Result<(), Refusal> {
    let vsock = identity.vsock.to_str().ok_or_else(|| Refusal::Invalid {
        what: "the guest's vsock path",
        why: format!("{} isn't UTF-8", identity.vsock.display()),
    })?;
    if vsock.contains('\n') || vsock.is_empty() {
        return Err(Refusal::Invalid {
            what: "the guest's vsock path",
            why: format!("{vsock:?} is empty or holds a line break"),
        });
    }
    let text = format!(
        "{HEADER}\npid {}\nstarted {}\nvsock {vsock}\n",
        identity.pid, identity.started
    );
    replace(path, text.as_bytes())
}

/// Replace one guest-owned record durably. The caller validates its bounded encoding first.
pub(crate) fn replace(path: &Path, text: &[u8]) -> Result<(), Refusal> {
    let dir = path.parent().ok_or_else(|| broken(path, "has no folder"))?;
    let name = path
        .file_name()
        .ok_or_else(|| broken(path, "has no name"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".tmp");
    let temp = dir.join(temp_name);
    let os = |op: &'static str| move |e: std::io::Error| Refusal::os(op, e);
    let mut file = std::fs::File::create(&temp).map_err(os("writing the node's guest record"))?;
    file.write_all(text)
        .and_then(|()| file.sync_all())
        .map_err(os("writing the node's guest record"))?;
    drop(file);
    #[cfg(windows)]
    crate::windows::sys::replace_durable(&temp, path)
        .map_err(os("placing the node's guest record"))?;
    #[cfg(not(windows))]
    std::fs::rename(&temp, path).map_err(os("placing the node's guest record"))?;
    sync_folder(dir).map_err(os("syncing the node's guest record's folder"))
}

/// Makes the rename into `dir` durable: the folder's own fsync(2).
#[cfg(unix)]
pub(crate) fn sync_folder(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|d| d.sync_all())
}

/// Windows publication uses the existing native write-through replacement helper
/// after the temp file's sync. This supplies no directory/hardware power-loss proof.
#[cfg(not(unix))]
pub(crate) fn sync_folder(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The identity recorded at `path`, `None` when there is no record.
pub fn read(path: &Path) -> Result<Option<shards_api::Identity>, Refusal> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Refusal::os("reading the node's guest record", e)),
    };
    if meta.len() > MAX_BYTES {
        return Err(broken(
            path,
            format!(
                "holds {} bytes, past the {MAX_BYTES} a record holds",
                meta.len()
            ),
        ));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| broken(path, format!("isn't text: {e}")))?;
    parse(&text).map(Some).map_err(|why| broken(path, why))
}

/// Removes the record at `path`; none there is nothing to remove.
pub fn remove(path: &Path) -> Result<(), Refusal> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Refusal::os("removing the node's guest record", e)),
    }
}

/// A record's text as its identity, or what is wrong with it.
fn parse(text: &str) -> Result<shards_api::Identity, String> {
    let mut lines = text.split('\n');
    if lines.next() != Some(HEADER) {
        return Err(format!("doesn't begin {HEADER:?}"));
    }
    let mut field = |name: &str| -> Result<&str, String> {
        lines
            .next()
            .and_then(|l| l.strip_prefix(name))
            .and_then(|l| l.strip_prefix(' '))
            .ok_or_else(|| format!("has no {name} line where one belongs"))
    };
    let pid = field("pid")?;
    let pid: u32 = pid.parse().map_err(|e| format!("pid {pid:?}: {e}"))?;
    let started = field("started")?;
    let started: u64 = started
        .parse()
        .map_err(|e| format!("started {started:?}: {e}"))?;
    let vsock = field("vsock")?;
    if vsock.is_empty() {
        return Err("names an empty vsock path".to_owned());
    }
    let vsock = PathBuf::from(vsock);
    match (lines.next(), lines.next()) {
        (Some(""), None) => Ok(shards_api::Identity {
            pid,
            started,
            vsock,
        }),
        _ => Err("holds more than a record, or lacks its last line break".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn identity() -> shards_api::Identity {
        shards_api::Identity {
            pid: 4242,
            started: u64::MAX,
            vsock: PathBuf::from("/var/folders/x/shards-vsock.sock"),
        }
    }

    /// Native plan step 8 (T-T7.1.2): write a guest's identity, read it back: expect the same; remove it:
    /// expect none, and a second remove Ok.
    #[test]
    fn a_record_reads_back_as_written_and_goes_with_its_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guest");
        assert_eq!(read(&path).unwrap(), None);
        write(&path, &identity()).unwrap();
        assert_eq!(read(&path).unwrap(), Some(identity()));
        let other = shards_api::Identity {
            pid: 7,
            ..identity()
        };
        write(&path, &other).unwrap();
        assert_eq!(read(&path).unwrap(), Some(other));
        remove(&path).unwrap();
        assert_eq!(read(&path).unwrap(), None);
        remove(&path).unwrap();
    }

    /// Native plan step 8 (hostile input, CLAUDE.md §6): a record cut at every byte, one with a line
    /// past it, a wrong header, a pid that isn't a u32, and a file past the bound are each refused by
    /// type, naming the file; never read as an identity.
    #[test]
    fn anything_but_a_record_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guest");
        write(&path, &identity()).unwrap();
        let whole = std::fs::read_to_string(&path).unwrap();
        let mut bad: Vec<String> = (0..whole.len())
            .map(|n| whole.get(..n).unwrap().to_owned())
            .collect();
        bad.push(format!("{whole}more\n"));
        bad.push(whole.replace("hex-guest 1", "hex-guest 2"));
        bad.push(whole.replace("pid 4242", "pid 4294967296"));
        bad.push(whole.replace("pid 4242", "pid -1"));
        bad.push("x".repeat(usize::try_from(MAX_BYTES).unwrap() + 1));
        for text in bad {
            std::fs::write(&path, &text).unwrap();
            match read(&path) {
                Err(Refusal::Broken(why)) => {
                    assert!(why.contains(&path.display().to_string()), "{why}")
                }
                other => panic!("{text:?} read as {other:?}"),
            }
        }
    }

    /// Native plan step 8: a vsock path holding a line break, or empty, is refused before anything is
    /// written, so no record can be read back as another.
    #[test]
    fn a_vsock_path_that_would_break_the_record_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guest");
        for vsock in ["/a\npid 1", ""] {
            let id = shards_api::Identity {
                vsock: PathBuf::from(vsock),
                ..identity()
            };
            assert!(matches!(write(&path, &id), Err(Refusal::Invalid { .. })));
            assert_eq!(read(&path).unwrap(), None);
        }
    }

    proptest! {
        /// Native plan step 8 (hostile input): any bytes as a record are read or refused, never a panic.
        #[test]
        fn any_bytes_are_a_record_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let text = String::from_utf8_lossy(&bytes);
            let _ = parse(&text);
        }

        /// Native plan step 8: any identity this writes reads back as itself.
        #[test]
        fn any_identity_reads_back(pid in any::<u32>(), started in any::<u64>(), name in "[a-z0-9/._-]{1,64}") {
            let id = shards_api::Identity { pid, started, vsock: PathBuf::from(format!("/{name}")) };
            let text = format!("{HEADER}\npid {pid}\nstarted {started}\nvsock /{name}\n");
            prop_assert_eq!(parse(&text).unwrap(), id);
        }
    }
}
