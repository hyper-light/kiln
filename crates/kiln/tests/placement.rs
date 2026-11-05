//! Goal 7 of the API runtime program (plan 01a1a59c2aea): where the server's work runs on the shards
//! (`kiln::placement`). With one shard held, steering sends new connections to the others (step 1), and
//! requests queued on it as movable tasks finish on the others (step 2); each placement serves HTTP from its
//! setting alone (step 3). A process of its own: the runtime, its probes and the placement are the process's.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln::http::server::{self as srv, Handler, Request, Response};
use kiln::placement::{self, Placement};

/// The runtime every test here shares: four shards, each probed every 10 ms (goal 1's), so a held shard's delay
/// shows within a probe's period.
fn runtime() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile { shards: 4, ..kiln::rt::Profile::tool() });
    anyhow::ensure!(kiln::rt::shard_count() == 4, "{} shards", kiln::rt::shard_count());
    kiln::shard_health::start(Duration::from_millis(10));
    let deadline = Instant::now() + Duration::from_secs(20);
    while (0..4).any(|i| kiln::shard_health::delay_now(i).is_none()) {
        anyhow::ensure!(Instant::now() < deadline, "the probes never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Step 1's acceptance: with one shard busy, new work goes to the others. Do: hold shard 0 (a task that sleeps in
/// its poll, as a blocking call would) until its probe is 50 ms overdue, then steer 200 new connections. Expect:
/// none of them to shard 0; and, where two or more of the others were never held meanwhile (a loaded host can keep
/// a shard's poll past the watchdog's limit), more than one of them taking them.
#[test]
fn g7_s1_with_one_shard_busy_new_connections_go_to_the_others() -> anyhow::Result<()> {
    runtime()?;
    let holders = kiln::rt::shard_holders();
    let busy = *holders.first().ok_or_else(|| anyhow::anyhow!("no shards"))?;
    anyhow::ensure!(kiln::rt::spawn_on(0, async { std::thread::sleep(Duration::from_millis(600)) }), "shard 0 refused the hold");
    let deadline = Instant::now() + Duration::from_secs(10);
    while kiln::shard_health::delay_now(0).is_none_or(|d| d < Duration::from_millis(50)) {
        anyhow::ensure!(Instant::now() < deadline, "shard 0's delay never showed: {:?}", kiln::shard_health::delay_now(0));
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut to = [0usize; 4];
    let mut held = [false; 4];
    for _ in 0..200 {
        for (i, h) in holders.iter().enumerate() {
            if let Some(was) = held.get_mut(i) {
                *was |= hyper_rt::watch::stalled(*h);
            }
        }
        let holder = placement::connection_shard().ok_or_else(|| anyhow::anyhow!("no shard picked"))?;
        anyhow::ensure!(holder != busy, "a new connection was steered to the busy shard (its delay {:?})", kiln::shard_health::delay_now(0));
        if let Some(n) = holders.iter().position(|h| *h == holder).and_then(|i| to.get_mut(i)) {
            *n += 1;
        }
    }
    let free = held.iter().skip(1).filter(|h| !**h).count();
    anyhow::ensure!(free < 2 || to.iter().filter(|n| **n > 0).count() > 1, "spread over the others: {to:?}, held {held:?}");
    Ok(())
}

/// Step 2's acceptance: with one shard busy, its queued requests finish on other shards. Do: on shard 0, place 16
/// requests as movable tasks (`steal`), each queued on shard 0 as it is first polled, then hold shard 0, spinning,
/// until they have run (at most 10 s). Expect: all 16 ran while shard 0 was held, none on it.
#[test]
fn g7_s2_with_one_shard_busy_its_queued_requests_finish_on_other_shards() -> anyhow::Result<()> {
    runtime()?;
    let holders = kiln::rt::shard_holders();
    let busy = holders.first().ok_or_else(|| anyhow::anyhow!("no shards"))?.shard();
    let done = Arc::new(AtomicUsize::new(0));
    let (ran, ran_on) = std::sync::mpsc::channel::<u16>();
    let (held, held_for) = std::sync::mpsc::channel::<(Duration, usize)>();
    let d = Arc::clone(&done);
    let placed = kiln::rt::spawn_on(0, async move {
        type Placed = Pin<Box<dyn Future<Output = Option<()>> + Send>>;
        let mut requests: Vec<Placed> = (0..16)
            .map(|_| {
                let (ran, done) = (ran.clone(), Arc::clone(&d));
                Box::pin(placement::request_in(Placement::Steal, kiln::overload::Class::Normal, async move {
                    let _ = ran.send(hyper_rt::futures::shard_id().map_or(u16::MAX, |s| s.0));
                    done.fetch_add(1, Ordering::AcqRel);
                })) as Placed
            })
            .collect();
        // One poll each queues it on this shard.
        std::future::poll_fn(|cx| {
            for r in &mut requests {
                let _ = r.as_mut().poll(cx);
            }
            Poll::Ready(())
        })
        .await;
        let began = Instant::now();
        while d.load(Ordering::Acquire) < 16 && began.elapsed() < Duration::from_secs(10) {
            std::hint::spin_loop();
        }
        let _ = held.send((began.elapsed(), d.load(Ordering::Acquire)));
        for r in requests {
            r.await;
        }
    });
    anyhow::ensure!(placed, "shard 0 refused the task");
    let (for_how_long, by_then) = held_for.recv_timeout(Duration::from_secs(30))?;
    anyhow::ensure!(by_then == 16, "{by_then} of 16 requests ran while shard 0 was held ({for_how_long:?})");
    let shards: Vec<u16> = ran_on.try_iter().collect();
    anyhow::ensure!(shards.len() == 16 && shards.iter().all(|s| *s != busy), "ran on {shards:?}, shard 0 being {busy}");
    let stats = kiln::rt::movable_stats().ok_or_else(|| anyhow::anyhow!("no runtime"))?;
    anyhow::ensure!(stats.stolen + stats.handed_off >= 1, "{stats:?}");
    Ok(())
}

/// Says what it was asked and what it was sent, and on which shard.
struct Echo;

impl Handler for Echo {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let what = format!("{} {}", req.method(), req.uri());
        let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
        srv::text(StatusCode::OK, &format!("{what} [{}]", String::from_utf8_lossy(&body)))
    }
}

/// Step 3's acceptance: each placement runs from the setting alone. Do: for each placement in turn, set it and send
/// a GET and a POST with a body to a listener serving kiln's HTTP. Expect: each answered with what it was sent.
#[test]
fn g7_s3_each_placement_serves_http_from_its_setting() -> anyhow::Result<()> {
    runtime()?;
    kiln::rt::block_on(async {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        kiln::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let remote = stream.peer_addr().ok();
                kiln::rt::spawn_connection(async move { srv::serve_accepted(stream, Arc::new(Echo), remote, None).await });
            }
        });
        for p in Placement::ALL {
            placement::set(p);
            let http = kiln::http::client::Client::public()?;
            for i in 0..8 {
                let got = http.get(&format!("http://{addr}/x/{i}")).send().await?;
                anyhow::ensure!(got.status() == 200, "{p}: GET {i}: {}", got.status());
                let text = String::from_utf8_lossy(&got.bytes(1 << 16).await?).into_owned();
                anyhow::ensure!(text == format!("GET /x/{i} []"), "{p}: GET {i}: {text}");
                let got = http.post(&format!("http://{addr}/y")).body(format!("body {i} of {p}")).send().await?;
                let text = String::from_utf8_lossy(&got.bytes(1 << 16).await?).into_owned();
                anyhow::ensure!(text == format!("POST /y [body {i} of {p}]"), "{p}: POST {i}: {text}");
            }
        }
        placement::set(Placement::RoundRobin);
        Ok(())
    })
}
