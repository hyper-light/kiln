//! CP-T3.2 under faults (the fifth adversarial check's): a session's inbox that can't take in the
//! ids its session brought, or can't be read as its session leaves; a handover that never finished;
//! a message with an uploaded file sent again. A retry is never a second prompt, and a message is
//! never sent without its files.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::future::BoxFuture;
use kiln_seams::records::{Changes, Consistency, Page, Records, Refusal as Failed, Rev, Row, Scan, SimRecords, Tx};

use super::{Inbox, Limits};
use crate::harness::host::{Taker, arrive, carry, take_message};
use crate::harness::model::{Command, SendMode};

/// Records whose `fail_commit`-th commit (from 1) fails once, and whose scans fail while
/// `scans_fail`: a disk that failed one write, or a read.
struct Flaky {
    inner: SimRecords,
    commits: AtomicUsize,
    fail_commit: usize,
    scans_fail: AtomicBool,
}

impl Flaky {
    fn new(fail_commit: usize) -> Self {
        Self { inner: SimRecords::new(), commits: AtomicUsize::new(0), fail_commit, scans_fail: AtomicBool::new(false) }
    }
}

impl Records for Flaky {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Failed>> {
        self.inner.get(family, key, at)
    }
    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Failed>> {
        if self.scans_fail.load(Ordering::SeqCst) {
            return Box::pin(async { Err(Failed::Unavailable("a read failed (injected)".into())) });
        }
        self.inner.scan(family, scan)
    }
    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Failed>> {
        let n = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_commit {
            return Box::pin(async { Err(Failed::Unavailable("a write failed (injected)".into())) });
        }
        self.inner.commit(tx)
    }
    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Failed>> {
        self.inner.changes(family, after, limit)
    }
    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Failed>> {
        self.inner.wait(family, after)
    }
    fn head(&self) -> BoxFuture<'_, Result<Rev, Failed>> {
        self.inner.head()
    }
}

/// A session's harness as the host hands it messages: what it was handed.
struct Harness {
    epoch: u64,
    handed: RefCell<Vec<String>>,
}

impl Taker for Harness {
    fn is_moving(&self) -> bool {
        false
    }
    fn epoch(&self) -> u64 {
        self.epoch
    }
    fn hand(&self, c: Command) -> bool {
        if let Command::Send { id, .. } = c {
            self.handed.borrow_mut().push(id);
        }
        true
    }
}

fn message(id: &str) -> Command {
    Command::Send { id: id.into(), text: "deploy the fix".into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
}

/// m1 taken and handed where session s1 was (`from`, epoch 0); then s1 moves as kiln's code moves
/// it: its ids carried (`carry`), the inbox it left fenced, and s1 arriving at `to` at epoch 1
/// (`arrive`). m1's sender, whose answer was lost, sends it again where s1 is now. How many times
/// m1 reached a harness.
async fn moved_and_sent_again(from: &Inbox, to: &Inbox, before_carry: impl FnOnce()) -> usize {
    let there = Harness { epoch: 0, handed: RefCell::new(Vec::new()) };
    arrive(from, "s1", 0, &[], false, 1).await;
    let first = take_message(Some(from), "s1", Some(&there), message("m1"), Some(0), 2).await;
    assert!(first.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| !a.repeat)), "{first:?}");
    before_carry();
    let brought = carry(from, "s1").await.unwrap_or_default();
    let _ = from.fence("s1", 0).await;
    arrive(to, "s1", 1, &brought, false, 3).await;
    let here = Harness { epoch: 1, handed: RefCell::new(Vec::new()) };
    let _ = take_message(Some(to), "s1", Some(&here), message("m1"), Some(1), 4).await;
    there.handed.borrow().len() + here.handed.borrow().len()
}

/// Where the session went, a write failed as it arrived (taking in the ids it brought, the first;
/// opening its inbox, the second): a retry of m1, delivered where it was, is its repeat there,
/// never handed to its harness again. Ids it couldn't take in wait, and each message tries them
/// first.
#[test]
fn an_inbox_that_could_not_take_the_ids_its_session_brought_takes_a_retry_as_its_repeat() {
    for failing in [1, 2] {
        let from = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let to = Inbox::new(Arc::new(Flaky::new(failing)), Limits::default());
        let reached = futures::executor::block_on(moved_and_sent_again(&from, &to, || {}));
        assert_eq!(reached, 1, "write {failing} failing: m1 reached a harness {reached} times");
    }
}

/// Where the session went, the ids it brought couldn't be taken in (a write failed), and it moves on
/// before a message comes: they go with it, so m1, sent again where it went next, is its repeat.
#[test]
fn ids_a_session_brought_and_has_not_taken_in_go_with_it_when_it_moves_on() {
    futures::executor::block_on(async {
        let first = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let second = Inbox::new(Arc::new(Flaky::new(1)), Limits::default());
        let third = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let there = Harness { epoch: 0, handed: RefCell::new(Vec::new()) };
        arrive(&first, "s1", 0, &[], false, 1).await;
        let took = take_message(Some(&first), "s1", Some(&there), message("m1"), Some(0), 2).await;
        assert!(took.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| !a.repeat)), "{took:?}");
        let brought = carry(&first, "s1").await.unwrap_or_default();
        let _ = first.fence("s1", 0).await;
        arrive(&second, "s1", 1, &brought, false, 3).await;
        assert_eq!(second.waiting("s1"), brought, "the ids it brought wait");
        let onward = carry(&second, "s1").await.unwrap_or_default();
        let _ = second.fence("s1", 1).await;
        arrive(&third, "s1", 2, &onward, false, 4).await;
        let here = Harness { epoch: 2, handed: RefCell::new(Vec::new()) };
        let again = take_message(Some(&third), "s1", Some(&here), message("m1"), Some(2), 5).await;
        assert!(again.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| a.repeat)), "carried on {onward:?}; the retry was answered {again:?}");
        assert!(here.handed.borrow().is_empty());
    });
}

/// A move whose package landed where it went but whose answer never came back (the sixth
/// adversarial check's): the inbox it left stays fenced while the machine it left asks again (it
/// may run there now). m2, sent where it was for its old epoch, is refused `moved`; sent again
/// where it went, it's taken there, once.
#[test]
fn a_move_whose_answer_was_lost_keeps_its_fence_and_a_retry_reaches_a_harness_once() {
    futures::executor::block_on(async {
        let from = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let to = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let there = Harness { epoch: 0, handed: RefCell::new(Vec::new()) };
        arrive(&from, "s1", 0, &[], false, 1).await;
        let ids = carry(&from, "s1").await.unwrap_or_default();
        assert!(from.fence("s1", 0).await.is_ok());
        arrive(&to, "s1", 1, &ids, false, 2).await;
        // No answer: the fence stays.
        let first = take_message(Some(&from), "s1", Some(&there), message("m2"), Some(0), 3).await;
        assert!(first.as_ref().is_some_and(|s| matches!(s.answer, Err(super::Refusal::Moved { .. }))), "{first:?}");
        let here = Harness { epoch: 1, handed: RefCell::new(Vec::new()) };
        let again = take_message(Some(&to), "s1", Some(&here), message("m2"), Some(1), 4).await;
        assert!(again.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| !a.repeat)), "{again:?}");
        assert_eq!(there.handed.borrow().len() + here.handed.borrow().len(), 1);
    });
}

/// The ids a session brought and hasn't taken in are its newest [`Limits::keep`] at most (no more
/// are taken in), and they go once taken in, or when it leaves for good.
#[test]
fn waiting_ids_are_bounded_and_go_once_in_or_when_their_session_leaves() {
    futures::executor::block_on(async {
        let inbox = Inbox::new(Arc::new(SimRecords::new()), Limits { keep: 3, ..Limits::default() });
        let pairs = |r: std::ops::Range<u32>| -> Vec<(String, String)> { r.map(|n| (format!("m{n}"), String::new())).collect() };
        inbox.bring("s1", &pairs(0..5));
        assert_eq!(inbox.waiting("s1"), pairs(2..5), "its newest three");
        inbox.let_go("s1");
        assert!(inbox.waiting("s1").is_empty(), "gone with it");
        arrive(&inbox, "s1", 0, &pairs(0..2), false, 1).await;
        assert!(inbox.waiting("s1").is_empty(), "taken in");
        assert_eq!(inbox.taken("s1", 10).await.ok(), Some(vec!["m0".to_owned(), "m1".to_owned()]));
    });
}

/// The inbox the session leaves can't be read as it moves: the ids it took can't go with it, so it
/// doesn't go (kiln's move fails, and the session stays where it is): m1, sent again there, is its
/// repeat.
#[test]
fn a_session_whose_ids_cannot_be_read_doesnt_move() {
    futures::executor::block_on(async {
        let flaky = Arc::new(Flaky::new(0));
        let from = Inbox::new(flaky.clone(), Limits::default());
        let there = Harness { epoch: 0, handed: RefCell::new(Vec::new()) };
        arrive(&from, "s1", 0, &[], false, 1).await;
        let first = take_message(Some(&from), "s1", Some(&there), message("m1"), Some(0), 2).await;
        assert!(first.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| !a.repeat)), "{first:?}");
        flaky.scans_fail.store(true, Ordering::SeqCst);
        assert!(carry(&from, "s1").await.is_err(), "no ids: it doesn't go");
        flaky.scans_fail.store(false, Ordering::SeqCst);
        let again = take_message(Some(&from), "s1", Some(&there), message("m1"), Some(0), 3).await;
        assert!(again.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| a.repeat)), "{again:?}");
        assert_eq!(there.handed.borrow().len(), 1);
    });
}

/// A blue/green handover whose old runner dies before the new build is promoted, its record of its
/// sessions not yet written again: the supervisor stops the new build with it and starts the old
/// runner again, which goes on with its sessions from that record, at the epoch they were handed
/// over at. A handover's fence is lifted as it does (a move's wouldn't be): each takes messages.
#[test]
fn a_runner_restarted_after_a_handover_that_never_completed_takes_its_sessions_messages() -> anyhow::Result<()> {
    use crate::harness::core::Update;
    use crate::harness::host::{Host, Limits as HostLimits};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    async fn next_said(rx: &async_channel::Receiver<(String, Update)>) -> anyhow::Result<super::Said> {
        let got = crate::rt::timeout(Duration::from_secs(30), async {
            loop {
                match rx.recv().await {
                    Ok((_, Update::Inbox { said })) => return Some(*said),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;
        got.ok().flatten().ok_or_else(|| anyhow::anyhow!("no answer came"))
    }
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("harness.json");
        let spec = Spec {
            kind: "claude".into(),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
            ..Spec::default()
        };
        {
            let (tx, rx) = async_channel::unbounded();
            let blue = Host::with_clock(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()));
            anyhow::ensure!(blue.inbox().is_some(), "no inbox");
            blue.open(Some("bg".into()), spec)?;
            // Its record written, as it was before the upgrade began.
            blue.stop_all();
            assert!(blue.command("bg", message("m0")));
            anyhow::ensure!(next_said(&rx).await?.answer.is_ok(), "m0 taken");
            let moved = blue.move_out().await;
            anyhow::ensure!(moved.len() == 1, "one session handed over");
            let inbox = blue.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let mut fenced = false;
            for _ in 0..500 {
                if inbox.epoch("bg").await?.is_some_and(|(_, m)| m) {
                    fenced = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            anyhow::ensure!(fenced, "never fenced");
            // It dies here, before its record is written again; the new build is stopped with it.
        }
        let (tx, rx) = async_channel::unbounded();
        let again = Host::with_clock(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()));
        anyhow::ensure!(again.inbox().is_some(), "the restarted runner has no inbox (its records didn't open)");
        again.restore();
        anyhow::ensure!(again.get("bg").is_some(), "the session wasn't restored");
        for n in 1..=3 {
            assert!(again.command("bg", message(&format!("m{n}"))));
            let said = next_said(&rx).await?;
            anyhow::ensure!(said.answer.as_ref().is_ok_and(|a| !a.repeat), "m{n}, sent to the only copy of its session, was answered {:?}", said.answer);
        }
        Ok(())
    })
}

/// A session comes to a runner with the id of a message it took where it was, and the runner can't
/// take it in (a write failed); the runner restarts before a message comes. Its record kept the id,
/// so the runner started again takes it in: m1, sent again, is its repeat.
#[test]
fn a_runner_restarted_before_its_session_took_in_the_ids_it_brought_takes_a_retry_as_its_repeat() -> anyhow::Result<()> {
    use crate::harness::core::Update;
    use crate::harness::host::{Host, Limits as HostLimits, Saved};
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("harness.json");
        let records = Arc::new(Flaky::new(1));
        let rec: Saved = serde_json::from_value(serde_json::json!({
            "id": "s1",
            "spec": {"kind": "claude", "cwd": dir.path(), "program": dir.path().join("no-such-claude")},
            "inbox": ["m1"],
        }))?;
        {
            let (tx, _rx) = async_channel::unbounded();
            let before = Host::with_records(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()), Some(records.clone() as Arc<dyn Records>));
            before.move_in(rec, None)?;
            let inbox = before.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let mut opened = false;
            for _ in 0..500 {
                if inbox.epoch("s1").await?.is_some() {
                    opened = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            anyhow::ensure!(opened, "its inbox was never opened");
            anyhow::ensure!(inbox.waiting("s1") == [("m1".to_owned(), String::new())], "m1 isn't waiting: {:?}", inbox.waiting("s1"));
            // Its record written; it stops here, m1 not taken in.
            before.stop_all();
        }
        let (tx, rx) = async_channel::unbounded();
        let after = Host::with_records(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()), Some(records as Arc<dyn Records>));
        after.restore();
        anyhow::ensure!(after.get("s1").is_some(), "the session wasn't restored");
        assert!(after.command("s1", message("m1")));
        let said = crate::rt::timeout(Duration::from_secs(30), async {
            loop {
                match rx.recv().await {
                    Ok((_, Update::Inbox { said })) => return Some(*said),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("no answer came"))?;
        anyhow::ensure!(said.answer.as_ref().is_ok_and(|a| a.repeat), "m1, taken where the session was, was answered {:?} after the restart", said.answer);
        Ok(())
    })
}

/// A hub running its own sessions, and one session on it whose harness isn't there (its inbox takes
/// messages to it all the same).
async fn hub_and_session() -> anyhow::Result<(Arc<crate::hub::Hub>, String, tempfile::TempDir)> {
    let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
    let h = Arc::clone(&hub);
    crate::rt::spawn(async move {
        let _ = crate::harness::registry::Registry::run_local(h).await;
    });
    let dir = tempfile::tempdir()?;
    let spec = crate::harness::session::Spec {
        kind: "claude".into(),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let id = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
    Ok((hub, id, dir))
}

/// Message m1 with an uploaded file, as the composer, Mission Control and the API send one.
fn with_file(file: String) -> Command {
    let shot = crate::harness::model::Attachment { name: "shot.png".into(), mime: "image/png".into(), data_base64: String::new(), file_id: Some(file), path: None };
    Command::Send { id: "m1".into(), text: "look at this".into(), mode: SendMode::Queue, attachments: vec![shot], overrides: None, reply_to: None, task: None }
}

/// A retry of a message carrying an uploaded file (the same id, words and file; its first answer
/// lost) is answered as its repeat: the upload is kept after the first try carried it, so the
/// retry carries it too and reads as the same message.
#[test]
fn a_retry_of_a_message_with_an_uploaded_file_is_answered_as_its_repeat() -> anyhow::Result<()> {
    use crate::harness::registry::Sent;
    crate::rt::block_on(async {
        let (hub, id, _dir) = hub_and_session().await?;
        let file = hub.harness.upload("shot.png".into(), "image/png".into(), bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"))?;
        let first = hub.harness.command(&hub, &id, with_file(file.clone())).await;
        anyhow::ensure!(matches!(&first, Sent::Accepted(a) if !a.repeat), "{first:?}");
        let again = hub.harness.command(&hub, &id, with_file(file)).await;
        anyhow::ensure!(matches!(&again, Sent::Accepted(a) if a.repeat), "the same message sent again was answered {again:?}");
        Ok(())
    })
}

/// CP-T3.2 (the twenty-second adversarial check's): a retry of a message carrying an uploaded file
/// whose upload left the server since its first try (short of room, as here; its 30 minutes up; or
/// the server started again: uploads are kept in memory) still carries the file's name, so it reads
/// as the same message and is answered as its repeat. Without its name it read as another message
/// under its id, refused `reused` for good, and the conversation's Retry sent it as a new message: a
/// second prompt.
#[test]
fn a_retry_of_a_message_whose_upload_left_the_server_is_answered_as_its_repeat() -> anyhow::Result<()> {
    use crate::harness::registry::Sent;
    crate::rt::block_on(async {
        let (hub, id, _dir) = hub_and_session().await?;
        let file = hub.harness.upload("shot.png".into(), "image/png".into(), bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"))?;
        let first = hub.harness.command(&hub, &id, with_file(file.clone())).await;
        anyhow::ensure!(matches!(&first, Sent::Accepted(a) if !a.repeat), "{first:?}");
        // Short of room (512 MiB in all), the files messages carried already go first.
        hub.harness.upload("big.bin".into(), "application/octet-stream".into(), bytes::Bytes::from(vec![0u8; (512 << 20) - 4]))?;
        let again = hub.harness.command(&hub, &id, with_file(file)).await;
        anyhow::ensure!(matches!(&again, Sent::Accepted(a) if a.repeat), "the same message sent again, its upload gone, was answered {again:?}");
        Ok(())
    })
}

/// CP-T3.2 (the twenty-second adversarial check's): Mission Control's message box sent again under
/// its key (its answer lost, as when the server starts again) once its upload left the server: the
/// agent that took the first try is told it was sent, as its repeat, never refused as another message
/// under its id (a report of `failed`, whose resend under a new key would be a second prompt).
#[test]
fn a_broadcast_sent_again_after_its_upload_left_the_server_is_sent_once() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let (hub, id, _dir) = hub_and_session().await?;
        let file = hub.harness.upload("shot.png".into(), "image/png".into(), bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"))?;
        let b: crate::harness::broadcast::Broadcast = serde_json::from_value(serde_json::json!({
            "to": {"ids": [id.clone()]}, "text": "look at this", "key": "bk-1",
            "attachments": [{"name": "shot.png", "mime": "image/png", "file_id": file}],
        }))?;
        let among = hub.harness.list();
        let first = crate::harness::broadcast::run(&hub, &b, "user:dev1", &among).await?;
        anyhow::ensure!(first.sent == 1, "{first:?}");
        // Short of room (512 MiB in all), the files messages carried already go first.
        hub.harness.upload("big.bin".into(), "application/octet-stream".into(), bytes::Bytes::from(vec![0u8; (512 << 20) - 4]))?;
        // What it carries to each agent keeps the file's name, its bytes gone.
        let carried = hub.harness.inline_uploads(b.attachments.clone());
        anyhow::ensure!(carried.iter().map(|a| (a.name.as_str(), a.data_base64.is_empty())).eq([("shot.png", true)]), "{carried:?}");
        let again = crate::harness::broadcast::run(&hub, &b, "user:dev1", &among).await?;
        anyhow::ensure!(again.sent == 1 && again.failed == 0, "sent again under its key, its upload gone: {again:?}");
        Ok(())
    })
}

/// The server's one send again for the epoch a `stale_epoch` refusal names (`again_for`) carries
/// the message's uploaded files: the session takes it with them.
#[test]
fn a_message_sent_again_for_the_epoch_named_keeps_its_files() -> anyhow::Result<()> {
    use crate::harness::registry::Sent;
    crate::rt::block_on(async {
        let (hub, id, _dir) = hub_and_session().await?;
        let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
        // Its inbox has gone on to epoch 3 (as a session that moved back here would have it); the
        // server last heard 0.
        inbox.open(&id, 3).await?;
        let file = hub.harness.upload("shot.png".into(), "image/png".into(), bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"))?;
        let sent = hub.harness.command(&hub, &id, with_file(file)).await;
        anyhow::ensure!(matches!(&sent, Sent::Accepted(a) if a.epoch == 3), "{sent:?}");
        let kept = inbox.carried(&id, 10).await?;
        let whole = super::digest("look at this", &["shot.png"]);
        anyhow::ensure!(kept == [("m1".to_owned(), whole.clone())], "the session took m1 as {kept:?}: with its file it would be {whole}, without it it's {}", super::digest("look at this", &[]));
        Ok(())
    })
}

/// A session fenced as it moves to another machine (before its conversation goes) refuses messages;
/// its move failing, the fence is lifted and it takes them again.
#[test]
fn a_move_that_fails_lifts_its_fence() {
    futures::executor::block_on(async {
        let inbox = Inbox::new(Arc::new(SimRecords::new()), Limits::default());
        let here = Harness { epoch: 2, handed: RefCell::new(Vec::new()) };
        arrive(&inbox, "s1", 2, &[], false, 1).await;
        let _ = inbox.fence("s1", 2).await;
        let refused = take_message(Some(&inbox), "s1", Some(&here), message("m1"), Some(2), 2).await;
        assert!(refused.as_ref().is_some_and(|s| matches!(s.answer, Err(super::Refusal::Moved { .. }))), "{refused:?}");
        let _ = inbox.take_back("s1", 2).await;
        let taken = take_message(Some(&inbox), "s1", Some(&here), message("m1"), Some(2), 3).await;
        assert!(taken.as_ref().is_some_and(|s| s.answer.as_ref().is_ok_and(|a| !a.repeat)), "{taken:?}");
    });
}

/// A stand-in for Claude Code at `program` that keeps every line it's sent in `heard`, and answers
/// kiln's control requests (so its session goes idle and hands it what its queue holds).
#[cfg(unix)]
fn answering_claude(program: &std::path::Path, heard: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/usr/bin/env python3\nimport json, sys\nwith open({:?}, 'a') as heard:\n    while True:\n        line = sys.stdin.readline()\n        if not line:\n            break\n        heard.write(line)\n        heard.flush()\n        try:\n            v = json.loads(line)\n        except Exception:\n            continue\n        if v.get('type') == 'control_request':\n            sys.stdout.write(json.dumps({{'type': 'control_response', 'response': {{'subtype': 'success', 'request_id': v.get('request_id'), 'response': {{}}}}}}) + '\\n')\n            sys.stdout.flush()\n",
        heard.display().to_string()
    );
    std::fs::write(program, script)?;
    std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The ids of the prompts a harness was sent (its user lines' `uuid`), in `heard`.
#[cfg(unix)]
fn prompts(heard: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(heard)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("user"))
        .filter_map(|v| v.get("uuid").and_then(serde_json::Value::as_str).map(str::to_owned))
        .collect()
}

#[cfg(unix)]
async fn until(what: &str, mut holds: impl FnMut() -> bool) -> anyhow::Result<()> {
    for _ in 0..1500 {
        if holds() {
            return Ok(());
        }
        crate::rt::sleep(std::time::Duration::from_millis(20)).await;
    }
    anyhow::bail!("{what}: not in 30 s")
}

/// The seventh adversarial check's: a hand-over sent again (its first answer lost on the way back)
/// after the session left the runner it went to (it ended there, or went on elsewhere) is answered
/// as taken, and isn't taken over a second time: what its queue held reaches a harness once.
#[cfg(unix)]
#[test]
fn a_hand_over_sent_again_after_its_session_left_is_answered_as_taken_and_not_taken_again() -> anyhow::Result<()> {
    use crate::harness::host::{Host, Limits as HostLimits};
    use crate::harness::model::{QueueItem, State};
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    crate::rt::block_on(async move {
        for leaves in ["ended there", "went on elsewhere"] {
            let dir = tempfile::tempdir()?;
            let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
            answering_claude(&program, &heard)?;
            // What box-a hands over: session s1, a message waiting in its queue.
            let cwd = dir.path().to_string_lossy().into_owned();
            let mut state = State::default();
            state.queue.push(QueueItem { id: "q1".into(), text: "carry on with the migration".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
            let mut pkg = pack("s1", Spec { kind: "claude".into(), cwd: Some(cwd.clone()), ..Spec::default() }, &cwd, "box-a", &cwd, Carry::Nothing, Vec::new(), state);
            pkg.spec.program = Some(program.to_string_lossy().into_owned());
            let bytes = serde_json::to_vec(&pkg)?;
            let (tx, _rx) = async_channel::unbounded();
            let b = Host::with_records(tx, Some(dir.path().join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
            // box-b takes it over; its answer to box-a is lost on the way back.
            b.adopt("box-a", &bytes).map_err(anyhow::Error::msg)?;
            until("q1 reached box-b's harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
            if leaves == "ended there" {
                anyhow::ensure!(b.close("s1"), "no session to end");
            } else {
                anyhow::ensure!(b.let_go("s1"), "no session to let go");
            }
            until("box-b let s1 go", || b.get("s1").is_none()).await?;
            // box-a asks again with the same package.
            let again = b.adopt("box-a", &bytes);
            crate::rt::sleep(Duration::from_millis(1500)).await;
            let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
            anyhow::ensure!(again.is_ok(), "s1 {leaves} on box-b; box-a's hand-over sent again was answered {again:?}");
            anyhow::ensure!(b.get("s1").is_none(), "s1 {leaves} on box-b, and runs there again from the old package");
            anyhow::ensure!(reached == 1, "s1 {leaves} on box-b; q1 reached a harness {reached} times");
        }
        Ok(())
    })
}

/// The same for a Claude Code session whose life on the runner it went to added to its transcript
/// there: the hand-over sent again is answered as taken (before what it carries is looked at), not
/// refused, so the runner it left never goes on with what went (the seventh check's).
#[cfg(unix)]
#[test]
fn a_claude_hand_over_sent_again_after_it_left_is_answered_as_taken() -> anyhow::Result<()> {
    use crate::harness::host::{Host, Limits as HostLimits};
    use crate::harness::model::{QueueItem, State};
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;
    use std::io::Write as _;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let config = dir.path().join("claude-config");
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut state = State::default();
        state.queue.push(QueueItem { id: "q1".into(), text: "carry on with the migration".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        let first_turn = serde_json::json!({"type": "user", "uuid": "u1", "sessionId": "h1", "message": {"role": "user", "content": "start the migration"}}).to_string();
        let spec = Spec { kind: "claude".into(), cwd: Some(cwd.clone()), resume: Some("h1".into()), env: vec![("CLAUDE_CONFIG_DIR".into(), config.to_string_lossy().into_owned())], ..Spec::default() };
        let mut pkg = pack("s1", spec, &cwd, "box-a", &cwd, Carry::ClaudeLines { session: "h1".into(), lines: vec![first_turn] }, Vec::new(), state);
        pkg.spec.program = Some(program.to_string_lossy().into_owned());
        let bytes = serde_json::to_vec(&pkg)?;
        let (tx, _rx) = async_channel::unbounded();
        let b = Host::with_records(tx, Some(dir.path().join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        b.adopt("box-a", &bytes).map_err(anyhow::Error::msg)?;
        until("q1 reached box-b's harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
        // Its life on box-b: a turn of its own in its transcript there; then it ends.
        let transcript = crate::harness::transcript::find(&config, "h1").ok_or_else(|| anyhow::anyhow!("no transcript of h1 on box-b"))?;
        let mut f = std::fs::OpenOptions::new().append(true).open(&transcript)?;
        writeln!(f, "{}", serde_json::json!({"type": "assistant", "uuid": "u2", "sessionId": "h1", "message": {"role": "assistant", "content": "migrated"}}))?;
        drop(f);
        anyhow::ensure!(b.close("s1"), "no session to end");
        until("box-b let s1 go", || b.get("s1").is_none()).await?;
        let again = b.adopt("box-a", &bytes);
        anyhow::ensure!(again.is_ok(), "box-a's hand-over sent again was answered {again:?}");
        anyhow::ensure!(b.get("s1").is_none(), "s1 runs on box-b again from the old package");
        Ok(())
    })
}

/// A runner's refusal of a hand-over it never took says so (`NOT_TAKEN`): one for a harness it
/// doesn't know, and one for a session a copy of which runs there from before.
#[test]
fn a_hand_over_refused_says_it_was_not_taken() -> anyhow::Result<()> {
    use crate::harness::host::{Host, Limits as HostLimits, RUNS_HERE};
    use crate::harness::model::State;
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    use crate::reach::node::NOT_TAKEN;
    use kiln_seams::clock::SimClock;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let (tx, _rx) = async_channel::unbounded();
        let b = Host::with_records(tx, Some(dir.path().join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        let unknown = pack("s1", Spec { kind: "no-such-harness".into(), cwd: Some(cwd.clone()), ..Spec::default() }, &cwd, "box-a", &cwd, Carry::Nothing, Vec::new(), State::default());
        let refused = b.adopt("box-a", &serde_json::to_vec(&unknown)?);
        anyhow::ensure!(refused.as_ref().err().is_some_and(|e| e.starts_with(NOT_TAKEN)), "{refused:?}");
        // A copy of s2 runs here from before (at its epoch 0); a hand-over of it at epoch 1 (from its
        // epoch 0 elsewhere, a copy of it left behind here) is refused, not taken.
        let spec = Spec { kind: "claude".into(), cwd: Some(cwd.clone()), program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()), ..Spec::default() };
        b.open(Some("s2".into()), spec.clone())?;
        let copy = pack("s2", spec, &cwd, "box-a", &cwd, Carry::Nothing, Vec::new(), State::default());
        let refused = b.adopt("box-a", &serde_json::to_vec(&copy)?);
        anyhow::ensure!(refused == Err(format!("{NOT_TAKEN}{RUNS_HERE}")), "{refused:?}");
        Ok(())
    })
}

/// The eighth adversarial check's: a session came to a runner with the id of a message it took
/// where it was, and the runner couldn't take it in (a write failed); before a message came, the
/// runner upgraded in place (its record written with the id still waiting, its harness's process
/// handed to the next image). The next image goes on with that process, the id with it: m1, sent
/// again, is its repeat.
#[cfg(unix)]
#[test]
fn an_in_place_upgrade_keeps_the_ids_a_session_brought_and_has_not_taken_in() -> anyhow::Result<()> {
    use crate::harness::core::Update;
    use crate::harness::host::{Host, Limits as HostLimits, Saved};
    use kiln_seams::clock::SimClock;
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("harness.json");
        let records = Arc::new(Flaky::new(1));
        let rec: Saved = serde_json::from_value(serde_json::json!({
            "id": "s1",
            "spec": {"kind": "claude", "cwd": dir.path(), "program": dir.path().join("no-such-claude")},
            "inbox": ["m1"],
        }))?;
        {
            let (tx, _rx) = async_channel::unbounded();
            let before = Host::with_records(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()), Some(records.clone() as Arc<dyn Records>));
            before.move_in(rec, None)?;
            let inbox = before.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            until("its inbox opened", || futures::executor::block_on(inbox.epoch("s1")).ok().flatten().is_some()).await?;
            anyhow::ensure!(inbox.waiting("s1") == [("m1".to_owned(), String::new())], "m1 isn't waiting: {:?}", inbox.waiting("s1"));
            // Its record written with m1 waiting, as hand_over_all writes it before the exec.
            before.stop_all();
        }
        // Its harness's process handed to the next image: a process of this test's own, its pipes kept.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()?;
        let (Some(i), Some(o), Some(e)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else { anyhow::bail!("no pipes") };
        let process = crate::proc::Detached { pid: child.id(), stdin: i.into_raw_fd(), stdout: o.into_raw_fd(), stderr: e.into_raw_fd(), foreign: true };
        let handed = crate::harness::session::Handed {
            kind: "claude".into(),
            process,
            core: crate::harness::core::Core::new().freeze(),
            adapter: crate::harness::kinds::Frozen::Claude(Box::new(crate::harness::claude::Claude::new(false))),
            partial: String::new(),
        };
        let map: std::collections::HashMap<String, Box<crate::harness::session::Handed>> = std::iter::once(("s1".to_owned(), Box::new(handed))).collect();
        std::fs::write(dir.path().join("agent-handover.json"), serde_json::to_vec(&map)?)?;
        // The next image: the same folder, the same records.
        let (tx, rx) = async_channel::unbounded();
        let after = Host::with_records(tx, Some(file.clone()), HostLimits::default(), Arc::new(SimClock::new()), Some(records as Arc<dyn Records>));
        after.restore();
        let restored = after.get("s1").is_some();
        let sent = after.command("s1", message("m1"));
        let said = crate::rt::timeout(Duration::from_secs(30), async {
            loop {
                match rx.recv().await {
                    Ok((_, Update::Inbox { said })) => return Some(*said),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten();
        let _ = child.kill();
        let _ = child.wait();
        anyhow::ensure!(restored && sent, "the session wasn't restored ({restored}) or took nothing ({sent})");
        let said = said.ok_or_else(|| anyhow::anyhow!("no answer came"))?;
        anyhow::ensure!(said.answer.as_ref().is_ok_and(|a| a.repeat), "m1, taken where the session was, was answered {:?} after an in-place upgrade", said.answer);
        Ok(())
    })
}

/// The eighth adversarial check's: the seventh's case through a blue/green upgrade. box-b took s1
/// over (its answer to box-a lost on the way back), then upgraded blue/green: its sessions went to
/// the new build, a runner process with a folder and records of its own (`instances/<id>`), and s1
/// ended there. box-a's hand-over, sent again, reaches the new build: it's answered as taken (the
/// machine's note of its hand-overs is the machine's, not a process's), and s1 isn't taken over again.
#[cfg(unix)]
#[test]
fn a_hand_over_sent_again_after_a_blue_green_upgrade_is_answered_as_taken() -> anyhow::Result<()> {
    use crate::harness::host::{Host, Limits as HostLimits};
    use crate::harness::model::{QueueItem, State};
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut state = State::default();
        state.queue.push(QueueItem { id: "q1".into(), text: "carry on with the migration".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        let mut pkg = pack("s1", Spec { kind: "claude".into(), cwd: Some(cwd.clone()), ..Spec::default() }, &cwd, "box-a", &cwd, Carry::Nothing, Vec::new(), state);
        pkg.spec.program = Some(program.to_string_lossy().into_owned());
        let bytes = serde_json::to_vec(&pkg)?;
        let (blue_dir, green_dir) = (dir.path().join("instances").join("blue"), dir.path().join("instances").join("green"));
        std::fs::create_dir_all(&blue_dir)?;
        std::fs::create_dir_all(&green_dir)?;
        let (tx, _rx) = async_channel::unbounded();
        let blue = Host::with_records(tx, Some(blue_dir.join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        blue.adopt("box-a", &bytes).map_err(anyhow::Error::msg)?;
        until("q1 reached box-b's harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
        // box-b upgrades blue/green: its sessions go to the new build.
        let (tx2, _rx2) = async_channel::unbounded();
        let green = Host::with_records(tx2, Some(green_dir.join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        for (rec, handed) in blue.move_out().await {
            green.move_in(rec, handed)?;
        }
        anyhow::ensure!(green.get("s1").is_some(), "s1 didn't go to the new build");
        anyhow::ensure!(green.close("s1"), "no session to end");
        until("the new build let s1 go", || green.get("s1").is_none()).await?;
        let again = green.adopt("box-a", &bytes);
        crate::rt::sleep(Duration::from_millis(1500)).await;
        let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
        let runs_again = green.get("s1").is_some();
        if runs_again {
            green.close("s1");
        }
        anyhow::ensure!(again.is_ok() && !runs_again && reached == 1, "s1 ended on box-b's new build; box-a's hand-over sent again was answered {again:?}, s1 runs there again: {runs_again}, q1 reached a harness {reached} times");
        Ok(())
    })
}

/// box-b takes s1 over from box-a but can't write down that it did (its note's lock can't be
/// opened); s1 ends there, and box-a's hand-over is sent again: box-b's runner still knows it took
/// it, and answers it as taken (s1 isn't taken over again).
#[cfg(unix)]
#[test]
fn a_hand_over_that_could_not_be_written_down_is_still_answered_as_taken() -> anyhow::Result<()> {
    use crate::harness::host::{Host, Limits as HostLimits};
    use crate::harness::model::{QueueItem, State};
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut state = State::default();
        state.queue.push(QueueItem { id: "q1".into(), text: "carry on with the migration".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
        let mut pkg = pack("s1", Spec { kind: "claude".into(), cwd: Some(cwd.clone()), ..Spec::default() }, &cwd, "box-a", &cwd, Carry::Nothing, Vec::new(), state);
        pkg.spec.program = Some(program.to_string_lossy().into_owned());
        let bytes = serde_json::to_vec(&pkg)?;
        std::fs::create_dir_all(dir.path().join("handovers-taken.lock"))?;
        let (tx, _rx) = async_channel::unbounded();
        let host = Host::with_records(tx, Some(dir.path().join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        host.adopt("box-a", &bytes).map_err(anyhow::Error::msg)?;
        until("q1 reached box-b's harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
        anyhow::ensure!(!dir.path().join("handovers-taken.json").exists(), "the note was written: this test needs it not to be");
        anyhow::ensure!(host.close("s1"), "no session to end");
        until("box-b let s1 go", || host.get("s1").is_none()).await?;
        let again = host.adopt("box-a", &bytes);
        crate::rt::sleep(Duration::from_millis(1500)).await;
        let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
        let runs_again = host.get("s1").is_some();
        if runs_again {
            host.close("s1");
        }
        anyhow::ensure!(again.is_ok() && !runs_again && reached == 1, "s1 ended on box-b, which couldn't write down taking it; box-a's hand-over sent again was answered {again:?}, s1 runs there again: {runs_again}, q1 reached a harness {reached} times");
        Ok(())
    })
}

/// CP-T3.2 (the ninth adversarial check's): s1 came to blue with the id of m1, a message it took
/// where it was, and blue couldn't take it in (a write failed): m1 waits, held by blue's host and
/// kept in s1's record. blue/green then hands s1 to the new build while blue's inbox can't be read:
/// s1 goes with the ids it holds itself, m1 among them. m1's sender, its answer lost, sends it
/// again to the new build: m1's repeat there, never a second prompt.
#[cfg(unix)]
#[test]
fn a_blue_green_handover_with_an_unreadable_inbox_keeps_the_ids_its_session_brought() -> anyhow::Result<()> {
    use crate::harness::core::Update;
    use crate::harness::host::{Host, Limits as HostLimits, Saved};
    use kiln_seams::clock::SimClock;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (blue_dir, green_dir) = (dir.path().join("instances").join("blue"), dir.path().join("instances").join("green"));
        std::fs::create_dir_all(&blue_dir)?;
        std::fs::create_dir_all(&green_dir)?;
        let records = Arc::new(Flaky::new(1));
        let rec: Saved = serde_json::from_value(serde_json::json!({
            "id": "s1",
            "spec": {"kind": "claude", "cwd": dir.path(), "program": dir.path().join("no-such-claude")},
            "inbox": ["m1"],
        }))?;
        let (tx, _rx) = async_channel::unbounded();
        let blue = Host::with_records(tx, Some(blue_dir.join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(records.clone() as Arc<dyn Records>));
        blue.move_in(rec, None)?;
        let inbox = blue.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
        until("its inbox opened", || futures::executor::block_on(inbox.epoch("s1")).ok().flatten().is_some()).await?;
        anyhow::ensure!(inbox.waiting("s1") == [("m1".to_owned(), String::new())], "m1 isn't waiting: {:?}", inbox.waiting("s1"));
        // Blue/green: blue's inbox can't be read as it hands s1 over.
        records.scans_fail.store(true, Ordering::SeqCst);
        let moved = blue.move_out().await;
        records.scans_fail.store(false, Ordering::SeqCst);
        let carried: Vec<Vec<String>> = moved.iter().map(|(r, _)| serde_json::to_value(r).ok().and_then(|v| v.get("inbox").cloned()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()).collect();
        let (tx2, rx2) = async_channel::unbounded();
        let green = Host::with_records(tx2, Some(green_dir.join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new())));
        for (rec, handed) in moved {
            green.move_in(rec, handed)?;
        }
        anyhow::ensure!(green.command("s1", message("m1")), "the new build took nothing");
        let said = crate::rt::timeout(Duration::from_secs(30), async {
            loop {
                match rx2.recv().await {
                    Ok((_, Update::Inbox { said })) => return Some(*said),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("no answer came"))?;
        anyhow::ensure!(said.answer.as_ref().is_ok_and(|a| a.repeat), "blue held m1 (waiting) but handed s1 over with the ids {carried:?}; m1 sent again to the new build was answered {:?}: a second prompt", said.answer);
        Ok(())
    })
}

/// What an older runner process of a machine hands to its newer one (CP-T3.2): session `id`, q1
/// waiting in its queue, as try `attempt` at handing it over (`moving::Package::attempt`), its
/// harness `program`.
#[cfg(unix)]
fn handed(id: &str, attempt: &str, program: &std::path::Path, cwd: &str) -> anyhow::Result<Vec<u8>> {
    use crate::harness::model::{QueueItem, State};
    use crate::harness::moving::{Carry, pack};
    use crate::harness::session::Spec;
    let mut state = State::default();
    state.queue.push(QueueItem { id: "q1".into(), text: "carry on with the migration".into(), at: 0, attachments: Vec::new(), overrides: None, reply_to: None });
    let mut pkg = pack(id, Spec { kind: "claude".into(), cwd: Some(cwd.to_owned()), ..Spec::default() }, cwd, "box-a", cwd, Carry::Nothing, Vec::new(), state);
    pkg.spec.program = Some(program.to_string_lossy().into_owned());
    pkg.handover = true;
    pkg.attempt = Some(attempt.to_owned());
    Ok(serde_json::to_vec(&pkg)?)
}

/// A runner process of the machine whose state folder is `state`: its own folder under it, as a
/// machine's newer processes have (`upgrade::own_dir`).
#[cfg(unix)]
fn process_of(state: &std::path::Path, name: &str) -> anyhow::Result<Arc<crate::harness::host::Host>> {
    use crate::harness::host::{Host, Limits as HostLimits};
    use kiln_seams::clock::SimClock;
    let own = state.join("instances").join(name);
    std::fs::create_dir_all(&own)?;
    let (tx, rx) = async_channel::unbounded();
    // What it says goes nowhere.
    crate::rt::spawn(async move { while rx.recv().await.is_ok() {} });
    Ok(Host::with_records(tx, Some(own.join("harness.json")), HostLimits::default(), Arc::new(SimClock::new()), Some(Arc::new(SimRecords::new()))))
}

/// CP-T3.2, AC-T3.2.1 (the nineteenth adversarial check's): one try at handing a session to its
/// machine's newer runner process, its package come to two of them at once (the older process's
/// link dropped, and its try sent again went to a newer process still, come meanwhile, while the
/// first was taking it over): one takes it over, the other answers it taken over on the machine
/// (never a refusal: the older process would go on with it), and q1 reaches one harness. The older
/// process, looking, finds it taken over. And a try the older process took back (decided not taken)
/// is taken over by none.
#[cfg(unix)]
#[test]
fn one_try_at_a_local_hand_over_is_taken_over_by_one_process_of_its_machine() -> anyhow::Result<()> {
    use crate::harness::host::{Decided, TAKEN_ELSEWHERE};
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let state = dir.path().join("state");
        let (older, two, three) = (process_of(&state, "one")?, process_of(&state, "two")?, process_of(&state, "three")?);
        let bytes = handed("s1", "a1", &program, &cwd)?;
        let (h2, h3, b2, b3) = (Arc::clone(&two), Arc::clone(&three), bytes.clone(), bytes);
        let (r2, r3) = futures::join!(
            crate::rt::blocking(move || h2.adopt("an older kiln runner of this machine", &b2)),
            crate::rt::blocking(move || h3.adopt("an older kiln runner of this machine", &b3))
        );
        let (r2, r3) = (r2?, r3?);
        let answers = [&r2, &r3];
        let took = answers.iter().filter(|r| r.is_ok()).count();
        let elsewhere = answers.iter().filter(|r| r.as_ref().err().is_some_and(|e| e.starts_with(TAKEN_ELSEWHERE))).count();
        anyhow::ensure!(took == 1 && elsewhere == 1, "one try, two newer processes: answered {r2:?} and {r3:?}");
        anyhow::ensure!(two.get("s1").is_some() != three.get("s1").is_some(), "s1 runs in both newer processes, or neither");
        until("q1 reached a harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
        crate::rt::sleep(Duration::from_millis(1500)).await;
        let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
        anyhow::ensure!(reached == 1, "q1 reached {reached} harnesses");
        // Sent again to the one that runs it: taken; to the other: taken over on the machine.
        let again = handed("s1", "a1", &program, &cwd)?;
        let (runs, other) = if two.get("s1").is_some() { (&two, &three) } else { (&three, &two) };
        anyhow::ensure!(runs.adopt("box-a", &again).is_ok(), "the process that runs it answered it as not taken");
        anyhow::ensure!(other.adopt("box-a", &again).is_err_and(|e| e.starts_with(TAKEN_ELSEWHERE)), "the other answered it as more than taken elsewhere");
        anyhow::ensure!(older.settle("a1", "s1", true) == Ok(Some(Decided::Taken)), "the older process looked and found {:?}", older.settle("a1", "s1", false));
        // A try the older process took back: taken over by none, said so.
        anyhow::ensure!(older.settle("a2", "s2", true) == Ok(Some(Decided::Refused)), "taken back");
        let refused = three.adopt("box-a", &handed("s2", "a2", &program, &cwd)?);
        anyhow::ensure!(refused.as_ref().is_err_and(|e| e.starts_with(crate::reach::node::NOT_TAKEN)) && three.get("s2").is_none(), "a try taken back was answered {refused:?}");
        crate::rt::sleep(Duration::from_millis(500)).await;
        let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
        anyhow::ensure!(reached == 1, "the try taken back ran: q1 reached {reached} harnesses");
        runs.close("s1");
        Ok(())
    })
}

/// CP-T3.2 (the nineteenth adversarial check's): a newer runner process that can't write down how
/// a try at handing a session over went takes nothing over and refuses nothing: it can't say
/// (neither reach's `NOT_TAKEN` nor taken over elsewhere), so the older process asks again, and
/// nothing of the session runs here meanwhile.
#[cfg(unix)]
#[test]
fn a_hand_over_whose_decision_can_t_be_written_is_neither_taken_nor_refused() -> anyhow::Result<()> {
    use crate::harness::host::TAKEN_ELSEWHERE;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let state = dir.path().join("state");
        let newer = process_of(&state, "newer")?;
        // Its record can't be written: where its write goes first is taken by a folder.
        let blocked = state.join(format!("handovers-decided.tmp.{}", std::process::id()));
        std::fs::create_dir_all(&blocked)?;
        let said = newer.adopt("box-a", &handed("s1", "a1", &program, &cwd)?);
        crate::rt::sleep(Duration::from_millis(1000)).await;
        anyhow::ensure!(
            said.as_ref().is_err_and(|e| !e.starts_with(crate::reach::node::NOT_TAKEN) && !e.starts_with(TAKEN_ELSEWHERE)) && newer.get("s1").is_none() && prompts(&heard).is_empty(),
            "its decision unwritable, it answered {said:?}; s1 here: {}; prompts {:?}",
            newer.get("s1").is_some(),
            prompts(&heard)
        );
        // Written again: taken over.
        std::fs::remove_dir(&blocked)?;
        anyhow::ensure!(newer.adopt("box-a", &handed("s1", "a1", &program, &cwd)?).is_ok(), "not taken over once it could be written");
        until("q1 reached its harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
        newer.close("s1");
        Ok(())
    })
}

/// CP-T3.2, AC-T3.2.1 (the nineteenth adversarial check's): a package that names no try (a move from
/// another machine, or a hand-over from a runner process before this release), come to two of a
/// machine's runner processes at once: they take it over one at a time, so one runs it, the other
/// finding it taken, and q1 reaches one harness.
#[cfg(unix)]
#[test]
fn a_package_naming_no_try_is_taken_over_by_one_process_of_its_machine() -> anyhow::Result<()> {
    use std::time::Duration;
    crate::rt::block_on(async move {
        for handover in [true, false] {
            let dir = tempfile::tempdir()?;
            let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
            answering_claude(&program, &heard)?;
            let cwd = dir.path().to_string_lossy().into_owned();
            let state = dir.path().join("state");
            let mut pkg: serde_json::Value = serde_json::from_slice(&handed("s1", "a1", &program, &cwd)?)?;
            if let Some(o) = pkg.as_object_mut() {
                o.remove("attempt");
                if !handover {
                    o.remove("handover");
                }
            }
            let bytes = serde_json::to_vec(&pkg)?;
            let (two, three) = (process_of(&state, "two")?, process_of(&state, "three")?);
            let (h2, h3, b2, b3) = (Arc::clone(&two), Arc::clone(&three), bytes.clone(), bytes);
            let (r2, r3) = futures::join!(crate::rt::blocking(move || h2.adopt("box-a", &b2)), crate::rt::blocking(move || h3.adopt("box-a", &b3)));
            let (r2, r3) = (r2?, r3?);
            anyhow::ensure!(two.get("s1").is_some() != three.get("s1").is_some(), "handed over {handover}: s1 runs in both processes, or neither: answered {r2:?} and {r3:?}");
            until("q1 reached a harness", || prompts(&heard).iter().any(|p| p == "q1")).await?;
            crate::rt::sleep(Duration::from_millis(1500)).await;
            let reached = prompts(&heard).iter().filter(|p| *p == "q1").count();
            anyhow::ensure!(reached == 1, "handed over {handover}: q1 reached {reached} harnesses (answered {r2:?} and {r3:?})");
            two.close("s1");
            three.close("s1");
        }
        Ok(())
    })
}

/// CP-T3.2 with main's hand-overs that wait out a blue/green upgrade: a runner process leaving (it
/// hands everything to a new build of it) answers a try at handing a session over as not taken, and
/// it's written down for the try: the older process, settling, finds it refused, and no other process
/// of the machine takes that try over after; nothing of it runs.
#[cfg(unix)]
#[test]
fn a_process_leaving_turns_a_try_away_for_good() -> anyhow::Result<()> {
    use crate::harness::host::Decided;
    use crate::reach::node::NOT_TAKEN;
    use std::time::Duration;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir()?;
        let (heard, program) = (dir.path().join("heard.jsonl"), dir.path().join("answering-claude"));
        answering_claude(&program, &heard)?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let state = dir.path().join("state");
        let (older, leaving, other) = (process_of(&state, "one")?, process_of(&state, "two")?, process_of(&state, "three")?);
        let path = dir.path().join("package.json");
        std::fs::write(&path, handed("s1", "a1", &program, &cwd)?)?;
        let (handover, said) = leaving.adopt_package(&path, true);
        anyhow::ensure!(handover && said.as_ref().is_err_and(|e| e.starts_with(NOT_TAKEN) && e.contains("leaving")), "the process leaving answered {said:?}");
        anyhow::ensure!(older.settle("a1", "s1", false) == Ok(Some(Decided::Refused)), "not written down as not taken: {:?}", older.settle("a1", "s1", false));
        // Sent again to another process of the machine (the new build, say): not taken over there.
        let again = other.adopt("an older kiln runner of this machine", &handed("s1", "a1", &program, &cwd)?);
        anyhow::ensure!(again.as_ref().is_err_and(|e| e.starts_with(NOT_TAKEN)) && other.get("s1").is_none() && leaving.get("s1").is_none(), "the try turned away was answered {again:?} after");
        crate::rt::sleep(Duration::from_millis(500)).await;
        anyhow::ensure!(prompts(&heard).is_empty(), "something of it ran: {:?}", prompts(&heard));
        Ok(())
    })
}
