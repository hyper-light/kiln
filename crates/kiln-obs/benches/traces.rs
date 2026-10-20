//! Traces (`docs/observe.md` §8.7, §12.23): OTLP trace requests as an SDK's batch span processor
//! exports them (512 spans, its `max_export_batch_size`, of a web service: its servers' spans, its
//! HTTP clients', its database calls', one in twenty failing with an exception), decoded into span
//! rows; time, allocations and page faults a span, the decoder's room kept from request to request,
//! and the rows' bytes, raw and as LZ4 packs them into a write. Then the same of a Datadog Agent's
//! payloads (`kiln_obs::ddtrace`: its indexed form, a ddtrace Flask app's traces of ten spans as
//! one was recorded sending them) and of its stats (a grouped stat a service's resource, its
//! sketches of some twenty bins). Then a trace found by its id in a span store of many blocks, by
//! their filters: the blocks read, and the time.
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

/// An Agent's indexed `AgentPayload` of `traces` traces of a ddtrace Flask app's ten spans each (its
/// request's, Flask's own, the view's, a database call's), payload `r` of a run: each payload's
/// strings once in its table, written last, as an Agent 7.84 writes it.
fn dd_payload(traces: usize, r: u64, at: u64) -> Vec<u8> {
    let mut strings: Vec<String> = vec![String::new()];
    let mut index = std::collections::HashMap::new();
    let mut s = |v: &str| -> u64 {
        *index.entry(v.to_owned()).or_insert_with(|| {
            strings.push(v.to_owned());
            (strings.len() - 1) as u64
        })
    };
    let str_attr = |w: &mut Writer, k: u64, v: u64| {
        w.message(9, |e| {
            e.varint(1, k);
            e.message(2, |a| a.varint(1, v));
        })
    };
    let num_attr = |w: &mut Writer, k: u64, v: f64| {
        w.message(9, |e| {
            e.varint(1, k);
            e.message(2, |a| a.double(3, v));
        })
    };
    let ops = [
        "flask.request",
        "flask.application",
        "flask.preprocess_request",
        "flask.dispatch_request",
        "main.cart",
        "postgres.query",
        "flask.process_response",
        "flask.do_teardown_request",
        "flask.do_teardown_appcontext",
        "flask.response",
    ];
    // Each op's parent's place in the trace (the request's span none).
    let parents = [usize::MAX, 0, 1, 1, 3, 4, 1, 1, 1, 0];
    let (shop, db, web, sql, flask, env, version) = (s("shop"), s("shop-db"), s("web"), s("sql"), s("flask"), s("e2e"), s("1.2.3"));
    let op_refs: Vec<u64> = ops.iter().map(|o| s(o)).collect();
    let (route, select) = (s("GET /cart/<int:n>"), s("SELECT carts"));
    let keys: Vec<u64> = [
        "component",
        "env",
        "http.method",
        "http.url",
        "http.route",
        "http.useragent",
        "http.status_code",
        "span.kind",
        "runtime-id",
        "language",
        "flask.endpoint",
        "flask.url_rule",
        "flask.version",
        "db.system",
        "_dd.base_service",
        "error.message",
        "error.type",
        "_sampling_priority_v1",
        "_dd.top_level",
        "_dd.measured",
        "process_id",
    ]
    .iter()
    .map(|k| s(k))
    .collect();
    let vals: Vec<u64> = [
        "flask",
        "e2e",
        "GET",
        "/cart/<int:n>",
        "Python-urllib/3.12",
        "200",
        "server",
        "c40ca5aa3a1d4398b41a8cf497c86f86",
        "python",
        "cart",
        "3.1.3",
        "postgresql",
        "shop",
        "cart is locked",
        "builtins.ValueError",
        "500",
    ]
    .iter()
    .map(|v| s(v))
    .collect();
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(traces);
    for t in 0..traces as u64 {
        let failed = t % 20 == 7;
        let url = s(&format!("http://127.0.0.1:18180/cart/{}", r * 100_000 + t));
        let mut c = Writer::default();
        c.varint(1, 1);
        let base = (r * 100_000 + t) * 16;
        let start = at + t * 25_000_000;
        for (i, op) in op_refs.iter().enumerate() {
            c.message(4, |sp| {
                sp.varint(1, if i == 5 { db } else { shop });
                sp.varint(2, *op);
                sp.varint(
                    3,
                    match i {
                        0 | 1 => route,
                        5 => select,
                        _ => *op,
                    },
                );
                sp.fixed64(4, base + i as u64 + 1);
                if let Some(&p) = parents.get(i).filter(|p| **p != usize::MAX) {
                    sp.varint(5, base + p as u64 + 1);
                }
                sp.fixed64(6, start + i as u64 * 200_000);
                sp.varint(7, 14_000_000 - i as u64 * 1_000_000);
                if failed && matches!(i, 0 | 3 | 4 | 5) {
                    sp.varint(8, 1);
                }
                match i {
                    0 => {
                        for (k, v) in [(0, 0), (1, 1), (2, 2), (4, 3), (5, 4), (7, 6), (8, 7), (9, 8), (10, 9), (11, 3), (12, 10)] {
                            str_attr(sp, keys[k], vals[v]);
                        }
                        str_attr(sp, keys[3], url);
                        str_attr(sp, keys[6], vals[if failed { 15 } else { 5 }]);
                        for k in [17, 18, 19, 20] {
                            num_attr(sp, keys[k], 1.0);
                        }
                        sp.varint(10, web);
                        sp.varint(16, 2);
                    }
                    5 => {
                        str_attr(sp, keys[1], vals[1]);
                        str_attr(sp, keys[13], vals[11]);
                        str_attr(sp, keys[14], vals[12]);
                        if failed {
                            str_attr(sp, keys[15], vals[13]);
                            str_attr(sp, keys[16], vals[14]);
                        }
                        num_attr(sp, keys[18], 1.0);
                        sp.varint(10, sql);
                    }
                    _ => {
                        str_attr(sp, keys[0], vals[0]);
                        str_attr(sp, keys[1], vals[1]);
                    }
                }
                sp.varint(13, env);
                if i != 5 {
                    sp.varint(14, version);
                    sp.varint(15, flask);
                }
            });
        }
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&(0x6ad6_d856_0000_0000u64 + r).to_be_bytes());
        id[8..].copy_from_slice(&(base | 1).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        c.bytes(6, &id);
        chunks.push(c.buf);
    }
    let mut w = Writer::default();
    w.str(1, "kiln-apm-bench");
    w.str(2, "none");
    w.str(7, "7.84.4");
    w.message(11, |p| {
        p.varint(3, 1);
        p.varint(7, env);
        p.varint(9, version);
        for c in &chunks {
            p.bytes(11, c);
        }
        for st in &strings {
            p.str(1, st);
        }
    });
    w.buf
}

/// A tracer's own `/v0.5/traces` payload (MessagePack: its strings first in a table, each span an
/// array of twelve) of `traces` traces of a ddtrace Flask app's ten spans, as ddtrace 4.16 was
/// recorded sending its Agent's port; payload `r` of a run.
fn tracer_v05(traces: usize, r: u64, at: i64) -> Vec<u8> {
    fn st(out: &mut Vec<u8>, v: &str) {
        out.push(0xdb);
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    fn u(out: &mut Vec<u8>, v: u64) {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn i(out: &mut Vec<u8>, v: i64) {
        out.push(0xd3);
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn head(out: &mut Vec<u8>, fix: u8, wide: u8, n: usize) {
        if n < 16 {
            out.push(fix | n as u8);
        } else {
            out.push(wide);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    let mut strings: Vec<String> = vec![String::new()];
    let mut index = std::collections::HashMap::new();
    let mut s = |v: &str| -> u64 {
        *index.entry(v.to_owned()).or_insert_with(|| {
            strings.push(v.to_owned());
            (strings.len() - 1) as u64
        })
    };
    let ops = [
        "flask.request",
        "flask.application",
        "flask.preprocess_request",
        "flask.dispatch_request",
        "__main__.cart",
        "postgres.query",
        "flask.process_response",
        "flask.do_teardown_request",
        "flask.do_teardown_appcontext",
        "flask.response",
    ];
    let parents = [usize::MAX, 0, 1, 1, 3, 4, 1, 1, 1, 0];
    let (shop, db, web, sql, route, select) = (s("shop"), s("shop-db"), s("web"), s("sql"), s("GET /cart/<int:n>"), s("SELECT carts"));
    let op_refs: Vec<u64> = ops.iter().map(|o| s(o)).collect();
    let common: Vec<(u64, u64)> = [("component", "flask"), ("env", "e2e"), ("version", "1.2.3"), ("language", "python")].iter().map(|(k, v)| (s(k), s(v))).collect();
    let root_meta: Vec<(u64, u64)> = [
        ("span.kind", "server"),
        ("http.method", "GET"),
        ("http.route", "/cart/<int:n>"),
        ("http.useragent", "Python-urllib/3.12"),
        ("flask.endpoint", "cart"),
        ("flask.url_rule", "/cart/<int:n>"),
        ("flask.version", "3.1.3"),
        ("runtime-id", "c40ca5aa3a1d4398b41a8cf497c86f86"),
        ("_dd.p.dm", "-0"),
        ("_dd.p.tid", "6ad6d85600000000"),
    ]
    .iter()
    .map(|(k, v)| (s(k), s(v)))
    .collect();
    let (status_k, ok, failed, url_k) = (s("http.status_code"), s("200"), s("500"), s("http.url"));
    let metrics: Vec<u64> = ["_dd.top_level", "_dd.measured", "_sampling_priority_v1", "process_id"].iter().map(|k| s(k)).collect();
    let (db_system, postgres) = (s("db.system"), s("postgresql"));
    let mut body = Vec::new();
    head(&mut body, 0x90, 0xdd, traces);
    for t in 0..traces as u64 {
        let fail = t % 20 == 7;
        let url = s(&format!("http://127.0.0.1:18180/cart/{}", r * 100_000 + t));
        let (trace, base) = ((r * 100_000 + t + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15), (r * 100_000 + t) * 16);
        let start = at + t as i64 * 25_000_000;
        head(&mut body, 0x90, 0xdd, ops.len());
        for (n, op) in op_refs.iter().enumerate() {
            body.push(0x9c);
            u(&mut body, if n == 5 { db } else { shop });
            u(&mut body, *op);
            u(
                &mut body,
                match n {
                    0 | 1 => route,
                    5 => select,
                    _ => *op,
                },
            );
            u(&mut body, trace);
            u(&mut body, base + n as u64 + 1);
            u(&mut body, parents.get(n).filter(|p| **p != usize::MAX).map_or(0, |p| base + *p as u64 + 1));
            i(&mut body, start + n as i64 * 200_000);
            i(&mut body, 14_000_000 - n as i64 * 1_000_000);
            i(&mut body, i64::from(fail && matches!(n, 0 | 3 | 4 | 5)));
            let mut meta: Vec<(u64, u64)> = common.clone();
            match n {
                0 => {
                    meta.extend(root_meta.iter().copied());
                    meta.push((status_k, if fail { failed } else { ok }));
                    meta.push((url_k, url));
                }
                5 => meta.push((db_system, postgres)),
                _ => {}
            }
            head(&mut body, 0x80, 0xdf, meta.len());
            for (k, v) in meta {
                u(&mut body, k);
                u(&mut body, v);
            }
            let m: &[u64] = if n == 0 { &metrics } else { &[] };
            head(&mut body, 0x80, 0xdf, m.len());
            for k in m {
                u(&mut body, *k);
                body.push(0xcb);
                body.extend_from_slice(&1.0f64.to_be_bytes());
            }
            u(
                &mut body,
                match n {
                    0 => web,
                    5 => sql,
                    _ => 0,
                },
            );
        }
    }
    let mut out = Vec::with_capacity(body.len() + strings.len() * 24);
    head(&mut out, 0x90, 0xdd, 2);
    head(&mut out, 0x90, 0xdd, strings.len());
    for v in &strings {
        st(&mut out, v);
    }
    out.extend_from_slice(&body);
    out
}

/// An Agent's `StatsPayload` of `groups` grouped stats in one bucket: as msgp writes it.
fn dd_stats(groups: usize) -> Vec<u8> {
    fn head(out: &mut Vec<u8>, base: u8, wide: u8, n: usize) {
        if n < 16 {
            out.push(base | n as u8);
        } else {
            out.push(wide);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    fn st(out: &mut Vec<u8>, v: &str) {
        out.push(0xdb);
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    fn u(out: &mut Vec<u8>, v: u64) {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
    // A sketch of 1% accuracy (the Agent's), twenty bins in a run from 800 (about 9 ms).
    let mut sk = Writer::default();
    sk.message(1, |m| m.double(1, 1.01 / 0.99));
    sk.message(2, |s| {
        s.bytes(2, &(0..20).flat_map(|i| f64::from(1 + i % 4).to_le_bytes()).collect::<Vec<u8>>());
        s.varint(3, 800 << 1);
    });
    let mut b = Vec::new();
    head(&mut b, 0x80, 0xdf, 3);
    st(&mut b, "AgentHostname");
    st(&mut b, "kiln-apm-bench");
    st(&mut b, "AgentEnv");
    st(&mut b, "none");
    st(&mut b, "Stats");
    head(&mut b, 0x90, 0xdd, 1);
    head(&mut b, 0x80, 0xdf, 4);
    for (k, v) in [("Hostname", "kiln-apm-bench"), ("Env", "production"), ("Version", "2.14.1")] {
        st(&mut b, k);
        st(&mut b, v);
    }
    st(&mut b, "Stats");
    head(&mut b, 0x90, 0xdd, 1);
    head(&mut b, 0x80, 0xdf, 3);
    st(&mut b, "Start");
    u(&mut b, 1_800_000_000_000_000_000);
    st(&mut b, "Duration");
    u(&mut b, 10_000_000_000);
    st(&mut b, "Stats");
    head(&mut b, 0x90, 0xdd, groups);
    for g in 0..groups {
        head(&mut b, 0x80, 0xdf, 13);
        for (k, v) in [
            ("Service", format!("svc-{}", g % 20)),
            ("Name", "flask.request".to_owned()),
            ("Resource", format!("GET /r/{}", g / 20)),
            ("Type", "web".to_owned()),
            ("SpanKind", if g % 3 == 0 { "client" } else { "server" }.to_owned()),
        ] {
            st(&mut b, k);
            st(&mut b, &v);
        }
        for (k, v) in [("HTTPStatusCode", if g % 7 == 0 { 500 } else { 200 }), ("Hits", 50), ("Errors", u64::from(g % 7 == 0) * 50), ("Duration", 500_000_000), ("TopLevelHits", 50)] {
            st(&mut b, k);
            u(&mut b, v);
        }
        st(&mut b, "OkSummary");
        b.push(0xc6);
        b.extend_from_slice(&(sk.buf.len() as u32).to_be_bytes());
        b.extend_from_slice(&sk.buf);
        st(&mut b, "ErrorSummary");
        b.push(0xc4);
        b.push(0);
        st(&mut b, "PeerTags");
        head(&mut b, 0x90, 0xdd, 2);
        st(&mut b, "peer.service:billing");
        st(&mut b, "out.host:10.0.0.9");
    }
    b
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

    // A Datadog Agent's payloads, indexed.
    for traces in [52usize, 410] {
        let req = dd_payload(traces, 1, at);
        let mut d = kiln_obs::ddtrace::Decoder::new();
        let mut out = Spans::default();
        d.decode(&req, 0, &mut out).unwrap();
        let spans = out.spans as usize;
        assert_eq!(spans, traces * 10);
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
            format!("{spans} spans, an Agent's"),
            spans,
            req.len(),
            rows as f64 / n,
            lz4 as f64 / n,
            ns as f64 / n,
            c.calls() as f64 / n,
            f1.since(&f0).total()
        );
    }
    // A tracer's own payloads, straight to kiln (kiln in its Agent's place): decoded, every span
    // counted by the node's concentrator (some 2 of a trace's 10: its request and its database's).
    for traces in [52usize, 410] {
        let body = tracer_v05(traces, 1, at as i64);
        let tracer = kiln_obs::ddtrace::Tracer { language: "python", container: "", host: "web-1" };
        let mut d = kiln_obs::ddtrace::Decoder::new();
        let (mut stats, mut roots) = (Vec::new(), Vec::new());
        let mut out = Spans::default();
        d.decode_tracer(true, &body, &tracer, 0, &mut out, &mut stats, &mut roots).unwrap();
        let spans = out.spans as usize;
        assert_eq!(spans, traces * 10);
        d.give(out);
        let ns = median(
            (0..51)
                .map(|_| {
                    let mut o = Spans::default();
                    stats.clear();
                    roots.clear();
                    let t = Instant::now();
                    d.decode_tracer(true, &body, &tracer, 0, &mut o, &mut stats, &mut roots).unwrap();
                    let took = t.elapsed().as_nanos() as u64;
                    d.give(o);
                    took
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        let mut o = Spans::default();
        stats.clear();
        roots.clear();
        d.decode_tracer(true, &body, &tracer, 0, &mut o, &mut stats, &mut roots).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        d.give(o);
        let n = spans as f64;
        println!(
            "{:<30} {:>6} {:>9} {:>10} {:>10} {:>11.0} {:>10.4} {:>8}",
            format!("{spans} spans, a tracer's own"),
            spans,
            body.len(),
            "",
            "",
            ns as f64 / n,
            c.calls() as f64 / n,
            f1.since(&f0).total()
        );
        // The concentrator: the payload's spans counted (a group made once, found after).
        let mut conc = kiln_obs::ddtrace::Concentrator::new("web-1");
        let now = (at / 1_000_000) as i64;
        for st in &stats {
            conc.add(st, now);
        }
        let ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    for st in &stats {
                        conc.add(st, now);
                    }
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        alloc::begin();
        for st in &stats {
            conc.add(st, now);
        }
        let c = alloc::end();
        println!(
            "{:<30} {:>6} {:>9} {:>10} {:>10} {:>11.0} {:>10.4}",
            format!("{} spans counted", stats.len()),
            stats.len(),
            "",
            "",
            "",
            ns as f64 / stats.len() as f64,
            c.calls() as f64 / stats.len() as f64
        );
    }

    // Its stats: the receivers' keys kept (a series seen before found, not made), the decoding's
    // room kept from payload to payload.
    for groups in [100usize, 2000] {
        let body = dd_stats(groups);
        let keys = kiln_obs::otlp::metrics::Keys::new();
        let mut d = kiln_obs::otlp::metrics::Decoded::default();
        let first = Instant::now();
        let taken = kiln_obs::ddtrace::stats_into(&body, &keys, &mut d).unwrap();
        let cold = first.elapsed().as_nanos() as u64;
        assert_eq!(taken.groups as usize, groups);
        let series = d.keys.len();
        let ns = median(
            (0..31)
                .map(|_| {
                    let t = Instant::now();
                    kiln_obs::ddtrace::stats_into(&body, &keys, &mut d).unwrap();
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        kiln_obs::ddtrace::stats_into(&body, &keys, &mut d).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        println!(
            "{:<30} {:>6} {:>9} {:>10} {:>10} {:>11.0} {:>10.2} {:>8}   ({series} series; the first payload {:.0} ns a group)",
            format!("{groups} grouped stats"),
            groups,
            body.len(),
            "",
            "",
            ns as f64 / groups as f64,
            c.calls() as f64 / groups as f64,
            f1.since(&f0).total(),
            cold as f64 / groups as f64
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

    // A service's figures over a long range (`kiln_obs::apm::red`): a week of trace metrics, a point
    // each ten seconds, of 20 groups (resource and status) of 14 series each (hits, errors, the
    // durations' sum and most, ten buckets), written a day at a time, a block a day; read from
    // rollups and from every sample.
    use kiln_obs::batch::{Builder, Kind, SeriesKey};
    let days = 7i64;
    let start = 1_800_000_000_000 - 1_800_000_000_000 % 86_400_000;
    let dir = std::env::temp_dir().join(format!("kiln-red-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut st = kiln_obs::store::Store::open(&dir, kiln_obs::hash::Keyed::with_keys(1, 2)).unwrap();
    let mut keys: Vec<SeriesKey> = Vec::new();
    for g in 0..20 {
        let resource = format!("GET /r/{}", g / 2);
        let status = if g % 2 == 0 { "200" } else { "500" };
        let tags = [("env", "prod"), ("http.status_code", status), ("resource_name", resource.as_str()), ("service", "web"), ("span.kind", "server")];
        for (name, kind) in [("trace.hits", Kind::Count), ("trace.errors", Kind::Count), ("trace.duration.sum", Kind::Count), ("trace.duration.max", Kind::Gauge)] {
            keys.push(SeriesKey::new(name, tags.iter().copied(), kind).unwrap());
        }
        for b in 0..10 {
            let v = format!("5:p{}", 150 + b);
            keys.push(SeriesKey::new("trace.duration.dist", tags.iter().copied().chain([("dist_bucket", v.as_str())]), Kind::Count).unwrap());
        }
    }
    let started = Instant::now();
    let mut rng = kiln_obs::testkit::Rng::new(3);
    for day in 0..days {
        let mut b = Builder::new();
        for k in &keys {
            b.define(k);
        }
        for k in &keys {
            let samples: Vec<(i64, f64)> = (0..8_640).map(|i| (start + day * 86_400_000 + i * 10_000, rng.below(20) as f64)).collect();
            b.run(k.fp, samples);
        }
        st.apply(&b.finish()).unwrap();
        st.cut(kiln_obs::manifest::Point { index: day as u64 + 1, term: 1 }).unwrap();
        st.compact().unwrap();
    }
    println!("\n{} series of a week's trace metrics ({:.1} M points) written in {:.1} s", keys.len(), keys.len() as f64 * days as f64 * 8_640.0 / 1e6, started.elapsed().as_secs_f64());
    for (name, range) in [("an hour", 3_600_000i64), ("a day", 86_400_000), ("a week", days * 86_400_000)] {
        let (from, to) = (start + days * 86_400_000 - range + 1_234, start + days * 86_400_000 - 5_678);
        let mut time = |on: bool| {
            st.set_read_rollups(on);
            median(
                (0..11)
                    .map(|_| {
                        let t = Instant::now();
                        let r = kiln_obs::apm::red(&st, false, &[("service", "web")], "resource_name", from, to).unwrap();
                        assert_eq!(r.len(), 10);
                        t.elapsed().as_nanos() as u64
                    })
                    .collect(),
            )
        };
        let (up, raw) = (time(true), time(false));
        println!("{name:<8} by resource: {:>8.2} ms read from rollups, {:>8.2} ms from every sample ({:.1}x)", up as f64 / 1e6, raw as f64 / 1e6, raw as f64 / up as f64);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
