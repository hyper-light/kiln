//! Frames of every kind, with values that reach each part of the encoding (empty and long,
//! multi-byte text, large integers, every variant of the enums that matter): what the round-trip
//! tests and the shared vectors are made of.

use crate::frames::*;
use crate::wire::{Bits, Bytes, MAX_SAFE};

fn topic(kind: u32, key: &str) -> Topic {
    Topic { kind, key: key.into() }
}

fn cursor(born: u64, rev: u64) -> Cursor {
    Cursor { born, rev }
}

/// One frame or more of each kind, every kind there.
pub fn frames() -> Vec<Frame> {
    vec![
        Frame::Open(Open {
            version: VERSION,
            oldest: OLDEST,
            caps: Bits(0b11),
            schema: Bits(schema_hash()),
            ticket: Bytes(vec![7; 32]),
            idle_ms: 15_000,
            window: 1 << 20,
            build: "0123abcd".into(),
        }),
        Frame::Accepted(Accepted { version: VERSION, caps: Bits(1), idle_ms: 15_000, born: 1_793_825_000_123, fragment: 1200, build: "0123abcd".into() }),
        Frame::Refused(Refused { code: OpenRefusal::TooOld, why: "this page is from an older build".into(), build: "beef".into() }),
        Frame::Refused(Refused { code: OpenRefusal::Reused, why: String::new(), build: String::new() }),
        Frame::Request(Request { id: 1, call: Call::Http { method: Method::Get, path: "/api/runs?limit=50".into(), body: Bytes::default() } }),
        Frame::Request(Request { id: MAX_SAFE, call: Call::Http { method: Method::Post, path: "/api/runs/r1/cancel".into(), body: Bytes::from("{}") } }),
        Frame::Request(Request { id: 300, call: Call::Command { session: "01a1a832".into(), command: Bytes::from("{\"type\":\"send\",\"text\":\"héllo 中 😀\"}") } }),
        Frame::Answer(Answer { id: 1, outcome: Outcome::Taken, at: None }),
        Frame::Answer(Answer { id: 2, outcome: Outcome::Accepted { epoch: 3, seq: 99, at: -1_793_825_000, repeat: true }, at: Some(Mark { topic: topic(10, "s1"), cursor: cursor(5, 6) }) }),
        Frame::Answer(Answer { id: 3, outcome: Outcome::Unchecked, at: None }),
        Frame::Answer(Answer { id: 4, outcome: Outcome::Pending, at: None }),
        Frame::Answer(Answer { id: 5, outcome: Outcome::Parked, at: None }),
        Frame::Answer(Answer { id: 6, outcome: Outcome::Nack { code: "unreachable".into(), why: "its runner is away".into(), again: true }, at: None }),
        Frame::Answer(Answer { id: 7, outcome: Outcome::Reply { status: 200, body: Bytes::from("[{\"id\":\"r1\"}]") }, at: None }),
        Frame::Subscribe(Subscribe { sub: 1, topic: topic(4, ""), cursor: None, credit: 65_536 }),
        Frame::Subscribe(Subscribe { sub: 2, topic: topic(11, "job-1/0"), cursor: Some(cursor(1_793_825_000_123, 1_000_000)), credit: 0 }),
        Frame::Unsubscribe(Unsubscribe { sub: 2 }),
        Frame::Snapshot(Begin { sub: 1, cursor: cursor(9, 0) }),
        Frame::Resumed(Begin { sub: 2, cursor: cursor(9, 41) }),
        Frame::Change(Change { sub: 1, rev: 42, step: 1, delta: Delta::Set { mask: Bits(0b101), fields: vec![Bytes(vec![1]), Bytes(vec![2])] } }),
        Frame::Change(Change { sub: 1, rev: 43, step: 1, delta: Delta::Put { key: Key::Str { s: "adas-mac-studio".into() }, fields: vec![Bytes(vec![3]), Bytes(vec![4, 5]), Bytes(vec![])] } }),
        Frame::Change(Change { sub: 1, rev: 44, step: 1, delta: Delta::Patch { key: Key::Ref { n: 0 }, mask: Bits(1 << 63), fields: vec![Bytes(vec![9])] } }),
        Frame::Change(Change { sub: 1, rev: 45, step: 0, delta: Delta::Remove { key: Key::Num { n: 17 } } }),
        Frame::Change(Change { sub: 2, rev: 46, step: 1, delta: Delta::Append { at: 4096, data: Bytes::from("line\n") } }),
        Frame::Change(Change { sub: 3, rev: 47, step: 1, delta: Delta::Extend { key: Key::Num { n: 12 }, field: 2, at: 10, data: Bytes::from("tok") } }),
        Frame::Tail(Tail { sub: 3, data: Bytes::from(" en") }),
        Frame::Ready(Begin { sub: 1, cursor: cursor(9, 47) }),
        Frame::End(End { sub: 2, code: EndCode::Denied, why: "not yours".into() }),
        Frame::Credit(Credit { sub: 0, upto: 1 << 30 }),
        Frame::Fragment(Fragment { lane: 3, last: false, data: Bytes(vec![9, 5, 1, 2, 3]) }),
        Frame::Idle(Idle { got: 123_456 }),
        Frame::Probe(Probe { nonce: u32::MAX }),
        Frame::ProbeAck(ProbeAck { nonce: 1, got: 0 }),
        Frame::Unread(Unread { kind: 99, code: UnreadCode::Unknown, why: UnreadWhy::UnknownKind }),
        Frame::Compressed(Compressed { ctx: 2, len: 4, data: Bytes(vec![0x2a, 0x49, 0x2d, 0x2e, 0x01, 0x00]) }),
        Frame::Close(Close { code: CloseCode::Expired, why: "the session ended".into(), build: String::new() }),
        Frame::Input(Input { sub: 5, data: Bytes::from("ls -la\r") }),
        Frame::SecretAnswer(Answer { id: 8, outcome: Outcome::Reply { status: 201, body: Bytes::from("{\"token\":\"…\"}") }, at: None }),
        Frame::Resize(Resize { sub: 5, cols: 120, rows: 40 }),
    ]
}
