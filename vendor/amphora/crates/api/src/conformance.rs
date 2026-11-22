//! The Records conformance suite (CP-T1.5, AC-T1.5.3): cases, each one clause of the contract,
//! run against a fresh store apiece. Amphora's `Namespace` and [`crate::sim::SimRecords`] both
//! pass every case.
//!
//! RECORDS-1 to RECORDS-14 are kiln-seams' Records cases (kiln's docs/seams.md, "Records"), ported
//! to this trait: kiln's `crates/kiln-seams/src/records.rs` at kiln 492af26f9 (blob 82f816c0; the
//! cases are unchanged since kiln 02d5f6837), each id and `says` as kiln has them, each check the
//! same requests and the same expectations. Where kiln's polls a wait once (`now_or_never`), the
//! port asks [`Records::changed`]; where kiln's awaits a wait while it commits, the port waits on
//! a thread of its own. Why a port and not kiln-seams itself: docs/design/records.md, "One suite".
//!
//! The cases after them are Amphora's own: the feed's atomic pages, index families and counters.

use std::time::{Duration, Instant};

use crate::counter::Counter;
use crate::index::Index;
use crate::{
    Change, Cond, Consistency, KEY_MAX, Page, Records, Refusal, Rev, Row, Scan, TX_MAX, Tx,
    VALUE_MAX, Write,
};

/// What a case found wrong, in words.
pub type Failure = String;

/// One clause of the contract, checked against a store under the case's deadline.
pub struct Case {
    /// The clause's id (kiln's docs/seams.md for RECORDS-n).
    pub id: &'static str,
    /// What the clause says.
    pub says: &'static str,
    pub check: fn(&dyn Records, Instant) -> Result<(), Failure>,
}

/// What a run found.
#[derive(Debug, Default)]
pub struct Report {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, Failure)>,
}

impl Report {
    /// Every case passed, and there was at least one.
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty() && !self.passed.is_empty()
    }
}

/// Runs each case against a store of its own (`make` opens one a case), in order. Each case's
/// calls carry a deadline `hang` after it starts: a store that never answers fails that case
/// (refused `Timeout`, or a wait never told), and the suite goes on.
pub fn run<R: Records>(
    cases: &[Case],
    mut make: impl FnMut() -> Result<R, Refusal>,
    hang: Duration,
) -> Report {
    let mut report = Report::default();
    for case in cases {
        let outcome = match (make(), Instant::now().checked_add(hang)) {
            (Ok(store), Some(until)) => (case.check)(&store, until),
            (Err(e), _) => Err(format!("opening its store: {e}")),
            (_, None) => Err("its deadline is past the clock's range".into()),
        };
        match outcome {
            Ok(()) => report.passed.push(case.id),
            Err(why) => report.failed.push((case.id, why)),
        }
    }
    report
}

/// `Ok` when `cond` holds, else the failure `why` says.
pub fn ensure(cond: bool, why: impl FnOnce() -> String) -> Result<(), Failure> {
    if cond { Ok(()) } else { Err(why()) }
}

/// `Ok` when `got == want`, else both, named.
pub fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), Failure> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

/// `v`'s value, or its refusal said as a failure.
pub fn fine<T>(what: &str, v: Result<T, Refusal>) -> Result<T, Failure> {
    v.map_err(|e| format!("{what}: {e}"))
}

/// The refusal `v` should have been, or a failure saying it wasn't.
pub fn refused<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<Refusal, Failure> {
    match v {
        Ok(t) => Err(format!(
            "{what}: accepted ({t:?}), should have been refused"
        )),
        Err(e) => Ok(e),
    }
}

const F: &str = "conformance";

fn put(key: &str, value: &str) -> Write {
    Write::Put {
        family: F.into(),
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
    }
}

fn tx(writes: Vec<Write>) -> Tx {
    Tx {
        conds: Vec::new(),
        writes,
    }
}

fn value(r: &dyn Records, key: &str, until: Instant) -> Result<Option<String>, Failure> {
    Ok(fine(
        "get",
        r.get(F, key.as_bytes(), Consistency::Linearizable, until),
    )?
    .map(|row| String::from_utf8_lossy(&row.value).into_owned()))
}

fn scan(prefix: &str, after: Option<&str>, limit: usize) -> Scan {
    Scan {
        prefix: prefix.as_bytes().to_vec(),
        after: after.map(|a| a.as_bytes().to_vec()),
        limit,
        at: Consistency::Linearizable,
    }
}

fn keys(p: &Page) -> Vec<String> {
    p.rows
        .iter()
        .map(|r| String::from_utf8_lossy(&r.key).into_owned())
        .collect()
}

fn c1(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let rev = fine("commit", r.commit(tx(vec![put("a", "one")]), until))?;
    let row = fine("get", r.get(F, b"a", Consistency::Linearizable, until))?;
    same(
        "the row read back",
        row,
        Some(Row {
            key: b"a".to_vec(),
            value: b"one".to_vec(),
            rev,
        }),
    )?;
    same(
        "a key never written",
        fine("get", r.get(F, b"zz", Consistency::Linearizable, until))?,
        None,
    )
}

fn c2(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let start = fine("head", r.head(until))?;
    let mut last = start;
    for i in 0..5 {
        let rev = fine(
            "commit",
            r.commit(tx(vec![put(&format!("k{i}"), "v")]), until),
        )?;
        ensure(rev > last, || {
            format!("commit {i} got revision {rev}, not above {last}")
        })?;
        last = rev;
    }
    same("head() after them", fine("head", r.head(until))?, last)
}

fn c3(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let only_new = |v: &str| Tx {
        conds: vec![Cond::Absent {
            family: F.into(),
            key: b"a".to_vec(),
        }],
        writes: vec![put("a", v)],
    };
    fine("the first create", r.commit(only_new("first"), until))?;
    let e = refused("a second create", r.commit(only_new("second"), until))?;
    ensure(matches!(e, Refusal::Conflict { .. }), || {
        format!("refused as {e:?}, not Conflict")
    })?;
    same(
        "the value after the refused create",
        value(r, "a", until)?,
        Some("first".into()),
    )
}

fn c4(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let first = fine("commit", r.commit(tx(vec![put("a", "one")]), until))?;
    let cas = |rev: Rev, v: &str| Tx {
        conds: vec![Cond::At {
            family: F.into(),
            key: b"a".to_vec(),
            rev,
        }],
        writes: vec![put("a", v)],
    };
    let second = fine(
        "a compare-and-set at the current revision",
        r.commit(cas(first, "two"), until),
    )?;
    let e = refused(
        "a compare-and-set at a stale revision",
        r.commit(cas(first, "three"), until),
    )?;
    same(
        "the refusal",
        e,
        Refusal::Conflict {
            family: F.into(),
            key: b"a".to_vec(),
            at: Some(second),
        },
    )?;
    same("the value", value(r, "a", until)?, Some("two".into()))
}

fn c5(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    fine("commit", r.commit(tx(vec![put("taken", "x")]), until))?;
    let head = fine("head", r.head(until))?;
    let bad = Tx {
        conds: vec![Cond::Absent {
            family: F.into(),
            key: b"taken".to_vec(),
        }],
        writes: vec![put("a", "1"), put("b", "2")],
    };
    refused("a transaction whose condition fails", r.commit(bad, until))?;
    same("a after it", value(r, "a", until)?, None)?;
    same("b after it", value(r, "b", until)?, None)?;
    same("the head after it", fine("head", r.head(until))?, head)?;
    let rev = fine(
        "both together",
        r.commit(tx(vec![put("a", "1"), put("b", "2")]), until),
    )?;
    let a = fine("get a", r.get(F, b"a", Consistency::Linearizable, until))?.map(|x| x.rev);
    let b = fine("get b", r.get(F, b"b", Consistency::Linearizable, until))?.map(|x| x.rev);
    same("both rows' revision", (a, b), (Some(rev), Some(rev)))
}

fn c6(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    fine("commit", r.commit(tx(vec![put("a", "one")]), until))?;
    let delete = Write::Delete {
        family: F.into(),
        key: b"a".to_vec(),
    };
    fine("delete", r.commit(tx(vec![delete]), until))?;
    same("a deleted row", value(r, "a", until)?, None)?;
    same(
        "a scan after the delete",
        keys(&fine("scan", r.scan(F, scan("", None, 10), until))?),
        Vec::<String>::new(),
    )?;
    // Absent holds once it's deleted.
    let again = Tx {
        conds: vec![Cond::Absent {
            family: F.into(),
            key: b"a".to_vec(),
        }],
        writes: vec![put("a", "again")],
    };
    fine("a create after the delete", r.commit(again, until)).map(|_| ())
}

fn c7(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let writes = ["b/2", "a/1", "b/1", "c/1", "b/3", "b/10"]
        .iter()
        .map(|k| put(k, "v"))
        .collect();
    fine("commit", r.commit(tx(writes), until))?;
    let first = fine("scan", r.scan(F, scan("b/", None, 2), until))?;
    same(
        "the first page",
        (keys(&first), first.more),
        (vec!["b/1".to_owned(), "b/10".to_owned()], true),
    )?;
    let second = fine("scan", r.scan(F, scan("b/", Some("b/10"), 2), until))?;
    same(
        "the second page",
        (keys(&second), second.more),
        (vec!["b/2".to_owned(), "b/3".to_owned()], false),
    )?;
    let e = refused("a scan of 0 rows", r.scan(F, scan("", None, 0), until))?;
    ensure(matches!(e, Refusal::Invalid { .. }), || {
        format!("refused as {e:?}")
    })
}

fn c8(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let w = |family: &str, v: &str| Write::Put {
        family: family.into(),
        key: b"k".to_vec(),
        value: v.as_bytes().to_vec(),
    };
    fine(
        "commit",
        r.commit(
            tx(vec![w("conformance-one", "1"), w("conformance-two", "2")]),
            until,
        ),
    )?;
    let one = fine(
        "get",
        r.get("conformance-one", b"k", Consistency::Linearizable, until),
    )?
    .map(|x| x.value);
    let two = fine(
        "get",
        r.get("conformance-two", b"k", Consistency::Linearizable, until),
    )?
    .map(|x| x.value);
    same(
        "one key in two families",
        (one, two),
        (Some(b"1".to_vec()), Some(b"2".to_vec())),
    )
}

fn c9(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let from = fine("head", r.head(until))?;
    let r1 = fine("commit", r.commit(tx(vec![put("a", "1")]), until))?;
    let other = Write::Put {
        family: "conformance-other".into(),
        key: b"x".to_vec(),
        value: b"y".to_vec(),
    };
    fine("another family", r.commit(tx(vec![other]), until))?;
    let r2 = fine(
        "commit",
        r.commit(tx(vec![put("a", "2"), put("b", "3")]), until),
    )?;
    let delete = Write::Delete {
        family: F.into(),
        key: b"a".to_vec(),
    };
    let r3 = fine("delete", r.commit(tx(vec![delete]), until))?;
    let feed = fine("changes", r.changes(F, from, 100, until))?;
    let want = vec![
        Change {
            rev: r1,
            key: b"a".to_vec(),
            value: Some(b"1".to_vec()),
        },
        Change {
            rev: r2,
            key: b"a".to_vec(),
            value: Some(b"2".to_vec()),
        },
        Change {
            rev: r2,
            key: b"b".to_vec(),
            value: Some(b"3".to_vec()),
        },
        Change {
            rev: r3,
            key: b"a".to_vec(),
            value: None,
        },
    ];
    same("the family's changes", feed.changes, want)?;
    let rest = fine("changes after the second", r.changes(F, r2, 100, until))?;
    same(
        "changes after a revision",
        rest.changes.iter().map(|c| c.rev).collect::<Vec<_>>(),
        vec![r3],
    )?;
    let page = fine("changes, one at a time", r.changes(F, from, 1, until))?;
    same("a limited read", page.changes.len(), 1)
}

fn c10(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let r1 = fine("commit", r.commit(tx(vec![put("a", "1")]), until))?;
    let before = r1.saturating_sub(1);
    same(
        "a wait for a change already made",
        r.changed(F, before, until),
        Ok(Some(r1)),
    )?;
    same(
        "a wait for a change already made, waiting",
        r.wait(F, before, until),
        Ok(r1),
    )?;
    same(
        "a wait with no newer change",
        r.changed(F, r1, until),
        Ok(None),
    )?;
    let (waited, committed) = std::thread::scope(|s| {
        let waiting = s.spawn(|| r.wait(F, r1, until));
        let committed = r.commit(tx(vec![put("a", "2")]), until);
        (waiting.join(), committed)
    });
    let r2 = fine("commit", committed)?;
    let waited = waited.map_err(|_| "the waiting thread panicked".to_owned())?;
    same("the wait, once a newer change is committed", waited, Ok(r2))
}

fn c11(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let r1 = fine(
        "commit",
        r.commit(tx(vec![put("a", "old"), put("b", "kept")]), until),
    )?;
    let delete = Write::Delete {
        family: F.into(),
        key: b"b".to_vec(),
    };
    fine(
        "commit",
        r.commit(tx(vec![put("a", "new"), delete, put("c", "later")]), until),
    )?;
    let at = Consistency::Snapshot(r1);
    let a = fine("get at the snapshot", r.get(F, b"a", at, until))?.map(|x| x.value);
    same("a at the snapshot", a, Some(b"old".to_vec()))?;
    let page = fine(
        "scan at the snapshot",
        r.scan(
            F,
            Scan {
                prefix: Vec::new(),
                after: None,
                limit: 10,
                at,
            },
            until,
        ),
    )?;
    same(
        "the keys at the snapshot",
        keys(&page),
        vec!["a".to_owned(), "b".to_owned()],
    )?;
    let head = fine("head", r.head(until))?;
    let e = refused(
        "a snapshot ahead of the head",
        r.get(
            F,
            b"a",
            Consistency::Snapshot(head.saturating_add(1)),
            until,
        ),
    )?;
    ensure(matches!(e, Refusal::Ahead { .. }), || {
        format!("refused as {e:?}, not Ahead")
    })
}

fn c12(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let bad_family = Write::Put {
        family: "Not A Family!".into(),
        key: b"k".to_vec(),
        value: Vec::new(),
    };
    ensure(
        matches!(
            r.commit(tx(vec![bad_family]), until),
            Err(Refusal::Invalid { .. })
        ),
        || "a bad family name wasn't refused Invalid".into(),
    )?;
    let empty_key = Write::Put {
        family: F.into(),
        key: Vec::new(),
        value: Vec::new(),
    };
    ensure(
        matches!(
            r.commit(tx(vec![empty_key]), until),
            Err(Refusal::Invalid { .. })
        ),
        || "an empty key wasn't refused Invalid".into(),
    )?;
    let long_key = Write::Put {
        family: F.into(),
        key: vec![b'k'; KEY_MAX.saturating_add(1)],
        value: Vec::new(),
    };
    ensure(
        matches!(
            r.commit(tx(vec![long_key]), until),
            Err(Refusal::TooLarge { .. })
        ),
        || "a key over the limit wasn't refused TooLarge".into(),
    )?;
    let big = Write::Put {
        family: F.into(),
        key: b"k".to_vec(),
        value: vec![0; VALUE_MAX.saturating_add(1)],
    };
    ensure(
        matches!(
            r.commit(tx(vec![big]), until),
            Err(Refusal::TooLarge { .. })
        ),
        || "a value over the limit wasn't refused TooLarge".into(),
    )?;
    let many = tx((0..=TX_MAX).map(|i| put(&format!("k{i}"), "v")).collect());
    ensure(
        matches!(r.commit(many, until), Err(Refusal::TooLarge { .. })),
        || "a transaction over the limit wasn't refused TooLarge".into(),
    )?;
    same(
        "what the refused writes left",
        keys(&fine("scan", r.scan(F, scan("", None, 10), until))?),
        Vec::<String>::new(),
    )
}

fn c13(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let head = fine("head", r.head(until))?;
    same(
        "a transaction with no writes",
        fine("commit", r.commit(Tx::default(), until))?,
        head,
    )?;
    let check = Tx {
        conds: vec![Cond::Absent {
            family: F.into(),
            key: b"a".to_vec(),
        }],
        writes: Vec::new(),
    };
    same(
        "a check with no writes",
        fine("commit", r.commit(check, until))?,
        head,
    )?;
    let rev = fine(
        "one key written twice in a commit",
        r.commit(tx(vec![put("a", "first"), put("a", "last")]), until),
    )?;
    same(
        "the key written twice",
        fine("get", r.get(F, b"a", Consistency::Linearizable, until))?,
        Some(Row {
            key: b"a".to_vec(),
            value: b"last".to_vec(),
            rev,
        }),
    )
}

fn c14(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let from = fine("head", r.head(until))?;
    let r1 = fine(
        "commit",
        r.commit(tx(vec![put("a", "1"), put("b", "1")]), until),
    )?;
    let r2 = fine("commit", r.commit(tx(vec![put("a", "2")]), until))?;
    // Then only another family's: this family's newest change is r2.
    let other = |v: &str| {
        tx(vec![Write::Put {
            family: "conformance-other".into(),
            key: b"x".to_vec(),
            value: v.as_bytes().to_vec(),
        }])
    };
    fine("another family", r.commit(other("1"), until))?;
    let at = fine("another family", r.commit(other("2"), until))?;
    fine("the compaction", r.compact(at, until))?;
    // From the compaction's revision on, the store reads as it did.
    let row = |key: &[u8], s: Rev| {
        fine("get", r.get(F, key, Consistency::Snapshot(s), until))
            .map(|x| x.map(|x| (x.value, x.rev)))
    };
    same(
        "a and b at the compaction's revision",
        (row(b"a", at)?, row(b"b", at)?),
        (Some((b"2".to_vec(), r2)), Some((b"1".to_vec(), r1))),
    )?;
    same(
        "the feed after it",
        fine("changes", r.changes(F, at, 10, until))?.changes,
        Vec::new(),
    )?;
    same(
        "a wait after it, with no change since",
        r.changed(F, at, until),
        Ok(None),
    )?;
    // From before it: refused Compacted, or answered right, and at once.
    match r.get(F, b"a", Consistency::Snapshot(r1), until) {
        Err(Refusal::Compacted { .. }) => {}
        Ok(Some(x)) if x.value == b"1" && x.rev == r1 => {}
        other => return Err(format!("a snapshot from before the compaction: {other:?}")),
    }
    let whole = vec![
        Change {
            rev: r1,
            key: b"a".to_vec(),
            value: Some(b"1".to_vec()),
        },
        Change {
            rev: r1,
            key: b"b".to_vec(),
            value: Some(b"1".to_vec()),
        },
        Change {
            rev: r2,
            key: b"a".to_vec(),
            value: Some(b"2".to_vec()),
        },
    ];
    match r.changes(F, from, 10, until) {
        Err(Refusal::Compacted { .. }) => {}
        Ok(c) if c.changes == whole => {}
        other => return Err(format!("the feed from before the compaction: {other:?}")),
    }
    match r.changed(F, from, until) {
        Err(Refusal::Compacted { .. }) => Ok(()),
        Ok(Some(rev)) if rev == r2 => Ok(()),
        Ok(None) => {
            Err("a wait from before the compaction, for a change made then, didn't complete".into())
        }
        other => Err(format!("a wait from before the compaction: {other:?}")),
    }
}

fn feed_1(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let from = fine("head", r.head(until))?;
    let r1 = fine("commit", r.commit(tx(vec![put("a", "1")]), until))?;
    let r2 = fine(
        "commit",
        r.commit(tx(vec![put("b", "1"), put("c", "1")]), until),
    )?;
    let page = fine("changes, two at most", r.changes(F, from, 2, until))?;
    same(
        "a page that would end inside a commit ends before it",
        page.changes.iter().map(|c| c.rev).collect::<Vec<_>>(),
        vec![r1],
    )?;
    let e = refused(
        "a commit of two changes asked one at a time",
        r.changes(F, r1, 1, until),
    )?;
    ensure(matches!(e, Refusal::Invalid { .. }), || {
        format!("refused as {e:?}, not Invalid")
    })?;
    let rest = fine("changes after the page", r.changes(F, r1, 2, until))?;
    same(
        "the commit after the page, whole",
        rest.changes
            .iter()
            .map(|c| (c.rev, c.key.clone()))
            .collect::<Vec<_>>(),
        vec![(r2, b"b".to_vec()), (r2, b"c".to_vec())],
    )
}

fn index_1(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let index = fine("an index family", Index::new("conformance-by-state"))?;
    let row = |id: &str, state: &str| Write::Put {
        family: F.into(),
        key: id.as_bytes().to_vec(),
        value: state.as_bytes().to_vec(),
    };
    // "run" is a prefix of "running", and "a\0" holds the escaped byte.
    let mut writes = vec![row("r1", "run"), row("r2", "running"), row("r3", "run")];
    let terms: [(&str, &[u8]); 4] = [
        ("r1", b"run"),
        ("r2", b"running"),
        ("r3", b"run"),
        ("r4", b"a\0"),
    ];
    for (id, term) in terms {
        writes.push(fine("an index write", index.put(term, id.as_bytes()))?);
    }
    fine("rows with their index", r.commit(tx(writes), until))?;
    let under = |term: &[u8]| -> Result<Vec<Vec<u8>>, Failure> {
        let prefix = fine("a term's prefix", Index::prefix(term))?;
        let page = fine(
            "a term's scan",
            r.scan(
                index.family(),
                Scan {
                    prefix,
                    after: None,
                    limit: 10,
                    at: Consistency::Linearizable,
                },
                until,
            ),
        )?;
        page.rows
            .iter()
            .map(|x| fine("an index key", Index::split(&x.key)).map(|(_, k)| k))
            .collect()
    };
    same(
        "rows under run",
        under(b"run")?,
        vec![b"r1".to_vec(), b"r3".to_vec()],
    )?;
    same(
        "rows under running",
        under(b"running")?,
        vec![b"r2".to_vec()],
    )?;
    same("rows under a\\0", under(b"a\0")?, vec![b"r4".to_vec()])?;
    same("rows under a", under(b"a")?, Vec::<Vec<u8>>::new())?;
    // r1 moves from run to running in the commit that changes it.
    let mut moved = fine("a move", index.moved(b"r1", &[b"run"], &[b"running"]))?;
    moved.push(row("r1", "running"));
    fine("a row and its index moved", r.commit(tx(moved), until))?;
    same("rows under run after", under(b"run")?, vec![b"r3".to_vec()])?;
    same(
        "rows under running after",
        under(b"running")?,
        vec![b"r1".to_vec(), b"r2".to_vec()],
    )
}

fn counter_1(r: &dyn Records, until: Instant) -> Result<(), Failure> {
    let counter = fine("a counter", Counter::new("conformance-counter", b"n"))?;
    let release = |n: u64, taken: &crate::counter::Taken| Tx {
        conds: vec![taken.cond.clone()],
        writes: vec![
            taken.write.clone(),
            Write::Put {
                family: F.into(),
                key: n.to_be_bytes().to_vec(),
                value: Vec::new(),
            },
        ],
    };
    let first = fine("the first number", counter.take(r, until))?;
    same("the first number", first.value, 1)?;
    fine(
        "a commit taking it",
        r.commit(release(first.value, &first), until),
    )?;
    // Two takers from one reading: one commits, the other is refused and writes nothing.
    let a = fine("a taker", counter.take(r, until))?;
    let b = fine("another taker", counter.take(r, until))?;
    same("both read", (a.value, b.value), (2, 2))?;
    fine("the first taker", r.commit(release(a.value, &a), until))?;
    let e = refused("the second taker", r.commit(release(b.value, &b), until))?;
    ensure(matches!(e, Refusal::Conflict { .. }), || {
        format!("refused as {e:?}, not Conflict")
    })?;
    let again = fine("the second taker, read again", counter.take(r, until))?;
    same("its number, read again", again.value, 3)?;
    fine(
        "the second taker's commit",
        r.commit(release(again.value, &again), until),
    )?;
    let taken = fine("the numbers taken", r.scan(F, scan("", None, 10), until))?;
    same(
        "each number once",
        taken.rows.iter().map(|x| x.key.clone()).collect::<Vec<_>>(),
        [1u64, 2, 3]
            .iter()
            .map(|n| n.to_be_bytes().to_vec())
            .collect::<Vec<_>>(),
    )
}

/// The Records contract, clause by clause.
pub fn cases() -> Vec<Case> {
    vec![
        Case {
            id: "RECORDS-1",
            says: "a row written is read back with its value and its commit's revision; a key never written reads as none",
            check: c1,
        },
        Case {
            id: "RECORDS-2",
            says: "every commit's revision is higher than every one before it, and head() is the newest",
            check: c2,
        },
        Case {
            id: "RECORDS-3",
            says: "a write conditioned on a row's absence succeeds once; the next is refused Conflict and writes nothing",
            check: c3,
        },
        Case {
            id: "RECORDS-4",
            says: "a write conditioned on a row's revision (compare-and-set) succeeds at the current one and is refused Conflict at a stale one, naming the current",
            check: c4,
        },
        Case {
            id: "RECORDS-5",
            says: "a transaction is all or nothing: one failed condition and no write applies, the head unmoved; together, its writes share one revision",
            check: c5,
        },
        Case {
            id: "RECORDS-6",
            says: "a deleted row reads as none, leaves scans, and satisfies an absence condition",
            check: c6,
        },
        Case {
            id: "RECORDS-7",
            says: "a scan returns a family's rows in key order, within its prefix, after its cursor, at most its limit, saying when more follow; a limit of 0 is refused",
            check: c7,
        },
        Case {
            id: "RECORDS-8",
            says: "families are apart: one key in two families is two rows",
            check: c8,
        },
        Case {
            id: "RECORDS-9",
            says: "a family's change feed gives every change committed after a revision, in commit order, a delete as none, at most its limit",
            check: c9,
        },
        Case {
            id: "RECORDS-10",
            says: "a wait completes once a change to the family is committed after its revision, at once when one already is",
            check: c10,
        },
        Case {
            id: "RECORDS-11",
            says: "a snapshot read sees the store as it was just after its revision; one ahead of the head is refused Ahead",
            check: c11,
        },
        Case {
            id: "RECORDS-12",
            says: "bad family names, empty or long keys, large values and large transactions are refused, typed, and write nothing",
            check: c12,
        },
        Case {
            id: "RECORDS-13",
            says: "a transaction with no writes commits nothing and returns the head; a key written twice in one commit keeps the last",
            check: c13,
        },
        Case {
            id: "RECORDS-14",
            says: "after a compaction, snapshots and the feed from its revision on read as before; from before it, a snapshot, a read of changes and a wait are each refused Compacted or answered right, at once",
            check: c14,
        },
        Case {
            id: "AMPHORA-RECORDS-FEED-1",
            says: "a page of changes never ends inside a commit: it ends before one that doesn't fit, and a commit alone over the limit is refused Invalid",
            check: feed_1,
        },
        Case {
            id: "AMPHORA-RECORDS-INDEX-1",
            says: "an index family written in its rows' commit gives, by a term's prefix, exactly the rows under that term in key order, and moves with them",
            check: index_1,
        },
        Case {
            id: "AMPHORA-RECORDS-COUNTER-1",
            says: "a counter gives 1, then each next number once: of two takers from one reading, one commits and the other is refused Conflict and writes nothing",
            check: counter_1,
        },
    ]
}
