//! Seeded fuzzing of the door's sans-IO layers, inside `cargo test` (goal 4 step 9, plan
//! 01a1a832eb597110a6dcfda227981604), in kiln-sim's style: one seed decides a run's inputs, a
//! failure names its seed and how to replay it alone, and each change request runs a budget of
//! seeds (`fuzz: ...` report lines). The nightly workflow (`.kiln/h3-fuzz.yaml`) runs a larger
//! budget from seeds no night before tried.
//!
//! * `KILN_H3_FUZZ_SEED=<seed>` replays one seed of a target (with `--exact` on its test).
//! * `KILN_H3_FUZZ_FROM=<first>` and `KILN_H3_FUZZ_SEEDS=<count>` move and size the budget.
//!
//! Four targets: the QPACK decoder (raw bytes and well-shaped lines with hostile lengths,
//! indexes and Huffman); HTTP/3 streams of every type into the connection core (frame shells with
//! hostile types, lengths, ids and values, and raw bytes, in random pieces); the capsule reader;
//! and whole connections, where seeded hyper-quic peers send interleaved streams, resets and
//! datagrams through a lossy, reordering network. Every step checks: no panic, every error a
//! value, and what's held within its bound.

use std::time::Duration;

use super::capsule::{self, Capsules};
use super::conn::{Admission, Config, Connect, Connection, Event};
use super::door::{DoorConfig, H3_FIXED};
use super::qpack::{self, Budget, Refused, put_int};
use super::testnet::{Echo, Net, Rng, open_session};
use super::tests::{CHROME, Mock, SAFARI, control, headers, v, wt_connect};
use super::{DOOR_BUDGET, frame, put_frame, setting, stream_type, varint};

/// The seeds a target runs: `KILN_H3_FUZZ_SEED` alone, else `KILN_H3_FUZZ_SEEDS` (or `default`)
/// from `KILN_H3_FUZZ_FROM` (or 0).
fn seeds(default: u64) -> std::ops::Range<u64> {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(s) = env("KILN_H3_FUZZ_SEED") {
        return s..s + 1;
    }
    let from = env("KILN_H3_FUZZ_FROM").unwrap_or(0);
    from..from + env("KILN_H3_FUZZ_SEEDS").unwrap_or(default)
}

/// Runs `one` for each seed; the first failure is reported with its seed and how to replay it.
fn run(target: &str, test: &str, default: u64, mut one: impl FnMut(u64) -> Result<usize, String>) {
    let range = seeds(default);
    let mut inputs = 0usize;
    for seed in range.clone() {
        match one(seed) {
            Ok(n) => inputs += n,
            Err(why) => {
                println!("fuzz: {target} FAILED at seed {seed}: {why}\n  replay: KILN_H3_FUZZ_SEED={seed} cargo test -p kiln-http --lib {test} -- --exact --nocapture");
                assert!(why.is_empty(), "{target}: seed {seed}: {why} (replay: KILN_H3_FUZZ_SEED={seed} cargo test -p kiln-http --lib {test} -- --exact)");
            }
        }
    }
    println!("fuzz: {target}: seeds {}..{} ({} seeds), {inputs} inputs, 0 failures", range.start, range.end, range.end - range.start);
}

fn bytes(rng: &mut Rng, max: usize) -> Vec<u8> {
    let n = rng.below(max as u64 + 1) as usize;
    (0..n).map(|_| rng.next() as u8).collect()
}

/// A hostile integer: small, at a prefix's edge, or huge.
fn hostile(rng: &mut Rng) -> u64 {
    match rng.below(6) {
        0 => rng.below(8),
        1 => rng.below(64),
        2 => 63 + rng.below(3),
        3 => rng.below(1 << 14),
        4 => varint::MAX - rng.below(4),
        _ => rng.next() >> rng.below(64),
    }
}

// ------------------------------------------------------------------- 1: the QPACK decoder

/// A field section: raw bytes, or field lines of every form with hostile values.
fn qpack_input(rng: &mut Rng) -> Vec<u8> {
    if rng.below(4) == 0 {
        return bytes(rng, 256);
    }
    let mut s = Vec::new();
    put_int(&mut s, 0, 8, if rng.below(20) == 0 { hostile(rng) } else { 0 });
    put_int(&mut s, if rng.below(2) == 0 { 0x80 } else { 0 }, 7, hostile(rng) % 128);
    for _ in 0..rng.below(64) {
        match rng.below(7) {
            0 => put_int(&mut s, 0xc0, 6, rng.below(110)),
            1 => put_int(&mut s, 0x80, 6, rng.below(8)),
            2 => {
                put_int(&mut s, 0x50, 4, rng.below(110));
                let v = bytes(rng, 40);
                let huff = rng.below(2) == 0;
                let len = if rng.below(10) == 0 { hostile(rng) } else { v.len() as u64 };
                put_int(&mut s, if huff { 0x80 } else { 0 }, 7, len);
                s.extend_from_slice(&v);
            }
            3 => {
                let n = bytes(rng, 12);
                put_int(&mut s, 0x20 | if rng.below(2) == 0 { 0x08 } else { 0 }, 3, n.len() as u64);
                s.extend_from_slice(&n);
                let v = bytes(rng, 40);
                put_int(&mut s, if rng.below(2) == 0 { 0x80 } else { 0 }, 7, v.len() as u64);
                s.extend_from_slice(&v);
            }
            4 => put_int(&mut s, 0x10, 4, hostile(rng) % 1000),
            5 => put_int(&mut s, 0x00, 3, hostile(rng) % 1000),
            _ => s.extend(bytes(rng, 4)),
        }
    }
    s
}

#[test]
fn fuzz_qpack_decoder() {
    let mut scratch = Vec::new();
    run("QPACK decoder", "h3::fuzz::fuzz_qpack_decoder", 6000, |seed| {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let budget = Budget { bytes: 64 + rng.below(DOOR_BUDGET.bytes as u64) as usize, fields: 1 + rng.below(64) as usize };
        let s = qpack_input(&mut rng);
        let mut size = 0usize;
        let mut fields = 0usize;
        let r = qpack::decode(&s, budget, &mut scratch, |n, v| {
            size += n.len() + v.len() + 32;
            fields += 1;
            Ok::<(), ()>(())
        });
        if size > budget.bytes || fields > budget.fields {
            return Err(format!("{size} B in {fields} fields past {budget:?}"));
        }
        if scratch.capacity() > (DOOR_BUDGET.bytes * 2).max(4096) {
            return Err(format!("scratch grew to {}", scratch.capacity()));
        }
        if let Ok(d) = r
            && (d.size != size || d.fields != fields)
        {
            return Err(format!("reported {d:?}, handed {size} B / {fields}"));
        }
        if let Err(Refused::Caller(())) = r {
            return Err("a caller stop it never asked for".into());
        }
        Ok(1)
    });
}

// ------------------------------------------------------------------- 2: HTTP/3 streams

/// A frame shell: a known or unknown type, a hostile length, a payload that may not match it.
fn frame_shell(rng: &mut Rng) -> Vec<u8> {
    let kinds = [frame::DATA, frame::HEADERS, frame::CANCEL_PUSH, frame::SETTINGS, frame::PUSH_PROMISE, frame::GOAWAY, frame::MAX_PUSH_ID, 0x02, 0x06, 0x08, 0x09, 0x21, 0x1f * 7 + 0x21, frame::WT_STREAM];
    let kind = if rng.below(5) == 0 { hostile(rng) } else { kinds.get(rng.below(kinds.len() as u64) as usize).copied().unwrap_or(0) };
    let payload = match kind {
        frame::HEADERS if rng.below(2) == 0 => {
            let mut b = Vec::new();
            qpack::Encoder::begin(&mut b);
            b.extend(qpack_input(rng).get(2..).unwrap_or_default());
            b
        }
        frame::SETTINGS => {
            let mut p = Vec::new();
            for _ in 0..rng.below(12) {
                varint::put(&mut p, if rng.below(2) == 0 { hostile(rng) } else { [0x01, 0x06, 0x07, 0x08, 0x33, setting::ENABLE_WEBTRANSPORT, setting::WT_MAX_SESSIONS, 0x02].get(rng.below(8) as usize).copied().unwrap_or(0) });
                varint::put(&mut p, hostile(rng));
            }
            p
        }
        _ => bytes(rng, 64),
    };
    let mut out = Vec::new();
    varint::put(&mut out, kind);
    let len = if rng.below(8) == 0 { hostile(rng) } else { payload.len() as u64 };
    varint::put(&mut out, len);
    out.extend_from_slice(&payload);
    out
}

/// A stream's bytes: what it starts with (a type, a signal, a frame), then shells or raw bytes.
fn stream_input(rng: &mut Rng, uni: bool) -> Vec<u8> {
    let mut s = Vec::new();
    if uni {
        let types = [stream_type::CONTROL, stream_type::ENCODER, stream_type::DECODER, stream_type::PUSH, stream_type::WEBTRANSPORT, 0x21, 0x1f * 3 + 0x21];
        varint::put(&mut s, if rng.below(6) == 0 { hostile(rng) } else { types.get(rng.below(types.len() as u64) as usize).copied().unwrap_or(0) });
        if rng.below(2) == 0 {
            s.extend(settings_or_session(rng));
        }
    } else if rng.below(3) == 0 {
        varint::put(&mut s, frame::WT_STREAM);
        varint::put(&mut s, if rng.below(4) == 0 { hostile(rng) } else { 0 });
    } else if rng.below(2) == 0 {
        s.extend(wt_connect(b"/wt"));
    }
    for _ in 0..rng.below(8) {
        if rng.below(4) == 0 {
            s.extend(bytes(rng, 48));
        } else {
            s.extend(frame_shell(rng));
        }
    }
    s
}

fn settings_or_session(rng: &mut Rng) -> Vec<u8> {
    if rng.below(2) == 0 {
        let mut f = Vec::new();
        put_frame(&mut f, frame::SETTINGS, &{
            let mut p = Vec::new();
            for &(id, val) in if rng.below(2) == 0 { CHROME } else { SAFARI } {
                varint::put(&mut p, id);
                varint::put(&mut p, val);
            }
            p
        });
        f
    } else {
        v(0)
    }
}

#[test]
fn fuzz_http3_streams() {
    let mut admitted_bad = 0usize;
    run("HTTP/3 streams", "h3::fuzz::fuzz_http3_streams", 3000, |seed| {
        let mut rng = Rng(seed.wrapping_mul(0xbf58_476d_1ce4_e5b9) | 1);
        let mut c = Connection::new(Config::door());
        let mut t = Mock::new();
        c.start(&mut t);
        // A valid control stream first, sometimes.
        if rng.below(2) == 0 {
            t.push(2, &control(CHROME));
            let mut admit = |_: &Connect| Admission::Accept;
            c.readable(&mut t, &mut admit, 2);
        }
        let mut inputs = 0usize;
        let mut now = 0u64;
        for _ in 0..1 + rng.below(12) {
            let uni = rng.below(2) == 0;
            let id = 4 * rng.below(40) + if uni { 2 } else { 0 };
            let s = stream_input(&mut rng, uni);
            // In random pieces, each read by the core as it arrives.
            let mut at = 0;
            while at < s.len() {
                let n = 1 + rng.below(32) as usize;
                let piece = s.get(at..(at + n).min(s.len())).unwrap_or_default();
                at += piece.len();
                t.push(id, piece);
                if rng.below(10) == 0 {
                    t.fin(id);
                }
                let mut arng = Rng(rng.next());
                let mut admit = |k: &Connect| {
                    // Only a well-formed WebTransport CONNECT may get here.
                    if k.authority.is_empty() || k.path.is_empty() {
                        admitted_bad += 1;
                    }
                    if arng.below(3) == 0 { Admission::Forbidden } else { Admission::Accept }
                };
                c.readable(&mut t, &mut admit, id);
                inputs += 1;
                if rng.below(20) == 0 {
                    now += rng.below(50_000_000);
                    let code = hostile(&mut rng);
                    t.inbound.entry(id).or_default().reset = Some(code);
                    c.reset_by_peer(&mut t, &mut admit, id, code, now);
                }
                while let Some(e) = c.poll_event() {
                    if let Event::Readable(sid) = e {
                        let _ = c.wt_read(&mut t, sid, 1 + rng.below(100) as usize);
                    }
                }
                let held = c.held();
                if held > H3_FIXED + 2 * DOOR_BUDGET.bytes {
                    return Err(format!("held {held} B"));
                }
                if c.closed().is_some() {
                    return Ok(inputs);
                }
            }
        }
        if admitted_bad > 0 {
            return Err(format!("{admitted_bad} malformed CONNECTs reached admit"));
        }
        Ok(inputs)
    });
}

// ------------------------------------------------------------------- 3: capsules

#[test]
fn fuzz_capsules() {
    run("capsules", "h3::fuzz::fuzz_capsules", 1500, |seed| {
        let mut rng = Rng(seed.wrapping_mul(0x94d0_49bb_1331_11eb) | 1);
        let max = 1 + rng.below(1500) as usize;
        let mut cap = Capsules::new(max);
        let mut s = Vec::new();
        for _ in 0..rng.below(20) {
            match rng.below(5) {
                0 => s.extend(capsule::close(rng.next() as u32, &bytes(&mut rng, 1100))),
                1 => s.extend(capsule::drain()),
                2 => {
                    varint::put(&mut s, capsule::kind::DATAGRAM);
                    let d = bytes(&mut rng, 2000);
                    varint::put(&mut s, d.len() as u64);
                    s.extend(d);
                }
                3 => {
                    varint::put(&mut s, hostile(&mut rng));
                    varint::put(&mut s, hostile(&mut rng));
                    s.extend(bytes(&mut rng, 64));
                }
                _ => s.extend(bytes(&mut rng, 64)),
            }
        }
        let mut at = 0;
        let mut inputs = 0;
        while at < s.len() {
            let n = 1 + rng.below(64) as usize;
            let piece = s.get(at..(at + n).min(s.len())).unwrap_or_default();
            at += piece.len();
            let mut count = 0usize;
            let r = cap.read(piece, &mut |c| {
                if let capsule::Capsule::Datagram(d) = c
                    && d.len() > max
                {
                    count += 1;
                }
            });
            inputs += 1;
            if count > 0 {
                return Err("a datagram past the bound was kept".into());
            }
            if cap.held() > max.max(4 + capsule::CLOSE_MESSAGE_MAX) {
                return Err(format!("held {} B", cap.held()));
            }
            if r.is_err() {
                break;
            }
        }
        Ok(inputs)
    });
}

// ------------------------------------------------------------------- 4: whole connections

#[test]
fn fuzz_whole_connections() {
    let bound = DoorConfig::door().bound(true) as i64;
    let mut worst = 0i64;
    run("whole connections", "h3::fuzz::fuzz_whole_connections", 60, |seed| {
        let mut net = Net::new(DoorConfig::door(), Echo, seed).ok_or("no network")?;
        net.loss_ppm = net.rng.below(50_000);
        net.reorder_ppm = net.rng.below(200_000);
        net.delay = Duration::from_millis(1 + net.rng.below(40));
        let peers = 1 + net.rng.below(3) as usize;
        for _ in 0..peers {
            net.dial();
        }
        net.until(200_000, |n| n.browsers.iter().all(|b| b.connected() || b.closed()));
        net.measure();
        let mut inputs = 0usize;
        for step in 0..40 {
            for i in 0..peers {
                let mut rng = Rng(net.rng.next());
                let Some(b) = net.b(i) else { continue };
                if !b.connected() {
                    continue;
                }
                match rng.below(9) {
                    0 if step == 0 || rng.below(4) == 0 => {
                        open_session(b, if rng.below(2) == 0 { CHROME } else { SAFARI }, b"/wt");
                    }
                    1 => {
                        if let Some(id) = b.open(rng.below(2) == 0) {
                            let s = stream_input(&mut rng, id & 2 == 2);
                            b.write(id, &s);
                            if rng.below(2) == 0 {
                                b.finish(id);
                            }
                        }
                    }
                    2 => {
                        if let Some(id) = b.open(true) {
                            b.write(id, &headers(&[(b":method", b"GET"), (b":path", b"/"), (b":scheme", b"https"), (b":authority", b"a")]));
                            b.reset(id, hostile(&mut rng) & 0xffff);
                        }
                    }
                    3 => {
                        let mut d = v(rng.below(4) * 4);
                        d.extend(bytes(&mut rng, 1100));
                        b.datagram(&d);
                    }
                    4 => {
                        if let Some(id) = b.open(false) {
                            let mut s = v(stream_type::WEBTRANSPORT);
                            s.extend(v(0));
                            s.extend(bytes(&mut rng, 3000));
                            b.write(id, &s);
                            b.finish(id);
                        }
                    }
                    5 => {
                        let shell = frame_shell(&mut rng);
                        b.write(0, &shell);
                    }
                    6 => {
                        if let Some(c) = b.c() {
                            c.ping();
                        }
                    }
                    7 => {
                        let mut s = v(frame::WT_STREAM);
                        s.extend(v(0));
                        s.extend(bytes(&mut rng, 2000));
                        if let Some(id) = b.open(true) {
                            b.write(id, &s);
                            b.watch(id);
                            if rng.below(2) == 0 {
                                b.stop(id, 3);
                            }
                        }
                    }
                    _ => {}
                }
                inputs += 1;
            }
            net.run_for(Duration::from_millis(5));
            // Within B at every step: the door's live bytes over every connection it holds.
            let (live, _) = net.door_live();
            let open = net.door.counts().0.max(1) as i64;
            worst = worst.max(live / open);
            if live / open > bound {
                hyper_measure::alloc::end();
                return Err(format!("step {step}: {} B a connection, past B = {bound}", live / open));
            }
        }
        hyper_measure::alloc::end();
        Ok(inputs)
    });
    println!("fuzz: whole connections: most live a connection {worst} B, B = {bound} B");
}
