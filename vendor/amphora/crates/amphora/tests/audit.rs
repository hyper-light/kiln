//! The audit log (CP-T1.7, AC-T1.7.4; AL.1, docs/design/audit.md): every byte of its rows flipped
//! and each flip caught on read; history rewritten whole caught by a reader's checkpoint; the same
//! log on Amphora's own Records.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use amphora::audit::{AuditError, AuditLog, Checkpoint, Tamper};
use amphora::database::Database;
use amphora_api::{
    self as api, Changes, Cond, Consistency, Page, Records, Refusal, Rev, Row, Scan, Tx, Write,
};

const FAMILY: &str = "audit";

fn until() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// A Records store in memory whose rows a test can change behind the log's back: the get and
/// commit the audit log uses, with the API's checks and conditions; nothing else.
/// Each key's value and the revision that wrote it.
type Table = BTreeMap<Vec<u8>, (Vec<u8>, Rev)>;

#[derive(Default)]
struct Rows {
    inner: Mutex<(Table, Rev)>,
}

impl Rows {
    fn all(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .0
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect()
    }
    /// Sets a row's bytes, as someone with the store's files might; the row it displaced, if any.
    fn set(&self, key: &[u8], value: Vec<u8>) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().unwrap();
        let rev = inner.1;
        inner.0.insert(key.to_vec(), (value, rev)).map(|(v, _)| v)
    }
    fn remove(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().0.remove(key).map(|(v, _)| v)
    }
    fn copy_from(&self, other: &Rows) {
        let theirs = other.inner.lock().unwrap().0.clone();
        let mut inner = self.inner.lock().unwrap();
        inner.0 = theirs;
    }
}

impl Records for Rows {
    fn get(
        &self,
        family: &str,
        key: &[u8],
        at: Consistency,
        _until: Instant,
    ) -> Result<Option<Row>, Refusal> {
        assert_eq!(family, FAMILY);
        assert_eq!(at, Consistency::Linearizable);
        api::key(key)?;
        let inner = self.inner.lock().unwrap();
        Ok(inner.0.get(key).map(|(value, rev)| Row {
            key: key.to_vec(),
            value: value.clone(),
            rev: *rev,
        }))
    }
    fn scan(&self, _: &str, _: Scan, _: Instant) -> Result<Page, Refusal> {
        Err(api::invalid("scan", "not in this test's store"))
    }
    fn commit(&self, tx: Tx, _until: Instant) -> Result<Rev, Refusal> {
        api::transaction(&tx)?;
        let mut inner = self.inner.lock().unwrap();
        for cond in &tx.conds {
            let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = cond;
            let now = inner.0.get(key).map(|(_, rev)| *rev);
            let holds = match cond {
                Cond::Absent { .. } => now.is_none(),
                Cond::At { rev, .. } => now == Some(*rev),
            };
            if !holds {
                return Err(Refusal::Conflict {
                    family: family.clone(),
                    key: key.clone(),
                    at: now,
                });
            }
        }
        inner.1 += 1;
        let rev = inner.1;
        for write in tx.writes {
            match write {
                Write::Put { key, value, .. } => {
                    inner.0.insert(key, (value, rev));
                }
                Write::Delete { key, .. } => {
                    inner.0.remove(&key);
                }
            }
        }
        Ok(rev)
    }
    fn changes(&self, _: &str, _: Rev, _: usize, _: Instant) -> Result<Changes, Refusal> {
        Err(api::invalid("changes", "not in this test's store"))
    }
    fn wait(&self, _: &str, _: Rev, _: Instant) -> Result<Rev, Refusal> {
        Err(api::invalid("wait", "not in this test's store"))
    }
    fn changed(&self, _: &str, _: Rev, _: Instant) -> Result<Option<Rev>, Refusal> {
        Err(api::invalid("changed", "not in this test's store"))
    }
    fn head(&self, _: Instant) -> Result<Rev, Refusal> {
        Ok(self.inner.lock().unwrap().1)
    }
    fn compact(&self, _: Rev, _: Instant) -> Result<(), Refusal> {
        Err(api::invalid("compact", "not in this test's store"))
    }
}

/// Events shaped like kiln's audit records: who did what to which record, of varied lengths.
fn events(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            format!(
                "{{\"at\":{},\"who\":\"agent-{:x}\",\"what\":\"records.put\",\"family\":\"runs\",\"key\":\"{}\"}}",
                1_794_000_000_000u64 + i as u64,
                i * 7919,
                "k".repeat(i % 13)
            )
            .into_bytes()
        })
        .collect()
}

/// Appends `events` in transactions of 1 to 7 events in turn, as kiln's writers would batch them.
fn append_all<R: Records + ?Sized>(log: &mut AuditLog<'_, R>, events: &[Vec<u8>]) {
    let mut at = 0;
    let mut batch = 1;
    while at < events.len() {
        let end = (at + batch).min(events.len());
        log.append(&events[at..end], until()).unwrap();
        at = end;
        batch = batch % 7 + 1;
    }
}

/// What a row of the log holds, from its key: the read whose checks cover its bytes. `Some(i)`:
/// reading event `i` (its event and leaf-hash rows, and the subtrees beside its path); `None`: the
/// open alone (the head, and the subtrees its root is folded from).
fn covering_read(key: &[u8], size: u64) -> Option<u64> {
    match key {
        [b'h'] => None,
        [b'e', index @ ..] => Some(u64::from_be_bytes(index.try_into().unwrap())),
        [b'n', 0, index @ ..] => Some(u64::from_be_bytes(index.try_into().unwrap())),
        [b'n', level, index @ ..] => {
            let index = u64::from_be_bytes(index.try_into().unwrap());
            // A subtree is beside the paths of its sibling's leaves; with no sibling it ends the
            // tree, and the head's root is folded from it.
            let sibling = (index ^ 1) << level;
            if sibling < size { Some(sibling) } else { None }
        }
        _ => panic!("a row the log doesn't write: {key:?}"),
    }
}

/// Whether opening the log, then the read covering the changed row, refuses it as changed. Any
/// other refusal fails the test: a change must read as tampering, not as an outage.
fn detected(rows: &Rows, read: Option<u64>) -> bool {
    match AuditLog::open(rows, FAMILY, None, until()) {
        Err(AuditError::Tampered(_)) => true,
        Err(other) => panic!("refused without detecting: {other}"),
        Ok(log) => match read.map(|index| log.read(index, until())) {
            Some(Err(AuditError::Tampered(_))) => true,
            Some(Err(other)) => panic!("refused without detecting: {other}"),
            Some(Ok(_)) | None => false,
        },
    }
}

/// AC-T1.7.4 (T-AL.1.1): do write a 1,000-event log, then flip every byte of every row it wrote,
/// value and key, one at a time; expect each flip refused as tampering by the open or the read
/// that covers the row, and the log to read whole again once each flip is put back.
#[test]
fn ac_t1_7_4_flipping_every_byte_of_a_1000_event_log_is_detected_on_read() {
    let rows = Rows::default();
    let events = events(1000);
    let mut log = AuditLog::open(&rows, FAMILY, None, until()).unwrap();
    append_all(&mut log, &events);
    let head = log.head();
    assert_eq!(head.size, 1000);
    let all = rows.all();
    // The head, 1,000 events, their 1,000 leaf hashes and the 994 subtrees above them: a tree of n
    // leaves has n minus its size's set bits (6 for 1,000) perfect subtrees of level 1 or more.
    let nodes = all.iter().filter(|(k, _)| k[0] == b'n').count();
    assert_eq!(nodes, 2 * 1000 - 1000u64.count_ones() as usize);
    assert_eq!(all.len(), 1 + 1000 + nodes);

    let mut flips = 0usize;
    let mut caught = 0usize;
    for (key, value) in &all {
        let read = covering_read(key, head.size);
        for at in 0..value.len() {
            let mut changed = value.clone();
            changed[at] ^= 0xff;
            rows.set(key, changed);
            flips += 1;
            if detected(&rows, read) {
                caught += 1;
            } else {
                panic!("byte {at} of {key:?}'s value flipped and read as whole");
            }
            rows.set(key, value.clone());
        }
        for at in 0..key.len() {
            let mut moved = key.clone();
            moved[at] ^= 0xff;
            rows.remove(key);
            let displaced = rows.set(&moved, value.clone());
            flips += 1;
            if detected(&rows, read) {
                caught += 1;
            } else {
                panic!("byte {at} of key {key:?} flipped and read as whole");
            }
            rows.remove(&moved);
            if let Some(displaced) = displaced {
                rows.set(&moved, displaced);
            }
            rows.set(key, value.clone());
        }
    }
    // Non-vacuity: every byte of every row flipped once, each caught.
    let bytes: usize = all.iter().map(|(k, v)| k.len() + v.len()).sum();
    assert_eq!(flips, bytes);
    assert_eq!(caught, bytes);
    let log = AuditLog::open(&rows, FAMILY, Some(&head), until()).unwrap();
    for (i, event) in events.iter().enumerate() {
        assert_eq!(&log.read(i as u64, until()).unwrap(), event);
    }
}

/// AL.1 (T-AL.1.4): do rewrite event 3 and every hash above it so the log agrees with itself;
/// expect a reader with no checkpoint to be fooled (the reason checkpoints exist), and one holding
/// a checkpoint from before the rewrite to refuse it; and a log cut back below a checkpoint to be
/// refused as rolled back.
#[test]
fn al1_a_checkpoint_catches_history_rewritten_whole() {
    let honest = Rows::default();
    let events = events(200);
    let mut log = AuditLog::open(&honest, FAMILY, None, until()).unwrap();
    append_all(&mut log, &events[..100]);
    let seen = log.head();
    append_all(&mut log, &events[100..]);
    let now = log.head();

    let mut forged_events = events.clone();
    forged_events[3] = b"{\"what\":\"nothing happened\"}".to_vec();
    let forged = Rows::default();
    append_all(
        &mut AuditLog::open(&forged, FAMILY, None, until()).unwrap(),
        &forged_events,
    );
    honest.copy_from(&forged);
    let fooled = AuditLog::open(&honest, FAMILY, None, until()).unwrap();
    assert_eq!(fooled.read(3, until()).unwrap(), forged_events[3]);
    assert_eq!(
        AuditLog::open(&honest, FAMILY, Some(&seen), until()).err(),
        Some(AuditError::Tampered(Tamper::Inconsistent {
            trusted: 100,
            stored: 200
        }))
    );
    assert!(matches!(
        AuditLog::open(&honest, FAMILY, Some(&now), until()),
        Err(AuditError::Tampered(Tamper::Inconsistent { .. }))
    ));

    let short = Rows::default();
    append_all(
        &mut AuditLog::open(&short, FAMILY, None, until()).unwrap(),
        &events[..150],
    );
    assert_eq!(
        AuditLog::open(&short, FAMILY, Some(&now), until()).err(),
        Some(AuditError::Tampered(Tamper::RolledBack {
            trusted: 200,
            stored: 150
        }))
    );
}

/// AL.1 (T-AL.1.3): do open a 100-event log with a checkpoint of each of its sizes, taken as it
/// grew one event at a time; expect every one to verify, and a checkpoint with another root to fail.
#[test]
fn al1_every_earlier_checkpoint_is_extended() {
    let rows = Rows::default();
    let events = events(100);
    let mut log = AuditLog::open(&rows, FAMILY, None, until()).unwrap();
    let mut seen = vec![log.head()];
    for event in &events {
        seen.push(log.append(std::slice::from_ref(event), until()).unwrap());
    }
    for (size, checkpoint) in seen.iter().enumerate() {
        assert_eq!(checkpoint.size, size as u64);
        AuditLog::open(&rows, FAMILY, Some(checkpoint), until()).unwrap();
        let mut wrong = *checkpoint;
        wrong.root[0] ^= 1;
        assert!(matches!(
            AuditLog::open(&rows, FAMILY, Some(&wrong), until()),
            Err(AuditError::Tampered(Tamper::Inconsistent { .. }))
        ));
    }
}

/// AL.1 (T-AL.1.5): do keep the log on Amphora's own Records (an in-memory database), then change
/// an event, a subtree's hash and the head through Records directly; expect each read back
/// verified before, and each change refused as tampering after. And do append from two handles;
/// expect the one that read the older head refused, never a fork.
#[test]
fn al1_the_log_on_amphoras_records() {
    let db = Database::in_memory(until()).unwrap();
    let records = db.namespace("kiln").unwrap();
    let events = events(40);
    let mut log = AuditLog::open(&records, FAMILY, None, until()).unwrap();
    append_all(&mut log, &events);
    let head = log.head();
    let reopened = AuditLog::open(&records, FAMILY, Some(&head), until()).unwrap();
    for (i, event) in events.iter().enumerate() {
        assert_eq!(&reopened.read(i as u64, until()).unwrap(), event);
    }

    let mut racing = AuditLog::open(&records, FAMILY, Some(&head), until()).unwrap();
    log.append(&[b"first".as_slice()], until()).unwrap();
    assert!(matches!(
        racing.append(&[b"second".as_slice()], until()),
        Err(AuditError::Refused(Refusal::Conflict { .. }))
    ));
    let head = log.head();

    let change = |key: Vec<u8>, value: Vec<u8>| {
        records
            .commit(
                Tx {
                    conds: vec![],
                    writes: vec![Write::Put {
                        family: FAMILY.into(),
                        key,
                        value,
                    }],
                },
                until(),
            )
            .unwrap();
    };
    let get = |key: &[u8]| {
        records
            .get(FAMILY, key, Consistency::Linearizable, until())
            .unwrap()
            .unwrap()
            .value
    };
    let event_key = |i: u64| [&b"e"[..], &i.to_be_bytes()].concat();

    let original = get(&event_key(5));
    let mut changed = original.clone();
    changed[0] ^= 0xff;
    change(event_key(5), changed);
    let log = AuditLog::open(&records, FAMILY, Some(&head), until()).unwrap();
    assert_eq!(
        log.read(5, until()).err(),
        Some(AuditError::Tampered(Tamper::Leaf(5)))
    );
    change(event_key(5), original);
    log.read(5, until()).unwrap();

    // Subtree (level 1, index 3) covers events 6 and 7, beside the path of events 4 and 5.
    let node = [&b"n\x01"[..], &3u64.to_be_bytes()].concat();
    let original = get(&node);
    let mut changed = original.clone();
    changed[31] ^= 0xff;
    change(node.clone(), changed);
    assert_eq!(
        log.read(4, until()).err(),
        Some(AuditError::Tampered(Tamper::Inclusion(4)))
    );
    change(node, original);
    log.read(4, until()).unwrap();

    let original = get(b"h");
    let mut changed = original.clone();
    changed[8] ^= 0xff;
    change(b"h".to_vec(), changed);
    assert!(matches!(
        AuditLog::open(&records, FAMILY, None, until()),
        Err(AuditError::Tampered(_))
    ));
    change(b"h".to_vec(), original);
    assert_eq!(
        AuditLog::open(&records, FAMILY, Some(&head), until())
            .unwrap()
            .head(),
        Checkpoint {
            size: 41,
            root: head.root
        }
    );
}
