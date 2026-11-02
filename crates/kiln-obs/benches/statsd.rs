//! DogStatsD aggregated (`docs/observe.md` §8.4, §12.20): datagrams as Datadog's clients pack them
//! (1,432 bytes over UDP, 8 KiB over a Unix socket), from a service's traffic and from many
//! contexts; time, allocations and page faults a message once its contexts exist, a new context's
//! cost, and a bucket's flush.
//!
//! `cargo bench -p kiln-obs --bench statsd`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::{alloc, faults};
use kiln_obs::statsd::{Aggregator, BUCKET_MS, Settings};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// Messages packed into datagrams of at most `size` bytes, as a client's buffer packs them.
fn pack(messages: &[String], size: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut d: Vec<u8> = Vec::with_capacity(size);
    for m in messages {
        if !d.is_empty() && d.len() + 1 + m.len() > size {
            out.push(std::mem::take(&mut d));
        }
        if !d.is_empty() {
            d.push(b'\n');
        }
        d.extend_from_slice(m.as_bytes());
    }
    if !d.is_empty() {
        out.push(d);
    }
    out
}

/// A web service's DogStatsD, its integrations' shape: per route and method, a request count and
/// a latency timer, by status; a gauge per queue; a set of users; a payload distribution; tags as
/// Datadog's unified service tagging gives them.
fn service(messages: usize, routes: usize) -> Vec<String> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..messages)
        .map(|i| {
            let r = next() as usize % routes;
            let tags = format!("#env:prod,service:checkout,version:4.2.0,route:/api/v1/resource{r},method:{},status:{}", ["GET", "POST"][r % 2], ["200", "201", "404", "500"][next() as usize % 4]);
            match i % 10 {
                0..=3 => format!("http.server.requests:1|c|{tags}"),
                4..=7 => format!("http.server.latency:{}|ms|{tags}", 1 + next() % 900),
                8 => format!("queue.depth:{}|g|#env:prod,service:checkout,queue:q{}", next() % 1000, r % 16),
                _ => {
                    if next() % 2 == 0 {
                        format!("users.active:u{}|s|#env:prod,service:checkout", next() % 5000)
                    } else {
                        format!("payload.bytes:{}|d|#env:prod,service:checkout,route:/api/v1/resource{r}", 100 + next() % 100_000)
                    }
                }
            }
        })
        .collect()
}

/// Many contexts: counts each of its own tag value.
fn many(contexts: usize) -> Vec<String> {
    (0..contexts).map(|i| format!("jobs.done:1|c|#env:prod,queue:default,job:j{i}")).collect()
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    println!(
        "{:<34} {:>9} {:>9} {:>9} {:>11} {:>11} {:>11} {:>9} {:>12} {:>12}",
        "traffic", "messages", "contexts", "datagrams", "first ns/m", "steady ns/m", "allocs/m", "faults", "flush ns/ctx", "flush allocs"
    );
    let cases: [(&str, Vec<String>, usize); 4] = [
        ("a service, 200 routes, UDP", service(200_000, 200), 1432),
        ("a service, 200 routes, Unix", service(200_000, 200), 8192),
        ("a service, 5,000 routes, UDP", service(200_000, 5000), 1432),
        ("100,000 contexts, UDP", many(100_000), 1432),
    ];
    for (name, messages, size) in cases {
        let datagrams = pack(&messages, size);
        let mut records = Vec::new();
        let mut agg = Aggregator::new(Settings { host: "web-1".into(), ..Settings::default() });
        // First: every context made as it is first seen.
        let t = Instant::now();
        for d in &datagrams {
            agg.take(d, 1_000, &mut records);
        }
        let first = t.elapsed().as_nanos() as u64;
        let contexts = agg.stats().contexts;
        // Steady: the same traffic again, its contexts known, a bucket of its own each pass.
        let mut at = 1_000 + BUCKET_MS;
        let (mut keys, mut samples) = (Vec::new(), Vec::new());
        agg.flush(at);
        agg.drain(&mut keys, &mut samples);
        let steady = median(
            (0..9)
                .map(|_| {
                    let t = Instant::now();
                    for d in &datagrams {
                        agg.take(d, at, &mut records);
                    }
                    let ns = t.elapsed().as_nanos() as u64;
                    at += BUCKET_MS;
                    agg.flush(at);
                    agg.drain(&mut keys, &mut samples);
                    ns
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        for d in &datagrams {
            agg.take(d, at, &mut records);
        }
        let taking = alloc::end();
        let f1 = faults::read().unwrap();
        // A bucket's flush: every context's points written into the output, its room kept.
        at += BUCKET_MS;
        alloc::begin();
        let t = Instant::now();
        agg.flush(at);
        let flush_ns = t.elapsed().as_nanos() as u64;
        let flushing = alloc::end();
        let points = agg.drain(&mut keys, &mut samples);
        assert!(points > 0 && records.is_empty());
        let n = messages.len() as f64;
        println!(
            "{:<34} {:>9} {:>9} {:>9} {:>11.1} {:>11.1} {:>11.4} {:>9} {:>12.1} {:>12}",
            name,
            messages.len(),
            contexts,
            datagrams.len(),
            first as f64 / n,
            steady as f64 / n,
            taking.calls() as f64 / n,
            f1.since(&f0).total(),
            flush_ns as f64 / contexts as f64,
            flushing.calls(),
        );
    }
}
