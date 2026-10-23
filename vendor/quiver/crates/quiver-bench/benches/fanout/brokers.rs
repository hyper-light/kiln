//! The competitors, class for class with Quiver's ephemeral class (bench/README.md, "The
//! comparison"): NATS core, Redis Pub/Sub and Kafka, each one node on this machine
//! (`bench/compare.sh` starts it), reached over loopback by its own client, as Focal's comparison
//! drives them (Focal's `tools/compare/src/pubsub.rs`): NATS and Redis as shipped; Kafka as a
//! latency-minded user sets it, no linger before a send (`linger.ms` 0), a fetch answered as soon as
//! a byte is there (`fetch.min.bytes` 1), a topic of one partition.
//!
//! The publisher keeps the open loop on this thread, on a tokio runtime of its own, yielding to its
//! client's tasks while it waits for a message's time. Each subscriber has a connection of its own
//! (for Kafka, a consumer of its own), the subscribers shared out over `--workers` threads, each a
//! tokio runtime of its own whose thread waits in epoll as its clients do or, `--busy`, polls
//! without waiting. Before the schedule the publisher sends priming messages until every subscriber
//! has taken one, so every subscription stands at the broker before the first message.
//!
//! A thread's end is a fact, bounded: each of its subscribers took every message published, or its
//! stream ended (the broker disconnected it), or `--drain` seconds passed since the schedule
//! closed. What a subscriber hadn't taken by then is undelivered.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use rdkafka::Message as _;
use rdkafka::consumer::Consumer as _;
use rdkafka::producer::Producer as _;

use crate::follow::{Follow, PRIMING, Schedule, Seen, Taken, Window, stamp};
use crate::report::{Run, tally};
use crate::{Args, System, thread_cpu_s};

/// Redis's publishes in flight at once, each awaiting its reply on the multiplexed connection; at
/// the bound the publisher waits for one, and the messages after it are late by that wait.
const INFLIGHT: usize = 1024;
/// How long setup may take: every subscriber connected, subscribed and primed.
const SETUP: Duration = Duration::from_secs(300);

struct Shared {
    base: Instant,
    window: Window,
    subscribed: AtomicUsize,
    primed: AtomicUsize,
    /// The messages published once the schedule closed, and when it closed (nanoseconds from
    /// `base`): `u64::MAX` until then.
    published: AtomicU64,
    closed: AtomicU64,
    abort: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, what: String) {
        self.error.lock().unwrap().get_or_insert(what);
        self.abort.store(true, Ordering::Release);
    }

    fn now(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }
}

/// One subscriber thread's subscribers.
struct Local {
    seen: Seen,
    follows: Vec<Follow>,
    primed: Vec<bool>,
    /// Errors a client returned and carried on from.
    errors: u64,
}

fn take(local: &RefCell<Local>, shared: &Shared, i: usize, payload: &[u8]) {
    let at = shared.now();
    let mut l = local.borrow_mut();
    let Local {
        seen,
        follows,
        primed,
        ..
    } = &mut *l;
    if seen.take(&mut follows[i], payload, at, &shared.window) == Taken::Priming && !primed[i] {
        primed[i] = true;
        shared.primed.fetch_add(1, Ordering::AcqRel);
    }
}

/// Subscriber `id` (the `i`th of its thread): connects, subscribes, and takes what comes until its
/// stream ends.
async fn subscriber(
    system: System,
    endpoint: &str,
    topic: &str,
    id: usize,
    i: usize,
    local: &RefCell<Local>,
    shared: &Shared,
) -> Result<(), String> {
    match system {
        System::Nats => {
            let client = async_nats::connect(format!("nats://{endpoint}"))
                .await
                .map_err(|e| e.to_string())?;
            let mut sub = client
                .subscribe(topic.to_owned())
                .await
                .map_err(|e| e.to_string())?;
            client.flush().await.map_err(|e| e.to_string())?;
            shared.subscribed.fetch_add(1, Ordering::AcqRel);
            while let Some(m) = sub.next().await {
                take(local, shared, i, &m.payload);
            }
        }
        System::Redis => {
            let mut pubsub = redis_client(endpoint)?
                .get_async_pubsub()
                .await
                .map_err(|e| e.to_string())?;
            pubsub.subscribe(topic).await.map_err(|e| e.to_string())?;
            shared.subscribed.fetch_add(1, Ordering::AcqRel);
            let mut messages = pubsub.into_on_message();
            while let Some(m) = messages.next().await {
                take(local, shared, i, m.get_payload_bytes());
            }
        }
        System::Kafka => {
            let consumer: rdkafka::consumer::StreamConsumer = rdkafka::ClientConfig::new()
                .set("bootstrap.servers", endpoint)
                .set("group.id", format!("{topic}-{id}"))
                .set("enable.auto.commit", "false")
                .set("fetch.min.bytes", "1")
                // A thousand consumers' read-ahead within this machine's memory: 1 MiB each
                // (librdkafka's default is 64 MiB).
                .set("queued.max.messages.kbytes", "1024")
                .create()
                .map_err(|e| e.to_string())?;
            // The run's own topic, from its first message: nothing published is missed.
            let mut assignment = rdkafka::TopicPartitionList::new();
            assignment
                .add_partition_offset(topic, 0, rdkafka::Offset::Beginning)
                .map_err(|e| e.to_string())?;
            consumer.assign(&assignment).map_err(|e| e.to_string())?;
            shared.subscribed.fetch_add(1, Ordering::AcqRel);
            loop {
                match consumer.recv().await {
                    Ok(m) => take(local, shared, i, m.payload().unwrap_or_default()),
                    Err(_) => {
                        local.borrow_mut().errors += 1;
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        System::Quiver | System::Wake => {
            return Err("Quiver and the wake-up floor run in this process".to_owned());
        }
    }
    Ok(())
}

/// A Redis client of `endpoint` whose connections send at once (`TCP_NODELAY`), as a latency-minded
/// user sets it: redis-rs leaves Nagle's algorithm on, where NATS's and Kafka's clients turn it off
/// themselves (and Redis turns it off on its side).
fn redis_client(endpoint: &str) -> Result<redis::Client, String> {
    use redis::IntoConnectionInfo as _;
    let info = format!("redis://{endpoint}")
        .into_connection_info()
        .map_err(|e| e.to_string())?
        .set_tcp_settings(redis::io::tcp::TcpSettings::default().set_nodelay(true));
    redis::Client::open(info).map_err(|e| e.to_string())
}

/// One subscriber thread: subscribers `ids`, on a tokio runtime of its own.
fn subscribers(
    system: System,
    endpoint: String,
    topic: String,
    ids: Range<usize>,
    shared: Arc<Shared>,
    busy: bool,
    drain: Duration,
) -> (Seen, Vec<Follow>, u64) {
    let count = ids.len();
    let local = Rc::new(RefCell::new(Local {
        seen: Seen::new(),
        follows: (0..count).map(|_| Follow::new()).collect(),
        primed: vec![false; count],
        errors: 0,
    }));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let set = tokio::task::LocalSet::new();
    set.block_on(&runtime, async {
        let mut tasks = tokio::task::JoinSet::new();
        for (i, id) in ids.enumerate() {
            let (local, shared) = (Rc::clone(&local), Arc::clone(&shared));
            let (endpoint, topic) = (endpoint.clone(), topic.clone());
            tasks.spawn_local(async move {
                if let Err(e) = subscriber(system, &endpoint, &topic, id, i, &local, &shared).await
                {
                    shared.fail(format!("subscriber {id}: {e}"));
                }
                local.borrow_mut().follows[i].ended = true;
            });
        }
        let done = Rc::new(Cell::new(false));
        if busy {
            let done = Rc::clone(&done);
            tasks.spawn_local(async move {
                while !done.get() {
                    tokio::task::yield_now().await;
                }
            });
        }
        let drain = drain.as_nanos() as u64;
        loop {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if shared.abort.load(Ordering::Acquire) {
                break;
            }
            let published = shared.published.load(Ordering::Acquire);
            if published == u64::MAX {
                continue;
            }
            if local
                .borrow()
                .follows
                .iter()
                .all(|f| f.ended || f.next() >= published)
            {
                break;
            }
            if shared.now() >= shared.closed.load(Ordering::Acquire).saturating_add(drain) {
                break;
            }
        }
        done.set(true);
        tasks.shutdown().await;
    });
    drop(set);
    let l = Rc::try_unwrap(local).ok().unwrap().into_inner();
    (l.seen, l.follows, l.errors)
}

/// What Kafka's producer reported it failed to deliver, from its delivery callback.
#[derive(Default)]
struct Deliveries {
    failed: AtomicU64,
}

impl rdkafka::ClientContext for Deliveries {}

impl rdkafka::producer::ProducerContext for Deliveries {
    type DeliveryOpaque = ();
    fn delivery(&self, result: &rdkafka::message::DeliveryResult<'_>, _: ()) {
        if result.is_err() {
            self.failed.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// A publisher: its system's client.
enum Publisher {
    Nats(async_nats::Client, async_nats::Subject),
    Redis(redis::aio::MultiplexedConnection),
    Kafka(rdkafka::producer::ThreadedProducer<Deliveries>),
}

impl Publisher {
    async fn connect(system: System, endpoint: &str, topic: &str) -> Result<Self, String> {
        match system {
            System::Nats => Ok(Self::Nats(
                async_nats::connect(format!("nats://{endpoint}"))
                    .await
                    .map_err(|e| e.to_string())?,
                async_nats::Subject::from(topic),
            )),
            System::Redis => {
                let client = redis_client(endpoint)?;
                // A reply waited for however long it takes: a slow one delays the messages after
                // it rather than failing (redis-rs's default gives up after 500 ms).
                let config = redis::AsyncConnectionConfig::new().set_response_timeout(None);
                Ok(Self::Redis(
                    client
                        .get_multiplexed_async_connection_with_config(&config)
                        .await
                        .map_err(|e| e.to_string())?,
                ))
            }
            System::Kafka => Ok(Self::Kafka(
                rdkafka::ClientConfig::new()
                    .set("bootstrap.servers", endpoint)
                    // A latency-minded producer sends at once (Kafka 4's own default lingers 5 ms).
                    .set("linger.ms", "0")
                    .set("message.timeout.ms", "120000")
                    .create_with_context(Deliveries::default())
                    .map_err(|e| e.to_string())?,
            )),
            System::Quiver | System::Wake => {
                Err("Quiver and the wake-up floor run in this process".to_owned())
            }
        }
    }

    /// Hands `payload` to the client; Redis's publishes await their replies in `flying`. Returns
    /// the failures it learned of.
    async fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        flying: &mut tokio::task::JoinSet<bool>,
    ) -> u64 {
        match self {
            Self::Nats(client, subject) => u64::from(
                client
                    .publish(subject.clone(), payload.to_vec().into())
                    .await
                    .is_err(),
            ),
            Self::Redis(connection) => {
                let mut failed = 0;
                while let Some(r) = flying.try_join_next() {
                    failed += u64::from(!r.unwrap_or(false));
                }
                while flying.len() >= INFLIGHT {
                    if let Some(r) = flying.join_next().await {
                        failed += u64::from(!r.unwrap_or(false));
                    }
                }
                let mut connection = connection.clone();
                let mut cmd = redis::cmd("PUBLISH");
                cmd.arg(topic).arg(payload);
                flying.spawn(async move { cmd.query_async::<i64>(&mut connection).await.is_ok() });
                failed
            }
            Self::Kafka(producer) => {
                let mut record =
                    rdkafka::producer::BaseRecord::<(), [u8]>::to(topic).payload(payload);
                loop {
                    match producer.send(record) {
                        Ok(()) => return 0,
                        // librdkafka's queue is full: offer it again once its threads have sent
                        // some (its `QueueFull` contract).
                        Err((
                            rdkafka::error::KafkaError::MessageProduction(
                                rdkafka::types::RDKafkaErrorCode::QueueFull,
                            ),
                            again,
                        )) => {
                            record = again;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => return 1,
                    }
                }
            }
        }
    }

    /// Waits until everything handed to the client has gone to the broker: NATS's flush, Redis's
    /// replies, Kafka's acknowledgements (each within its timeout). Returns the failures it
    /// learned of.
    async fn finish(&self, flying: &mut tokio::task::JoinSet<bool>) -> Result<u64, String> {
        match self {
            Self::Nats(client, _) => {
                client.flush().await.map_err(|e| e.to_string())?;
                Ok(0)
            }
            Self::Redis(_) => {
                let mut failed = 0;
                while let Some(r) = flying.join_next().await {
                    failed += u64::from(!r.unwrap_or(false));
                }
                Ok(failed)
            }
            Self::Kafka(producer) => {
                producer
                    .flush(rdkafka::util::Timeout::After(Duration::from_secs(120)))
                    .map_err(|e| e.to_string())?;
                Ok(producer.context().failed.swap(0, Ordering::AcqRel))
            }
        }
    }
}

/// What the publisher did.
struct Published {
    schedule: Schedule,
    errors: u64,
    seconds: f64,
    cpu_s: f64,
}

async fn publish(
    a: &Args,
    system: System,
    seed: u64,
    topic: &str,
    shared: &Shared,
) -> Result<Published, String> {
    let publisher = Publisher::connect(system, &a.endpoint, topic).await?;
    let mut flying = tokio::task::JoinSet::new();
    let setup = Instant::now() + SETUP;
    let stuck = |what: &str, n: usize| {
        if shared.abort.load(Ordering::Acquire) {
            Some(shared.error.lock().unwrap().clone().unwrap_or_default())
        } else if Instant::now() > setup {
            Some(format!(
                "only {n} of {} subscribers {what} within {SETUP:?}",
                a.subscribers
            ))
        } else {
            None
        }
    };
    loop {
        let n = shared.subscribed.load(Ordering::Acquire);
        if n >= a.subscribers {
            break;
        }
        if let Some(e) = stuck("subscribed", n) {
            return Err(e);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut primer = vec![0u8; a.payload];
    stamp(&mut primer, 0, PRIMING);
    loop {
        let n = shared.primed.load(Ordering::Acquire);
        if n >= a.subscribers {
            break;
        }
        if let Some(e) = stuck("took a priming message", n) {
            return Err(e);
        }
        publisher.publish(topic, &primer, &mut flying).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    publisher.finish(&mut flying).await?;

    // A millisecond on, the schedule.
    let mut schedule = Schedule::new(a, seed, shared.now() + 1_000_000, &shared.window);
    let mut payload = vec![0u8; a.payload];
    let mut errors = 0;
    let began = Instant::now();
    let cpu_before = thread_cpu_s();
    loop {
        if schedule.over(shared.now()) {
            break;
        }
        let Some((at, number)) = schedule.next() else {
            break;
        };
        while shared.now() < at {
            tokio::task::yield_now().await;
        }
        stamp(&mut payload, at, number);
        errors += publisher.publish(topic, &payload, &mut flying).await;
    }
    let seconds = began.elapsed().as_secs_f64();
    errors += publisher.finish(&mut flying).await?;
    let cpu_s = match (cpu_before, thread_cpu_s()) {
        (Some(b), Some(a)) => a - b,
        _ => f64::NAN,
    };
    shared.closed.store(shared.now(), Ordering::Release);
    shared
        .published
        .store(schedule.offered(), Ordering::Release);
    Ok(Published {
        schedule,
        errors,
        seconds,
        cpu_s,
    })
}

/// Makes the run's own Kafka topic: one partition, on the one broker.
fn create_topic(endpoint: &str, topic: &str) -> Result<(), String> {
    use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let admin: AdminClient<rdkafka::client::DefaultClientContext> =
            rdkafka::ClientConfig::new()
                .set("bootstrap.servers", endpoint)
                .create()
                .map_err(|e| e.to_string())?;
        let made = admin
            .create_topics(
                [&NewTopic::new(topic, 1, TopicReplication::Fixed(1))],
                &AdminOptions::new(),
            )
            .await
            .map_err(|e| e.to_string())?;
        for r in made {
            r.map_err(|(t, e)| format!("creating {t}: {e}"))?;
        }
        Ok(())
    })
}

/// The broker's CPU so far, seconds, as its container's cgroup counts it.
fn broker_cpu_s(a: &Args) -> Option<f64> {
    let text = std::fs::read_to_string(a.broker_cgroup.as_ref()?.join("cpu.stat")).ok()?;
    let usec: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()?;
    Some(usec as f64 / 1e6)
}

/// Run `index` of `system`.
pub fn run(a: &Args, system: System, seed: u64, index: usize) -> Run {
    let topic = format!("quiver-compare-{}-{index}", std::process::id());
    if system == System::Kafka {
        create_topic(&a.endpoint, &topic).unwrap_or_else(|e| panic!("kafka: {e}"));
    }
    let shared = Arc::new(Shared {
        base: Instant::now(),
        window: Window::new(),
        subscribed: AtomicUsize::new(0),
        primed: AtomicUsize::new(0),
        published: AtomicU64::new(u64::MAX),
        closed: AtomicU64::new(u64::MAX),
        abort: AtomicBool::new(false),
        error: Mutex::new(None),
    });
    let workers = a.workers.min(a.subscribers).max(1);
    let load_before = hyper_measure::usage::load();
    let cpu_before = hyper_measure::usage::this().ok();
    let broker_before = broker_cpu_s(a);
    let handles: Vec<_> = (0..workers)
        .map(|w| {
            let (per, extra) = (a.subscribers / workers, a.subscribers % workers);
            let start = w * per + w.min(extra);
            let ids = start..start + per + usize::from(w < extra);
            let (shared, endpoint, topic) =
                (Arc::clone(&shared), a.endpoint.clone(), topic.clone());
            let (busy, drain) = (a.busy, Duration::from_secs(a.drain));
            std::thread::Builder::new()
                .name(format!("sub-{w}"))
                .spawn(move || subscribers(system, endpoint, topic, ids, shared, busy, drain))
                .unwrap()
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let published = runtime.block_on(publish(a, system, seed, &topic, &shared));
    if let Err(e) = &published {
        shared.fail(e.clone());
    }
    let mut seen = Seen::new();
    let mut follows = Vec::new();
    let mut client_errors = 0;
    for h in handles {
        let (s, f, e) = h.join().unwrap();
        seen.merge(&s);
        follows.extend(f);
        client_errors += e;
    }
    let p = published.unwrap_or_else(|e| panic!("{}: {e}", system.name()));
    if let Some(e) = shared.error.lock().unwrap().as_ref() {
        println!("- {}: a subscriber failed: {e}", system.name());
    }
    if client_errors > 0 {
        println!(
            "- {}: its subscribers' client returned {client_errors} errors and carried on",
            system.name()
        );
    }
    let cpu_s = match (cpu_before, hyper_measure::usage::this().ok()) {
        (Some(b), Some(a)) => a.since(&b).cpu_ns() as f64 / 1e9,
        _ => f64::NAN,
    };
    let broker_cpu_s = match (broker_before, broker_cpu_s(a)) {
        (Some(b), Some(a)) => Some(a - b),
        _ => None,
    };
    let t = tally(&mut seen, &follows, &p.schedule);
    Run {
        system,
        busy: a.busy,
        rate: a.rate,
        subscribers: a.subscribers,
        subscriptions: a.subscribers,
        workers,
        published: p.schedule.offered(),
        unoffered: p.schedule.unoffered,
        publish_errors: p.errors,
        seconds: p.seconds,
        window_s: a.seconds,
        seen,
        undelivered: t.undelivered,
        silent: t.silent,
        lags: t.lags,
        doubled: t.doubled,
        kept_up: t.kept_up,
        cpu_s,
        publisher_cpu_s: p.cpu_s,
        broker_cpu_s,
        load_before,
        load_after: hyper_measure::usage::load(),
    }
}
