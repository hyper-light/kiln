//! The shared test vectors (plan step 4): written by Rust into `ui/tests/bp-vectors.json`, read by
//! both test suites. Each vector is a carrier message as hex and what reading it comes to, frame by
//! frame: the frame as JSON (with its newer trailing bytes and its re-encoding), or its refusal.
//! Every kind has one, every refusal the codec makes has one, and each version-skew case has one;
//! joins hold the fragment rules; the fuzz section holds the digests of a million seeded inputs in
//! blocks, which the TypeScript side makes and reads the same way (`crate::fuzz`).
//! `cargo test -p kiln-bp --test vectors` fails when the file differs from what this build writes;
//! `KILN_WRITE_BP=1` writes it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::frames::*;
use crate::join::Joiner;
use crate::wire::{Bytes, Why, Writer, hex, json_str};

/// The fuzz run the vectors hold: its seed, how many inputs, and how many to a digest.
pub const FUZZ_SEED: u64 = 0x6B69_6C6E_6270_0002;
pub const FUZZ_COUNT: u64 = 1_000_000;
pub const FUZZ_BLOCK: u64 = 10_000;

pub fn path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/bp-vectors.json")
}

/// A frame read: its JSON, newer bytes and re-encoding; or a refusal: kind and why.
fn outcome_json(r: &Result<Read, Fault>) -> String {
    match r {
        Ok(r) => {
            let mut h = String::new();
            hex(&r.frame.encode(VERSION), &mut h);
            format!("{{\"frame\":{},\"extra\":{},\"reencoded\":\"{h}\"}}", r.frame.json(), r.extra)
        }
        Err(e) => {
            let kind = e.kind.map_or("null".to_owned(), |k| k.to_string());
            format!("{{\"kind\":{kind},\"why\":\"{}\"}}", e.why.name())
        }
    }
}

fn vector(out: &mut String, name: &str, msg: &[u8]) {
    let outcomes: Vec<String> = decode_message(msg).iter().map(outcome_json).collect();
    let mut h = String::new();
    hex(msg, &mut h);
    let mut n = String::new();
    json_str(name, &mut n);
    let _ = write!(out, "    {{\"name\":{n},\"hex\":\"{h}\",\"outcomes\":[{}]}}", outcomes.join(","));
}

fn envelope(kind: u64, body: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(kind);
    w.bytes(body);
    w.buf
}

/// The hostile and version-skew messages, by name.
pub fn hostile() -> Vec<(&'static str, Vec<u8>)> {
    let idle = Frame::Idle(Idle { got: 7 }).encode(VERSION);
    let then = |mut m: Vec<u8>| {
        m.extend_from_slice(&idle);
        m
    };
    let mut ten = Writer::new();
    ten.varint(u64::from(Kind::Tail.index()));
    ten.varint(u64::from(u32::MAX));
    vec![
        ("refusal cut_short: a field past the body", then(envelope(u64::from(Kind::Tail.index()), &[1, 9, 1]))),
        ("refusal cut_short: a length past the message", {
            let mut m = idle.clone();
            m.pop();
            m
        }),
        ("refusal too_large: a frame past 1 MiB", ten.buf),
        ("refusal bad_varint: eleven bytes", then(envelope(u64::from(Kind::Idle.index()), &[0xff; 11]))),
        ("refusal bad_varint: a u53 past 2^53 - 1", then(envelope(u64::from(Kind::Idle.index()), &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x10]))),
        ("refusal bad_varint: a u32 past 2^32 - 1", then(envelope(u64::from(Kind::Unsubscribe.index()), &[0x80, 0x80, 0x80, 0x80, 0x10]))),
        ("refusal bad_varint: a kind past u32", vec![0x80, 0x80, 0x80, 0x80, 0x10, 0]),
        ("refusal bad_value: a bool of 2", then(envelope(u64::from(Kind::Fragment.index()), &[3, 2, 0]))),
        ("refusal bad_value: an option tag of 2", then(envelope(u64::from(Kind::Answer.index()), &[1, 0, 2]))),
        ("refusal bad_value: text that isn't UTF-8", then(envelope(u64::from(Kind::Close.index()), &[0, 2, 0xc3, 0x28]))),
        ("refusal bad_value: an overlong in UTF-8", then(envelope(u64::from(Kind::Close.index()), &[0, 2, 0xc0, 0x80]))),
        ("refusal bad_value: a surrogate in UTF-8", then(envelope(u64::from(Kind::Close.index()), &[0, 3, 0xed, 0xa0, 0x80]))),
        ("refusal unknown_kind: kind 4000, skipped, the next read", then(envelope(4000, &[1, 2, 3]))),
        ("refusal unknown_variant: an outcome of 77", then(envelope(u64::from(Kind::Answer.index()), &[1, 77]))),
        ("skew: a newer build's trailing fields, counted", then(envelope(u64::from(Kind::Idle.index()), &[7, 0xde, 0xad]))),
        ("skew: an older build's frame, missing trailing fields, defaults", envelope(u64::from(Kind::Answer.index()), &[5, 0])),
        ("skew: an empty body reads as all defaults", envelope(u64::from(Kind::Accepted.index()), &[])),
        ("skew: a newer build's delta variant is unknown", then(envelope(u64::from(Kind::Change.index()), &[1, 2, 1, 9]))),
        ("text: a long string through TextDecoder", Frame::Close(Close { code: CloseCode::Bye, why: "é".repeat(150), build: "中".repeat(70) }).encode(VERSION)),
        ("text: a byte order mark is kept", Frame::Close(Close { code: CloseCode::Bye, why: "\u{feff}x".into(), build: "\u{feff}".repeat(80) }).encode(VERSION)),
        ("varint: a non-canonical zero reads", envelope(u64::from(Kind::Idle.index()), &[0x80, 0x00])),
        ("varint: the largest u53 and i53", {
            let mut m = Frame::Idle(Idle { got: crate::wire::MAX_SAFE }).encode(VERSION);
            m.extend(Frame::Answer(Answer { id: 1, outcome: Outcome::Accepted { epoch: 0, seq: 0, at: -(crate::wire::MAX_SAFE as i64), repeat: false }, at: None }).encode(VERSION));
            m.extend(Frame::Answer(Answer { id: 1, outcome: Outcome::Accepted { epoch: 0, seq: 0, at: crate::wire::MAX_SAFE as i64, repeat: false }, at: None }).encode(VERSION));
            m
        }),
    ]
}

/// Fragment sequences and what joining them comes to.
pub fn joins() -> Vec<(&'static str, Vec<Fragment>)> {
    let idle = Frame::Idle(Idle { got: 7 }).encode(VERSION);
    let big = Frame::Tail(Tail { sub: 4, data: Bytes(vec![b'z'; 3000]) }).encode(VERSION);
    let frags = |lane: u32, b: &[u8], n: usize| -> Vec<Fragment> {
        crate::join::cut(lane, b, n).into_iter().filter_map(|f| if let Frame::Fragment(f) = f { Some(f) } else { None }).collect()
    };
    let mut two = idle.clone();
    two.extend_from_slice(&idle);
    vec![
        ("join: a frame cut in many", frags(4, &big, 400)),
        ("join: two lanes interleaved", {
            let a = frags(1, &big, 1000);
            let b = frags(2, &idle, 100);
            let mut v = Vec::new();
            for (i, f) in a.into_iter().enumerate() {
                v.push(f);
                if i == 0 {
                    v.extend(b.clone());
                }
            }
            v
        }),
        ("join: left_over, two frames in one", vec![Fragment { lane: 1, last: true, data: Bytes(two) }]),
        ("join: too_deep, a fragment in a fragment", vec![Fragment { lane: 1, last: true, data: Bytes(Frame::Fragment(Fragment { lane: 1, last: true, data: Bytes(idle.clone()) }).encode(VERSION)) }]),
        ("join: too_deep, a compressed batch in a fragment", vec![Fragment { lane: 1, last: true, data: Bytes(Frame::Compressed(Compressed { ctx: 1, len: 1, data: Bytes(vec![1]) }).encode(VERSION)) }]),
        ("join: cut_short, nothing joined", vec![Fragment { lane: 1, last: true, data: Bytes(vec![]) }]),
        ("join: too_large, a lane past the bound", {
            let mut v: Vec<Fragment> = (0..1100).map(|_| Fragment { lane: 9, last: false, data: Bytes(vec![0; 1000]) }).collect();
            v.push(Fragment { lane: 9, last: true, data: Bytes(idle.clone()) });
            v
        }),
    ]
}

/// What a join sequence comes to: each fragment's outcome (`null` while joining).
fn join_json(fs: &[Fragment]) -> String {
    let mut j = Joiner::new();
    let mut outs = Vec::new();
    for f in fs {
        outs.push(match j.take(f.clone()) {
            None => "null".to_owned(),
            Some(r) => outcome_json(&r),
        });
    }
    // Long runs of nulls are summed up, so the file stays small.
    let mut compact: Vec<String> = Vec::new();
    let mut nulls = 0usize;
    for o in outs {
        if o == "null" {
            nulls += 1;
            continue;
        }
        if nulls > 0 {
            compact.push(format!("{{\"waiting\":{nulls}}}"));
            nulls = 0;
        }
        compact.push(o);
    }
    if nulls > 0 {
        compact.push(format!("{{\"waiting\":{nulls}}}"));
    }
    compact.join(",")
}

/// The fragments as hex frames (each one `Fragment` frame), compacted where they repeat.
fn fragments_json(fs: &[Fragment]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(f) = fs.get(i) {
        let mut n = 1;
        while fs.get(i + n) == Some(f) {
            n += 1;
        }
        let mut h = String::new();
        hex(&Frame::Fragment(f.clone()).encode(VERSION), &mut h);
        parts.push(if n > 1 { format!("{{\"hex\":\"{h}\",\"times\":{n}}}") } else { format!("{{\"hex\":\"{h}\"}}") });
        i += n;
    }
    parts.join(",")
}

/// The file's text.
pub fn render() -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"about\": \"kiln-bp's shared test vectors (crates/kiln-bp/src/vectors.rs): KILN_WRITE_BP=1 cargo test -p kiln-bp --test vectors writes them\",");
    let _ = writeln!(out, "  \"version\": {VERSION},");
    let _ = writeln!(out, "  \"schema\": \"{}\",", schema_hash());
    out.push_str("  \"vectors\": [\n");
    let mut all: Vec<String> = Vec::new();
    for (i, f) in crate::samples::frames().iter().enumerate() {
        let mut s = String::new();
        vector(&mut s, &format!("frame {i}: {}", f.kind().name()), &f.encode(VERSION));
        all.push(s);
    }
    let mut s = String::new();
    vector(&mut s, "message: every sample frame in one", &encode_message(&crate::samples::frames(), VERSION));
    all.push(s);
    for (name, msg) in hostile() {
        let mut s = String::new();
        vector(&mut s, name, &msg);
        all.push(s);
    }
    out.push_str(&all.join(",\n"));
    out.push_str("\n  ],\n  \"joins\": [\n");
    let js: Vec<String> = joins().iter().map(|(name, fs)| {
        let mut n = String::new();
        json_str(name, &mut n);
        format!("    {{\"name\":{n},\"fragments\":[{}],\"outcomes\":[{}]}}", fragments_json(fs), join_json(fs))
    }).collect();
    out.push_str(&js.join(",\n"));
    out.push_str("\n  ],\n");
    // Batches deflated through one context, sync-flushed: a browser's one long-lived
    // DecompressionStream must give each back whole before the next (plan step 8).
    let mut z = crate::compress::Compressors::new(false);
    let batches: Vec<String> = inflate_batches()
        .iter()
        .map(|plain| {
            let (_, data) = z.deflate(crate::conn::Lane::Control, &crate::compress::Compressible::of(Kind::Change, crate::conn::Trust::Text, plain).into_iter().collect::<Vec<_>>());
            let (mut p, mut d) = (String::new(), String::new());
            hex(plain, &mut p);
            hex(&data, &mut d);
            format!("    {{\"len\":{},\"data\":\"{d}\",\"plain\":\"{p}\"}}", plain.len())
        })
        .collect();
    out.push_str("  \"inflate\": [\n");
    out.push_str(&batches.join(",\n"));
    out.push_str("\n  ],\n");
    let (msgs, ends) = conversation();
    let hexes: Vec<String> = msgs
        .iter()
        .map(|m| {
            let mut h = String::new();
            hex(m, &mut h);
            format!("\"{h}\"")
        })
        .collect();
    let ends: Vec<String> = ends.iter().map(|(sub, kind, key, digest, rev)| format!("{{\"sub\":{sub},\"kind\":{kind},\"key\":\"{key}\",\"digest\":\"{digest}\",\"rev\":{rev}}}")).collect();
    let _ = writeln!(out, "  \"conversation\": {{\"down\": [{}], \"ends\": [{}]}},", hexes.join(","), ends.join(","));
    let traces: Vec<String> = liveness_traces().iter().map(|(ops, verdicts)| format!("    {{\"ops\":[{}],\"verdicts\":\"{verdicts}\"}}", ops.join(","))).collect();
    out.push_str("  \"liveness\": [\n");
    out.push_str(&traces.join(",\n"));
    out.push_str("\n  ],\n");
    let blocks: Vec<String> = (0..FUZZ_COUNT / FUZZ_BLOCK).map(|b| format!("\"{}\"", crate::fuzz::digest(FUZZ_SEED, b * FUZZ_BLOCK, (b + 1) * FUZZ_BLOCK))).collect();
    let _ = writeln!(out, "  \"fuzz\": {{\"seed\": \"{FUZZ_SEED}\", \"count\": {FUZZ_COUNT}, \"block\": {FUZZ_BLOCK}, \"digests\": [{}]}}", blocks.join(", "));
    out.push_str("}\n");
    out
}

/// Which refusals the vectors hold (each the codec makes; `out_of_order` is the connection's,
/// held by the loopback tests).
pub fn refusals_held() -> Vec<Why> {
    let mut seen: Vec<Why> = Vec::new();
    let mut note = |r: &Result<Read, Fault>| {
        if let Err(e) = r
            && !seen.contains(&e.why)
        {
            seen.push(e.why);
        }
    };
    for (_, m) in hostile() {
        for r in decode_message(&m) {
            note(&r);
        }
    }
    for (_, fs) in joins() {
        let mut j = Joiner::new();
        for f in fs {
            if let Some(r) = j.take(f) {
                note(&r);
            }
        }
    }
    seen
}

/// What the inflate vectors deflate: frames as a log subscription sends them, a long one, and a
/// run of samples alike but for a field.
pub fn inflate_batches() -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    for i in 0..6u64 {
        let line = format!("step {i}: compiling kiln-bp v0.2.0 (crates/kiln-bp) ... ok\n");
        v.push(Frame::Change(Change { sub: 2, rev: 10 + i, step: 1, delta: Delta::Append { at: i * 60, data: Bytes::from(line.as_str()) } }).encode(VERSION));
    }
    v.push(Frame::Tail(Tail { sub: 2, data: Bytes(b"0123456789abcdef".repeat(400)) }).encode(VERSION));
    for i in 0..4u8 {
        v.push(Frame::Change(Change { sub: 1, rev: 50 + u64::from(i), step: 1, delta: Delta::Patch { key: Key::Ref { n: 0 }, mask: crate::wire::Bits(0b11), fields: vec![Bytes(vec![i]), Bytes(vec![7, 7])] } }).encode(VERSION));
    }
    v
}

/// Seeded traces through the liveness rule: arrivals (`a`), round trips (`r`), probes sent
/// (`p`) and verdicts asked (`v`, each a letter: live, suspect, dead), which the TypeScript rule
/// must give the same.
pub fn liveness_traces() -> Vec<(Vec<String>, String)> {
    use crate::liveness::{Liveness, Verdict};
    let mut out = Vec::new();
    for seed in 0..24u64 {
        let mut g = crate::rng::Rng::new(0x11FE ^ seed);
        let idle = 200 + g.below(2000);
        let mut l = Liveness::new(idle, 0);
        let mut ops = vec![format!("[\"i\",{idle}]")];
        let mut verdicts = String::new();
        let mut t = 0u64;
        let dark_from = 40 + g.below(40);
        for step in 0..120u64 {
            t += g.below(idle * 2);
            if step < dark_from {
                l.arrived(t);
                ops.push(format!("[\"a\",{t}]"));
                if g.below(3) == 0 {
                    let ms = 20 + g.below(400);
                    l.round_trip(ms);
                    ops.push(format!("[\"r\",{ms}]"));
                }
            }
            if g.below(4) == 0 {
                l.probe_sent(t);
                ops.push(format!("[\"p\",{t}]"));
            }
            let v = l.verdict(t);
            ops.push(format!("[\"v\",{t}]"));
            verdicts.push(match v {
                Verdict::Live => 'l',
                Verdict::Suspect => 's',
                Verdict::Dead => 'd',
            });
        }
        out.push((ops, verdicts));
    }
    out
}

/// A conversation the Rust server holds with a client, as the bytes it sent down (compressed,
/// fragmented, aliased, with tails), and each subscription's state digest and revision at its
/// end: a TypeScript client fed the same bytes must end the same.
pub fn conversation() -> (Vec<Vec<u8>>, Vec<(u32, u32, String, u64, u64)>) {
    use crate::answers::Answers;
    use crate::client::{Client, ClientConfig};
    use crate::conn::{Class, Prio, TopicSpec, Trust};
    use crate::server::{Event, Server, ServerConfig};
    use crate::tickets::{Principal, Tickets};
    use crate::topic::TopicLog;
    let born = 1_793_825_000_000u64;
    let spec = |kind: u32| match kind {
        1 => TopicSpec { kind, class: Class::State, prio: Prio::State, trust: Trust::Records },
        2 => TopicSpec { kind, class: Class::Telemetry, prio: Prio::State, trust: Trust::Records },
        _ => TopicSpec { kind, class: Class::Stream, prio: Prio::Stream, trust: Trust::Text },
    };
    let topics = [(1, 1u32, "runs"), (2, 2, "samples"), (3, 3, "log"), (4, 10, "session")];
    let mut tickets = Tickets::new();
    let mut answers = Answers::new();
    let t = tickets.mint([4; 32], Principal { who: "ada".into(), ..Default::default() }, 0);
    let mut srv = Server::new(ServerConfig { fragment: 300, ..Default::default() }, 1, born, 0);
    let mut cli = Client::new(ClientConfig::default(), &t, 0);
    let mut logs: Vec<TopicLog> = topics.iter().map(|(_, k, _)| TopicLog::new(spec(*k), born)).collect();
    for (sub, kind, key) in topics {
        cli.subscribe(sub, Topic { kind, key: key.into() }, 0);
    }
    let mut down = Vec::new();
    let mut g = crate::rng::Rng::new(0xC0FFEE);
    let mut log_at = 0u64;
    let mut text_at = 0u64;
    for now in 0..400u64 {
        if now > 5 {
            let i = g.below(4);
            let d = match i {
                0 => Delta::Put { key: Key::Str { s: format!("run-{}", g.below(6)) }, fields: vec![Bytes(vec![g.byte()]), Bytes(vec![g.byte(); 3])] },
                1 => Delta::Put { key: Key::Str { s: format!("machine-{}", g.below(3)) }, fields: vec![Bytes(vec![g.byte(); 2])] },
                2 => {
                    let line = format!("{now}: {}\n", "word ".repeat(g.index(12)));
                    let d = Delta::Append { at: log_at, data: Bytes::from(line.as_str()) };
                    log_at += line.len() as u64;
                    d
                }
                _ if text_at == 0 => {
                    text_at = 1;
                    Delta::Put { key: Key::Num { n: 1 }, fields: vec![Bytes(vec![1]), Bytes::default()] }
                }
                _ => {
                    let tok = format!(" tok{}", g.below(100));
                    let d = Delta::Extend { key: Key::Num { n: 1 }, field: 1, at: text_at - 1, data: Bytes::from(tok.as_str()) };
                    text_at += tok.len() as u64;
                    d
                }
            };
            let at = usize::try_from(i).unwrap_or(0);
            if let Some(log) = logs.get_mut(at) {
                if let Ok(p) = log.publish(d) {
                    if let Some((sub, _, _)) = topics.get(at) {
                        srv.push(*sub, &p);
                    }
                }
            }
        }
        if let Some(t) = cli.poll_transmit(64 * 1024, now) {
            srv.receive(&t.bytes, now, &mut tickets, &mut answers);
        }
        while let Some(e) = srv.poll_event() {
            match e {
                Event::Subscribe { sub, cursor, .. } | Event::Resume { sub, cursor, .. } => {
                    if let Some(at) = topics.iter().position(|x| x.0 == sub) {
                        if let (Some(log), Some((_, kind, _))) = (logs.get(at), topics.get(at)) {
                            srv.start(sub, spec(*kind), log.start(cursor.as_ref()));
                        }
                    }
                }
                Event::Request { id, .. } => {
                    srv.answer(id, Outcome::Taken, None, Trust::Secret, &mut answers, now);
                }
                _ => {}
            }
        }
        if let Some(t) = srv.poll_transmit(if now % 3 == 0 { 400 } else { 2000 }, now) {
            cli.receive(&t.bytes, now);
            down.push(t.bytes);
        }
    }
    let ends = topics
        .iter()
        .map(|(sub, kind, key)| (*sub, *kind, (*key).to_owned(), cli.state(*sub).map_or(0, |s| s.digest()), cli.cursor(*sub).map_or(0, |c| c.rev)))
        .collect();
    (down, ends)
}
