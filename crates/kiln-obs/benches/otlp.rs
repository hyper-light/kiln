//! OTLP metrics decoded (`docs/observe.md` §12.10): an exporter's request into series, as a
//! receiver takes it, for the shapes exporters send. Time, allocations and page faults a point;
//! gzip's inflating and the series keys' making apart, to see what the rest costs. Then OTLP logs:
//! a request into its records and their rows (what the batcher writes of them), and straight into
//! rows (`otlp::logs::Decoder`, warm, its buffers handed back), a record's time, allocations and
//! bytes.
//!
//! `cargo bench -p kiln-obs --bench otlp` (`KOBS_SIGNAL=metrics` or `logs`: one of them)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::io::{Read as _, Write as _};
use std::time::Instant;

use hyper_measure::{alloc, faults};
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::otlp::metrics::{Decoded, Keys, decode, decode_into, decode_with};
use kiln_obs::otlp::pb::Writer;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const NS: u64 = 1_760_000_000_000_000_000;

fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
    w.message(field, |kv| {
        kv.str(1, k);
        kv.message(2, |av| av.str(1, v));
    });
}

/// The load test's (`crates/kiln/tests/observe_load.rs`): one resource attribute, one gauge, each
/// point its own series by one attribute.
fn flat(points: usize) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rm| {
        rm.message(1, |r| kv(r, 1, "host.name", "load-0"));
        rm.message(2, |sm| {
            sm.message(2, |m| {
                m.str(1, "load.v");
                m.message(5, |g| {
                    for j in 0..points {
                        g.message(1, |dp| {
                            kv(dp, 7, "s", &j.to_string());
                            dp.fixed64(3, NS);
                            dp.double(4, 1.0);
                        });
                    }
                });
            });
        });
    });
    w.buf
}

/// A service's export as OpenTelemetry's SDKs send one (its semantic conventions): a resource of
/// nine attributes; `http.server.request.duration`, a histogram of the SDKs' default bounds, and
/// `http.server.requests`, a cumulative monotonic sum, by method, route and status code; and four
/// runtime gauges.
fn sdk(routes: usize) -> Vec<u8> {
    const BOUNDS: [f64; 14] = [0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0];
    let mut w = Writer::default();
    w.message(1, |rm| {
        rm.message(1, |r| {
            for (k, v) in [
                ("service.name", "checkout"),
                ("service.version", "4.2.0"),
                ("service.instance.id", "6f1c2a8e-2b1d-4c3e-9a7f-0e5d8c4b3a21"),
                ("host.name", "web-17"),
                ("os.type", "linux"),
                ("telemetry.sdk.name", "opentelemetry"),
                ("telemetry.sdk.language", "go"),
                ("telemetry.sdk.version", "1.31.0"),
                ("k8s.pod.name", "checkout-7d9f8b6c5-x2x7k"),
            ] {
                kv(r, 1, k, v);
            }
        });
        rm.message(2, |sm| {
            sm.message(1, |scope| scope.str(1, "go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"));
            let labels = |dp: &mut Writer, field: u32, i: usize| {
                kv(dp, field, "http.request.method", ["GET", "POST", "PUT", "DELETE"][i % 4]);
                kv(dp, field, "http.route", &format!("/api/v1/resource{}/{{id}}", i / 4));
                kv(dp, field, "http.response.status_code", ["200", "201", "404", "500"][(i / 3) % 4]);
            };
            sm.message(2, |m| {
                m.str(1, "http.server.request.duration");
                m.str(3, "s");
                m.message(9, |h| {
                    for i in 0..routes {
                        h.message(1, |dp| {
                            labels(dp, 9, i);
                            dp.fixed64(2, NS - 60_000_000_000);
                            dp.fixed64(3, NS);
                            dp.fixed64(4, 1000 + i as u64);
                            dp.double(5, 12.5 * i as f64);
                            dp.message(6, |c| {
                                for b in 0..=BOUNDS.len() {
                                    c.buf.extend_from_slice(&((b * 7 + i) as u64).to_le_bytes());
                                }
                            });
                            dp.message(7, |b| {
                                for x in BOUNDS {
                                    b.buf.extend_from_slice(&x.to_le_bytes());
                                }
                            });
                        });
                    }
                    h.varint(2, 2);
                });
            });
            sm.message(2, |m| {
                m.str(1, "http.server.requests");
                m.message(7, |s| {
                    for i in 0..routes {
                        s.message(1, |dp| {
                            labels(dp, 7, i);
                            dp.fixed64(3, NS);
                            dp.fixed64(6, 1000 + i as u64);
                        });
                    }
                    s.varint(2, 2);
                    s.varint(3, 1);
                });
            });
            for g in ["go.memory.used", "go.goroutine.count", "go.memory.allocated", "go.processor.limit"] {
                sm.message(2, |m| {
                    m.str(1, g);
                    m.message(5, |gauge| {
                        gauge.message(1, |dp| {
                            dp.fixed64(3, NS);
                            dp.double(4, 1.0e8);
                        });
                    });
                });
            }
        });
    });
    w.buf
}

/// The keys of the attributes an SDK's log records carry (logback's through OpenTelemetry's
/// appender, and an app's own).
const LOG_ATTRS: [&str; 12] = ["thread.name", "thread.id", "code.namespace", "code.function", "code.lineno", "http.route", "order.id", "user.id", "payment.method", "region", "attempt", "cart.items"];

/// An SDK's log export: the resource an SDK sends, one scope, `n` records each with its times, a
/// severity (an error one in fifty), a body (a string, or with `map` a map of four), `attrs`
/// attributes and its trace's ids.
fn sdk_logs(n: usize, attrs: usize, map: bool) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rl| {
        rl.message(1, |r| {
            for (k, v) in [
                ("service.name", "checkout"),
                ("service.version", "4.2.0"),
                ("service.instance.id", "6f1c2a8e-2b1d-4c3e-9a7f-0e5d8c4b3a21"),
                ("host.name", "web-17"),
                ("deployment.environment.name", "prod"),
                ("telemetry.sdk.name", "opentelemetry"),
                ("telemetry.sdk.language", "java"),
                ("telemetry.sdk.version", "1.43.0"),
                ("k8s.pod.name", "checkout-7d9f8b6c5-x2x7k"),
            ] {
                kv(r, 1, k, v);
            }
        });
        rl.message(2, |sl| {
            sl.message(1, |scope| scope.str(1, "com.shop.checkout.PaymentService"));
            for i in 0..n {
                sl.message(2, |rec| {
                    rec.fixed64(1, NS + i as u64 * 1_000_000);
                    rec.fixed64(11, NS + i as u64 * 1_000_000 + 50_000);
                    let error = i % 50 == 0;
                    rec.varint(2, if error { 17 } else { 9 });
                    rec.str(3, if error { "ERROR" } else { "INFO" });
                    rec.message(5, |b| {
                        if map {
                            b.message(6, |kvl| {
                                for (k, v) in [("event", "payment"), ("order", "o-1842"), ("amount", "12.50"), ("currency", "EUR")] {
                                    kvl.message(1, |e| {
                                        e.str(1, k);
                                        e.message(2, |a| a.str(1, v));
                                    });
                                }
                            });
                        } else {
                            b.str(1, &format!("order {i} paid in {} ms by a card ending 4242", i % 900));
                        }
                    });
                    for a in 0..attrs {
                        kv(rec, 6, LOG_ATTRS[a % LOG_ATTRS.len()], &format!("v{}", (i + a) % 97));
                    }
                    rec.bytes(9, &[(i % 251) as u8 + 1; 16]);
                    rec.bytes(10, &[(i % 241) as u8 + 1; 8]);
                });
            }
        });
    });
    w.buf
}

/// OTLP logs: each request decoded into records, then each record's row written as the batcher
/// writes it; the median of 51, allocations of one.
fn logs() {
    println!(
        "{:<36} {:>7} {:>7} {:>13} {:>10} {:>10} {:>12} {:>11} {:>10} {:>10}",
        "request", "bytes", "records", "decode ns/rec", "allocs/rec", "bytes/rec", "+ row ns/rec", "rows ns/rec", "allocs/rec", "bytes/rec"
    );
    for (name, body) in [
        ("an SDK's, 500 records, 4 attributes", sdk_logs(500, 4, false)),
        ("an SDK's, 500, 12 attributes, a map", sdk_logs(500, 12, true)),
        ("bare, 500 records, no attributes", sdk_logs(500, 0, false)),
    ] {
        let d = kiln_obs::otlp::logs::decode(&body, NS as i64).unwrap();
        let records = d.records.len() as u64;
        for _ in 0..20 {
            std::hint::black_box(kiln_obs::otlp::logs::decode(&body, NS as i64).unwrap());
        }
        let ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(kiln_obs::otlp::logs::decode(&body, NS as i64).unwrap());
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        alloc::begin();
        let out = kiln_obs::otlp::logs::decode(&body, NS as i64).unwrap();
        let c = alloc::end();
        let mut raw = Vec::with_capacity(1 << 20);
        let row_ns = median(
            (0..51)
                .map(|_| {
                    raw.clear();
                    let t = Instant::now();
                    let mut prev = 0;
                    for r in &out.records {
                        kiln_obs::logs::record::put_row(&mut raw, r, prev);
                        prev = r.ts;
                    }
                    std::hint::black_box(&raw);
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        // Straight into rows, as a receiver decodes: warm, its buffers handed back each time.
        let mut d = kiln_obs::otlp::logs::Decoder::new();
        let once = |d: &mut kiln_obs::otlp::logs::Decoder| {
            let mut out = kiln_obs::otlp::logs::Decoded::default();
            d.decode(&body, NS as i64, &mut |_| false, &mut out).unwrap();
            for part in out.parts {
                if let kiln_obs::logs::record::LogPart::Rows { raw, .. } = part {
                    d.give_rows(raw);
                }
            }
        };
        for _ in 0..20 {
            once(&mut d);
        }
        let rows_ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    once(&mut d);
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        alloc::begin();
        once(&mut d);
        let r = alloc::end();
        println!(
            "{:<36} {:>7} {:>7} {:>13.1} {:>10.2} {:>10.1} {:>12.1} {:>11.1} {:>10.3} {:>10.2}",
            name,
            body.len(),
            records,
            ns as f64 / records as f64,
            c.calls() as f64 / records as f64,
            c.bytes as f64 / records as f64,
            row_ns as f64 / records as f64,
            rows_ns as f64 / records as f64,
            r.calls() as f64 / records as f64,
            r.bytes as f64 / records as f64
        );
    }
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn median(mut ns: Vec<u64>) -> u64 {
    ns.sort_unstable();
    ns[ns.len() / 2]
}

fn main() {
    let signal = std::env::var("KOBS_SIGNAL").unwrap_or_default();
    if signal != "metrics" {
        logs();
    }
    if signal == "logs" {
        return;
    }
    println!(
        "{:<20} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8} {:>10} {:>11} {:>10} {:>10}",
        "request", "bytes", "points", "cold ns/pt", "allocs/pt", "warm ns/pt", "allocs/pt", "bytes/pt", "faults", "inflate", "keys ns/pt", "kept ns/pt", "allocs/pt", "bytes/pt"
    );
    for (name, body) in [("flat, 1,000 series", flat(1000)), ("an SDK's, 50 routes", sdk(50)), ("an SDK's, 500 routes", sdk(500))] {
        let now_ms = (NS / 1_000_000) as i64;
        let d = decode(&body, now_ms).unwrap();
        let points = d.samples.len() as u64;
        // Warm, then the median of 51 decodes.
        for _ in 0..20 {
            std::hint::black_box(decode(&body, now_ms).unwrap());
        }
        let ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(decode(&body, now_ms).unwrap());
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        alloc::begin();
        let out = decode(&body, now_ms).unwrap();
        let cold = alloc::end();
        drop(out);
        // A receiver's steady state: its keys made by the requests before.
        let keys = Keys::new();
        decode_with(&body, now_ms, &keys).unwrap();
        let warm_ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(decode_with(&body, now_ms, &keys).unwrap());
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        let out = decode_with(&body, now_ms, &keys).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        drop(out);
        // A receiver that hands its decoding back (`decode_into`): its buffers grown once.
        let mut kept = Decoded::default();
        decode_into(&body, now_ms, &keys, &mut kept).unwrap();
        let kept_ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    decode_into(&body, now_ms, &keys, &mut kept).unwrap();
                    std::hint::black_box(&kept);
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        alloc::begin();
        decode_into(&body, now_ms, &keys, &mut kept).unwrap();
        let k = alloc::end();
        // gzip's part: inflating the request as it came, a point's worth.
        let gz = gzip(&body);
        let inflate = median(
            (0..51)
                .map(|_| {
                    let mut plain = Vec::with_capacity(body.len());
                    let t = Instant::now();
                    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut plain).unwrap();
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        // The series keys' part: each one made again from its canonical parts.
        let keys: Vec<(String, Vec<(String, String)>)> = d.keys.iter().map(|k| canonical(&k.key)).collect();
        let key_ns = median(
            (0..21)
                .map(|_| {
                    let t = Instant::now();
                    for (m, tags) in &keys {
                        std::hint::black_box(SeriesKey::new(m, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), Kind::Gauge).unwrap());
                    }
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        println!(
            "{:<20} {:>7} {:>7} {:>10.1} {:>10.2} {:>10.1} {:>10.2} {:>10.1} {:>10} {:>6.1}ns {:>10.1} {:>11.1} {:>10.3} {:>10.1}",
            name,
            body.len(),
            points,
            ns as f64 / points as f64,
            cold.calls() as f64 / points as f64,
            warm_ns as f64 / points as f64,
            c.calls() as f64 / points as f64,
            c.bytes as f64 / points as f64,
            f1.since(&f0).total(),
            inflate as f64 / points as f64,
            key_ns as f64 / points as f64,
            kept_ns as f64 / points as f64,
            k.calls() as f64 / points as f64,
            k.bytes as f64 / points as f64,
        );
    }
}

/// A canonical key's metric and tags, read back (`batch::SeriesKey::new`'s layout: each string its
/// length and bytes, the tags' count between).
fn canonical(key: &[u8]) -> (String, Vec<(String, String)>) {
    fn take(at: &mut &[u8]) -> String {
        let n = kiln_obs::varint::get(at).unwrap() as usize;
        let s = String::from_utf8(at[..n].to_vec()).unwrap();
        *at = &at[n..];
        s
    }
    let mut at = key;
    let metric = take(&mut at);
    let count = kiln_obs::varint::get(&mut at).unwrap();
    let tags = (0..count).map(|_| (take(&mut at), take(&mut at))).collect();
    (metric, tags)
}
