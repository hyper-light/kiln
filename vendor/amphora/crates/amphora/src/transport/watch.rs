//! One bounded metadata-check reactor; socket I/O never runs on the polling thread.
use super::{
    common::{Client, Namespace, encode, read_limit, remaining, send, unavailable},
    endpoint::Stream,
    wire::{Answer, FRAME_MAX, Request, Response, SESSION_MAX, WATCH_BATCH_MAX, WATCH_REPLY_MAX},
};
use amphora_api::{self as api, Refusal};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    net::Shutdown,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

const REGISTRATION_MAX: usize = 4096;
const PROBE_MAX: usize = 4096;
// Admission/notification policy, not a measured performance claim. Metadata checks never
// enter the SQL writer or hold a server wait slot; a dormant batch checks at most 100/s.
const CHECK_INTERVAL: Duration = Duration::from_millis(10);
const REQUEST_MAX: Duration = Duration::from_secs(2);
static REACTOR: Mutex<Option<Arc<Reactor>>> = Mutex::new(None);

#[derive(Default)]
struct State {
    result: Option<Result<api::Rev, Refusal>>,
    waker: Option<Waker>,
}
struct Registration {
    client: Client,
    namespace: String,
    family: String,
    after: api::Rev,
    until: Instant,
    check_at: Instant,
    probes: usize,
    state: Arc<Mutex<State>>,
}
struct Flight {
    ids: Vec<u64>,
    stream: Stream,
}
#[derive(Default)]
struct Queue {
    next: u64,
    entries: BTreeMap<u64, Registration>,
    order: VecDeque<u64>,
    flight: Option<Flight>,
}
struct Reactor {
    queue: Mutex<Queue>,
    changed: Condvar,
    alive: AtomicBool,
}
struct Batch {
    client: Client,
    ids: Vec<u64>,
    requests: Vec<(String, String, api::Rev)>,
    until: Instant,
}

/// One remote Records registration on the shared, bounded metadata-check reactor.
pub struct Watch {
    id: u64,
    shared: Arc<Mutex<State>>,
    reactor: Arc<Reactor>,
    until: Instant,
    finished: bool,
}

fn reactor() -> Result<Arc<Reactor>, Refusal> {
    let mut global = REACTOR
        .lock()
        .map_err(|_| unavailable("watch reactor initialization lock poisoned"))?;
    if let Some(reactor) = global.as_ref() {
        return Ok(Arc::clone(reactor));
    }
    let reactor = Arc::new(Reactor {
        queue: Mutex::new(Queue::default()),
        changed: Condvar::new(),
        alive: AtomicBool::new(true),
    });
    let running = Arc::clone(&reactor);
    // Exactly one process-lifetime worker. Idle waits are bounded by SESSION_MAX;
    // the registry holds at most 4096 entries, including the current batch.
    thread::Builder::new()
        .name("amphora.watch".into())
        .spawn(move || {
            if let Err(error) = run(&running) {
                running.alive.store(false, Ordering::Release);
                fail_all(&running, error);
            }
        })
        .map_err(unavailable)?;
    *global = Some(Arc::clone(&reactor));
    Ok(reactor)
}
impl Namespace {
    /// Watches durable metadata with at most 4096 registrations and one reactor thread.
    /// The absolute deadline is capped at the transport's thirty-second session budget.
    pub fn watch(&self, family: &str, after: api::Rev, until: Instant) -> Result<Watch, Refusal> {
        api::family(family)?;
        let until = until.min(
            Instant::now()
                .checked_add(SESSION_MAX)
                .ok_or(Refusal::Timeout)?,
        );
        remaining(until)?;
        let reactor = reactor()?;
        let shared = Arc::new(Mutex::new(State::default()));
        let id = {
            let mut queue = reactor
                .queue
                .lock()
                .map_err(|_| unavailable("watch reactor queue lock poisoned"))?;
            if !reactor.alive.load(Ordering::Acquire) {
                return Err(unavailable("watch reactor stopped"));
            }
            if queue.entries.len() >= REGISTRATION_MAX {
                return Err(Refusal::Full {
                    limit: REGISTRATION_MAX,
                });
            }
            let id = queue
                .next
                .checked_add(1)
                .ok_or_else(|| unavailable("watch registration identity exhausted"))?;
            queue.next = id;
            queue.entries.insert(
                id,
                Registration {
                    client: self.client.clone(),
                    namespace: self.name.clone(),
                    family: family.into(),
                    after,
                    until,
                    check_at: Instant::now(),
                    probes: 0,
                    state: Arc::clone(&shared),
                },
            );
            queue.order.push_back(id);
            id
        };
        reactor.changed.notify_one();
        Ok(Watch {
            id,
            shared,
            reactor,
            until,
            finished: false,
        })
    }
}

fn next_batch(reactor: &Reactor) -> Result<Batch, Refusal> {
    loop {
        let mut queue = reactor
            .queue
            .lock()
            .map_err(|_| unavailable("watch reactor queue lock poisoned"))?;
        let now = Instant::now();
        let expired: Vec<_> = queue
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                if entry.until <= now {
                    Some((*id, Refusal::Timeout))
                } else if entry.probes >= PROBE_MAX {
                    Some((*id, unavailable("watch metadata probe budget exhausted")))
                } else {
                    None
                }
            })
            .collect();
        if !expired.is_empty() {
            let mut states = Vec::new();
            for (id, error) in expired {
                if let Some(entry) = queue.entries.remove(&id) {
                    states.push((entry.state, error));
                }
            }
            let Queue { entries, order, .. } = &mut *queue;
            order.retain(|id| entries.contains_key(id));
            drop(queue);
            for (state, error) in states {
                complete(&state, Err(error));
            }
            continue;
        }
        let mut client: Option<Client> = None;
        let mut ids = Vec::new();
        let mut requests = Vec::new();
        // Scan the bounded queue once. Nonselected owners keep their order; selected
        // entries return at the tail, so an owner cannot be skipped indefinitely.
        for _ in 0..queue.order.len() {
            let Some(id) = queue.order.pop_front() else {
                break;
            };
            let Some(entry) = queue.entries.get_mut(&id) else {
                continue;
            };
            if ids.len() >= WATCH_BATCH_MAX
                || entry.check_at > now
                || client.as_ref().is_some_and(|c| c.path != entry.client.path)
            {
                queue.order.push_back(id);
                continue;
            }
            if client.is_none() {
                client = Some(entry.client.clone());
            }
            entry.probes = entry
                .probes
                .checked_add(1)
                .filter(|probes| *probes <= PROBE_MAX)
                .ok_or_else(|| unavailable("watch metadata probe budget exhausted"))?;
            ids.push(id);
            requests.push((entry.namespace.clone(), entry.family.clone(), entry.after));
        }
        let budget = now.checked_add(REQUEST_MAX).ok_or(Refusal::Timeout)?;
        let until = ids
            .iter()
            .filter_map(|id| queue.entries.get(id))
            .map(|entry| entry.until)
            .min()
            .unwrap_or(budget)
            .min(budget);
        if let Some(client) = client {
            return Ok(Batch {
                client,
                ids,
                requests,
                until,
            });
        }
        let idle_until = now.checked_add(SESSION_MAX).ok_or(Refusal::Timeout)?;
        let wake_at = queue
            .entries
            .values()
            .map(|entry| entry.check_at.min(entry.until))
            .min()
            .unwrap_or(idle_until);
        let (queue, _) = reactor
            .changed
            .wait_timeout(queue, wake_at.saturating_duration_since(Instant::now()))
            .map_err(|_| unavailable("watch reactor queue lock poisoned"))?;
        drop(queue);
    }
}
fn check(
    reactor: &Reactor,
    batch: &Batch,
) -> Result<Vec<Result<Option<api::Rev>, Refusal>>, Refusal> {
    let encoded = encode(&Request::WatchBatch(batch.requests.clone()), FRAME_MAX)?;
    let mut stream = batch.client.stream(batch.until)?;
    let cancellation = stream.try_clone().map_err(unavailable)?;
    {
        let mut queue = reactor
            .queue
            .lock()
            .map_err(|_| unavailable("watch reactor queue lock poisoned"))?;
        // Connect/authentication has one two-second deadline. Dropping every subscriber
        // during that phase prevents submission; later drops interrupt the shared read.
        if !batch.ids.iter().any(|id| queue.entries.contains_key(id)) {
            return Ok(Vec::new());
        }
        if queue.flight.is_some() {
            return Err(unavailable("watch reactor already has an in-flight batch"));
        }
        queue.flight = Some(Flight {
            ids: batch.ids.clone(),
            stream: cancellation,
        });
    }
    send(&mut stream, &encoded, batch.until).map_err(|_| Refusal::UnknownOutcome)?;
    match read_limit::<Response>(&mut stream, batch.until, WATCH_REPLY_MAX)
        .map_err(|_| Refusal::UnknownOutcome)??
    {
        Answer::WatchBatch(answers) if answers.len() == batch.ids.len() => Ok(answers),
        _ => Err(Refusal::UnknownOutcome),
    }
}
fn apply(
    reactor: &Reactor,
    batch: Batch,
    answers: Result<Vec<Result<Option<api::Rev>, Refusal>>, Refusal>,
) -> Result<(), Refusal> {
    let (mut answers, failure) = match answers {
        Ok(answers) => (answers.into_iter(), None),
        Err(error) => (Vec::new().into_iter(), Some(error)),
    };
    let mut completed = Vec::new();
    let now = Instant::now();
    let check_at = now.checked_add(CHECK_INTERVAL).ok_or(Refusal::Timeout)?;
    let stream = {
        let mut queue = reactor
            .queue
            .lock()
            .map_err(|_| unavailable("watch reactor queue lock poisoned"))?;
        let stream = queue.flight.take().map(|flight| flight.stream);
        for id in batch.ids {
            let answer = match &failure {
                Some(error) => Err(error.clone()),
                None => answers.next().unwrap_or(Err(Refusal::UnknownOutcome)),
            };
            let Some(entry) = queue.entries.get_mut(&id) else {
                continue;
            };
            let result = if now >= entry.until {
                Some(Err(Refusal::Timeout))
            } else {
                match answer {
                    Ok(None) => None,
                    Ok(Some(revision)) => Some(Ok(revision)),
                    // These operations inspect durable metadata only. Repeating a failed
                    // probe cannot replay a write or application closure. Both the absolute
                    // deadline and PROBE_MAX bound recovery through owner replacement.
                    Err(Refusal::Unavailable(_) | Refusal::Timeout | Refusal::UnknownOutcome) => {
                        None
                    }
                    Err(error) => Some(Err(error)),
                }
            };
            match result {
                Some(result) => {
                    if let Some(entry) = queue.entries.remove(&id) {
                        completed.push((entry.state, result));
                    }
                }
                None => {
                    entry.check_at = check_at.min(entry.until);
                    queue.order.push_back(id);
                }
            }
        }
        stream
    };
    drop(stream);
    for (state, result) in completed {
        complete(&state, result);
    }
    Ok(())
}
fn run(reactor: &Reactor) -> Result<(), Refusal> {
    loop {
        let batch = next_batch(reactor)?;
        let answers = check(reactor, &batch);
        apply(reactor, batch, answers)?;
    }
}
fn complete(shared: &Mutex<State>, result: Result<api::Rev, Refusal>) {
    let waker = match shared.lock() {
        Ok(mut state) => {
            state.result = Some(result);
            state.waker.take()
        }
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.result = Some(Err(unavailable("watch state lock poisoned")));
            state.waker.take()
        }
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}
fn fail_all(reactor: &Reactor, error: Refusal) {
    let (entries, stream) = {
        let mut queue = match reactor.queue.lock() {
            Ok(queue) => queue,
            Err(poisoned) => poisoned.into_inner(),
        };
        queue.order.clear();
        (
            std::mem::take(&mut queue.entries),
            queue.flight.take().map(|flight| flight.stream),
        )
    };
    drop(stream);
    for entry in entries.into_values() {
        complete(&entry.state, Err(error.clone()));
    }
}
impl Future for Watch {
    type Output = Result<api::Rev, Refusal>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Err(api::invalid("watch", "already completed")));
        }
        // Completion and registration share one short local lock. No socket I/O, reactor
        // queue work, owner selection or metadata RPC runs on the polling thread.
        let waker = cx.waker().clone();
        let (result, replaced) = match this.shared.lock() {
            Ok(mut state) => match state.result.take() {
                Some(result) => (Some(result), None),
                None if Instant::now() >= this.until => (Some(Err(Refusal::Timeout)), None),
                None => (None, state.waker.replace(waker)),
            },
            Err(_) => {
                this.finished = true;
                return Poll::Ready(Err(unavailable("watch state lock poisoned")));
            }
        };
        drop(replaced);
        match result {
            Some(result) => {
                this.finished = true;
                Poll::Ready(result)
            }
            None => Poll::Pending,
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        let stream =
            {
                let mut queue = match self.reactor.queue.lock() {
                    Ok(queue) => queue,
                    Err(poisoned) => poisoned.into_inner(),
                };
                queue.entries.remove(&self.id);
                queue.order.retain(|id| *id != self.id);
                if queue.flight.as_ref().is_some_and(|flight| {
                    flight.ids.iter().all(|id| !queue.entries.contains_key(id))
                }) {
                    queue.flight.take().map(|flight| flight.stream)
                } else {
                    None
                }
            };
        self.reactor.changed.notify_one();
        let waker = match self.shared.lock() {
            Ok(mut state) => state.waker.take(),
            Err(poisoned) => poisoned.into_inner().waker.take(),
        };
        drop(waker);
        // Interrupt a shared request only after its last subscriber drops.
        if let Some(stream) = stream
            && let Err(error) = stream.shutdown(Shutdown::Both)
        {
            tracing::debug!(target: "amphora.transport", event = "watch_shutdown_failed", %error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{database::Database, transport::Owner};
    use amphora_api::Records;
    use std::{fs::File, task::Wake};

    // These tests share the deliberately process-wide registration admission budget.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn until() -> Instant {
        Instant::now() + crate::transport::HANG
    }

    struct Fixture {
        namespace: Namespace,
        client: Client,
        _owner: Owner,
        _database: Arc<Database>,
        _lock: File,
        _directory: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner");
        let lock = File::create(&path).unwrap();
        lock.try_lock().unwrap();
        // An explicit owner over the real in-memory Database pipeline keeps this test
        // independent of SDK startup selection while exercising authenticated TCP I/O.
        let database = Arc::new(Database::in_memory(until()).unwrap());
        let acquiring = Arc::downgrade(&database);
        let owner = Owner::start(&path, move || {
            acquiring.upgrade().map(|database| (*database).clone())
        })
        .unwrap();
        let client = Client::connect(&path, until()).unwrap();
        let namespace = client.namespace("ledger").unwrap();
        Fixture {
            namespace,
            client,
            _owner: owner,
            _database: database,
            _lock: lock,
            _directory: directory,
        }
    }

    #[derive(Default)]
    struct Notice {
        ready: Mutex<bool>,
        event: Condvar,
    }

    impl Wake for Notice {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            *self.ready.lock().unwrap() = true;
            self.event.notify_all();
        }
    }

    fn wait_for_result(watch: &mut Pin<Box<Watch>>, notice: &Arc<Notice>) -> api::Rev {
        let waker = Waker::from(Arc::clone(notice));
        let mut cx = Context::from_waker(&waker);
        let deadline = until();
        loop {
            if let Poll::Ready(result) = watch.as_mut().poll(&mut cx) {
                return result.unwrap();
            }
            let ready = notice.ready.lock().unwrap();
            let (mut ready, _) = notice
                .event
                .wait_timeout_while(
                    ready,
                    deadline.saturating_duration_since(Instant::now()),
                    |v| !*v,
                )
                .unwrap();
            assert!(*ready, "watch did not publish a result before its deadline");
            *ready = false;
        }
    }

    fn commit(namespace: &Namespace) -> api::Rev {
        namespace
            .commit(
                api::Tx {
                    conds: vec![],
                    writes: vec![api::Write::Put {
                        family: "work".into(),
                        key: b"claim".to_vec(),
                        value: b"durable".to_vec(),
                    }],
                },
                until(),
            )
            .unwrap()
    }

    #[test]
    fn sixty_four_dormant_watches_share_one_reactor_and_leave_sql_usable() {
        // CP-T1.5/U05/U06: 64 dormant watches share a reactor, observe durable Records
        // publication and leave the owner's SQL connection/writer admission usable.
        let _serial = SERIAL.lock().unwrap();
        let fixture = fixture();
        let mut watches: Vec<_> = (0..64)
            .map(|_| Box::pin(fixture.namespace.watch("work", 0, until()).unwrap()))
            .collect();
        let reactor = reactor().unwrap();
        let notice = Arc::new(Notice::default());
        let waker = Waker::from(Arc::clone(&notice));
        let mut cx = Context::from_waker(&waker);
        for watch in &mut watches {
            assert!(Arc::ptr_eq(&watch.reactor, &reactor));
            assert!(watch.as_mut().poll(&mut cx).is_pending());
        }
        fixture
            .client
            .call(until(), |connection| {
                connection.execute_batch(
                    "CREATE TABLE work(value INTEGER); INSERT INTO work VALUES(7)",
                )?;
                assert_eq!(
                    connection
                        .query_row("SELECT value FROM work", [], |row| row.get::<_, i64>(0))?,
                    7
                );
                Ok(())
            })
            .unwrap();
        let revision = commit(&fixture.namespace);
        for watch in &mut watches {
            assert_eq!(wait_for_result(watch, &notice), revision);
            assert!(matches!(
                watch.as_mut().poll(&mut cx),
                Poll::Ready(Err(Refusal::Invalid { .. }))
            ));
        }
    }

    #[test]
    fn completion_before_poll_does_not_need_a_registered_waker() {
        // CP-T1.5/U05: result publication racing ahead of registration cannot lose readiness.
        let _serial = SERIAL.lock().unwrap();
        let fixture = fixture();
        let revision = commit(&fixture.namespace);
        let mut watch = Box::pin(fixture.namespace.watch("work", 0, until()).unwrap());
        let deadline = until();
        while watch.shared.lock().unwrap().result.is_none() {
            assert!(Instant::now() < deadline, "remote wait did not complete");
            thread::yield_now();
        }
        let notice = Arc::new(Notice::default());
        let waker = Waker::from(Arc::clone(&notice));
        assert_eq!(
            watch.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(revision))
        );
        assert!(!*notice.ready.lock().unwrap());
    }

    #[test]
    fn registration_overflow_is_typed_and_drop_releases_capacity() {
        // CP-T1.5/U06: dormant subscriptions consume bounded registrations, rather than
        // one thread or socket each; dropping them immediately releases admission.
        let _serial = SERIAL.lock().unwrap();
        let fixture = fixture();
        let deadline = until();
        let watches: Vec<_> = (0..REGISTRATION_MAX)
            .map(|_| fixture.namespace.watch("work", 0, deadline).unwrap())
            .collect();
        assert!(matches!(
            fixture.namespace.watch("work", 0, deadline),
            Err(Refusal::Full {
                limit: REGISTRATION_MAX
            })
        ));
        let reactor = Arc::clone(&watches[0].reactor);
        drop(watches);
        assert!(reactor.queue.lock().unwrap().entries.is_empty());
        let revision = commit(&fixture.namespace);
        let mut watch = Box::pin(fixture.namespace.watch("work", 0, until()).unwrap());
        assert_eq!(
            wait_for_result(&mut watch, &Arc::new(Notice::default())),
            revision
        );
    }

    #[test]
    fn hostile_watch_batch_lengths_are_bounded_before_element_decode() {
        // CP-T1.5: neither peer can allocate an arbitrary metadata-check/result array.
        let requests = vec![("ledger".into(), "work".into(), 0); WATCH_BATCH_MAX + 1];
        let frame = postcard::to_allocvec(&Request::WatchBatch(requests)).unwrap();
        assert!(postcard::from_bytes::<Request>(&frame).is_err());
        let answers = vec![Ok(None); WATCH_BATCH_MAX + 1];
        let frame = postcard::to_allocvec(&Answer::WatchBatch(answers)).unwrap();
        assert!(postcard::from_bytes::<Answer>(&frame).is_err());
    }
}
