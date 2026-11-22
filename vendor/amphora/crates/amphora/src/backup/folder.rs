//! A folder as the archive's object store (docs/design/backup.md, "The folder store"): the store
//! Amphora owns, needing no cloud. A peer machine's folder is the same store over a mount.
//!
//! A key's segments are directories and its last segment a file. Each name is the segment's bytes
//! with everything outside `[a-z0-9_-]` written `%xx` (lowercase hex), after one letter saying what
//! it is: `d` a directory, `f` an object, `t` a create under way. So names are the same under case
//! folding (APFS and NTFS fold case by default), never `.` or `..`, never a Windows device name, and
//! a key and a longer key under it (`a`, `a/b`) live side by side (`fa`, `da/fb`).
//!
//! A create writes a temporary file beside the object's place, flushes it, and links it there:
//! link(2) refuses a name that exists, atomically, on local filesystems and NFS alike (open(2),
//! O_EXCL: "create a unique file on the same filesystem ... and use link(2) to make a link to the
//! lockfile"), so two writers never both create one key, and a reader sees an object whole or not
//! at all. The directory is flushed after (fsync(2): "Calling fsync() does not necessarily ensure
//! that the entry in the directory containing the file has also reached disk. For that an explicit
//! fsync() on a file descriptor for the directory is also needed").

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use amphora_api::blobs::{BlobRefusal, Blobs, Listing, check_key};

/// The bytes of one name in a folder at most: a format limit, not a choice. ext4, XFS and btrfs
/// (Linux's NAME_MAX, limits.h), APFS (255 UTF-8 bytes) and NTFS (255 UTF-16 units) all allow 255,
/// and the names written here are ASCII, so it is the same count on each.
const NAME_MAX: usize = 255;

/// A folder holding a backup archive's objects, each `object_max` bytes at most (a get reads an
/// object whole, so the bound is the memory the caller gives one).
pub struct FolderBlobs {
    root: PathBuf,
    object_max: usize,
}

fn unavailable(path: &Path, e: &std::io::Error) -> BlobRefusal {
    BlobRefusal::Unavailable(format!("{}: {e}", path.display()))
}

fn invalid(what: &str, why: String) -> BlobRefusal {
    BlobRefusal::Invalid {
        what: what.into(),
        why,
    }
}

fn expired(until: Instant) -> Result<(), BlobRefusal> {
    if Instant::now() >= until {
        Err(BlobRefusal::Timeout)
    } else {
        Ok(())
    }
}

/// `kind` then the segment's bytes, `[a-z0-9_-]` as they are and the rest `%xx`.
fn encode(kind: char, segment: &str) -> Result<String, BlobRefusal> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(segment.len().saturating_add(1));
    name.push(kind);
    for b in segment.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' {
            name.push(char::from(b));
        } else {
            name.push('%');
            for nibble in [b >> 4, b & 0xf] {
                let hex = HEX.get(usize::from(nibble)).copied().unwrap_or(b'0');
                name.push(char::from(hex));
            }
        }
    }
    if name.len() > NAME_MAX {
        return Err(invalid(
            "key",
            format!(
                "segment {segment:?} names a file of {} bytes, over a folder's {NAME_MAX}",
                name.len()
            ),
        ));
    }
    Ok(name)
}

/// A name's kind and segment, or none for a name not written here (a name another program left,
/// such as `desktop.ini` or `lost+found`, or one not in the form [`encode`] writes).
fn decode(name: &str) -> Option<(char, String)> {
    let mut chars = name.chars();
    let kind = chars.next().filter(|k| matches!(k, 'd' | 'f'))?;
    let mut bytes = Vec::with_capacity(name.len());
    let mut rest = chars.as_str().bytes();
    while let Some(b) = rest.next() {
        if b == b'%' {
            let digit = |c: Option<u8>| {
                c.and_then(|c| char::from(c).to_digit(16))
                    .and_then(|d| u8::try_from(d).ok())
            };
            let (hi, lo) = (digit(rest.next())?, digit(rest.next())?);
            bytes.push(hi.wrapping_shl(4) | lo);
        } else {
            bytes.push(b);
        }
    }
    let segment = String::from_utf8(bytes).ok()?;
    if encode(kind, &segment).ok()?.as_str() == name {
        Some((kind, segment))
    } else {
        None
    }
}

/// Flushes a directory's entries (the fsync(2) quote above). Windows has no portable way to open a
/// directory for flushing from std; NTFS journals its directory changes itself.
fn sync_dir(dir: &Path) -> Result<(), BlobRefusal> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| unavailable(dir, &e))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// One step of a listing's walk, in key order.
enum Item {
    Object(String, PathBuf),
    Folder(String, PathBuf),
}

impl FolderBlobs {
    /// The store in the folder at `root`, made if it isn't there.
    pub fn open(root: &Path, object_max: usize) -> Result<Self, BlobRefusal> {
        fs::create_dir_all(root).map_err(|e| unavailable(root, &e))?;
        if !root.is_dir() {
            return Err(invalid(
                "folder",
                format!("{} isn't a folder", root.display()),
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
            object_max,
        })
    }

    /// The directory names and the file name of `key`.
    fn names(key: &str) -> Result<(Vec<String>, String), BlobRefusal> {
        check_key(key)?;
        let mut segments: Vec<&str> = key.split('/').collect();
        let last = segments
            .pop()
            .ok_or_else(|| invalid("key", "empty".into()))?;
        let dirs = segments
            .into_iter()
            .map(|s| encode('d', s))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((dirs, encode('f', last)?))
    }

    fn dir_of(&self, dirs: &[String]) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(dirs);
        path
    }

    /// Makes `dirs` under the root, flushing each parent a directory was made in.
    fn make_dirs(&self, dirs: &[String]) -> Result<PathBuf, BlobRefusal> {
        let mut path = self.root.clone();
        for dir in dirs {
            let parent = path.clone();
            path.push(dir);
            match fs::create_dir(&path) {
                Ok(()) => sync_dir(&parent)?,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(unavailable(&path, &e)),
            }
        }
        Ok(path)
    }

    /// Writes `bytes` to a new temporary file in `dir`, flushed: none when `dir` is gone.
    fn write_temp(dir: &Path, bytes: &[u8]) -> Result<Option<PathBuf>, BlobRefusal> {
        let mut tag = [0u8; 8];
        getrandom::fill(&mut tag)
            .map_err(|e| BlobRefusal::Unavailable(format!("no randomness for a name: {e}")))?;
        let temp = dir.join(format!("t{:016x}", u64::from_le_bytes(tag)));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unavailable(&temp, &e)),
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written {
            Self::remove_temp(&temp);
            return Err(unavailable(&temp, &e));
        }
        Ok(Some(temp))
    }

    /// Removes a temporary file. One left behind (this failed, or the process ended first) is
    /// never listed or read: it holds space, and the event says where.
    fn remove_temp(temp: &Path) {
        if let Err(e) = fs::remove_file(temp) {
            tracing::warn!(
                target: "amphora.backup.folder",
                event = "temp_left",
                path = %temp.display(),
                error = %e,
            );
        }
    }

    /// Removes the empty directories of `dirs`, deepest first, flushing each parent; stops at the
    /// first that holds anything.
    fn prune(&self, dirs: &[String]) -> Result<(), BlobRefusal> {
        let mut depth = dirs.len();
        while depth > 0 {
            let path = self.dir_of(dirs.get(..depth).unwrap_or_default());
            if fs::remove_dir(&path).is_err() {
                return Ok(());
            }
            depth = depth.saturating_sub(1);
            sync_dir(&self.dir_of(dirs.get(..depth).unwrap_or_default()))?;
        }
        Ok(())
    }
}

impl Blobs for FolderBlobs {
    fn object_max(&self) -> usize {
        self.object_max
    }

    fn create(&self, key: &str, bytes: &[u8], until: Instant) -> Result<(), BlobRefusal> {
        expired(until)?;
        let (dirs, file) = Self::names(key)?;
        if bytes.len() > self.object_max {
            return Err(BlobRefusal::TooLarge {
                what: "object".into(),
                size: bytes.len(),
                limit: self.object_max,
            });
        }
        // A delete of the key's last neighbour may take its directory between making it and
        // linking into it; then the create makes it again, until the deadline.
        loop {
            expired(until)?;
            let dir = self.make_dirs(&dirs)?;
            let target = dir.join(&file);
            if target.try_exists().map_err(|e| unavailable(&target, &e))? {
                return Err(BlobRefusal::Exists(key.into()));
            }
            let Some(temp) = Self::write_temp(&dir, bytes)? else {
                continue;
            };
            let linked = fs::hard_link(&temp, &target);
            Self::remove_temp(&temp);
            match linked {
                Ok(()) => return sync_dir(&dir),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    return Err(BlobRefusal::Exists(key.into()));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(unavailable(&target, &e)),
            }
        }
    }

    fn get(&self, key: &str, until: Instant) -> Result<Vec<u8>, BlobRefusal> {
        expired(until)?;
        let (dirs, file) = Self::names(key)?;
        let path = self.dir_of(&dirs).join(file);
        let mut handle = match File::open(&path) {
            Ok(handle) => handle,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(BlobRefusal::NotFound(key.into()));
            }
            Err(e) => return Err(unavailable(&path, &e)),
        };
        let size = handle.metadata().map_err(|e| unavailable(&path, &e))?.len();
        let size = usize::try_from(size)
            .ok()
            .filter(|s| *s <= self.object_max)
            .ok_or_else(|| BlobRefusal::TooLarge {
                what: format!("object {key}"),
                size: usize::try_from(size).unwrap_or(usize::MAX),
                limit: self.object_max,
            })?;
        let mut bytes = Vec::with_capacity(size);
        handle
            .read_to_end(&mut bytes)
            .map_err(|e| unavailable(&path, &e))?;
        Ok(bytes)
    }

    fn list(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
        until: Instant,
    ) -> Result<Listing, BlobRefusal> {
        expired(until)?;
        if limit == 0 {
            return Err(invalid("limit", "zero".into()));
        }
        let mut listing = Listing::default();
        // The walk starts at the directory of the prefix's whole segments.
        let (whole, _) = prefix.rsplit_once('/').unwrap_or(("", prefix));
        let mut start = self.root.clone();
        let mut start_key = String::new();
        if !whole.is_empty() {
            for segment in whole.split('/') {
                if segment.is_empty() {
                    return Ok(listing);
                }
                start.push(encode('d', segment)?);
                start_key.push_str(segment);
                start_key.push('/');
            }
        }
        // Objects and folders in key order: a folder sorts as its key and a '/', so it falls among
        // its siblings where its keys do. The stack holds the siblings not yet walked, last first.
        let mut stack = vec![Item::Folder(start_key, start)];
        while let Some(item) = stack.pop() {
            expired(until)?;
            let (folder_key, folder) = match item {
                Item::Object(key, path) => {
                    if listing.objects.len() >= limit {
                        listing.truncated = true;
                        break;
                    }
                    let size = fs::metadata(&path)
                        .map_err(|e| unavailable(&path, &e))?
                        .len();
                    listing.objects.push((key, size));
                    continue;
                }
                Item::Folder(key, path) => (key, path),
            };
            let entries = match fs::read_dir(&folder) {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(unavailable(&folder, &e)),
            };
            let mut children = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| unavailable(&folder, &e))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Some((kind, segment)) = decode(name) else {
                    continue;
                };
                let path = entry.path();
                if kind == 'd' {
                    let key = format!("{folder_key}{segment}/");
                    let reaches = key.starts_with(prefix) || prefix.starts_with(&key);
                    let all_passed =
                        after.is_some_and(|a| a > key.as_str() && !a.starts_with(&key));
                    if reaches && !all_passed {
                        children.push(Item::Folder(key, path));
                    }
                } else {
                    let key = format!("{folder_key}{segment}");
                    if key.starts_with(prefix) && after.is_none_or(|a| key.as_str() > a) {
                        children.push(Item::Object(key, path));
                    }
                }
            }
            children.sort_by(|a, b| {
                let key = |i: &Item| match i {
                    Item::Object(k, _) | Item::Folder(k, _) => k.clone(),
                };
                key(b).cmp(&key(a))
            });
            stack.extend(children);
        }
        Ok(listing)
    }

    fn delete(&self, key: &str, until: Instant) -> Result<(), BlobRefusal> {
        expired(until)?;
        let (dirs, file) = Self::names(key)?;
        let dir = self.dir_of(&dirs);
        let path = dir.join(file);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(unavailable(&path, &e)),
        }
        sync_dir(&dir)?;
        self.prune(&dirs)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use amphora_api::blobs::{SimBlobs, conformance};

    use super::*;

    fn until() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// AMPHORA-BLOBS-1 to 7: the folder store passes the seam's conformance suite, and leaves
    /// nothing behind.
    #[test]
    fn amphora_blobs_conformance_on_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let store = FolderBlobs::open(dir.path(), 16).unwrap();
        conformance(&store, "suite", until()).unwrap();
        conformance(&store, "deeper/still", until()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// AMPHORA-BLOBS-8: keys that differ only in case, a key beside a longer key under it, and
    /// keys of '.', '..' and Windows device names are each their own object.
    #[test]
    fn amphora_blobs_8_folder_names_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let store = FolderBlobs::open(dir.path(), 16).unwrap();
        let keys = [
            "A", "a", "a/b", "a/B", ".", "../x", "CON", "con/nul", "a.b", "a%2f",
        ];
        for (i, key) in keys.iter().enumerate() {
            store
                .create(key, &[u8::try_from(i).unwrap()], until())
                .unwrap();
        }
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                store.get(key, until()).unwrap(),
                vec![u8::try_from(i).unwrap()]
            );
        }
        let listed: Vec<String> = store
            .list("", None, 100, until())
            .unwrap()
            .objects
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let mut sorted: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        sorted.sort();
        assert_eq!(listed, sorted);
    }

    /// AMPHORA-BLOBS-8: a segment whose name would pass a folder's name limit is refused Invalid,
    /// and a temporary file a crashed create left is never listed.
    #[test]
    fn amphora_blobs_8_long_segments_refused_temps_unlisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = FolderBlobs::open(dir.path(), 16).unwrap();
        assert!(matches!(
            store.create(&"X".repeat(100), b"", until()),
            Err(BlobRefusal::Invalid { .. })
        ));
        store.create("p/a", b"1", until()).unwrap();
        fs::write(dir.path().join("dp").join("t00"), b"torn").unwrap();
        let listing = store.list("p/", None, 10, until()).unwrap();
        assert_eq!(listing.objects, vec![("p/a".to_string(), 1)]);
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(n).unwrap()).unwrap()
        }
    }

    /// AMPHORA-BLOBS-8: random creates, deletes, gets and listings (random prefixes, cursors and
    /// limits) on the folder answer as the simulator does, the oracle.
    #[test]
    fn amphora_blobs_8_folder_answers_as_the_simulator() {
        let parts = ["a", "A", "b", ".", "a.", "z-", "é", "0"];
        for seed in 1..=4u64 {
            let dir = tempfile::tempdir().unwrap();
            let folder = FolderBlobs::open(dir.path(), 8).unwrap();
            let sim = SimBlobs::new(8, 10_000, 1 << 20);
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let key = |rng: &mut Rng| {
                let n = 1 + rng.below(3);
                (0..n)
                    .map(|_| {
                        (0..1 + rng.below(2))
                            .map(|_| parts[rng.below(parts.len())])
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            };
            let mut made = BTreeMap::new();
            for step in 0..400 {
                let k = key(&mut rng);
                match rng.below(4) {
                    0 | 1 => {
                        let bytes = vec![u8::try_from(step % 251).unwrap(); rng.below(10)];
                        let f = folder.create(&k, &bytes, until());
                        let s = sim.create(&k, &bytes, until());
                        assert_eq!(f, s, "create {k:?}");
                        if s.is_ok() {
                            made.insert(k, bytes);
                        }
                    }
                    2 => {
                        assert_eq!(folder.delete(&k, until()), sim.delete(&k, until()));
                        made.remove(&k);
                    }
                    _ => assert_eq!(folder.get(&k, until()), sim.get(&k, until())),
                }
                let full = key(&mut rng);
                let prefix: String = full
                    .chars()
                    .take(rng.below(full.chars().count() + 1))
                    .collect();
                let after = if rng.below(2) == 0 {
                    None
                } else {
                    Some(key(&mut rng))
                };
                let limit = 1 + rng.below(6);
                assert_eq!(
                    folder.list(&prefix, after.as_deref(), limit, until()),
                    sim.list(&prefix, after.as_deref(), limit, until()),
                    "list {prefix:?} after {after:?} limit {limit}"
                );
            }
            assert!(!made.is_empty(), "non-vacuous: some objects were made");
        }
    }
}
