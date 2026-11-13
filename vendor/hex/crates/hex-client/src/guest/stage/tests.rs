//! The host packer's tests (docs/design/shards-guest.md §4a, HEX-GUEST-15; native plan step 5): a host
//! tree packs to the entries it holds, each parent before its children, files with their bytes; what is
//! never staged (a device node, a socket) is refused by its path; and the stage's bounds are refused
//! before anything past them is sent.

use std::collections::BTreeMap;

#[cfg(unix)]
use super::super::own_duplex;
use super::*;
use hex_core::stream::files::{LIMITS, Step, Unpack};

/// A whole stage of `items` for `id` as the packer sends it to a relay: its opening, every entry, its end.
pub(super) fn pack(
    out: &mut dyn Write,
    id: &BoxId,
    items: &[Item],
    limits: &Limits,
) -> Result<(), Refusal> {
    let digests: Vec<Option<files::Digest>> = items
        .iter()
        .filter_map(|item| match &item.source {
            Source::Layers(layers) => Some(layers),
            _ => None,
        })
        .flatten()
        .map(|l| l.digest)
        .collect();
    let opening = files::open_stage(id.as_str(), &digests).map_err(refused)?;
    out.write_all(&opening)
        .map_err(|e| Refusal::os("sending staged files", e))?;
    // A guest that holds none of the layers.
    // A guest that admits what it lacks: every named layer is checked.
    pack_items(out, items, limits, &vec![false; digests.len()], true)?;
    out.write_all(&files::END)
        .map_err(|e| Refusal::os("sending staged files", e))
}

/// `paths` as an image's layers, each named by its digest: its bytes' sha256.
pub(super) fn layers_of(paths: &[PathBuf]) -> Vec<Layer> {
    paths
        .iter()
        .map(|path| Layer {
            path: path.clone(),
            digest: Some(
                <sha2::Sha256 as sha2::Digest>::digest(std::fs::read(path).unwrap()).into(),
            ),
        })
        .collect()
}

pub(super) fn id() -> BoxId {
    BoxId::new("box-1").unwrap()
}

/// What an unpacker reads from a stage: each entry by its path, a file's bytes beside it, in order.
pub(super) fn unpack(bytes: &[u8]) -> Vec<(Entry, Vec<u8>)> {
    let mut u = Unpack::new(LIMITS);
    let mut out: Vec<(Entry, Vec<u8>)> = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (took, step) = u.take(rest).unwrap();
        match step {
            Some(Step::Entry(e)) => out.push((e, Vec::new())),
            Some(Step::Data { .. }) => out.last_mut().unwrap().1.extend(&rest[..took]),
            Some(Step::Open { id, .. }) => assert_eq!(id, "box-1"),
            _ => {}
        }
        rest = &rest[took..];
    }
    u.finish().unwrap();
    out
}

/// HEX-GUEST-15: pack a folder holding a subfolder, a file, an empty file and (on Unix) a symlink, and
/// a bytes item: expect each as an entry under its item's name, the file's bytes after it, every
/// parent before its children.
#[test]
fn a_host_tree_packs_to_its_entries_parents_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("rootfs");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/sh"), b"#!sh\n").unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/usr/lib", root.join("lib")).unwrap();
    let items = [
        Item {
            name: "root".to_owned(),
            source: Source::Path(root),
        },
        Item {
            name: "config.json".to_owned(),
            source: Source::Bytes(b"{}".to_vec()),
        },
    ];
    let mut out = Vec::new();
    pack(&mut out, &id(), &items, &LIMITS).unwrap();
    let entries = unpack(&out);
    let order: Vec<&str> = entries.iter().map(|(e, _)| e.path.as_str()).collect();
    for (i, path) in order.iter().enumerate() {
        if let Some((parent, _)) = path.rsplit_once('/') {
            assert!(order[..i].contains(&parent), "{path} came before {parent}");
        }
    }
    let by: BTreeMap<&str, &(Entry, Vec<u8>)> =
        entries.iter().map(|e| (e.0.path.as_str(), e)).collect();
    assert_eq!(by["root"].0.what, What::Dir);
    assert_eq!(by["root/bin"].0.what, What::Dir);
    assert_eq!(by["root/bin/sh"].0.what, What::File { size: 5 });
    assert_eq!(by["root/bin/sh"].1, b"#!sh\n");
    assert_eq!(by["root/empty"].0.what, What::File { size: 0 });
    assert_eq!(by["config.json"].1, b"{}");
    assert_eq!(by["config.json"].0.mode, 0o644);
    #[cfg(unix)]
    assert_eq!(
        by["root/lib"].0.what,
        What::Symlink {
            target: "/usr/lib".to_owned()
        }
    );
    assert_eq!(entries.len(), if cfg!(unix) { 6 } else { 5 });
}

/// HEX-GUEST-15: pack a root holding a device node and one holding a socket: expect each refused
/// naming the path and its kind, nothing staged of it.
#[cfg(unix)]
#[test]
fn a_device_node_or_socket_is_refused_by_its_path() {
    let item = |p: &Path| Item {
        name: "root".to_owned(),
        source: Source::Path(p.to_owned()),
    };
    let why = pack(
        &mut Vec::new(),
        &id(),
        &[item(Path::new("/dev/null"))],
        &LIMITS,
    )
    .unwrap_err();
    assert!(
        why.to_string()
            .contains("/dev/null: a device node is never staged"),
        "{why}"
    );
    let dir = tempfile::tempdir().unwrap();
    let _sock = std::os::unix::net::UnixListener::bind(dir.path().join("s.sock")).unwrap();
    let why = pack(&mut Vec::new(), &id(), &[item(dir.path())], &LIMITS).unwrap_err();
    assert!(
        why.to_string().contains("s.sock: a socket is never staged"),
        "{why}"
    );
}

/// HEX-GUEST-15: pack past the entries bound and past the bytes bound: expect each refused as
/// exhausted, naming the bound, before the entry past it is sent.
#[test]
fn a_stage_past_its_bounds_is_refused_before_it_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    for n in ["a", "b", "c"] {
        std::fs::write(dir.path().join(n), b"12345").unwrap();
    }
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Path(dir.path().to_owned()),
    }];
    let few = Limits {
        entries: 3,
        ..LIMITS
    };
    let mut out = Vec::new();
    assert_eq!(
        pack(&mut out, &id(), &items, &few),
        Err(Refusal::Exhausted {
            what: "entries in one stage",
            bound: 3
        })
    );
    let small = Limits { bytes: 9, ..LIMITS };
    assert_eq!(
        pack(&mut Vec::new(), &id(), &items, &small),
        Err(Refusal::Exhausted {
            what: "file and xattr bytes in one stage",
            bound: 9
        })
    );
}

/// A relay stand-in: each dial a socket pair whose far end answers a stage's opening with `budget`, then
/// reads to the stream's in-band end, as the relay does (or to the connection's end, where the packer refused
/// part way and ended its sending half), and answers `Done`.
#[cfg(unix)]
struct Budgeted {
    budget: (u64, u64),
}

#[cfg(unix)]
impl super::super::Link for Budgeted {
    fn dial(&self, _port: u32) -> Result<DuplexOwner, Refusal> {
        use std::os::unix::net::UnixStream;
        let (near, far) = UnixStream::pair().unwrap();
        let budget = self.budget;
        std::thread::spawn(move || serve_stage(far, budget, None));
        own_duplex(near)
    }
}

/// Where a [`Held`] relay waits for its test's word.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Before it answers the stage's opening.
    Opening,
    /// Past the budget it answered, before it reads the stream.
    Reading,
}

/// A relay stand-in as [`Budgeted`], held where `at` says until its test lets it go: its one dial's relay.
#[cfg(unix)]
struct Held {
    at: Hold,
    go: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[cfg(unix)]
impl super::super::Link for Held {
    fn dial(&self, _port: u32) -> Result<DuplexOwner, Refusal> {
        use std::os::unix::net::UnixStream;
        let (near, far) = UnixStream::pair().unwrap();
        let go = self.go.lock().unwrap().take().expect("dialed once");
        let at = self.at;
        std::thread::spawn(move || serve_stage(far, (8, u64::MAX), Some((at, go))));
        own_duplex(near)
    }
}

/// A relay's far end serving one stage, as the relay does: it answers the opening with `budget`, reads to the
/// stream's in-band end (or to the connection's end, where the packer refused part way and ended its sending half),
/// and answers `Done`; held, where `held` says, until its test's word comes or its test has gone.
#[cfg(unix)]
fn serve_stage(
    mut far: std::os::unix::net::UnixStream,
    (entries, bytes): (u64, u64),
    held: Option<(Hold, std::sync::mpsc::Receiver<()>)>,
) {
    let wait_at = |at: Hold| {
        if let Some((hold, go)) = &held
            && *hold == at
        {
            let _ = go.recv();
        }
    };
    // OPEN_BYTES, the id `box-1`, and a stage's count of layers (none).
    let mut open = [0u8; 10 + 5 + 2];
    far.read_exact(&mut open).unwrap();
    wait_at(Hold::Opening);
    let budget = hex_core::stream::frame(
        &files::budget(&files::Budget {
            entries,
            bytes,
            present: vec![],
            capacity: 1,
            admitting: true,
        }),
        wire::MAX_MESSAGE,
    )
    .unwrap();
    far.write_all(&budget).unwrap();
    wait_at(Hold::Reading);
    let mut u = Unpack::new(LIMITS);
    let mut buf = vec![0u8; 64 << 10];
    let mut n = open.len();
    buf[..n].copy_from_slice(&open);
    while n > 0 {
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            let Ok((took, _)) = u.take(rest) else { break };
            rest = &rest[took..];
        }
        if u.finish().is_ok() {
            break;
        }
        n = far.read(&mut buf).unwrap_or(0);
    }
    let done = wire::answer(0, &Answer::Done, wire::MAX_MESSAGE).unwrap();
    let _ = far.write_all(&hex_core::stream::frame(&done, wire::MAX_MESSAGE).unwrap());
}

/// A file of `n` bytes, `config.json`, as a stage's item.
#[cfg(unix)]
fn bytes_item(n: usize) -> Item {
    Item {
        name: "config.json".to_owned(),
        source: Source::Bytes(vec![b'x'; n]),
    }
}

/// A stage of `items` for [`id`] on a thread of its own, through `relay`, its waits as `wait` has learned them.
#[cfg(unix)]
fn staging(
    relay: Held,
    wait: &'static Patience,
    items: Vec<Item>,
) -> std::thread::JoinHandle<Result<Staged, Refusal>> {
    std::thread::spawn(move || stage_with(&relay, &id(), &items, &LIMITS, wait))
}

/// Waits until `wait` has run out, told (its next wait following), or `stage` has ended: at most 30 s, a bound so a
/// stage that does neither fails the test rather than hanging it.
#[cfg(unix)]
fn ran_out_or_ended(wait: &Patience, stage: &std::thread::JoinHandle<Result<Staged, Refusal>>) {
    let began = std::time::Instant::now();
    while wait.deadline().is_some() && !stage.is_finished() {
        assert!(
            began.elapsed() < std::time::Duration::from_secs(30),
            "the stage neither ran out nor ended"
        );
        std::thread::yield_now();
    }
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): a staging
/// connection's read timed out at W*, the slowest answer a stage had had, and the stage was refused (EAGAIN on
/// "reading the relay's staging answer", hex-client's a_stage_spends_the_relays_learned_budget and seven hex-node
/// relay::tests in run r433), though its relay was there to answer. Do: a wait whose W* is 1 ms; a relay that answers
/// the stage's opening only once the stage's wait has run out (told) or the stage has ended. Expect: the stage staged,
/// never refused at W*, and W* risen to its answer.
#[cfg(unix)]
#[test]
fn original_a_stage_whose_relay_answers_past_w_star_is_followed_to_its_answer() {
    static WAIT: Patience = Patience::measuring("test.hex_client.stage.answer_past");
    let learned = std::time::Duration::from_millis(1);
    WAIT.answered(learned);
    let (go, held) = std::sync::mpsc::channel();
    let relay = Held {
        at: Hold::Opening,
        go: std::sync::Mutex::new(Some(held)),
    };
    let stage = staging(relay, &WAIT, vec![bytes_item(4)]);
    ran_out_or_ended(&WAIT, &stage);
    let _ = go.send(());
    let staged = stage.join().unwrap();
    assert!(staged.is_ok(), "refused at W*: {staged:?}");
    assert!(
        WAIT.slowest().is_some_and(|slowest| slowest > learned),
        "W* didn't rise to the answer past it: {:?}",
        WAIT.slowest()
    );
}

/// Decision 9 (original; the person's ruling of 2026-11-13, and past W* a connection followed by its relay's life
/// alone, as at every local site): a staging connection's write timed out at W* and the stage was refused, though its
/// relay went on to read it. Do: a wait whose W* is 1 ms; a relay that answers the opening at once, but reads the
/// stream only once the stage's wait has run out (told) and a hundred windows of W* have passed, or the stage has
/// ended; an item larger than a connection holds unread (16 MiB). Expect: the stage not ended while its relay holds,
/// then staged, its writes followed past W* by the relay's life.
#[cfg(unix)]
#[test]
fn original_a_stage_its_relay_reads_past_w_star_is_followed_while_it_reads() {
    static WAIT: Patience = Patience::measuring("test.hex_client.stage.writes_past");
    let learned = std::time::Duration::from_millis(1);
    WAIT.answered(learned);
    let (go, held) = std::sync::mpsc::channel();
    let relay = Held {
        at: Hold::Reading,
        go: std::sync::Mutex::new(Some(held)),
    };
    let stage = staging(relay, &WAIT, vec![bytes_item(16 << 20)]);
    ran_out_or_ended(&WAIT, &stage);
    let told = std::time::Instant::now();
    while !stage.is_finished() && told.elapsed() < learned.saturating_mul(100) {
        std::thread::park_timeout(learned);
    }
    if stage.is_finished() {
        panic!(
            "the stage ended while its relay held: {:?}",
            stage.join().unwrap()
        );
    }
    let _ = go.send(());
    let staged = stage.join().unwrap();
    assert!(staged.is_ok(), "refused at W*: {staged:?}");
}

/// HEX-GUEST-15 (Ada's rule on constants: a learned bound): stage through a relay whose budget is 4 bytes,
/// what its file system has free, an item of 5: expect it refused by type, naming the budget, before its
/// bytes are sent; and one of 4 staged. Through one with one inode free, two entries: expect the second
/// refused.
#[cfg(unix)]
#[test]
fn a_stage_spends_the_relays_learned_budget() {
    let relay = Budgeted { budget: (8, 4) };
    let item = |n: usize| Item {
        name: "config.json".to_owned(),
        source: Source::Bytes(vec![b'x'; n]),
    };
    assert_eq!(
        stage(&relay, &id(), &[item(5)], &LIMITS),
        Err(Refusal::Exhausted {
            what: "file and xattr bytes in one stage",
            bound: 4
        })
    );
    assert_eq!(
        stage(&relay, &id(), &[item(4)], &LIMITS).map(|_| ()),
        Ok(())
    );
    // The entries too: a file system with one inode free takes one entry, not two.
    let relay = Budgeted { budget: (1, 100) };
    assert_eq!(
        stage(&relay, &id(), &[item(1), item(1)], &LIMITS),
        Err(Refusal::Exhausted {
            what: "entries in one stage",
            bound: 1
        })
    );
}

/// A relay stand-in for a box whose staged copy already exists: it reads a stage's opening, answers
/// `Refused(Exists)` as the relay does, and reports every byte the packer sent after the opening.
#[cfg(unix)]
struct RefusesAtOpening {
    sent_after: std::sync::mpsc::Sender<usize>,
}

#[cfg(unix)]
impl super::super::Link for RefusesAtOpening {
    fn dial(&self, _port: u32) -> Result<DuplexOwner, Refusal> {
        use std::os::unix::net::UnixStream;
        let (near, mut far) = UnixStream::pair().unwrap();
        let report = self.sent_after.clone();
        std::thread::spawn(move || {
            // OPEN_BYTES, the id `box-1`, and a stage's count of layers (none).
            let mut open = [0u8; 10 + 5 + 2];
            far.read_exact(&mut open).unwrap();
            let refused = wire::answer(
                0,
                &Answer::Refused(Refusal::Exists(id())),
                wire::MAX_MESSAGE,
            )
            .unwrap();
            far.write_all(&hex_core::stream::frame(&refused, wire::MAX_MESSAGE).unwrap())
                .unwrap();
            // Whatever the packer sends after the opening, until it closes its side.
            let mut rest = Vec::new();
            let _ = far.read_to_end(&mut rest);
            report.send(rest.len()).unwrap();
        });
        own_duplex(near)
    }
}

/// HEX-GUEST-15 (CLAUDE.md §3; HEX-GUEST-19's HEX-RT-1 in a real guest): stage for a box whose staged copy
/// the relay already holds, so it refuses the opening as `Exists` and closes: expect the packer to return
/// that typed `Exists`, and to have sent nothing after the opening. A packer that writes on past a refused
/// opening leaves bytes the relay never reads, and over vsock the close then resets the connection and
/// the answer is lost: the host saw "Broken pipe", never `Exists`.
#[cfg(unix)]
#[test]
fn a_stage_refused_at_its_opening_is_refused_typed_and_sends_nothing_more() {
    let (tx, rx) = std::sync::mpsc::channel();
    let relay = RefusesAtOpening { sent_after: tx };
    let items = [Item {
        name: "config.json".to_owned(),
        source: Source::Bytes(vec![b'x'; 64 << 10]),
    }];
    assert_eq!(
        stage(&relay, &id(), &items, &LIMITS),
        Err(Refusal::Exists(id()))
    );
    assert_eq!(
        rx.recv().unwrap(),
        0,
        "the packer wrote on past a refused opening"
    );
}
