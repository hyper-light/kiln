//! Goal 6, plan 01a1a59c2abe steps 3 and 4: a server overloaded on purpose stays up, serves its Critical
//! requests, sheds Background requests first and then Normal ones, each at once (503, `Retry-After`,
//! `x-kiln-shed`) and before its body is read, and counts its sheds by class; once the load goes, it serves
//! everything again. kiln's HTTP server (`kiln::http::server`) and the per-shard monitors (`kiln::overload`)
//! as the server runs them, with a handler that sheds as the API's does. A process of its own: its runtime and
//! monitors are the process's.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln::http::server::{self as srv, Handler, Request, Response};
use kiln::overload::Class;

/// Serves what it is asked after the shed check `kiln::api::Api` makes first; reads a body it is sent, and
/// counts the requests it reached.
struct Probe {
    reached: AtomicU64,
}

impl Handler for Probe {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if let Some(shed) = kiln::overload::shed_request(&req) {
            return shed;
        }
        self.reached.fetch_add(1, Ordering::Relaxed);
        let _ = srv::body_bytes(req.into_body(), 1 << 20).await;
        srv::text(StatusCode::OK, "ok")
    }
}

/// kiln's listener serving [`Probe`] on a free port, its connections spread over the shards.
fn listen(probe: Arc<Probe>) -> anyhow::Result<SocketAddr> {
    let tcp = hyper_rt::tcp::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    kiln::rt::spawn(async move {
        // As the server's own accept loop: Critical.
        let _ = hyper_rt::coop::set_class(Class::Critical);
        while let Ok(stream) = tcp.accept().await {
            let (probe, remote) = (Arc::clone(&probe), stream.peer_addr().ok());
            kiln::rt::spawn_connection(async move { srv::serve_accepted(stream, probe, remote, None).await });
        }
    });
    Ok(addr)
}

/// One HTTP/1.1 request on a connection of its own: its status, its headers (lowercase names), how long it
/// took. A body is announced as `announce` bytes and only `send` of them sent: a server that answers without
/// reading it answers all the same.
fn ask(addr: SocketAddr, method: &str, path: &str, headers: &str, announce: usize, send: &[u8]) -> anyhow::Result<(u16, Vec<(String, String)>, Duration)> {
    let started = Instant::now();
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(20)))?;
    let length = if announce > 0 { format!("content-length: {announce}\r\n") } else { String::new() };
    write!(s, "{method} {path} HTTP/1.1\r\nhost: kiln.test\r\nconnection: close\r\n{headers}{length}\r\n")?;
    s.write_all(send)?;
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    // Read the head (a shed answer comes with its connection closing; a served one with its body).
    loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        answer.extend_from_slice(buf.get(..n).unwrap_or_default());
        if answer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&answer).into_owned();
    let mut lines = text.split("\r\n");
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers = lines.take_while(|l| !l.is_empty()).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    Ok((status, headers, started.elapsed()))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// A Background request (sync's), a Normal one (the API with a token) and a Critical one (health).
const BACKGROUND: (&str, &str, &str) = ("PUT", "/api/sync/blobs/abc", "authorization: Bearer kiln_pat_test\r\n");
const NORMAL: (&str, &str, &str) = ("GET", "/api/runs", "authorization: Bearer kiln_pat_test\r\n");
const CRITICAL: (&str, &str, &str) = ("GET", "/healthz", "");

fn status_of(addr: SocketAddr, (method, path, headers): (&str, &str, &str)) -> anyhow::Result<u16> {
    let body: &[u8] = if method == "PUT" { b"x" } else { b"" };
    Ok(ask(addr, method, path, headers, body.len(), body)?.0)
}

/// Busy tasks of `class` on every shard, each working a step quantum at a time and yielding between, until `stop`.
fn load(class: Class, per_shard: usize, stop: &Arc<AtomicBool>) -> anyhow::Result<()> {
    let quantum = Duration::from_nanos(kiln::rt::runtime_config(1, 1).step_budget_ns);
    for holder in kiln::rt::shard_holders() {
        for _ in 0..per_shard {
            let stop = Arc::clone(stop);
            hyper_rt::runtime::submit_to_holder(holder, async move {
                let _ = hyper_rt::coop::set_class(class);
                while !stop.load(Ordering::Acquire) {
                    let until = Instant::now() + quantum;
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    hyper_rt::futures::yield_now().await;
                }
            })
            .map_err(|e| anyhow::anyhow!("a shard refused the load: {e}"))?;
        }
    }
    Ok(())
}

/// Waits until `done` holds, for at most 30 s, asking every 20 ms.
fn until(what: &str, mut done: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "waited 30 s for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The tests of this file share the process's runtime: one at a time, so one's load or wedge is not the other's.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn runtime() {
    kiln::rt::configure(kiln::rt::Profile { shards: 2, ..kiln::rt::Profile::tool() });
}

/// Step 2: a shard held in one poll past the watchdog's limit gets no new work: spread spawns (what accepted
/// connections and spawns from off the runtime use) go to the other shard while it is held, and to it again
/// once it lets go.
#[test]
fn g6_s2_spread_work_passes_over_a_held_shard() -> anyhow::Result<()> {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    runtime();
    let holders = kiln::rt::shard_holders();
    let (Some(held), Some(other)) = (holders.first().copied(), holders.get(1).copied()) else { anyhow::bail!("two shards") };
    let release = Arc::new(AtomicBool::new(false));
    let release_in = Arc::clone(&release);
    hyper_rt::runtime::submit_to_holder(held, async move {
        while !release_in.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    until("the watchdog to mark the shard held", || Ok(hyper_rt::watch::stalled(held)))?;
    let (ran, ran_on) = std::sync::mpsc::channel();
    for _ in 0..16 {
        let ran = ran.clone();
        kiln::rt::spawn_spread(async move {
            let _ = ran.send(hyper_rt::futures::shard_id());
        });
    }
    for _ in 0..16 {
        let on = ran_on.recv_timeout(Duration::from_secs(20))?;
        anyhow::ensure!(on == Some(hyper_rt::ShardId(other.shard())), "spread work ran on {on:?}, not the held shard {}", held.shard());
    }
    release.store(true, Ordering::Release);
    until("the mark to lift", || Ok(!hyper_rt::watch::stalled(held)))?;
    let stalls = hyper_rt::watch::stalls(held).ok_or_else(|| anyhow::anyhow!("the held shard's stalls"))?;
    anyhow::ensure!(stalls.episodes >= 1, "{stalls:?}");
    anyhow::ensure!(kiln::metrics::render().contains("kiln_shard_stalls_total{shard=\"0\"}"), "the watchdog's findings on /metrics");
    Ok(())
}

#[test]
fn g6_s4_an_overloaded_server_stays_up_serves_critical_and_sheds_background_first() -> anyhow::Result<()> {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    runtime();
    let probe = Arc::new(Probe { reached: AtomicU64::new(0) });
    let addr = listen(Arc::clone(&probe))?;
    anyhow::ensure!(kiln::overload::start(true) == 2, "a shard refused its monitor");

    // Calm: everything is served, and the monitors learn each class's calm.
    let calm_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < calm_until {
        for request in [BACKGROUND, NORMAL, CRITICAL] {
            anyhow::ensure!(status_of(addr, request)? == 200, "calm: {request:?} served");
        }
    }
    anyhow::ensure!(kiln::overload::shed_counts() == [0, 0, 0], "nothing shed while calm");

    // Background work floods every shard: Background requests are shed, Normal and Critical served.
    let background_load = Arc::new(AtomicBool::new(false));
    load(Class::Background, 200, &background_load)?;
    until("Background requests shed", || Ok(status_of(addr, BACKGROUND)? == 503))?;
    let reached = probe.reached.load(Ordering::Relaxed);
    // A shed request's body is never read: one announced at a megabyte and never sent is answered at once.
    let (status, headers, took) = ask(addr, BACKGROUND.0, BACKGROUND.1, BACKGROUND.2, 1 << 20, b"")?;
    anyhow::ensure!(status == 503, "shed: {status}");
    anyhow::ensure!(header(&headers, "x-kiln-shed") == Some("background"), "{headers:?}");
    anyhow::ensure!(header(&headers, "retry-after").and_then(|v| v.parse::<u64>().ok()).is_some_and(|s| s >= 1), "{headers:?}");
    anyhow::ensure!(took < Duration::from_secs(5), "answered at once, without the body: {took:?}");
    anyhow::ensure!(probe.reached.load(Ordering::Relaxed) == reached, "the handler never ran for a shed request");
    anyhow::ensure!(status_of(addr, NORMAL)? == 200, "Normal still served while Background is shed");
    anyhow::ensure!(status_of(addr, CRITICAL)? == 200, "Critical served");

    // Normal work floods them too: Normal requests are shed as well; Critical ones are served, and promptly.
    let normal_load = Arc::new(AtomicBool::new(false));
    load(Class::Normal, 200, &normal_load)?;
    until("Normal requests shed", || Ok(status_of(addr, NORMAL)? == 503))?;
    let mut critical = Vec::new();
    for _ in 0..20 {
        let (status, _, took) = ask(addr, CRITICAL.0, CRITICAL.1, CRITICAL.2, 0, b"")?;
        anyhow::ensure!(status == 200, "Critical served under overload: {status}");
        critical.push(took);
    }
    critical.sort();
    let median = critical.get(critical.len() / 2).copied().unwrap_or_default();
    eprintln!("critical under overload: median {median:?}, most {:?}", critical.last());
    anyhow::ensure!(median < Duration::from_millis(200), "Critical requests kept prompt under overload: {median:?} at the median");
    let [background, normal, critical_shed] = kiln::overload::shed_counts();
    eprintln!("shed: background {background}, normal {normal}, critical {critical_shed}");
    anyhow::ensure!(background > 0 && normal > 0 && critical_shed == 0, "sheds counted by class: {background}, {normal}, {critical_shed}");
    // /readyz says so: not ready while every shard sheds the API.
    let ready = kiln::overload::readiness();
    anyhow::ensure!(ready.as_ref().is_some_and(|(every, why)| *every && why.contains("sheds normal")), "{ready:?}");
    let metrics = kiln::metrics::render();
    anyhow::ensure!(metrics.contains("kiln_shed_total{class=\"background\"}") && metrics.contains("kiln_shard_shedding{shard=\"0\",class=\"normal\"}"), "sheds on /metrics");

    // The load goes: everything is served again.
    normal_load.store(true, Ordering::Release);
    background_load.store(true, Ordering::Release);
    until("everything served again", || Ok(status_of(addr, BACKGROUND)? == 200 && status_of(addr, NORMAL)? == 200))?;
    anyhow::ensure!(status_of(addr, CRITICAL)? == 200);
    Ok(())
}
