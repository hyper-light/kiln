//! The directory as a replicated group's state (docs/design/reshard.md, "What is left", 1): a
//! [`hyper_durable::StateMachine`] whose entries are the directory's [`Change`]s. Every member that
//! applies the same committed log holds the same [`Directory`], so routers on any member route by
//! one map, and the group's leader runs the [`Driver`] with its term as the driver's epoch: an
//! answer to a deposed leader's ask never matches the new leader's.
//!
//! The machine keeps one thing durable of its own: the last image it installed, in one file
//! (`kept`), written whole and in place before [`StateMachine::install`] returns (hyper-durable's
//! `docs/durable.md`, I8: the log's start moves past the image only after). A restart opens at that
//! image and the shell replays what the log states committed after it (§4.3), so a member that
//! installed an image needs no catch-up from its leader after a restart. Every entry is acted on at
//! start (the driver takes each operation up again from its phase), so each is applied only once
//! the member's durable commit covers it (§4.1, I5). Its images are the whole directory, which the
//! plan's merge rule keeps to at most `8C + 1` groups (`plan`), checked whole before one is
//! installed or opened.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use hyper_durable::{EntryRef, Fatal, Point, StateMachine};
use hyper_raft::proto::{ConfChangeV2, ConfState};

/// The memory budget's room for one image now (SB.1), as the directory's machine fails: a budget
/// that can't be read stops the machine as a directory image past it would.
fn image_room() -> Result<usize, Fatal> {
    crate::replication::image_room().map_err(|_| Fatal("the process's memory budget can't be read"))
}

use super::directory::{Change, Directory};
use super::driver::{Driver, Out};
use super::{NodeId, Refused};

/// A change as the directory group's log carries it.
pub fn encode(change: &Change) -> Result<Vec<u8>, Refused> {
    let bytes = postcard::to_allocvec(change)
        .map_err(|_| Refused::Invalid("a directory change that doesn't encode"))?;
    if bytes.len() > crate::replication::COMMAND_BYTES {
        return Err(Refused::Invalid(
            "a directory change larger than a replicated command",
        ));
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<Change, Refused> {
    if bytes.len() > crate::replication::COMMAND_BYTES {
        return Err(Refused::Invalid(
            "a directory change larger than a replicated command",
        ));
    }
    postcard::from_bytes(bytes)
        .map_err(|_| Refused::Invalid("a directory entry that doesn't decode"))
}

/// The last image a member installed, as its file keeps it: the directory, the point it is of
/// and the configuration the group held there (hyper-raft's `ConfState`, field by field).
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    index: u64,
    term: u64,
    voters: Vec<u64>,
    learners: Vec<u64>,
    voters_outgoing: Vec<u64>,
    learners_next: Vec<u64>,
    auto_leave: bool,
    directory: Directory,
}

/// `bytes` in place of `file`, durably: written beside it, flushed, renamed over it, and the
/// rename flushed with its folder, so a crash leaves the old file or the new one, whole.
pub(crate) fn replace(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let folder = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut next = file.as_os_str().to_owned();
    next.push(".new");
    let next = PathBuf::from(next);
    {
        let mut f = std::fs::File::create(&next)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&next, file)?;
    open_folder(folder)?.sync_all()
}

#[cfg(not(windows))]
fn open_folder(folder: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(folder)
}

/// Windows opens a folder only with `FILE_FLAG_BACKUP_SEMANTICS` (the Win32 API's flag, its value
/// from `winbase.h`), and flushes a folder's handle with `FlushFileBuffers`, as `sync_all` does.
#[cfg(windows)]
fn open_folder(folder: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(folder)
}

/// The directory group's state machine.
#[derive(Debug, Clone)]
pub struct DirectoryMachine {
    directory: Directory,
    applied: Point,
    configuration: ConfState,
    /// The point of the last image installed (else the point before the log's first entry): what
    /// a restart opens at, replaying the rest.
    start: Point,
    /// Where the last image installed is kept.
    kept: PathBuf,
}

impl DirectoryMachine {
    /// A directory group's member, keeping its images in `kept`: opened at the image kept there,
    /// or, with none, at its log's start, one group holding every position on `node` (AC-T1.9.1)
    /// under `configuration`. A kept image that doesn't decode, or whose spans don't cover every
    /// position once, is refused: the member is not opened on a directory it can't trust.
    pub fn open(node: NodeId, configuration: ConfState, kept: PathBuf) -> Result<Self, Fatal> {
        let fresh = Self {
            directory: Directory::new(node),
            applied: Point::default(),
            configuration,
            start: Point::default(),
            kept,
        };
        let bytes = match std::fs::read(&fresh.kept) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(fresh),
            Err(_) => return Err(Fatal("the directory's kept image can't be read")),
        };
        if bytes.len() > image_room()? {
            return Err(Fatal("a kept directory image larger than a snapshot"));
        }
        let k: Kept = postcard::from_bytes(&bytes)
            .map_err(|_| Fatal("a kept directory image that doesn't decode"))?;
        k.directory.check().map_err(|_| {
            Fatal("a kept directory image whose spans don't cover every position once")
        })?;
        let at = Point {
            index: k.index,
            term: k.term,
        };
        Ok(Self {
            directory: k.directory,
            applied: at,
            configuration: ConfState {
                voters: k.voters,
                learners: k.learners,
                voters_outgoing: k.voters_outgoing,
                learners_next: k.learners_next,
                auto_leave: k.auto_leave,
            },
            start: at,
            kept: fresh.kept,
        })
    }

    /// The directory as of the last entry applied: what routers on this member route by.
    #[must_use]
    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    /// The last entry applied.
    #[must_use]
    pub fn applied(&self) -> Point {
        self.applied
    }

    /// This member has become the group's leader in `term`: the driver it runs, taking every
    /// operation up again from its phase, and what it does first. Each [`Out::Commit`] is proposed
    /// to the group ([`encode`]), and an action that follows it is carried out only once it has
    /// applied (docs/design/reshard.md, "The directory").
    pub fn lead(&self, term: u64) -> Result<(Driver, Vec<Out>), Refused> {
        Driver::recover(self.directory.clone(), term)
    }
}

impl StateMachine for DirectoryMachine {
    /// Each entry's directory generation after it, or why its change was refused. A refused
    /// change is refused alike on every member (the directory is deterministic) and changes
    /// nothing, so it is the proposer's answer, never a member's failure.
    type Answer = Result<u64, Refused>;

    fn apply(
        &mut self,
        entry: &EntryRef<'_>,
        answers: &mut Vec<Self::Answer>,
    ) -> Result<(), Fatal> {
        self.applied = Point {
            index: entry.index,
            term: entry.term,
        };
        // A leader's empty entry on taking office carries no change.
        if entry.data.is_empty() {
            return Ok(());
        }
        let answer = decode(entry.data).and_then(|change| {
            self.directory.apply(&change)?;
            Ok(self.directory.generation())
        });
        answers.push(answer);
        Ok(())
    }

    fn apply_change(
        &mut self,
        at: Point,
        _change: &ConfChangeV2,
        configuration: &ConfState,
    ) -> Result<(), Fatal> {
        self.applied = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn durable(&self) -> Point {
        self.start
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        true
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        into.clear();
        let bytes = postcard::to_allocvec(&self.directory)
            .map_err(|_| Fatal("the directory doesn't encode"))?;
        if bytes.len() > image_room()? {
            return Err(Fatal("the directory's image is larger than a snapshot"));
        }
        into.extend_from_slice(&bytes);
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        None
    }

    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        if image.len() > image_room()? {
            return Err(Fatal("a directory image larger than a snapshot"));
        }
        let directory: Directory = postcard::from_bytes(image)
            .map_err(|_| Fatal("a directory image that doesn't decode"))?;
        directory
            .check()
            .map_err(|_| Fatal("a directory image whose spans don't cover every position once"))?;
        let kept = Kept {
            index: at.index,
            term: at.term,
            voters: configuration.voters.clone(),
            learners: configuration.learners.clone(),
            voters_outgoing: configuration.voters_outgoing.clone(),
            learners_next: configuration.learners_next.clone(),
            auto_leave: configuration.auto_leave,
            directory,
        };
        let bytes = postcard::to_allocvec(&kept)
            .map_err(|_| Fatal("the directory's image doesn't encode to be kept"))?;
        // Durable before the machine takes it, as `install` must be (I8).
        replace(&self.kept, &bytes).map_err(|_| Fatal("the directory's image couldn't be kept"))?;
        self.directory = kept.directory;
        self.applied = at;
        self.start = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use hyper_raft::proto::EntryType;

    use super::*;
    use crate::groups::Groups;
    use crate::reshard::directory::Proposal;
    use crate::reshard::holding::{Away, Fenced};
    use crate::reshard::{GroupId, OpId, Span, position};
    use crate::store::{Config, Engine, Refused as Store};

    /// Where a test group's engine keeps its holding: it outlives the group's store, as a disk
    /// would, so a group started again finds it.
    type Kept = Arc<Mutex<Option<Vec<u8>>>>;

    /// A group's rows, in memory, and its holding, kept in `kept`: what this test's groups write.
    #[derive(Default)]
    struct Mem {
        rows: BTreeMap<String, i64>,
        kept: Kept,
    }

    /// Group `g`'s engine, on its disk in `disks`.
    fn mem(disks: &Mutex<BTreeMap<GroupId, Kept>>, g: GroupId) -> Mem {
        let kept = Arc::clone(disks.lock().unwrap().entry(g).or_default());
        Mem {
            rows: BTreeMap::new(),
            kept,
        }
    }

    impl Engine for Mem {
        type Row = (String, i64);
        type Reader = ();

        fn key(row: &Self::Row) -> &str {
            &row.0
        }

        fn apply(&mut self, rows: &[Self::Row]) -> anyhow::Result<usize> {
            for (k, v) in rows {
                self.rows.insert(k.clone(), *v);
            }
            Ok(rows.len())
        }

        fn holding(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.kept.lock().unwrap().clone())
        }

        fn keep_holding(&mut self, holding: &[u8]) -> anyhow::Result<()> {
            *self.kept.lock().unwrap() = Some(holding.to_vec());
            Ok(())
        }
    }

    /// The directory group's members, applying one committed log in order, each keeping its
    /// images in a file of its own in `dir`.
    struct Log {
        members: Vec<DirectoryMachine>,
        index: u64,
        dir: tempfile::TempDir,
    }

    impl Log {
        fn new(n: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let members = (0..n)
                .map(|i| {
                    let kept = dir.path().join(format!("member-{i}"));
                    DirectoryMachine::open(NodeId(0), ConfState::default(), kept).unwrap()
                })
                .collect();
            Self {
                members,
                index: 0,
                dir,
            }
        }

        /// `change` committed at the next index in `term`: what every member answers.
        fn commit(&mut self, term: u64, change: &Change) -> Vec<Result<u64, Refused>> {
            self.index += 1;
            let data = encode(change).unwrap();
            let entry = EntryRef {
                index: self.index,
                term,
                kind: EntryType::EntryNormal,
                context: &[],
                data: &data,
            };
            let mut answers = Vec::new();
            for m in &mut self.members {
                m.apply(&entry, &mut answers).unwrap();
            }
            answers
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    /// The first key `k0`, `k1`, ... whose position `want` takes.
    fn key(want: impl Fn(u64) -> bool) -> String {
        (0..)
            .map(|i| format!("k{i}"))
            .find(|k| want(position(k)))
            .unwrap()
    }

    fn value(groups: &Groups<Mem>, key: &str) -> Option<i64> {
        let k = key.to_owned();
        groups
            .read(
                key,
                soon(),
                |()| Ok(None),
                move |e| Ok(e.rows.get(&k).copied()),
            )
            .unwrap()
    }

    /// T-T1.9.R1 (AC-T1.9.1): do a split through the directory group's log, carrying out its
    /// steps at the groups; expect every member to hold the same directory, writes to the moving
    /// range refused while it is fenced (the pause), a router still on the old directory refused by
    /// the old group's holding after the flip, and once routed by the new directory the range
    /// written to and read from the new group, the rest staying where it was; and groups started
    /// again by a stale directory answering from the holdings they kept, not from the directory.
    #[test]
    fn t_t1_9_r1_routes_through_the_replicated_directory_as_a_split_moves_a_range() {
        let mut log = Log::new(3);
        let disks = Mutex::new(BTreeMap::new());
        let stale = log.members[0].directory().clone();
        let book = tempfile::tempdir().unwrap();
        let retired = book.path().join("retired");
        let groups = Groups::on(NodeId(0), &stale, Config::default(), &retired, |g| {
            Ok(mem(&disks, g))
        })
        .unwrap();
        let at = 1u64 << 63;
        let (low, high) = (key(|p| p < at), key(|p| p >= at));
        let t = groups.put(vec![(low.clone(), 1)]).unwrap();
        groups.durable(&t, soon()).unwrap();
        assert_eq!(t.group(), GroupId(0));

        // The leader proposes the split; every member applies it.
        let (mut driver, first) = log.members[0].lead(1).unwrap();
        assert!(first.is_empty());
        let outs = driver
            .begin(&Proposal::Split {
                group: GroupId(0),
                at,
            })
            .unwrap();
        let Some(Out::Commit(begin)) = outs.first() else {
            panic!("a split begins with its commit: {outs:?}");
        };
        let Change::Begin { op, .. } = begin.clone() else {
            panic!("{begin:?}");
        };
        assert_eq!(op.span, Span::new(at, u64::MAX).unwrap());
        assert!(log.commit(1, begin).iter().all(Result::is_ok));

        // A new leader takes the operation up from the replicated directory, under its own term.
        let (taken, _) = log.members[1].lead(2).unwrap();
        assert!(!taken.idle(), "the new leader carries the split on");

        // The new group, taking the (empty) range in.
        groups.create(op.to, || Ok(mem(&disks, op.to))).unwrap();
        groups
            .hold(op.to, soon(), |h| {
                h.receive(op.id, op.span)?;
                h.copied(op.id, None, true, 0)
            })
            .unwrap();

        // Cutover: the old group fences the range. A write to it is refused, typed; the rest
        // goes on.
        assert!(
            log.commit(2, &Change::Cutover(op.id))
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(
            groups
                .hold(op.from, soon(), |h| h.fence(op.id, op.span, 1))
                .unwrap(),
            Fenced::New(1)
        );
        assert_eq!(
            groups.put(vec![(high.clone(), 2)]).err(),
            Some(Store::Away(Away::Fenced(op.id)))
        );
        assert_eq!(groups.put(vec![(low.clone(), 3)]).unwrap().group(), op.from);

        // The flip: the range is the new group's. A router still on the old directory sends the
        // write to the old group, which refuses it.
        assert!(
            log.commit(2, &Change::Flip(op.id))
                .iter()
                .all(Result::is_ok)
        );
        groups.hold(op.to, soon(), |h| h.activate(op.id)).unwrap();
        groups.hold(op.from, soon(), |h| h.release(op.id)).unwrap();
        assert_eq!(
            groups.put(vec![(high.clone(), 4)]).err(),
            Some(Store::Away(Away::Elsewhere))
        );

        // Routed by the replicated directory: the range goes to the new group.
        groups.route(log.members[2].directory()).unwrap();
        assert_eq!(groups.group(&high), Some(op.to));
        assert_eq!(groups.group(&low), Some(op.from));
        let t = groups.put(vec![(high.clone(), 5)]).unwrap();
        assert_eq!(t.group(), op.to);
        groups.durable(&t, soon()).unwrap();
        assert_eq!(value(&groups, &high), Some(5));
        assert_eq!(value(&groups, &low), Some(3));
        assert!(log.commit(2, &Change::End(op.id)).iter().all(Result::is_ok));

        // Every member holds the same directory, every position in exactly one span.
        let d = log.members[0].directory().clone();
        d.check().unwrap();
        assert_eq!(d.spans().len(), 2);
        assert!(log.members.iter().all(|m| *m.directory() == d));
        // An older directory doesn't replace a newer one.
        let before = groups.generation();
        groups.route(&Directory::new(NodeId(0))).unwrap();
        assert_eq!(groups.generation(), before);

        // Started again by the directory from before the split: the old group answers from the
        // holding it kept, which no longer holds the range, so the stale route is refused.
        drop(groups);
        let again = Groups::on(NodeId(0), &stale, Config::default(), &retired, |g| {
            Ok(mem(&disks, g))
        })
        .unwrap();
        assert_eq!(
            again.put(vec![(high.clone(), 6)]).err(),
            Some(Store::Away(Away::Elsewhere))
        );
        assert_eq!(again.put(vec![(low, 7)]).unwrap().group(), op.from);
        again.create(op.to, || Ok(mem(&disks, op.to))).unwrap();
        again.route(log.members[2].directory()).unwrap();
        assert_eq!(again.put(vec![(high, 8)]).unwrap().group(), op.to);
    }

    /// T-T1.9.R2: a change the directory refuses is refused alike on every member and changes
    /// nothing; an image carries the directory whole to a member that lacks the log, and an image
    /// that doesn't decode is refused.
    #[test]
    fn t_t1_9_r2_refused_changes_and_images_leave_members_alike() {
        let mut log = Log::new(2);
        let before = log.members[0].directory().clone();
        let answers = log.commit(1, &Change::Flip(OpId(7)));
        assert_eq!(answers, vec![Err(Refused::NoOp(OpId(7))); 2]);
        assert!(log.members.iter().all(|m| *m.directory() == before));

        let begin = before
            .propose(&Proposal::Split {
                group: GroupId(0),
                at: 1,
            })
            .unwrap();
        assert!(log.commit(1, &begin).iter().all(Result::is_ok));
        let mut image = Vec::new();
        let (at, conf) = log.members[0].image(&mut image).unwrap();
        let kept = log.dir.path().join("fresh");
        let mut fresh =
            DirectoryMachine::open(NodeId(9), ConfState::default(), kept.clone()).unwrap();
        fresh.install(&image, at, &conf).unwrap();
        assert_eq!(fresh.directory(), log.members[0].directory());
        assert_eq!(fresh.durable(), at);

        let cut = image.len() / 2;
        assert!(fresh.install(&image[..cut], at, &conf).is_err());
        assert_eq!(fresh.directory(), log.members[0].directory());

        // A restart opens at the image installed, with no catch-up from the leader.
        let again = DirectoryMachine::open(NodeId(9), ConfState::default(), kept.clone()).unwrap();
        assert_eq!(again.directory(), log.members[0].directory());
        assert_eq!((again.durable(), again.applied()), (at, at));
        assert_eq!(again.configuration(), &conf);

        // A kept image cut short is refused at open, never taken for a directory.
        let whole = std::fs::read(&kept).unwrap();
        std::fs::write(&kept, &whole[..whole.len() / 2]).unwrap();
        assert!(DirectoryMachine::open(NodeId(9), ConfState::default(), kept).is_err());
    }
}
