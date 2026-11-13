//! A node's placement records in files: the shell `hex_sim::peer` writes through on a laptop with no
//! plane and no Amphora, and on every node of a cluster with no plane. One file per record, written
//! whole to a temp file, flushed, and renamed over the record, so a crash of the node agent or of its
//! host leaves the old record or the new one, never a torn one; a temp file a crash left is removed
//! at open.
//!
//! The rename is `std::fs::rename` on every OS: rename(2) on Linux and macOS, `MoveFileExW` with
//! `MOVEFILE_REPLACE_EXISTING` on Windows, which replaces a record another handle has open so long as
//! that handle shares delete, as every handle std opens does (an outside program holding a record
//! without it makes the write fail, typed). On Linux and macOS the directory is flushed after a rename
//! or removal, so the name change itself survives a host crash; Windows offers no directory flush
//! through std, and NTFS journals the rename itself.
//!
//! Bounds (CLAUDE.md §2): the caller supplies its read/list record count. Each record can have one
//! temporary beside it during replacement; more records or temporaries are refused, typed. Each
//! value is at most [`MAX_BYTES`], the wire message bound, on write and before a persisted read allocates.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use hex_sim::peer::{Records, Stop};

/// Historical fixture population; production readers always supply their own count.
#[cfg(test)]
pub const MOST: usize = hex_runtime::MAX_BOXES * 2 + 1;

/// CP-T7.4: one durable placement record is bounded by the protocol message it represents.
pub const MAX_BYTES: usize = hex_core::wire::MAX_MESSAGE;

const RECORD: &str = "rec";
const TEMP: &str = "tmp";

/// A node's placement records in `dir`.
#[derive(Debug)]
pub struct Files {
    dir: PathBuf,
    most: usize,
    entries: usize,
}

fn io(op: &'static str) -> impl Fn(std::io::Error) -> Stop {
    move |e| Stop::Io(format!("{op}: {e}"))
}

/// Full-width `own/ran-u32-u64-u64`, `peer-u32`, or `epoch`: the actual key grammar determines
/// filename room, independently of caller record counts. Checked widths also cover narrow usize.
fn name_bound() -> Result<usize, Stop> {
    let owner = usize::try_from(u32::MAX.ilog10())
        .ok()
        .and_then(|n| n.checked_add(1));
    let sequence = usize::try_from(u64::MAX.ilog10())
        .ok()
        .and_then(|n| n.checked_add(1));
    owner
        .zip(sequence)
        .and_then(|(owner, sequence)| {
            let key = "own---"
                .len()
                .max("ran---".len())
                .checked_add(owner)?
                .checked_add(sequence.checked_mul(2)?)?;
            let peer = "peer-".len().checked_add(owner)?;
            Some(key.max(peer).max("epoch".len()))
        })
        .ok_or_else(|| Stop::Damaged("record filename bound overflow".into()))
}

/// A record's name uses lowercase letters, digits and `-`, never path separators.
fn checked(name: &str) -> Result<&str, Stop> {
    let ok = !name.is_empty()
        && name.len() <= name_bound()?
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(name)
    } else {
        Err(Stop::Damaged(format!("a record named {name:?}")))
    }
}

/// Limit the reader before allocating. The extra byte distinguishes an exact-size value from an
/// oversized file even if it grows after open; decoding happens only after the size is established.
pub(crate) fn read_record(reader: impl Read, path: &Path) -> Result<String, Stop> {
    let mut bytes = Vec::new();
    reader
        .take(
            u64::try_from(MAX_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(io("reading a record"))?;
    if bytes.len() > MAX_BYTES {
        return Err(Stop::Damaged(format!(
            "record {} exceeds its {MAX_BYTES}-byte bound",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|e| Stop::Damaged(format!("record {} is not UTF-8: {e}", path.display())))
}

impl Files {
    /// Historical fixtures only; production callers supply their admitted read count.
    #[cfg(test)]
    pub fn open(dir: &Path) -> Result<Files, Stop> {
        Self::open_with_limit(dir, MOST)
    }

    /// Opens a caller-bounded reader/listing, made if missing, removing crash temporaries.
    /// Write admission remains the caller's responsibility; put does not scan or cache counts.
    pub fn open_with_limit(dir: &Path, most: usize) -> Result<Files, Stop> {
        // One durable record plus one temporary per admitted record is the structural limit.
        let entries = most.checked_mul(2).ok_or(Stop::Full {
            what: "placement record directory entry budget",
            bound: most,
        })?;
        fs::create_dir_all(dir).map_err(io("making the records' folder"))?;
        let files = Files {
            dir: dir.to_path_buf(),
            most,
            entries,
        };
        for path in files.listed()? {
            if path.extension().is_some_and(|e| e == TEMP) {
                fs::remove_file(&path).map_err(io("removing a temp record"))?;
            }
        }
        files.sync_dir()?;
        Ok(files)
    }

    fn path(&self, name: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{name}.{ext}"))
    }

    /// At most the caller's records and temporaries, with a checked structural directory bound.
    fn listed(&self) -> Result<Vec<PathBuf>, Stop> {
        let mut out = Vec::new();
        let (mut records, mut temps) = (0usize, 0usize);
        for entry in fs::read_dir(&self.dir).map_err(io("listing the records"))? {
            let entry = entry.map_err(io("listing the records"))?;
            if out.len() >= self.entries {
                return Err(Stop::Full {
                    what: "placement record directory entries",
                    bound: self.entries,
                });
            }
            let path = entry.path();
            let counted = match path.extension() {
                Some(ext) if ext == RECORD => Some((&mut records, "placement records")),
                Some(ext) if ext == TEMP => Some((&mut temps, "placement record temporaries")),
                _ => None,
            };
            if let Some((count, what)) = counted {
                *count = count
                    .checked_add(1)
                    .filter(|n| *n <= self.most)
                    .ok_or(Stop::Full {
                        what,
                        bound: self.most,
                    })?;
            }
            out.push(path);
        }
        Ok(out)
    }

    /// The folder's own entry flushed, so a rename or removal in it survives the host.
    #[cfg(unix)]
    fn sync_dir(&self) -> Result<(), Stop> {
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(io("flushing the records' folder"))
    }

    /// NTFS journals the rename itself; Windows offers no directory flush through std.
    #[cfg(not(unix))]
    fn sync_dir(&self) -> Result<(), Stop> {
        Ok(())
    }
}

impl Records for Files {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        let name = checked(name)?;
        if value.len() > MAX_BYTES {
            return Err(Stop::Full {
                what: "a placement record's bytes",
                bound: MAX_BYTES,
            });
        }
        let (temp, record) = (self.path(name, TEMP), self.path(name, RECORD));
        {
            let mut f = File::create(&temp).map_err(io("making a temp record"))?;
            f.write_all(value.as_bytes())
                .map_err(io("writing a temp record"))?;
            f.sync_all().map_err(io("flushing a temp record"))?;
        }
        fs::rename(&temp, &record).map_err(io("renaming a record into place"))?;
        self.sync_dir()
    }

    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        let name = checked(name)?;
        match fs::remove_file(self.path(name, RECORD)) {
            Ok(()) => self.sync_dir(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("removing a record")(e)),
        }
    }

    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        let mut out = Vec::new();
        for path in self.listed()? {
            if path.extension().is_none_or(|e| e != RECORD) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                return Err(Stop::Damaged(format!("a record at {}", path.display())));
            };
            let name = checked(name)?.to_owned();
            let value = read_record(File::open(&path).map_err(io("opening a record"))?, &path)?;
            out.push((name, value));
        }
        out.sort_unstable();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_sim::model::{NodeId, NodeSpec, Resources};
    use hex_sim::peer::World;

    /// AC-T7.4.1: caller record room admits the exact record+temp boundary, removes crash temps,
    /// and rejects an extra durable record even below the structural total-entry bound.
    #[test]
    fn records_caller_limit_counts_records_and_crash_temporaries_separately() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["epoch", "own-0-1-0"] {
            fs::write(dir.path().join(format!("{name}.rec")), "durable").unwrap();
            fs::write(dir.path().join(format!("{name}.tmp")), "torn").unwrap();
        }
        let mut files = Files::open_with_limit(dir.path(), 2).unwrap();
        assert_eq!(files.all().unwrap().len(), 2);
        assert!(!dir.path().join("epoch.tmp").exists());
        assert!(!dir.path().join("own-0-1-0.tmp").exists());
        files.put("epoch", "replaced").unwrap();
        assert_eq!(files.all().unwrap().len(), 2);
        // Write admission belongs to the core/caller; this shell bounds read/list without scanning
        // the directory on every put or retaining a second mutable count cache.
        files.put("ran-0-1-0", "excess").unwrap();
        assert!(matches!(files.all(), Err(Stop::Full { bound: 2, .. })));
        assert!(matches!(
            Files::open_with_limit(dir.path(), 2),
            Err(Stop::Full { bound: 2, .. })
        ));
        let temps = tempfile::tempdir().unwrap();
        for name in ["epoch", "own-0-1-0", "ran-0-1-0"] {
            fs::write(temps.path().join(format!("{name}.tmp")), "torn").unwrap();
        }
        assert!(matches!(
            Files::open_with_limit(temps.path(), 2),
            Err(Stop::Full { bound: 2, .. })
        ));
        assert_eq!(fs::read_dir(temps.path()).unwrap().count(), 3);
    }

    /// AC-T7.4.1: checked structural entry-count overflow refuses before making a directory;
    /// a zero-record reader permits only an empty directory and no hidden picked minimum.
    #[test]
    fn records_caller_limit_checks_overflow_and_zero_before_listing() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("not-created");
        assert!(matches!(
            Files::open_with_limit(&absent, usize::MAX),
            Err(Stop::Full { .. })
        ));
        assert!(!absent.exists());
        let mut files = Files::open_with_limit(root.path(), 0).unwrap();
        assert!(files.all().unwrap().is_empty());
        files
            .put("epoch", "caller exceeded zero write admission")
            .unwrap();
        assert!(matches!(files.all(), Err(Stop::Full { bound: 0, .. })));
    }

    /// AC-T7.4.1: the widest actual key and peer names fit; an additional byte and invalid path
    /// characters refuse before write, and an oversized persisted record name is damaged.
    #[test]
    fn records_names_derive_their_limit_from_full_width_key_grammar() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open_with_limit(dir.path(), 3).unwrap();
        let widest = format!("own-{}-{}-{}", u32::MAX, u64::MAX, u64::MAX);
        let ran = format!("ran-{}-{}-{}", u32::MAX, u64::MAX, u64::MAX);
        let peer = format!("peer-{}", u32::MAX);
        for name in [&widest, &ran, &peer] {
            files.put(name, "valid").unwrap();
        }
        assert_eq!(files.all().unwrap().len(), 3);
        for invalid in [
            format!("{widest}0"),
            "../epoch".into(),
            "own_0".into(),
            "A".into(),
        ] {
            assert!(matches!(files.put(&invalid, "bad"), Err(Stop::Damaged(_))));
        }
        files.remove(&peer).unwrap();
        fs::write(dir.path().join(format!("{widest}0.rec")), "hostile").unwrap();
        assert!(matches!(files.all(), Err(Stop::Damaged(_))));
    }

    /// CP-T7.4, laptop-only state in files: do write, replace and remove records, with one replaced
    /// while another handle has it open for reading, and a temp file a crash left beside them; expect
    /// each read back whole, the replaced one new, the removed one gone, and the temp gone at open, on
    /// every OS (Windows' rename replaces a file a std handle has open, since std shares delete).
    #[test]
    fn records_are_written_whole_and_replaced_by_rename_on_every_os() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = Files::open(dir.path()).unwrap();
        f.put("epoch", "1").unwrap();
        f.put("own-0-1-0", "asked 0 1 1 0 0").unwrap();
        let reader = File::open(dir.path().join("own-0-1-0.rec")).unwrap();
        f.put("own-0-1-0", "at 0 1 1 0 0").unwrap();
        drop(reader);
        f.put("ran-0-1-0", "0 running 1 1 0 0").unwrap();
        f.remove("ran-0-1-0").unwrap();
        f.remove("ran-0-1-0").unwrap();
        fs::write(dir.path().join("own-0-1-1.tmp"), "torn").unwrap();
        let mut again = Files::open(dir.path()).unwrap();
        assert_eq!(
            again.all().unwrap(),
            vec![
                ("epoch".to_owned(), "1".to_owned()),
                ("own-0-1-0".to_owned(), "at 0 1 1 0 0".to_owned()),
            ]
        );
        assert!(!dir.path().join("own-0-1-1.tmp").exists());
        for bad in ["../x", "a/b", "A", "", "own_0"] {
            assert!(
                matches!(again.put(bad, "x"), Err(Stop::Damaged(_))),
                "{bad}"
            );
        }
    }

    /// CP-T7.4: oversized input is refused before opening a temporary or replacing durable state;
    /// the wire bound itself still round-trips exactly, measured in bytes.
    #[test]
    fn records_refuse_oversized_put_without_changing_durable_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path()).unwrap();
        files.put("epoch", "old").unwrap();
        let oversized = "x".repeat(MAX_BYTES + 1);
        assert!(matches!(
            files.put("epoch", &oversized),
            Err(Stop::Full {
                bound: MAX_BYTES,
                ..
            })
        ));
        assert!(matches!(
            files.put("own-0-1-0", &oversized),
            Err(Stop::Full {
                bound: MAX_BYTES,
                ..
            })
        ));
        assert_eq!(files.all().unwrap(), [("epoch".into(), "old".into())]);
        for name in ["epoch.tmp", "own-0-1-0.tmp", "own-0-1-0.rec"] {
            assert!(!dir.path().join(name).exists());
        }
        let exact = "x".repeat(MAX_BYTES);
        files.put("epoch", &exact).unwrap();
        assert_eq!(files.all().unwrap(), [("epoch".into(), exact)]);
    }

    /// CP-T7.4: hostile persisted bytes are damaged records, not unbounded allocations or UTF-8 I/O
    /// failures. A reader spy proves only the bound plus its sentinel byte is consumed.
    #[test]
    fn records_bound_hostile_reads_before_decoding() {
        struct Counted {
            input: std::io::Cursor<Vec<u8>>,
            consumed: usize,
        }
        impl Read for Counted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.input.read(buf)?;
                self.consumed += n;
                Ok(n)
            }
        }
        let mut source = Counted {
            input: std::io::Cursor::new(vec![b'x'; MAX_BYTES * 2]),
            consumed: 0,
        };
        assert!(matches!(
            read_record(&mut source, Path::new("hostile.rec")),
            Err(Stop::Damaged(_))
        ));
        assert_eq!(source.consumed, MAX_BYTES + 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epoch.rec");
        let hostile = File::create(&path).unwrap();
        hostile
            .set_len(u64::try_from(MAX_BYTES + 1).unwrap())
            .unwrap();
        drop(hostile);
        let mut files = Files::open(dir.path()).unwrap();
        assert!(matches!(files.all(), Err(Stop::Damaged(_))));
        fs::write(&path, [0xff]).unwrap();
        assert!(matches!(files.all(), Err(Stop::Damaged(_))));
    }

    /// CP-T7.4, laptop-only: do run one node with no plane and no Amphora, its records in files, for 6
    /// seeds of 120 steps under the crash nemesis (its agent crashing at any write, before or after it
    /// lands, and starting again from the files while its boxes run on); expect every box asked and
    /// answered created once and its end told (lost-or-double 0), and nothing left in its ledger or its
    /// records after every box ended; and (non-vacuity) crashes struck and boxes ran.
    #[test]
    fn a_laptop_keeps_every_box_once_with_its_records_in_files() {
        let (mut started, mut crashes) = (0u64, 0u64);
        for seed in 0..6 {
            let dir = tempfile::tempdir().unwrap();
            let capacity = Resources {
                cpu_milli: 4_000,
                mem_bytes: 1 << 30,
                ..Resources::default()
            };
            let mut w = World::new(
                vec![NodeSpec::new(NodeId(0), capacity)],
                vec![Files::open(dir.path()).unwrap()],
                seed,
            )
            .unwrap();
            for _ in 0..120 {
                w.step().unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
            }
            let r = w
                .finish(1 << 16)
                .unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
            assert_eq!(r.lost_or_double(), 0, "seed {seed}: {r:?}");
            assert_eq!(
                Files::open(dir.path()).unwrap().all().unwrap().len(),
                1,
                "seed {seed}: only the epoch is left"
            );
            started += r.started;
            crashes += r.crashes;
        }
        assert!(
            started > 0 && crashes > 0,
            "started {started}, crashes {crashes}"
        );
    }
}
