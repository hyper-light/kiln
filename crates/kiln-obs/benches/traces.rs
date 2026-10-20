//! Traces (`docs/observe.md` §8.7, §12.23): OTLP trace requests as an SDK's batch span processor
//! exports them (512 spans, its `max_export_batch_size`, of a web service: its servers' spans, its
//! HTTP clients', its database calls', one in twenty failing with an exception), decoded into span
//! rows; time, allocations and page faults a span, the decoder's room kept from request to request,
//! and the rows' bytes, raw and as LZ4 packs them into a write. Then a trace found by its id in a
//! span store of many blocks, by their filters: the blocks read, and the time.
//!
//! `cargo bench -p kiln-obs --bench traces`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::Instant;

use hyper_measure::{alloc, faults};
use kiln_obs::logs::search::Search;
use kiln_obs::logs::store::LogStore;
use kiln_obs::otlp::pb::Writer;
use kiln_obs::otlp::traces::{Decoder, Spans};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
    w.message(field, |m| {
        m.str(1, k);
        m.message(2, |a| a.str(1, v));
    });
}

fn kv_int(w: &mut Writer, field: u32, k: &str, v: u64) {
    w.message(field, |m| {
        m.str(1, k);
        m.message(2, |a| a.varint(3, v));
    });
}

/// A request of `spans` spans, request `r` of a run: traces of a server span, an HTTP client's call
/// under it and a database call under that (three spans a trace), ids distinct across requests.
fn request(spans: usize, r: u64, at: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rs| {
        rs.message(1, |res| {
            kv(res, 1, "service.name", "checkout");
            kv(res, 1, "service.version", "2.14.1");
            kv(res, 1, "deployment.environment.name", "production");
            kv(res, 1, "host.name", "checkout-7d9f8b6c5-x2kqp");
            kv(res, 1, "telemetry.sdk.name", "opentelemetry");
            kv(res, 1, "telemetry.sdk.language", "python");
            kv(res, 1, "telemetry.sdk.version", "1.45.1");
            kv(res, 1, "process.runtime.name", "cpython");
            kv(res, 1, "process.runtime.version", "3.12.7");
            kv_int(res, 1, "process.pid", 4242);
        });
        rs.message(2, |ss| {
            ss.message(1, |s| s.str(1, "opentelemetry.instrumentation.flask"));
            for i in 0..spans as u64 {
                let trace = r * 1_000_000 + i / 3 + 1;
                let start = at + i * 1_000_000;
                ss.message(2, |s| {
                    s.bytes(1, &(u128::from(trace) * 0x9e37_79b9_7f4a_7c15).to_le_bytes());
                    s.bytes(2, &(r * 1_000_000 + i + 1).to_le_bytes());
                    s.str(3, "");
                    if i % 3 > 0 {
                        s.bytes(4, &(r * 1_000_000 + i).to_le_bytes());
                    }
                    s.fixed64(7, start);
                    s.fixed64(8, start + 3_000_000 + (i % 97) * 50_000);
                    s.message(15, |st| st.varint(3, u64::from(i % 20 == 2)));
                    match i % 3 {
                        0 => {
                            s.str(5, "POST /checkout/{cart_id}");
                            s.varint(6, 2);
                            kv(s, 9, "http.request.method", "POST");
                            kv(s, 9, "url.path", &format!("/checkout/{}", trace % 10_000));
                            kv(s, 9, "http.route", "/checkout/{cart_id}");
                            kv_int(s, 9, "http.response.status_code", if i % 20 == 2 { 500 } else { 200 });
                            kv(s, 9, "server.address", "checkout.internal");
                            kv(s, 9, "network.protocol.version", "1.1");
                            kv(s, 9, "user_agent.original", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36");
                        }
                        1 => {
                            s.str(5, "GET");
                            s.varint(6, 3);
                            kv(s, 9, "http.request.method", "GET");
                            kv(s, 9, "url.full", &format!("http://cart.internal:8080/cart/{}", trace % 10_000));
                            kv(s, 9, "server.address", "cart.internal");
                            kv_int(s, 9, "server.port", 8080);
                            kv_int(s, 9, "http.response.status_code", 200);
                        }
                        _ => {
                            s.str(5, "SELECT carts");
                            s.varint(6, 3);
                            kv(s, 9, "db.system.name", "postgresql");
                            kv(s, 9, "db.namespace", "shop");
                            kv(s, 9, "db.query.text", "SELECT c.id, c.owner_id, c.items, c.updated_at FROM carts AS c WHERE c.id = $1 AND c.deleted_at IS NULL FOR UPDATE");
                            kv(s, 9, "server.address", "db-primary.internal");
                            if i % 20 == 2 {
                                s.message(11, |e| {
                                    e.fixed64(1, start + 2_900_000);
                                    e.str(2, "exception");
                                    kv(e, 3, "exception.type", "psycopg.errors.LockNotAvailable");
                                    kv(e, 3, "exception.message", "could not obtain lock on row in relation \"carts\"");
                                    kv(e, 3, "exception.stacktrace", "Traceback (most recent call last):\n  File \"/app/cart.py\", line 88, in load\n    cur.execute(Q, (cart_id,))\npsycopg.errors.LockNotAvailable: could not obtain lock on row in relation \"carts\"\n");
                                });
                            }
                        }
                    }
                });
            }
        });
    });
    w.buf
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    println!("{:<30} {:>6} {:>9} {:>10} {:>10} {:>11} {:>10} {:>8}", "request", "spans", "bytes", "rows B/s", "lz4 B/s", "decode ns/s", "allocs/s", "faults");
    let at = 1_800_000_000_000_000_000u64;
    for spans in [512usize, 4096] {
        let req = request(spans, 1, at);
        let mut d = Decoder::new();
        // The decoder's room grown by the first; buffers handed back as a node's batcher does.
        let mut out = Spans::default();
        d.decode(&req, 0, &mut out).unwrap();
        assert_eq!(out.spans as usize, spans);
        let rows: usize = out.bytes();
        let mut packed = Vec::new();
        let mut lz4 = 0usize;
        for (raw, _) in &out.segments {
            packed.clear();
            kiln_obs::lz4::compress(raw, &mut packed);
            lz4 += packed.len();
        }
        d.give(out);
        let ns = median(
            (0..51)
                .map(|_| {
                    let mut o = Spans::default();
                    let t = Instant::now();
                    d.decode(&req, 0, &mut o).unwrap();
                    let took = t.elapsed().as_nanos() as u64;
                    d.give(o);
                    took
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        let mut o = Spans::default();
        d.decode(&req, 0, &mut o).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        d.give(o);
        let n = spans as f64;
        println!(
            "{:<30} {:>6} {:>9} {:>10.0} {:>10.0} {:>11.0} {:>10.4} {:>8}",
            format!("{spans} spans, a web service's"),
            spans,
            req.len(),
            rows as f64 / n,
            lz4 as f64 / n,
            ns as f64 / n,
            c.calls() as f64 / n,
            f1.since(&f0).total()
        );
    }

    // A trace found by its id: 2,000 requests of 512 spans (1,024,000 spans, 341,334 traces) in a
    // span store of blocks of 256 KiB.
    let mut store = LogStore::spans();
    let mut d = Decoder::new();
    let started = Instant::now();
    for r in 0..2000u64 {
        let mut o = Spans::default();
        d.decode(&request(512, r, at + r * 512_000_000), 0, &mut o).unwrap();
        for (raw, n) in o.segments {
            store.append(raw, n).unwrap();
        }
    }
    let dir = std::env::temp_dir().join(format!("kiln-traces-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = store.cut(&dir.join("1.spans")).unwrap().unwrap();
    let blocks = file.blocks.len();
    store.files.push(kiln_obs::logs::store::HeldFile { seq: 1, file: "1.spans".into(), log: file });
    println!("\n{} spans in {} blocks, {:.1} MB on disk, taken in {:.2} s", 2000 * 512, blocks, store.disk_bytes() as f64 / 1e6, started.elapsed().as_secs_f64());
    let mut times = Vec::new();
    let mut read = Vec::new();
    for k in 0..101u64 {
        let r = (k * 7919) % 2000;
        let trace = r * 1_000_000 + 77;
        let id: [u8; 16] = (u128::from(trace) * 0x9e37_79b9_7f4a_7c15).to_le_bytes();
        let t = Instant::now();
        let found = store.search(&Search::trace(id), i64::MIN, i64::MAX, 100).unwrap();
        times.push(t.elapsed().as_nanos() as u64);
        assert_eq!(found.records.len(), 3, "a trace's three spans");
        read.push(found.blocks_total - found.blocks_pruned);
    }
    let reads = median(read.clone());
    println!("a trace by its id, the whole store: median {:.2} ms, blocks read median {} (most {}) of {}", median(times) as f64 / 1e6, reads, read.iter().max().unwrap(), blocks);
    let _ = std::fs::remove_dir_all(&dir);
}
