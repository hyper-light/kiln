//! The `Topics` contract, clause by clause (`QUIVER-n`, docs/design/topics.md): cases each run
//! against a fresh implementation of their own. The simulator passes them, and so does Quiver's
//! node; an implementation that never answers fails at its deadline ([`run_within`]).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use futures::FutureExt as _;
use futures::future::BoxFuture;

use crate::sim::SimTopics;
use crate::{Class, Message, Next, Offset, Refusal, Start, Subscription, Topic, Topics};

/// What a case found wrong, in words.
pub type Failure = String;

/// What a case runs against: an empty implementation of its own.
pub trait TopicsFixture: Send + Sync {
    fn topics(&self) -> &dyn Topics;
    /// Whether the implementation serves topics of `class`.
    fn serves(&self, class: Class) -> bool;
}

/// The fixture for a [`SimTopics`]: it serves every class.
pub struct SimFixture(pub SimTopics);

impl TopicsFixture for SimFixture {
    fn topics(&self) -> &dyn Topics {
        &self.0
    }

    fn serves(&self, _class: Class) -> bool {
        true
    }
}

/// One clause of the contract, checked against what fixture `F` gives.
pub struct Case<F: ?Sized> {
    /// The clause's id (`QUIVER-3`).
    pub id: &'static str,
    /// What the clause says.
    pub says: &'static str,
    pub check: for<'a> fn(&'a F) -> BoxFuture<'a, Result<(), Failure>>,
}

/// What a suite's run found.
#[derive(Debug, Default)]
pub struct Report {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, Failure)>,
    seam: &'static str,
}

impl Report {
    /// Every case passed (and there was at least one).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty() && !self.passed.is_empty()
    }

    /// How many passed, or every failure by its clause.
    pub fn into_result(self) -> Result<usize, String> {
        if self.is_ok() {
            return Ok(self.passed.len());
        }
        let lines: Vec<String> = self
            .failed
            .iter()
            .map(|(id, why)| format!("{id}: {why}"))
            .collect();
        Err(format!(
            "{}: {} of {} cases failed\n{}",
            self.seam,
            self.failed.len(),
            self.passed.len().saturating_add(self.failed.len()),
            lines.join("\n")
        ))
    }
}

/// Runs each case against a fixture of its own (`make` builds one a case), in order, with no
/// deadline.
pub async fn run<F, M, Fut>(seam: &'static str, cases: &[Case<F>], make: M) -> Report
where
    M: FnMut() -> Fut,
    Fut: Future<Output = F>,
{
    run_within(seam, cases, make, futures::future::pending::<()>).await
}

/// [`run`], each case failed once its deadline (`deadline()`, made afresh a case) completes first:
/// a case whose implementation never answers fails, named, and the suite goes on.
pub async fn run_within<F, M, Fut, D, DFut>(
    seam: &'static str,
    cases: &[Case<F>],
    mut make: M,
    mut deadline: D,
) -> Report
where
    M: FnMut() -> Fut,
    Fut: Future<Output = F>,
    D: FnMut() -> DFut,
    DFut: Future<Output = ()>,
{
    let mut report = Report {
        seam,
        ..Report::default()
    };
    for case in cases {
        let fixture = make().await;
        let late = deadline();
        futures::pin_mut!(late);
        let outcome = match futures::future::select((case.check)(&fixture), late).await {
            futures::future::Either::Left((outcome, _)) => outcome,
            futures::future::Either::Right(((), _)) => {
                Err("it didn't end by its deadline: the implementation never answered".to_owned())
            }
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
pub fn fine<T, E: std::fmt::Display>(what: &str, v: Result<T, E>) -> Result<T, Failure> {
    v.map_err(|e| format!("{what}: {e}"))
}

/// The refusal `v` should have been, or a failure saying it wasn't.
pub fn refused<T: std::fmt::Debug, E>(what: &str, v: Result<T, E>) -> Result<E, Failure> {
    match v {
        Ok(t) => Err(format!(
            "{what}: accepted ({t:?}), should have been refused"
        )),
        Err(e) => Ok(e),
    }
}

// ------------------------------------------------------------------------------------ helpers

const T: &str = "conformance";

fn topic(retain_messages: u64) -> Topic {
    Topic {
        class: Class::Ephemeral,
        retain_messages,
        retain_bytes: 1 << 20,
    }
}

fn payloads(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| String::from_utf8_lossy(&m.payload).into_owned())
        .collect()
}

async fn publish_all(t: &dyn Topics, name: &str, ps: &[&str]) -> Result<Vec<Offset>, Failure> {
    let mut out = Vec::with_capacity(ps.len());
    for p in ps {
        out.push(fine("publish", t.publish(name, None, p.as_bytes()).await)?);
    }
    Ok(out)
}

/// What a subscription took, as its own copy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Took {
    Message(String),
    Lag(Offset, Offset),
    Ended,
}

/// Everything a subscription has now, up to the end (at most 100,000 takes).
fn drain(s: &mut dyn Subscription) -> Vec<Took> {
    let mut out = Vec::new();
    for _ in 0..100_000 {
        match s.take() {
            None => break,
            Some(Next::Message(m)) => out.push(Took::Message(
                String::from_utf8_lossy(m.payload).into_owned(),
            )),
            Some(Next::Lag { from, to }) => out.push(Took::Lag(from, to)),
            Some(Next::Ended) => {
                out.push(Took::Ended);
                break;
            }
        }
    }
    out
}

fn messages(ps: &[&str]) -> Vec<Took> {
    ps.iter().map(|p| Took::Message((*p).to_owned())).collect()
}

/// A waker that counts its wakes, and says so to whoever awaits one ([`Count::woken`]).
#[derive(Default)]
struct Count {
    wakes: AtomicUsize,
    said: event_listener::Event,
}

impl Count {
    /// Completes once it has been woken at least once.
    async fn woken(&self) {
        loop {
            let listener = self.said.listen();
            if self.wakes.load(Ordering::SeqCst) > 0 {
                return;
            }
            listener.await;
        }
    }
}

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.said.notify(usize::MAX);
    }
}

// -------------------------------------------------------------------------------------- cases

fn c1<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        same(
            "the offsets given",
            publish_all(t, T, &["a", "b", "c"]).await?,
            vec![0, 1, 2],
        )?;
        same("the bounds", fine("bounds", t.bounds(T).await)?, (0, 3))
    })
}

fn c2<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        fine("publish", t.publish(T, Some(b"k1"), b"first").await)?;
        fine("publish", t.publish(T, None, b"second").await)?;
        let all = fine("read", t.read(T, 0, 10).await)?;
        same(
            "what was read",
            payloads(&all.messages),
            vec!["first".to_owned(), "second".to_owned()],
        )?;
        same(
            "the first message's key",
            all.messages.first().and_then(|m| m.key.clone()),
            Some(b"k1".to_vec()),
        )?;
        same(
            "the second message's key",
            all.messages.get(1).and_then(|m| m.key.clone()),
            None,
        )?;
        same(
            "the offsets read",
            all.messages.iter().map(|m| m.offset).collect::<Vec<_>>(),
            vec![0, 1],
        )?;
        same("where reading goes on", all.next, 2)?;
        let tail = fine("read from 1", t.read(T, 1, 10).await)?;
        same(
            "a read from an offset",
            payloads(&tail.messages),
            vec!["second".to_owned()],
        )
    })
}

fn c3<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["x", "y"]).await?;
        let one = fine("a first reader", t.read(T, 0, 10).await)?;
        let two = fine("a second reader", t.read(T, 0, 10).await)?;
        same(
            "what two readers read",
            payloads(&one.messages),
            payloads(&two.messages),
        )?;
        same("how many", one.messages.len(), 2)?;
        let mut a = fine("a first subscription", t.subscribe(T, Start::Oldest).await)?;
        let mut b = fine("a second subscription", t.subscribe(T, Start::Oldest).await)?;
        same(
            "what the first subscription took",
            drain(a.as_mut()),
            messages(&["x", "y"]),
        )?;
        same(
            "what the second subscription took",
            drain(b.as_mut()),
            messages(&["x", "y"]),
        )
    })
}

fn c4<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(2)).await)?;
        publish_all(t, T, &["a", "b", "c", "d"]).await?;
        same(
            "the bounds past retention",
            fine("bounds", t.bounds(T).await)?,
            (2, 4),
        )?;
        let e = refused("a read from before what's kept", t.read(T, 0, 10).await)?;
        same("the refusal", e, Refusal::Behind { asked: 0, first: 2 })?;
        same(
            "what's kept",
            payloads(&fine("read", t.read(T, 2, 10).await)?.messages),
            vec!["c".to_owned(), "d".to_owned()],
        )
    })
}

fn c5<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        let small = Topic {
            class: Class::Ephemeral,
            retain_messages: 100,
            retain_bytes: 10,
        };
        fine("create", t.create(T, small).await)?;
        publish_all(t, T, &["aaaa", "bbbb", "cccc"]).await?;
        same(
            "the bounds past the retention in bytes",
            fine("bounds", t.bounds(T).await)?,
            (1, 3),
        )?;
        fine("publish", t.publish(T, Some(b"kk"), b"dddddddd").await)?;
        same(
            "the bounds once a message of 10 bytes is kept",
            fine("bounds", t.bounds(T).await)?,
            (3, 4),
        )?;
        let e = refused(
            "a message over the topic's retention",
            t.publish(T, None, b"eeeeeeeeeee").await,
        )?;
        ensure(
            matches!(
                e,
                Refusal::TooLarge {
                    size: 11,
                    limit: 10,
                    ..
                }
            ),
            || format!("refused as {e:?}"),
        )?;
        same(
            "the bounds after it",
            fine("bounds", t.bounds(T).await)?,
            (3, 4),
        )
    })
}

fn c6<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a"]).await?;
        ensure(
            t.wait(T, 0).now_or_never().is_some_and(|r| r.is_ok()),
            || "a wait for a message already there didn't complete at once".into(),
        )?;
        let mut waiting = t.wait(T, 1);
        ensure((&mut waiting).now_or_never().is_none(), || {
            "a wait for a message not yet published completed".into()
        })?;
        publish_all(t, T, &["b"]).await?;
        same("the wait, once it's published", waiting.await, Ok(()))
    })
}

fn c7<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        let none = || Refusal::NoTopic("conformance-none".into());
        same(
            "a publish to no topic",
            t.publish("conformance-none", None, b"x").await,
            Err(none()),
        )?;
        same(
            "a read of no topic",
            t.read("conformance-none", 0, 1).await,
            Err(none()),
        )?;
        same(
            "a wait on no topic",
            t.wait("conformance-none", 0).await,
            Err(none()),
        )?;
        same(
            "the bounds of no topic",
            t.bounds("conformance-none").await,
            Err(none()),
        )?;
        same(
            "a delete of no topic",
            t.delete("conformance-none").await,
            Err(none()),
        )?;
        same(
            "a subscription to no topic",
            t.subscribe("conformance-none", Start::Oldest)
                .await
                .map(|_| ()),
            Err(none()),
        )
    })
}

fn c8<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(10)).await)?;
        fine("the same create again", t.create(T, topic(10)).await)?;
        same(
            "a create with other settings",
            t.create(T, topic(11)).await,
            Err(Refusal::Exists(T.into())),
        )?;
        ensure(
            matches!(
                t.create("Not a name!", topic(10)).await,
                Err(Refusal::Invalid { .. })
            ),
            || "a bad topic name wasn't refused Invalid".into(),
        )?;
        ensure(
            matches!(
                t.create("conformance-zero", topic(0)).await,
                Err(Refusal::Invalid { .. })
            ),
            || "a retention of 0 messages wasn't refused Invalid".into(),
        )?;
        let no_bytes = Topic {
            retain_bytes: 0,
            ..topic(10)
        };
        ensure(
            matches!(
                t.create("conformance-zero", no_bytes).await,
                Err(Refusal::Invalid { .. })
            ),
            || "a retention of 0 bytes wasn't refused Invalid".into(),
        )?;
        let too_many = topic(crate::RETAIN_MESSAGES_MAX.saturating_add(1));
        ensure(
            matches!(
                t.create("conformance-many", too_many).await,
                Err(Refusal::Invalid { .. })
            ),
            || "a retention past the most wasn't refused Invalid".into(),
        )?;
        same(
            "a refused create made no topic",
            t.bounds("conformance-zero").await,
            Err(Refusal::NoTopic("conformance-zero".into())),
        )
    })
}

fn c9<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        let wide = Topic {
            class: Class::Ephemeral,
            retain_messages: 10,
            retain_bytes: crate::RETAIN_BYTES_MAX,
        };
        fine("create", t.create(T, wide).await)?;
        let big = vec![0; crate::PAYLOAD_MAX.saturating_add(1)];
        let e = refused("a payload over the limit", t.publish(T, None, &big).await)?;
        ensure(
            matches!(
                e,
                Refusal::TooLarge {
                    what: "a payload",
                    ..
                }
            ),
            || format!("refused as {e:?}"),
        )?;
        let long = vec![0; crate::KEY_MAX.saturating_add(1)];
        let e = refused(
            "a key over the limit",
            t.publish(T, Some(&long), b"x").await,
        )?;
        ensure(matches!(e, Refusal::TooLarge { what: "a key", .. }), || {
            format!("refused as {e:?}")
        })?;
        same(
            "the bounds after them",
            fine("bounds", t.bounds(T).await)?,
            (0, 0),
        )?;
        let largest = vec![7; crate::PAYLOAD_MAX];
        same(
            "the largest payload",
            fine("publish", t.publish(T, None, &largest).await)?,
            0,
        )?;
        let read = fine("read", t.read(T, 0, 1).await)?;
        same(
            "the largest payload read back",
            read.messages.first().map(|m| m.payload.len()),
            Some(crate::PAYLOAD_MAX),
        )
    })
}

fn c10<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a", "b", "c", "d", "e"]).await?;
        let first = fine("read", t.read(T, 0, 2).await)?;
        same(
            "a limited read",
            (payloads(&first.messages), first.next),
            (vec!["a".to_owned(), "b".to_owned()], 2),
        )?;
        let rest = fine("read on", t.read(T, first.next, 10).await)?;
        same(
            "reading on",
            (payloads(&rest.messages), rest.next),
            (vec!["c".to_owned(), "d".to_owned(), "e".to_owned()], 5),
        )?;
        let none = fine("read at the end", t.read(T, rest.next, 10).await)?;
        same(
            "a read at the end",
            (none.messages.len(), none.next),
            (0, 5),
        )?;
        let past = fine("read past the end", t.read(T, 9, 10).await)?;
        same(
            "a read past the end",
            (past.messages.len(), past.next),
            (0, 5),
        )?;
        ensure(
            matches!(t.read(T, 0, 0).await, Err(Refusal::Invalid { .. })),
            || "a read of 0 wasn't refused Invalid".into(),
        )?;
        let many = fine("a read past the most", t.read(T, 0, usize::MAX).await)?;
        same(
            "a read past the most gives what's there",
            many.messages.len(),
            5,
        )
    })
}

fn c11<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create("conformance-one", topic(10)).await)?;
        fine("create", t.create("conformance-two", topic(10)).await)?;
        publish_all(t, "conformance-one", &["only in one"]).await?;
        same(
            "the other topic",
            fine("read", t.read("conformance-two", 0, 10).await)?
                .messages
                .len(),
            0,
        )?;
        let mut two = fine(
            "subscribe",
            t.subscribe("conformance-two", Start::Oldest).await,
        )?;
        same(
            "a subscription to the other topic",
            drain(two.as_mut()),
            Vec::new(),
        )?;
        same(
            "the other topic's offsets",
            publish_all(t, "conformance-two", &["first there"]).await?,
            vec![0],
        )
    })
}

fn c12<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a", "b"]).await?;
        let mut s = fine("subscribe", t.subscribe(T, Start::Oldest).await)?;
        same(
            "what it takes from the oldest",
            drain(s.as_mut()),
            messages(&["a", "b"]),
        )?;
        same("where it goes on", s.position(), 2)?;
        same("a take with nothing new", s.take().map(|_| ()), None)?;
        publish_all(t, T, &["c"]).await?;
        let took = s.take().map(|n| match n {
            Next::Message(m) => (m.offset, m.payload.to_vec()),
            other => (Offset::MAX, format!("{other:?}").into_bytes()),
        });
        same(
            "the next message, as published",
            took,
            Some((2, b"c".to_vec())),
        )
    })
}

fn c13<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a", "b", "c"]).await?;
        let mut latest = fine(
            "subscribe at the latest",
            t.subscribe(T, Start::Latest).await,
        )?;
        let mut at = fine("subscribe at 1", t.subscribe(T, Start::At(1)).await)?;
        let mut past = fine("subscribe past the end", t.subscribe(T, Start::At(9)).await)?;
        same("where past the end starts", past.position(), 3)?;
        publish_all(t, T, &["d"]).await?;
        same("from the latest", drain(latest.as_mut()), messages(&["d"]))?;
        same(
            "from offset 1",
            drain(at.as_mut()),
            messages(&["b", "c", "d"]),
        )?;
        same("from past the end", drain(past.as_mut()), messages(&["d"]))
    })
}

fn c14<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(3)).await)?;
        let mut s = fine("subscribe", t.subscribe(T, Start::Oldest).await)?;
        publish_all(t, T, &["a", "b", "c", "d", "e", "f", "g"]).await?;
        same(
            "what a subscription that fell behind takes",
            drain(s.as_mut()),
            vec![
                Took::Lag(0, 4),
                Took::Message("e".into()),
                Took::Message("f".into()),
                Took::Message("g".into()),
            ],
        )?;
        let mut at = fine(
            "subscribe at a gone offset",
            t.subscribe(T, Start::At(1)).await,
        )?;
        same(
            "a subscription started at a gone offset",
            drain(at.as_mut()),
            vec![
                Took::Lag(1, 4),
                Took::Message("e".into()),
                Took::Message("f".into()),
                Took::Message("g".into()),
            ],
        )
    })
}

fn c15<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(4)).await)?;
        let mut stalled = fine(
            "subscribe the stalled one",
            t.subscribe(T, Start::Oldest).await,
        )?;
        let mut keeping_up = fine(
            "subscribe the one keeping up",
            t.subscribe(T, Start::Oldest).await,
        )?;
        let mut took = Vec::new();
        for n in 0..100_u32 {
            // The stalled subscription is never taken from: each publish still completes.
            fine(
                "publish",
                t.publish(T, None, n.to_string().as_bytes()).await,
            )?;
            took.extend(drain(keeping_up.as_mut()));
        }
        let all: Vec<String> = (0..100_u32).map(|n| n.to_string()).collect();
        same(
            "what the one keeping up took",
            took,
            all.iter().map(|p| Took::Message(p.clone())).collect(),
        )?;
        same(
            "what the stalled one takes at last",
            drain(stalled.as_mut()),
            vec![
                Took::Lag(0, 96),
                Took::Message("96".into()),
                Took::Message("97".into()),
                Took::Message("98".into()),
                Took::Message("99".into()),
            ],
        )
    })
}

fn c16<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        let mut s = fine("subscribe", t.subscribe(T, Start::Latest).await)?;
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let poll = |s: &mut dyn Subscription| s.poll_ready(&mut Context::from_waker(&waker));
        same(
            "ready with nothing published",
            poll(s.as_mut()),
            Poll::Pending,
        )?;
        same(
            "woken before anything is published",
            count.wakes.load(Ordering::SeqCst),
            0,
        )?;
        publish_all(t, T, &["a"]).await?;
        // The wake may come from another thread a moment after the publish returns: it's awaited
        // (an implementation that never wakes fails at the suite's deadline).
        count.woken().await;
        same("ready once woken", poll(s.as_mut()), Poll::Ready(()))?;
        same("what it takes", drain(s.as_mut()), messages(&["a"]))?;
        same(
            "ready again with nothing new",
            poll(s.as_mut()),
            Poll::Pending,
        )?;
        let mut ready = s.ready();
        ensure((&mut ready).now_or_never().is_none(), || {
            "a ready() with nothing new completed".into()
        })?;
        publish_all(t, T, &["b"]).await?;
        ready.await;
        same(
            "what it takes after ready()",
            drain(s.as_mut()),
            messages(&["b"]),
        )
    })
}

fn c17<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a", "b"]).await?;
        let mut s = fine("subscribe", t.subscribe(T, Start::Latest).await)?;
        let mut waiting = t.wait(T, 5);
        ensure((&mut waiting).now_or_never().is_none(), || {
            "a wait for a later message completed".into()
        })?;
        fine("delete", t.delete(T).await)?;
        same(
            "the wait, once the topic is deleted",
            waiting.await,
            Err(Refusal::NoTopic(T.into())),
        )?;
        same(
            "the subscription, once the topic is deleted",
            drain(s.as_mut()),
            vec![Took::Ended],
        )?;
        same(
            "a publish to the deleted topic",
            t.publish(T, None, b"c").await,
            Err(Refusal::NoTopic(T.into())),
        )?;
        same(
            "a read of the deleted topic",
            t.read(T, 0, 1).await.map(|_| ()),
            Err(Refusal::NoTopic(T.into())),
        )?;
        fine("create it again", t.create(T, topic(100)).await)?;
        same(
            "its offsets start again",
            publish_all(t, T, &["first again"]).await?,
            vec![0],
        )?;
        same(
            "the old subscription stays ended",
            drain(s.as_mut()),
            vec![Took::Ended],
        )
    })
}

fn c18<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        ensure(f.serves(Class::Ephemeral), || {
            "every implementation serves the ephemeral class".into()
        })?;
        for class in Class::ALL {
            let name = format!("conformance-{}", class.name());
            let created = t.create(&name, Topic { class, ..topic(10) }).await;
            if f.serves(class) {
                fine(&format!("create a {} topic", class.name()), created)?;
                same(
                    &format!("a publish to a {} topic", class.name()),
                    t.publish(&name, None, b"x").await,
                    Ok(0),
                )?;
            } else {
                same(
                    &format!("a create of a {} topic, not served", class.name()),
                    created,
                    Err(Refusal::Unsupported(class)),
                )?;
                same(
                    "no topic made",
                    t.bounds(&name).await,
                    Err(Refusal::NoTopic(name.clone())),
                )?;
            }
        }
        Ok(())
    })
}

fn c19<F: TopicsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let t = f.topics();
        fine("create", t.create(T, topic(100)).await)?;
        publish_all(t, T, &["a", "b", "c"]).await?;
        let read = fine("read", t.read(T, 0, 10).await)?;
        let stamps: Vec<u64> = read.messages.iter().map(|m| m.at_us).collect();
        ensure(stamps.iter().all(|s| *s > 0), || {
            format!("unstamped messages: {stamps:?}")
        })?;
        ensure(
            stamps.windows(2).all(|w| matches!(w, [a, b] if a <= b)),
            || format!("stamps out of order: {stamps:?}"),
        )?;
        let mut s = fine("subscribe", t.subscribe(T, Start::Oldest).await)?;
        let taken: Vec<u64> = (0..3)
            .filter_map(|_| match s.take() {
                Some(Next::Message(m)) => Some(m.at_us),
                _ => None,
            })
            .collect();
        same("a subscription's stamps, as read", taken, stamps)
    })
}

/// The `Topics` contract, clause by clause.
#[must_use]
pub fn cases<F: TopicsFixture>() -> Vec<Case<F>> {
    vec![
        Case {
            id: "QUIVER-1",
            says: "a topic's messages get offsets 0, 1, 2… in the order they were published",
            check: c1::<F>,
        },
        Case {
            id: "QUIVER-2",
            says: "a read from an offset gives the messages from it in publish order, keys and payloads intact, and where to go on",
            check: c2::<F>,
        },
        Case {
            id: "QUIVER-3",
            says: "every reader and every subscription takes every message kept: none takes them from another",
            check: c3::<F>,
        },
        Case {
            id: "QUIVER-4",
            says: "a topic keeps its newest messages up to its retention in messages; a read from before them is refused Behind, naming where they start",
            check: c4::<F>,
        },
        Case {
            id: "QUIVER-5",
            says: "a topic keeps its newest messages up to its retention in bytes (keys and payloads); a message it can't keep alone is refused TooLarge",
            check: c5::<F>,
        },
        Case {
            id: "QUIVER-6",
            says: "a wait completes once a message at its offset or later is there, at once when one is",
            check: c6::<F>,
        },
        Case {
            id: "QUIVER-7",
            says: "every request about a topic that doesn't exist is refused NoTopic",
            check: c7::<F>,
        },
        Case {
            id: "QUIVER-8",
            says: "creating a topic again with the same settings changes nothing; with others it's refused Exists; bad names and retentions are refused Invalid and make nothing",
            check: c8::<F>,
        },
        Case {
            id: "QUIVER-9",
            says: "a payload or key over its limit is refused TooLarge and publishes nothing; the largest allowed is kept whole",
            check: c9::<F>,
        },
        Case {
            id: "QUIVER-10",
            says: "a read gives at most its limit and says where to go on; at or past the end it's empty; a limit of 0 is refused",
            check: c10::<F>,
        },
        Case {
            id: "QUIVER-11",
            says: "topics are apart: each has its own messages, offsets and subscriptions",
            check: c11::<F>,
        },
        Case {
            id: "QUIVER-12",
            says: "a subscription takes every message from its start, in order, and each one published after, as published",
            check: c12::<F>,
        },
        Case {
            id: "QUIVER-13",
            says: "a subscription starts at the oldest kept, at the latest, or at an offset (past the end: the end)",
            check: c13::<F>,
        },
        Case {
            id: "QUIVER-14",
            says: "a subscription that falls behind what's kept takes a lag notice naming the offsets it missed, then goes on from the oldest kept",
            check: c14::<F>,
        },
        Case {
            id: "QUIVER-15",
            says: "a stalled subscription holds up no publish and no other subscription; it takes a lag notice when it goes on",
            check: c15::<F>,
        },
        Case {
            id: "QUIVER-16",
            says: "a subscription is ready once a take has something, and its waker is woken when a publish gives it something",
            check: c16::<F>,
        },
        Case {
            id: "QUIVER-17",
            says: "a deleted topic's subscriptions end, its waits are refused NoTopic, and created again its offsets start at 0",
            check: c17::<F>,
        },
        Case {
            id: "QUIVER-18",
            says: "each class an implementation serves takes topics; one it doesn't is refused Unsupported, never served as another",
            check: c18::<F>,
        },
        Case {
            id: "QUIVER-19",
            says: "each message is stamped with when it was published, in offset order, the same in reads and subscriptions",
            check: c19::<F>,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    fn hangs(_: &Nothing) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(futures::future::pending())
    }

    fn holds(_: &Nothing) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }

    /// A case whose implementation never answers fails at its deadline, named, and the suite goes
    /// on to the next.
    #[test]
    fn a_case_that_never_ends_fails_at_its_deadline_and_the_suite_goes_on() {
        let cases = [
            Case {
                id: "X-1",
                says: "never ends",
                check: hangs,
            },
            Case {
                id: "X-2",
                says: "holds",
                check: holds,
            },
        ];
        let report = futures::executor::block_on(run_within(
            "X",
            &cases,
            || async { Nothing },
            || futures::future::ready(()),
        ));
        assert_eq!(report.passed, ["X-2"]);
        assert_eq!(
            report.failed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            ["X-1"]
        );
    }

    /// Every clause has its own id, in order.
    #[test]
    fn the_clauses_are_numbered_in_order() {
        let ids: Vec<&str> = cases::<SimFixture>().iter().map(|c| c.id).collect();
        let want: Vec<String> = (1..=ids.len()).map(|n| format!("QUIVER-{n}")).collect();
        assert_eq!(ids, want);
    }
}
