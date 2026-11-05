//! bp-g2 step 2 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): the Rust codec. Every kind round-trips,
//! every kind's index is pinned, builds of different ages read each other's frames, hostile frames
//! get their documented refusals with the next valid frame read after them, and a million seeded
//! inputs read without a panic.

use kiln_bp::frames::*;
use kiln_bp::join::{Joiner, cut};
use kiln_bp::rng::Rng;
use kiln_bp::wire::{Body, Bytes, Reader, Why, Wire, Writer};
use kiln_bp::{bp_struct, fuzz, samples};

type R = Result<(), Box<dyn std::error::Error>>;

fn one(msg: &[u8]) -> Result<Read, Fault> {
    let mut all = decode_message(msg);
    if all.len() != 1 {
        return Err(Fault { kind: None, why: Why::LeftOver });
    }
    all.pop().unwrap_or(Err(Fault { kind: None, why: Why::CutShort }))
}

#[test]
fn bp_g2_s2_every_kind_round_trips() -> R {
    let all = samples::frames();
    for k in KINDS {
        assert!(all.iter().any(|f| f.kind() == *k), "no sample of {k:?}");
    }
    for f in &all {
        let b = f.encode(VERSION);
        let back = one(&b).map_err(|e| format!("{f:?}: {e:?}"))?;
        assert_eq!(&back.frame, f);
        assert_eq!(back.extra, 0);
        assert_eq!(back.frame.encode(VERSION), b);
    }
    // And every frame drawn by type.
    let mut g = Rng::new(2);
    for _ in 0..20_000 {
        let f = Frame::arb(&mut g);
        let b = f.encode(VERSION);
        assert_eq!(one(&b).map(|r| r.frame), Ok(f));
    }
    // Several frames in one message.
    let msg = encode_message(&all, VERSION);
    let back: Vec<Frame> = decode_message(&msg).into_iter().filter_map(Result::ok).map(|r| r.frame).collect();
    assert_eq!(back, all);
    Ok(())
}

/// Older peers read by index: each kind's is pinned here, and new kinds go after every one.
#[test]
fn bp_g2_s2_kind_indexes_are_pinned() {
    let pinned = [
        ("Open", 0),
        ("Accepted", 1),
        ("Refused", 2),
        ("Request", 3),
        ("Answer", 4),
        ("Subscribe", 5),
        ("Unsubscribe", 6),
        ("Snapshot", 7),
        ("Resumed", 8),
        ("Change", 9),
        ("Tail", 10),
        ("Ready", 11),
        ("End", 12),
        ("Credit", 13),
        ("Fragment", 14),
        ("Idle", 15),
        ("Probe", 16),
        ("ProbeAck", 17),
        ("Unread", 18),
        ("Compressed", 19),
        ("Close", 20),
        ("Input", 21),
        ("SecretAnswer", 22),
        ("Resize", 23),
    ];
    assert_eq!(KINDS.len(), pinned.len(), "a kind was added: pin its index here, after every other");
    for (k, (name, i)) in KINDS.iter().zip(pinned) {
        assert_eq!((k.name(), k.index()), (name, i));
        assert_eq!(Kind::of_index(i), Some(*k));
    }
    // And the variants that ride inside them.
    assert_eq!(Outcome::VARIANTS, &["Taken", "Accepted", "Unchecked", "Pending", "Parked", "Nack", "Reply"]);
    assert_eq!(Delta::VARIANTS, &["Set", "Put", "Patch", "Remove", "Append", "Extend"]);
    assert_eq!(Key::VARIANTS, &["Num", "Str", "Ref"]);
    assert_eq!(Call::VARIANTS, &["Http", "Command"]);
    assert_eq!(OpenRefusal::VARIANTS, &["BadTicket", "Expired", "Reused", "TooOld", "TooNew", "Schema", "Busy", "Malformed"]);
    assert_eq!(EndCode::VARIANTS, &["Done", "Gone", "Denied", "Unknown", "Limit"]);
    assert_eq!(CloseCode::VARIANTS, &["Bye", "Expired", "SwitchBuild", "Revoked", "Shutdown", "Protocol"]);
    assert_eq!(UnreadWhy::VARIANTS.len(), kiln_bp::wire::WHYS.len());
    for w in kiln_bp::wire::WHYS {
        assert_eq!(UnreadWhy::of(w).variant(), w.index());
    }
}

bp_struct! {
    /// A frame body as version 1 has it.
    pub struct Old {
        pub id: u64,
        pub name: String,
    }
}

bp_struct! {
    /// The same body as version 2 has it: a field appended.
    pub struct New {
        pub id: u64,
        pub name: String,
        pub tags: Vec<String> = since 2,
        pub ok: bool = since 2,
    }
}

fn body_of<T: Body>(v: &T, version: u32) -> Vec<u8> {
    let mut w = Writer::new();
    v.put_body(&mut w, version);
    w.buf
}

/// Older and newer layouts of a frame read both ways: the newer's trailing fields are counted,
/// the older's missing ones read as defaults, and a peer speaking version 1 isn't sent them.
#[test]
fn bp_g2_s2_older_and_newer_layouts_read_both_ways() -> R {
    let new = New { id: 9, name: "x".into(), tags: vec!["a".into()], ok: true };
    let b2 = body_of(&new, 2);
    let mut r = Reader::new(&b2);
    let old = Old::take_body(&mut r)?;
    assert_eq!(old, Old { id: 9, name: "x".into() });
    assert_eq!(r.left(), 4, "the newer fields are counted, not refused");

    let b1 = body_of(&Old { id: 9, name: "x".into() }, 1);
    let mut r = Reader::new(&b1);
    assert_eq!(New::take_body(&mut r)?, New { id: 9, name: "x".into(), tags: vec![], ok: false });

    assert_eq!(body_of(&new, 1), b1, "a version 1 peer gets only what it knows");
    // A body cut inside a field is still refused.
    let mut r = Reader::new(b2.get(..b2.len() - 2).unwrap_or(&[]));
    assert_eq!(New::take_body(&mut r).err().map(|e| e.why), Some(Why::CutShort));
    Ok(())
}

fn envelope(kind: u64, body: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(kind);
    w.bytes(body);
    w.buf
}

/// The hostile frames: each gets its documented refusal, and the next valid frame is read.
#[test]
fn bp_g2_s2_hostile_frames_are_refused_and_the_next_reads() -> R {
    let good = Frame::Idle(Idle { got: 7 });
    let gb = good.encode(VERSION);

    // An unknown kind: skipped by its length, counted, and the next frame read.
    let mut msg = envelope(4000, &[1, 2, 3]);
    msg.extend(&gb);
    let all = decode_message(&msg);
    assert_eq!(all.first().cloned().and_then(Result::err), Some(Fault { kind: Some(4000), why: Why::UnknownKind }));
    assert_eq!(all.get(1).cloned().and_then(Result::ok).map(|r| r.frame), Some(good.clone()));

    // A malformed body (a bool that isn't 0 or 1), then the next.
    let mut msg = envelope(u64::from(Kind::Fragment.index()), &[3, 9, 0]);
    msg.extend(&gb);
    let all = decode_message(&msg);
    assert_eq!(all.first().cloned().and_then(Result::err).map(|e| e.why), Some(Why::BadValue));
    assert_eq!(all.get(1).cloned().and_then(Result::ok).map(|r| r.frame), Some(good.clone()));

    // An unknown variant inside a known kind.
    let mut msg = envelope(u64::from(Kind::Answer.index()), &[1, 77]);
    msg.extend(&gb);
    let all = decode_message(&msg);
    assert_eq!(all.first().cloned().and_then(Result::err).map(|e| (e.why, e.why.code().name())), Some((Why::UnknownVariant, "unknown")));
    assert_eq!(all.get(1).cloned().and_then(Result::ok).map(|r| r.frame), Some(good.clone()));

    // Cut short inside a field.
    let mut msg = envelope(u64::from(Kind::Tail.index()), &[1, 9, 1]);
    msg.extend(&gb);
    let all = decode_message(&msg);
    assert_eq!(all.first().cloned().and_then(Result::err).map(|e| e.why), Some(Why::CutShort));
    assert_eq!(all.get(1).cloned().and_then(Result::ok).map(|r| r.frame), Some(good.clone()));

    // A length past the message: the rest of the message is refused; the next message reads.
    let mut msg = gb.clone();
    msg.truncate(msg.len() - 1);
    assert_eq!(decode_message(&msg).first().cloned().and_then(Result::err).map(|e| e.why), Some(Why::CutShort));
    assert_eq!(one(&gb).map(|r| r.frame), Ok(good.clone()));

    // An oversize length, up to u64::MAX: refused before anything is read or held.
    for claim in [MAX_FRAME as u64 + 1, u64::from(u32::MAX), u64::MAX] {
        let mut w = Writer::new();
        w.varint(u64::from(Kind::Tail.index()));
        w.varint(claim);
        assert_eq!(decode_message(&w.buf).first().cloned().and_then(Result::err).map(|e| e.why), Some(Why::TooLarge));
    }
    assert_eq!(one(&gb).map(|r| r.frame), Ok(good.clone()));

    // Bytes left over: after a body's known fields they're a newer build's, counted.
    let body = [7, 0xde, 0xad];
    let r = one(&envelope(u64::from(Kind::Idle.index()), &body)).map_err(|e| format!("{e:?}"))?;
    assert_eq!((r.frame, r.extra), (good.clone(), 2));
    // After the one frame a fragment holds, they're refused.
    let mut j = Joiner::new();
    let mut joined = gb.clone();
    joined.extend(&gb);
    let got = j.take(Fragment { lane: 1, last: true, data: Bytes(joined) });
    assert_eq!(got.and_then(Result::err).map(|e| e.why), Some(Why::LeftOver));

    // Nested depth: a fragment holding a fragment, or a compressed batch.
    let inner = Frame::Fragment(Fragment { lane: 1, last: true, data: Bytes(gb.clone()) }).encode(VERSION);
    let got = j.take(Fragment { lane: 1, last: true, data: Bytes(inner) });
    assert_eq!(got.and_then(Result::err).map(|e| e.why), Some(Why::TooDeep));
    let inner = Frame::Compressed(Compressed { ctx: 1, len: 1, data: Bytes(vec![1]) }).encode(VERSION);
    let got = j.take(Fragment { lane: 2, last: true, data: Bytes(inner) });
    assert_eq!(got.and_then(Result::err).map(|e| e.why), Some(Why::TooDeep));
    // A lane past the bound is let go.
    let mut small = Joiner::with_max(64);
    assert!(small.take(Fragment { lane: 1, last: false, data: Bytes(vec![0; 60]) }).is_none());
    assert_eq!(small.take(Fragment { lane: 1, last: false, data: Bytes(vec![0; 60]) }).and_then(Result::err).map(|e| e.why), Some(Why::TooLarge));
    assert_eq!(small.held(), 0);

    // And the next valid frame, cut and joined, reads.
    let big = Frame::Tail(Tail { sub: 1, data: Bytes(vec![b'x'; 5000]) });
    let pieces = cut(1, &big.encode(VERSION), 300);
    assert!(pieces.len() > 10);
    let mut out = None;
    for p in pieces {
        let b = p.encode(VERSION);
        assert!(b.len() <= 300, "a fragment of {} bytes", b.len());
        if let Ok(Read { frame: Frame::Fragment(f), .. }) = one(&b) {
            out = j.take(f);
        }
    }
    assert_eq!(out.and_then(Result::ok).map(|r| r.frame), Some(big));
    Ok(())
}

/// AC: no panic or abort over a million seeded inputs (random bytes, truncations, extensions and
/// bit flips of valid frames).
#[test]
fn bp_g2_s2_a_million_seeded_inputs_read_without_a_panic() {
    let n: u64 = std::env::var("KILN_BP_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let mut counts = [0u64; 5];
    let (mut read, mut refused) = (0u64, 0u64);
    for i in 0..n {
        let (made, msg) = fuzz::input(0x0B0B_5EED, i);
        if let Some(c) = counts.get_mut(made as usize) {
            *c += 1;
        }
        for r in decode_message(&msg) {
            match r {
                Ok(r) => {
                    read += 1;
                    let _ = r.frame.encode(VERSION);
                }
                Err(_) => refused += 1,
            }
        }
    }
    println!("{n} inputs (random, valid, truncated, extended, flipped: {counts:?}); {read} frames read, {refused} refused; 0 panics");
    assert!(counts.iter().all(|c| *c > 0));
    assert!(read > 0 && refused > 0);
}

#[test]
fn bp_g2_s2_the_schema_hash_is_stable_within_a_build() {
    assert_eq!(schema_hash(), schema_hash());
    assert!(schema_text().contains("kind Resize=23@1"));
    assert_eq!(fields_of(Kind::Change), "sub: u32, rev: u53, step: u53, delta: Delta");
    let _ = Bytes::arb(&mut Rng::new(1));
}
