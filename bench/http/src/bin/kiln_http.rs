//! kiln's HTTP: kiln-http on hyper-rt, `--threads` shards, each accepting on its own `SO_REUSEPORT`
//! listener (hyper-rt's `tcp::serve`) and serving its connections in their own tasks. `--proto h1`
//! (HTTP/1.1) or `h2` (HTTP/2 with prior knowledge).
//!
//! Every request is answered `200 OK` with `Hello, World!` (13 bytes, plus the `date`), the same
//! response as the other servers.

use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper_rt::tcp::ConnectionBudget;
use kiln_http::{Incoming, TcpIo, service_fn};

async fn hello(_req: http::Request<Incoming>) -> http::Response<Full<Bytes>> {
    http::Response::new(Full::new(Bytes::from_static(b"Hello, World!")))
}

fn config(shards: u16, tasks: usize) -> hyper_rt::RuntimeConfig {
    hyper_rt::RuntimeConfig {
        shards,
        tasks_per_shard: tasks,
        timers_per_shard: tasks * 2,
        interests_per_shard: hyper_rt::runtime::interests_for(tasks),
        ring_entries: 1024,
        step_budget_ns: 250_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut port, mut threads, mut proto, mut tasks) = (8080u16, 4u16, String::from("h1"), 32_768usize);
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--port" => port = it.next().ok_or("--port")?.parse()?,
            "--threads" => threads = it.next().ok_or("--threads")?.parse()?,
            "--proto" => proto = it.next().ok_or("--proto")?,
            "--tasks" => tasks = it.next().ok_or("--tasks")?.parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let rt = hyper_rt::Runtime::start(&config(threads, tasks))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    // One protocol per run, so a connection's task is sized for its own protocol only.
    let budget = ConnectionBudget::new(4_000_000)?;
    let _serving = if proto == "h2" {
        hyper_rt::tcp::serve(&rt, addr, 4096, budget, move |stream| async move {
            let _ = kiln_http::server::http2::Builder::new().serve(TcpIo::new(stream), service_fn(hello)).await;
        })?
    } else {
        hyper_rt::tcp::serve(&rt, addr, 4096, budget, move |stream| async move {
            let _ = kiln_http::server::http1::Builder::new().serve(TcpIo::new(stream), service_fn(hello)).await;
        })?
    };
    eprintln!("kiln-http: {proto} on {addr}, {threads} shards");
    loop {
        std::thread::park();
    }
}
