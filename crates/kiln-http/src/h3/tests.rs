//! The HTTP/3 door's connection core against frames written by hand, as kiln-http's h1 and h2
//! tests write theirs (goal 4 steps 3 and 4, plan 01a1a832eb597110a6dcfda227981604). Each test
//! names the RFC 9114, 9204, 9220, 9297 or WebTransport draft section it holds the code to.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use hyper_measure::alloc;

use super::capsule;
use super::conn::{Admission, Config, Connect, Connection, Event, Read, Transport};
use super::qpack::Encoder;
use super::{Draft, code, frame, put_frame, setting, stream_type, varint};

// ------------------------------------------------------------------------------- the mock

#[derive(Default, Debug)]
pub(crate) struct In {
    pub chunks: VecDeque<Bytes>,
    pub fin: bool,
    pub reset: Option<u64>,
    /// Bytes the connection read.
    pub read: usize,
}

#[derive(Default, Debug)]
pub(crate) struct Out {
    pub bytes: Vec<u8>,
    pub fin: bool,
    pub reset: Option<u64>,
    /// Credit left: `None` is unlimited.
    pub credit: Option<usize>,
}

/// QUIC as the core sees it, in memory: what the peer sent on each stream, what the core wrote.
#[derive(Default, Debug)]
pub(crate) struct Mock {
    pub inbound: BTreeMap<u64, In>,
    pub out: BTreeMap<u64, Out>,
    pub stops: Vec<(u64, u64)>,
    pub opened: Vec<u64>,
    next_uni: u64,
    next_bidi: u64,
    pub closed: Option<(u64, Vec<u8>)>,
    pub dgram_in: VecDeque<Bytes>,
    pub dgram_out: Vec<Bytes>,
    /// Credit the next opened stream starts with.
    pub open_credit: Option<usize>,
}

impl Mock {
    pub(crate) fn new() -> Mock {
        Mock { next_uni: 3, next_bidi: 1, ..Mock::default() }
    }

    pub(crate) fn push(&mut self, id: u64, bytes: &[u8]) {
        self.inbound.entry(id).or_default().chunks.push_back(Bytes::copy_from_slice(bytes));
    }

    pub(crate) fn push_static(&mut self, id: u64, bytes: &'static [u8]) {
        self.inbound.entry(id).or_default().chunks.push_back(Bytes::from_static(bytes));
    }

    pub(crate) fn fin(&mut self, id: u64) {
        self.inbound.entry(id).or_default().fin = true;
    }

    pub(crate) fn written(&self, id: u64) -> &[u8] {
        self.out.get(&id).map_or(&[][..], |o| o.bytes.as_slice())
    }

    pub(crate) fn reset_code(&self, id: u64) -> Option<u64> {
        self.out.get(&id).and_then(|o| o.reset)
    }

    pub(crate) fn stopped(&self, id: u64) -> Option<u64> {
        self.stops.iter().find(|(s, _)| *s == id).map(|(_, c)| *c)
    }

    pub(crate) fn read_of(&self, id: u64) -> usize {
        self.inbound.get(&id).map_or(0, |i| i.read)
    }
}

impl Transport for Mock {
    fn read(&mut self, id: u64, max: usize) -> Read {
        let Some(s) = self.inbound.get_mut(&id) else { return Read::Gone };
        if let Some(c) = s.reset {
            return Read::Reset(c);
        }
        let Some(front) = s.chunks.front_mut() else { return if s.fin { Read::Fin } else { Read::Blocked } };
        let n = max.min(front.len());
        let got = front.split_to(n);
        if front.is_empty() {
            s.chunks.pop_front();
        }
        s.read += got.len();
        if got.is_empty() {
            return Read::Blocked;
        }
        Read::Data(got)
    }
    fn write(&mut self, id: u64, data: &[u8]) -> Option<usize> {
        let o = self.out.entry(id).or_default();
        if o.reset.is_some() || o.fin {
            return None;
        }
        let n = o.credit.map_or(data.len(), |c| c.min(data.len()));
        if let Some(c) = &mut o.credit {
            *c -= n;
        }
        o.bytes.extend_from_slice(data.get(..n).unwrap_or_default());
        Some(n)
    }
    fn finish(&mut self, id: u64) {
        self.out.entry(id).or_default().fin = true;
    }
    fn reset(&mut self, id: u64, code: u64) {
        self.out.entry(id).or_default().reset = Some(code);
    }
    fn stop(&mut self, id: u64, code: u64) {
        self.stops.push((id, code));
    }
    fn open(&mut self, bidi: bool) -> Option<u64> {
        let id = if bidi { &mut self.next_bidi } else { &mut self.next_uni };
        let got = *id;
        *id += 4;
        self.opened.push(got);
        self.out.entry(got).or_default().credit = self.open_credit;
        Some(got)
    }
    fn send_datagram(&mut self, data: Bytes) -> bool {
        self.dgram_out.push(data);
        true
    }
    fn recv_datagram(&mut self) -> Option<Bytes> {
        self.dgram_in.pop_front()
    }
    fn max_datagram(&self) -> Option<usize> {
        Some(1200)
    }
    fn close(&mut self, code: u64, reason: &[u8]) {
        self.closed = Some((code, reason.to_vec()));
    }
}

// ------------------------------------------------------------------------------- frames by hand

pub(crate) fn v(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    varint::put(&mut out, n);
    out
}

pub(crate) fn settings_frame(pairs: &[(u64, u64)]) -> Vec<u8> {
    let mut p = Vec::new();
    for &(id, val) in pairs {
        varint::put(&mut p, id);
        varint::put(&mut p, val);
    }
    let mut out = Vec::new();
    put_frame(&mut out, frame::SETTINGS, &p);
    out
}

/// A control stream's first bytes: its type, then SETTINGS.
pub(crate) fn control(pairs: &[(u64, u64)]) -> Vec<u8> {
    let mut out = v(stream_type::CONTROL);
    out.extend(settings_frame(pairs));
    out
}

/// What Chrome's and Firefox's SETTINGS say (draft-02), with a GREASE setting.
pub(crate) const CHROME: &[(u64, u64)] = &[(setting::QPACK_MAX_TABLE_CAPACITY, 0), (setting::QPACK_BLOCKED_STREAMS, 0), (setting::H3_DATAGRAM, 1), (setting::H3_DATAGRAM_DRAFT04, 1), (setting::ENABLE_WEBTRANSPORT, 1), (0x1f * 7 + 0x21, 99)];
/// What Safari's say (drafts 13-14).
pub(crate) const SAFARI: &[(u64, u64)] = &[(setting::H3_DATAGRAM, 1), (setting::WT_MAX_SESSIONS, 1), (setting::ENABLE_CONNECT_PROTOCOL, 1)];

pub(crate) fn headers(fields: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut block = Vec::new();
    Encoder::begin(&mut block);
    for (n, val) in fields {
        Encoder::field(&mut block, n, val);
    }
    let mut out = Vec::new();
    put_frame(&mut out, frame::HEADERS, &block);
    out
}

pub(crate) fn wt_connect(path: &[u8]) -> Vec<u8> {
    headers(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"https"), (b":authority", b"kiln.test:7443"), (b":path", path), (b"origin", b"https://kiln.test"), (b"sec-webtransport-http3-draft02", b"1")])
}

/// Decodes the HEADERS frame at the start of `bytes` (as the browser would): its fields.
pub(crate) fn response(bytes: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let (t, n) = varint::decode(bytes)?;
    if t != frame::HEADERS {
        return None;
    }
    let (len, m) = varint::decode(bytes.get(n..)?)?;
    let block = bytes.get(n + m..n + m + len as usize)?;
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    super::qpack::decode(block, super::DOOR_BUDGET, &mut scratch, |a, b| {
        out.push((a.to_vec(), b.to_vec()));
        Ok::<(), ()>(())
    })
    .ok()?;
    Some(out)
}

pub(crate) fn status(bytes: &[u8]) -> Option<Vec<u8>> {
    response(bytes)?.into_iter().find(|(n, _)| n == b":status").map(|(_, v)| v)
}

/// A connection, a mock, and an admit that counts what reaches it.
pub(crate) struct Rig {
    pub c: Connection,
    pub t: Mock,
    pub asked: Vec<Connect>,
    pub answer: Admission,
    pub now: u64,
}

impl Rig {
    pub(crate) fn new() -> Rig {
        let mut r = Rig { c: Connection::new(Config::door()), t: Mock::new(), asked: Vec::new(), answer: Admission::Accept, now: 0 };
        r.c.start(&mut r.t);
        r
    }

    pub(crate) fn send(&mut self, id: u64, bytes: &[u8]) {
        self.t.push(id, bytes);
        self.readable(id);
    }

    pub(crate) fn readable(&mut self, id: u64) {
        let answer = self.answer;
        let asked = &mut self.asked;
        let mut admit = |c: &Connect| {
            asked.push(c.clone());
            answer
        };
        self.c.readable(&mut self.t, &mut admit, id);
    }

    pub(crate) fn reset(&mut self, id: u64, code: u64) {
        self.t.inbound.entry(id).or_default().reset = Some(code);
        let mut admit = |_: &Connect| Admission::Forbidden;
        self.c.reset_by_peer(&mut self.t, &mut admit, id, code, self.now);
    }

    pub(crate) fn events(&mut self) -> Vec<Event> {
        std::iter::from_fn(|| self.c.poll_event()).collect()
    }

    /// Client control stream (2) with `settings`, then a CONNECT on stream 0.
    pub(crate) fn open_session(&mut self, settings: &[(u64, u64)]) -> Vec<Event> {
        self.send(2, &control(settings));
        self.send(0, &wt_connect(b"/wt?ticket=abc"));
        self.events()
    }

    pub(crate) fn closed(&self) -> Option<u64> {
        self.t.closed.as_ref().map(|(c, _)| *c)
    }
}

fn parse_settings(bytes: &[u8]) -> Option<Vec<(u64, u64)>> {
    let (t, n) = varint::decode(bytes)?;
    if t != stream_type::CONTROL {
        return None;
    }
    let rest = bytes.get(n..)?;
    let (f, n) = varint::decode(rest)?;
    if f != frame::SETTINGS {
        return None;
    }
    let (len, m) = varint::decode(rest.get(n..)?)?;
    let mut p = rest.get(n + m..n + m + len as usize)?;
    let mut out = Vec::new();
    while !p.is_empty() {
        let (id, a) = varint::decode(p)?;
        let (val, b) = varint::decode(p.get(a..)?)?;
        out.push((id, val));
        p = p.get(a + b..)?;
    }
    Some(out)
}

// ------------------------------------------------------------------------------- step 3

#[test]
fn rfc9114_6_2_1_the_server_sends_one_control_stream_with_every_dialect() {
    let r = Rig::new();
    assert_eq!(r.t.opened, vec![3]);
    let s = parse_settings(r.t.written(3)).unwrap_or_default();
    for want in [
        (setting::QPACK_MAX_TABLE_CAPACITY, 0),
        (setting::QPACK_BLOCKED_STREAMS, 0),
        (setting::MAX_FIELD_SECTION_SIZE, super::DOOR_BUDGET.bytes as u64),
        (setting::ENABLE_CONNECT_PROTOCOL, 1),
        (setting::H3_DATAGRAM, 1),
        (setting::H3_DATAGRAM_DRAFT04, 1),
        (setting::ENABLE_WEBTRANSPORT, 1),
        (setting::WEBTRANSPORT_MAX_SESSIONS_DRAFT07, 1),
        (setting::WT_MAX_SESSIONS, 1),
    ] {
        assert!(s.contains(&want), "{want:x?} missing from {s:x?}");
    }
    // The door opens no QPACK encoder or decoder stream (RFC 9204 §4.2, at capacity 0).
    assert_eq!(r.t.out.len(), 1);
}

#[test]
fn draft02_chrome_and_firefox_are_admitted_and_answered_in_kind() {
    let mut r = Rig::new();
    let ev = r.open_session(CHROME);
    assert_eq!(r.asked.len(), 1);
    let c = r.asked.first().cloned();
    assert_eq!(c.as_ref().map(|c| c.draft), Some(Draft::Draft02));
    assert_eq!(c.as_ref().map(|c| c.path.clone()), Some(b"/wt?ticket=abc".to_vec()));
    assert_eq!(c.as_ref().and_then(|c| c.origin.clone()), Some(b"https://kiln.test".to_vec()));
    assert!(matches!(ev.first(), Some(Event::Accepted(_))));
    let resp = response(r.t.written(0)).unwrap_or_default();
    assert!(resp.contains(&(b":status".to_vec(), b"200".to_vec())));
    assert!(resp.contains(&(b"sec-webtransport-http3-draft".to_vec(), b"draft02".to_vec())));
    assert_eq!(r.c.session().map(|s| s.stream), Some(0));
}

#[test]
fn draft14_3_1_safari_is_admitted_under_its_dialect() {
    let mut r = Rig::new();
    r.send(2, &control(SAFARI));
    r.send(0, &headers(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"https"), (b":authority", b"kiln.test"), (b":path", b"/wt"), (b"origin", b"https://kiln.test")]));
    assert_eq!(r.asked.first().map(|c| c.draft), Some(Draft::Draft14));
    let resp = response(r.t.written(0)).unwrap_or_default();
    assert_eq!(resp, vec![(b":status".to_vec(), b"200".to_vec())]);
}

#[test]
fn draft14_3_1_no_connect_is_processed_before_the_client_settings() {
    let mut r = Rig::new();
    r.send(0, &wt_connect(b"/wt"));
    assert!(r.asked.is_empty());
    assert_eq!(r.t.read_of(0), 0, "the request waits unread");
    assert!(r.events().is_empty());
    r.send(2, &control(CHROME));
    assert_eq!(r.asked.len(), 1);
    assert_eq!(status(r.t.written(0)), Some(b"200".to_vec()));
}

#[test]
fn rfc9114_4_4_other_methods_are_reset_with_h3_request_rejected() {
    for method in [&b"GET"[..], b"POST"] {
        let mut r = Rig::new();
        r.send(2, &control(CHROME));
        r.send(0, &headers(&[(b":method", method), (b":scheme", b"https"), (b":authority", b"kiln.test"), (b":path", b"/")]));
        assert_eq!(r.t.reset_code(0), Some(code::H3_REQUEST_REJECTED));
        assert_eq!(r.t.stopped(0), Some(code::H3_REQUEST_REJECTED));
        assert!(r.asked.is_empty());
        assert_eq!(r.closed(), None, "the connection stays");
        // And the connection still admits a CONNECT after.
        r.send(4, &wt_connect(b"/wt"));
        assert_eq!(r.asked.len(), 1);
    }
}

#[test]
fn rfc9220_3_connect_without_protocol_is_rejected_and_websocket_gets_501() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.send(0, &headers(&[(b":method", b"CONNECT"), (b":authority", b"kiln.test:443")]));
    assert_eq!(r.t.reset_code(0), Some(code::H3_REQUEST_REJECTED));
    r.send(4, &headers(&[(b":method", b"CONNECT"), (b":protocol", b"websocket"), (b":scheme", b"https"), (b":authority", b"kiln.test"), (b":path", b"/ws")]));
    assert_eq!(status(r.t.written(4)), Some(b"501".to_vec()));
    assert!(r.t.out.get(&4).is_some_and(|o| o.fin));
    assert!(r.asked.is_empty());
    assert_eq!(r.closed(), None);
}

#[test]
fn draft14_5_1_a_second_connect_is_rejected_unread() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.send(4, &wt_connect(b"/again"));
    assert_eq!(r.t.reset_code(4), Some(code::H3_REQUEST_REJECTED));
    assert_eq!(r.asked.len(), 1);
    // Only its first frame's type was read.
    assert_eq!(r.t.read_of(4), 1);
}

#[test]
fn draft14_3_2_admit_answers_403_and_404() {
    for (answer, want) in [(Admission::Forbidden, &b"403"[..]), (Admission::NotFound, b"404")] {
        let mut r = Rig::new();
        r.answer = answer;
        r.open_session(CHROME);
        assert_eq!(status(r.t.written(0)).as_deref(), Some(want));
        assert!(r.c.session().is_none());
        assert_eq!(r.t.out.get(&0).map(|o| o.fin), Some(true));
    }
}

#[test]
fn rfc9114_4_1_2_malformed_requests_are_h3_message_error() {
    let cases: [Vec<u8>; 5] = [
        headers(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"http"), (b":authority", b"a"), (b":path", b"/")]),
        headers(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"https"), (b":path", b"/")]),
        headers(&[(b":method", b"CONNECT"), (b"origin", b"x"), (b":protocol", b"webtransport")]),
        headers(&[(b":method", b"CONNECT"), (b"Origin", b"x")]),
        headers(&[(b":method", b"CONNECT"), (b":method", b"CONNECT")]),
    ];
    for c in cases {
        let mut r = Rig::new();
        r.send(2, &control(CHROME));
        r.send(0, &c);
        assert_eq!(r.t.reset_code(0), Some(code::H3_MESSAGE_ERROR));
        assert!(r.asked.is_empty());
    }
}

#[test]
fn draft14_3_1_a_client_whose_settings_lack_webtransport_is_not_admitted() {
    let mut r = Rig::new();
    r.send(2, &control(&[(setting::QPACK_MAX_TABLE_CAPACITY, 0)]));
    r.send(0, &wt_connect(b"/wt"));
    assert_eq!(r.t.reset_code(0), Some(code::H3_MESSAGE_ERROR));
    assert!(r.asked.is_empty());
}

#[test]
fn rfc9114_6_2_2_a_push_stream_from_the_client_is_a_connection_error() {
    let mut r = Rig::new();
    r.send(2, &v(stream_type::PUSH));
    assert_eq!(r.closed(), Some(code::H3_STREAM_CREATION_ERROR));
}

#[test]
fn rfc9114_6_2_1_a_second_control_stream_is_a_connection_error() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.send(6, &control(CHROME));
    assert_eq!(r.closed(), Some(code::H3_STREAM_CREATION_ERROR));
}

#[test]
fn rfc9204_4_2_cve_2026_42530_a_reopened_encoder_stream_fails_before_it_is_kept() {
    // nginx's shape: open an encoder stream, close it, open another.
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.send(6, &v(stream_type::ENCODER));
    r.t.fin(6);
    r.readable(6);
    assert_eq!(r.closed(), Some(code::H3_CLOSED_CRITICAL_STREAM));
    // Without the close: the second is H3_STREAM_CREATION_ERROR, and none of its bytes are read
    // past its type.
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.send(6, &v(stream_type::ENCODER));
    r.send(10, &[stream_type::ENCODER as u8, 0x3f, 0xe1, 0x1f]);
    assert_eq!(r.closed(), Some(code::H3_STREAM_CREATION_ERROR));
    assert_eq!(r.t.read_of(10), 1);
    // And a duplicate decoder stream.
    let mut r = Rig::new();
    r.send(6, &v(stream_type::DECODER));
    r.send(10, &v(stream_type::DECODER));
    assert_eq!(r.closed(), Some(code::H3_STREAM_CREATION_ERROR));
}

#[test]
fn rfc9204_4_3_an_encoder_stream_insert_is_a_connection_error() {
    let mut r = Rig::new();
    r.send(6, &[stream_type::ENCODER as u8, 0x20, 0x20]);
    assert_eq!(r.closed(), None, "Set Dynamic Table Capacity 0 is all it may say");
    r.send(6, &[0xc0 | 17, 0x01, b'x']);
    assert_eq!(r.closed(), Some(super::qpack::code::ENCODER_STREAM_ERROR));
}

#[test]
fn rfc9204_4_4_a_section_acknowledgment_is_a_connection_error() {
    let mut r = Rig::new();
    r.send(6, &[stream_type::DECODER as u8, 0x40]);
    assert_eq!(r.closed(), None, "Stream Cancellation is dropped");
    r.send(6, &[0x80]);
    assert_eq!(r.closed(), Some(super::qpack::code::DECODER_STREAM_ERROR));
}

#[test]
fn rfc9114_6_2_unknown_stream_types_are_stopped_and_forgotten() {
    let mut r = Rig::new();
    let grease = 0x1f * 3 + 0x21;
    let mut b = v(grease);
    b.extend_from_slice(&[0u8; 100]);
    r.send(2, &b);
    assert_eq!(r.t.stopped(2), Some(code::H3_STREAM_CREATION_ERROR));
    assert_eq!(r.t.read_of(2), varint::len(grease));
    assert_eq!(r.closed(), None);
    assert!(r.c.held() < 512, "it holds nothing of it: {}", r.c.held());
}

#[test]
fn rfc9114_7_2_4_settings_rules() {
    // Unknown settings (GREASE) are ignored.
    let mut r = Rig::new();
    r.send(2, &control(&[(0x21, 7), (0x1f * 1000 + 0x21, 1), (setting::ENABLE_WEBTRANSPORT, 1), (setting::H3_DATAGRAM, 1)]));
    assert_eq!(r.closed(), None);
    assert_eq!(r.c.peer_settings().and_then(|p| p.draft()), Some(Draft::Draft02));
    // HTTP/2's reserved ids and a duplicate are H3_SETTINGS_ERROR.
    for bad in [&[(0x02u64, 0u64)][..], &[(0x05, 1)], &[(setting::H3_DATAGRAM, 1), (setting::H3_DATAGRAM, 1)]] {
        let mut r = Rig::new();
        r.send(2, &control(bad));
        assert_eq!(r.closed(), Some(code::H3_SETTINGS_ERROR), "{bad:x?}");
    }
}

#[test]
fn rfc9114_6_2_1_the_control_stream_starts_with_settings_once() {
    let mut r = Rig::new();
    let mut b = v(stream_type::CONTROL);
    put_frame(&mut b, frame::GOAWAY, &v(0));
    r.send(2, &b);
    assert_eq!(r.closed(), Some(code::H3_MISSING_SETTINGS));
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.send(2, &settings_frame(CHROME));
    assert_eq!(r.closed(), Some(code::H3_FRAME_UNEXPECTED));
    for bad in [frame::DATA, frame::HEADERS, 0x02, 0x06, 0x08, 0x09] {
        let mut r = Rig::new();
        r.send(2, &control(CHROME));
        let mut f = Vec::new();
        put_frame(&mut f, bad, &[0]);
        r.send(2, &f);
        assert_eq!(r.closed(), Some(code::H3_FRAME_UNEXPECTED), "{bad:x}");
    }
}

#[test]
fn rfc9114_6_2_1_a_closed_critical_stream_is_a_connection_error() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.t.fin(2);
    r.readable(2);
    assert_eq!(r.closed(), Some(code::H3_CLOSED_CRITICAL_STREAM));
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    r.reset(2, 0);
    assert_eq!(r.closed(), Some(code::H3_CLOSED_CRITICAL_STREAM));
}

static ZEROS: [u8; 1 << 16] = [0; 1 << 16];

#[test]
fn rfc9114_9_unknown_frames_are_skipped_by_length_without_buffering() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    // A 1 MiB unknown frame on the control stream, then on the request stream before HEADERS.
    let grease = 0x1f * 5 + 0x21;
    let mut head = Vec::new();
    varint::put(&mut head, grease);
    varint::put(&mut head, 1 << 20);
    r.send(2, &head);
    for _ in 0..16 {
        r.t.push_static(2, &ZEROS);
        r.readable(2);
        assert!(r.c.held() < 1024, "{}", r.c.held());
    }
    r.send(0, &head);
    for _ in 0..16 {
        r.t.push_static(0, &ZEROS);
        r.readable(0);
        assert!(r.c.held() < 1024, "{}", r.c.held());
    }
    r.send(0, &wt_connect(b"/wt"));
    assert_eq!(r.asked.len(), 1);
    assert_eq!(r.closed(), None);
}

#[test]
fn rfc9114_4_2_2_a_headers_frame_past_the_budget_is_refused_before_it_is_read() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    let mut f = Vec::new();
    varint::put(&mut f, frame::HEADERS);
    varint::put(&mut f, super::DOOR_BUDGET.bytes as u64 + 1);
    let len = f.len();
    f.extend(vec![0u8; 64]);
    r.send(0, &f);
    assert_eq!(status(r.t.written(0)), Some(b"431".to_vec()));
    assert_eq!(r.t.read_of(0), len, "none of its payload was read");
    assert!(r.asked.is_empty());
}

#[test]
fn rfc9114_5_2_shutdown_sends_goaway_and_drain() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.c.drain(&mut r.t, true);
    let ctl = r.t.written(3);
    let mut goaway = Vec::new();
    put_frame(&mut goaway, frame::GOAWAY, &v(4));
    assert!(ctl.ends_with(&goaway), "{ctl:02x?}");
    // The CONNECT stream carries WT_DRAIN_SESSION in a DATA frame.
    let mut drain = Vec::new();
    put_frame(&mut drain, frame::DATA, &capsule::drain());
    assert!(r.t.written(0).ends_with(&drain));
}

#[test]
fn cve_2023_44487_resets_past_twice_the_stream_limit_in_a_second_end_the_connection() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    let limit = Config::door().resets_per_second;
    // Spread over seconds: fine.
    for i in 0..u64::from(limit) * 3 {
        r.now = i * 20_000_000;
        r.reset(4 * (i + 1), 0x10c);
    }
    assert_eq!(r.closed(), None);
    // In one second: the connection ends.
    for i in 0..=u64::from(limit) {
        r.reset(4 * (1000 + i), 0x10c);
    }
    assert_eq!(r.closed(), Some(code::H3_EXCESSIVE_LOAD));
}

#[test]
fn no_refused_request_reaches_admit() {
    // Every refusal above, counted: what reaches the admit function is only a WebTransport
    // CONNECT the dialect and the fields allow.
    let mut r = Rig::new();
    r.send(0, &headers(&[(b":method", b"GET"), (b":scheme", b"https"), (b":authority", b"a"), (b":path", b"/")]));
    r.send(2, &control(CHROME));
    r.send(4, &headers(&[(b":method", b"POST"), (b":scheme", b"https"), (b":authority", b"a"), (b":path", b"/")]));
    r.send(8, &headers(&[(b":method", b"CONNECT"), (b":authority", b"a:1")]));
    r.send(12, &headers(&[(b":method", b"CONNECT"), (b":protocol", b"websocket"), (b":scheme", b"https"), (b":authority", b"a"), (b":path", b"/")]));
    r.send(16, &headers(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"http"), (b":authority", b"a"), (b":path", b"/")]));
    assert_eq!(r.asked.len(), 0);
    assert_eq!(r.c.stats.refused, 5);
    r.send(20, &wt_connect(b"/wt"));
    r.send(24, &wt_connect(b"/wt"));
    assert_eq!(r.asked.len(), 1);
}

// ------------------------------------------------------------------------------- step 4

/// A DATA frame carrying `capsules`.
fn data(capsules: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_frame(&mut out, frame::DATA, capsules);
    out
}

#[test]
fn draft14_4_streams_of_the_session_reach_the_application() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    // A uni stream and a bidi stream naming session 0.
    let mut uni = v(stream_type::WEBTRANSPORT);
    uni.extend(v(0));
    uni.extend_from_slice(b"hello-uni");
    r.send(6, &uni);
    let mut bidi = v(frame::WT_STREAM);
    bidi.extend(v(0));
    bidi.extend_from_slice(b"hello-bidi");
    r.send(4, &bidi);
    let ev = r.events();
    assert!(ev.contains(&Event::Stream { id: 6, bidi: false }), "{ev:?}");
    assert!(ev.contains(&Event::Stream { id: 4, bidi: true }), "{ev:?}");
    assert_eq!(r.c.wt_read(&mut r.t, 6, 100), Read::Data(Bytes::from_static(b"hello-uni")));
    assert_eq!(r.c.wt_read(&mut r.t, 4, 100), Read::Data(Bytes::from_static(b"hello-bidi")));
    // Our streams carry their header first.
    let ours = r.c.wt_open(&mut r.t, true);
    assert_eq!(ours, Some(1));
    assert_eq!(r.c.wt_write(&mut r.t, 1, b"back"), Some(4));
    let mut want = v(frame::WT_STREAM);
    want.extend(v(0));
    want.extend_from_slice(b"back");
    assert_eq!(r.t.written(1), want.as_slice());
    let uni = r.c.wt_open(&mut r.t, false);
    assert_eq!(uni, Some(7));
    assert_eq!(r.t.written(7), [&v(stream_type::WEBTRANSPORT)[..], &v(0)[..]].concat().as_slice());
}

#[test]
fn draft14_4_3_our_stream_header_waits_for_credit() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.events();
    r.t.open_credit = Some(0);
    let id = r.c.wt_open(&mut r.t, true).unwrap_or_default();
    assert_eq!(r.c.wt_write(&mut r.t, id, b"x"), Some(0));
    if let Some(o) = r.t.out.get_mut(&id) {
        o.credit = Some(100);
    }
    r.c.writable(&mut r.t, id);
    assert!(r.events().contains(&Event::Writable(id)));
    assert_eq!(r.c.wt_write(&mut r.t, id, b"x"), Some(1));
    assert_eq!(r.t.written(id), &[0x40, 0x41, 0x00, b'x']);
}

#[test]
fn draft14_4_6_a_pre_session_stream_flood_is_held_unread_up_to_the_bound() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    // 1,000 WebTransport uni streams naming session 0 before its CONNECT.
    let mut hdr = v(stream_type::WEBTRANSPORT);
    hdr.extend(v(0));
    let header = hdr.len();
    hdr.extend_from_slice(&[7u8; 1000]);
    for i in 0..1000u64 {
        r.send(6 + 4 * i, &hdr);
    }
    let held = Config::door().pending_streams;
    let refused = (0..1000u64).filter(|i| r.t.stopped(6 + 4 * i) == Some(code::WT_BUFFERED_STREAM_REJECTED)).count();
    assert_eq!(refused, 1000 - held);
    assert_eq!(r.c.stats.buffered_rejected, (1000 - held) as u64);
    // None of their bytes were read past the header.
    assert!((0..1000u64).all(|i| r.t.read_of(6 + 4 * i) <= header));
    assert!(r.c.held() < 2048, "{}", r.c.held());
    // The held ones join the session once it's admitted.
    r.send(0, &wt_connect(b"/wt"));
    let joined = r.events().iter().filter(|e| matches!(e, Event::Stream { .. })).count();
    assert_eq!(joined, held);
}

#[test]
fn draft14_4_a_session_id_that_cannot_be_one_is_h3_id_error() {
    let mut r = Rig::new();
    r.send(2, &control(CHROME));
    let mut hdr = v(stream_type::WEBTRANSPORT);
    hdr.extend(v(2));
    r.send(6, &hdr);
    assert_eq!(r.closed(), Some(code::H3_ID_ERROR));
}

#[test]
fn draft14_4_6_streams_for_a_refused_session_are_gone() {
    let mut r = Rig::new();
    r.answer = Admission::Forbidden;
    r.send(2, &control(CHROME));
    let mut hdr = v(stream_type::WEBTRANSPORT);
    hdr.extend(v(0));
    r.send(6, &hdr);
    r.send(0, &wt_connect(b"/wt"));
    assert_eq!(r.t.stopped(6), Some(code::WT_SESSION_GONE));
    r.send(10, &hdr);
    assert_eq!(r.t.stopped(10), Some(code::WT_SESSION_GONE));
}

#[test]
fn rfc9297_2_datagrams_carry_the_quarter_stream_id_and_others_are_dropped() {
    let mut r = Rig::new();
    // Before any session: dropped.
    r.t.dgram_in.push_back(Bytes::from_static(&[0x00, b'x']));
    assert_eq!(r.c.recv_datagram(&mut r.t), None);
    r.open_session(CHROME);
    r.t.dgram_in.push_back(Bytes::from_static(&[0x01, b'y'])); // session 4: not ours
    r.t.dgram_in.push_back(Bytes::from_static(&[0x40])); // a truncated integer
    r.t.dgram_in.push_back(Bytes::from_static(&[0x00, b'h', b'i']));
    assert_eq!(r.c.recv_datagram(&mut r.t), Some(Bytes::from_static(b"hi")));
    assert_eq!(r.c.stats.datagrams_dropped, 3);
    assert!(r.c.send_datagram(&mut r.t, b"yo"));
    assert_eq!(r.t.dgram_out.last().map(|b| b.to_vec()), Some(vec![0x00, b'y', b'o']));
    assert_eq!(r.c.max_datagram(&r.t), Some(1199));
}

#[test]
fn rfc9297_3_5_datagram_capsules_are_bounded_and_oversized_ones_discarded() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    let mut caps = Vec::new();
    for _ in 0..100 {
        varint::put(&mut caps, capsule::kind::DATAGRAM);
        varint::put(&mut caps, 3);
        caps.extend_from_slice(b"abc");
    }
    varint::put(&mut caps, capsule::kind::DATAGRAM);
    varint::put(&mut caps, 5000);
    caps.extend(vec![0u8; 5000]);
    r.send(0, &data(&caps));
    let mut got = 0;
    while r.c.recv_datagram(&mut r.t).is_some() {
        got += 1;
    }
    assert!(got <= 4, "{got}");
    assert!(r.c.held() < 4096, "{}", r.c.held());
    assert_eq!(r.closed(), None);
}

#[test]
fn draft14_6_the_peers_close_ends_the_session_and_its_streams() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    let mut bidi = v(frame::WT_STREAM);
    bidi.extend(v(0));
    r.send(4, &bidi);
    r.events();
    r.send(0, &data(&capsule::close(7, b"bye")));
    let ev = r.events();
    assert!(ev.contains(&Event::Closed { code: 7, message: b"bye".to_vec() }), "{ev:?}");
    assert_eq!(r.t.reset_code(4), Some(code::WT_SESSION_GONE));
    assert_eq!(r.t.out.get(&0).map(|o| o.fin), Some(true));
    // Then its FIN and a STOP_SENDING (Safari 26.4 sends both): the session ends once.
    r.t.fin(0);
    r.readable(0);
    r.c.stopped_by_peer(&mut r.t, 0, 0);
    assert!(!r.events().iter().any(|e| matches!(e, Event::Closed { .. })));
    // A close message over 1024 bytes is malformed.
    let mut r = Rig::new();
    r.open_session(CHROME);
    let mut big = Vec::new();
    varint::put(&mut big, capsule::kind::CLOSE_SESSION);
    varint::put(&mut big, 4 + 1025);
    r.send(0, &data(&big));
    assert_eq!(r.t.reset_code(0), Some(code::H3_MESSAGE_ERROR));
}

#[test]
fn draft14_4_7_a_drain_capsule_is_told_to_the_application() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.send(0, &data(&capsule::drain()));
    assert!(r.events().contains(&Event::Draining));
}

#[test]
fn cve_2026_21435_a_close_never_waits_on_credit() {
    // The peer has granted no more credit on the CONNECT stream: the close resets it in the same
    // call, and nothing is left owed (there is no timer to arm: the core has no clock).
    let mut r = Rig::new();
    r.open_session(CHROME);
    if let Some(o) = r.t.out.get_mut(&0) {
        o.credit = Some(0);
    }
    r.c.close(&mut r.t, 9, b"done");
    assert_eq!(r.t.reset_code(0), Some(code::WT_SESSION_GONE));
    assert!(r.events().contains(&Event::Closed { code: 9, message: b"done".to_vec() }));
    assert!(r.c.held() < 1024, "{}", r.c.held());
    // With credit, the capsule goes out and the stream ends.
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.c.close(&mut r.t, 9, b"done");
    assert!(r.t.written(0).ends_with(&data(&capsule::close(9, b"done"))));
    assert_eq!(r.t.out.get(&0).map(|o| (o.fin, o.reset)), Some((true, None)));
}

#[test]
fn cve_2026_57497_a_huge_unknown_capsule_is_skipped_holding_only_its_header() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    r.events();
    // One DATA frame as long as QUIC allows, carrying one unknown capsule declaring 2^62 - 1
    // bytes, then 100 MiB of it.
    let mut head = Vec::new();
    varint::put(&mut head, frame::DATA);
    varint::put(&mut head, varint::MAX);
    varint::put(&mut head, 0x1f * 9 + 0x29);
    varint::put(&mut head, varint::MAX - 16);
    r.send(0, &head);
    r.t.inbound.entry(0).or_default().chunks.reserve(4);
    assert!(alloc::installed());
    let base = r.c.held();
    alloc::begin();
    let mut most = 0i64;
    for _ in 0..1600 {
        r.t.push_static(0, &ZEROS);
        r.readable(0);
        most = most.max(alloc::read().live);
    }
    let c = alloc::end();
    println!("capsule: 100 MiB of one unknown capsule: live {} B, peak {} B, most live {most} B, held {} B (idle {base} B)", c.live, c.peak, r.c.held());
    assert!(most <= 64 && c.peak <= 64, "live {most}, peak {}", c.peak);
    assert_eq!(r.c.held(), base);
    assert_eq!(r.closed(), None);
}

#[test]
fn draft14_4_4_application_error_codes_map_into_their_range() {
    for n in [0u32, 1, 0x1d, 0x1e, 0x1f, 1000, u32::MAX] {
        let h = super::wt_error_to_h3(n);
        assert!((code::WT_APPLICATION_ERROR_FIRST..=code::WT_APPLICATION_ERROR_LAST).contains(&h));
        assert!(!(h - 0x21).is_multiple_of(0x1f), "a reserved codepoint");
        assert_eq!(super::h3_error_to_wt(h), Some(n));
    }
    assert_eq!(super::wt_error_to_h3(u32::MAX), code::WT_APPLICATION_ERROR_LAST);
    assert_eq!(super::h3_error_to_wt(code::H3_NO_ERROR), None);
}

#[test]
fn a_peer_reset_of_a_session_stream_is_told_with_its_code() {
    let mut r = Rig::new();
    r.open_session(CHROME);
    let mut bidi = v(frame::WT_STREAM);
    bidi.extend(v(0));
    r.send(4, &bidi);
    r.events();
    r.reset(4, super::wt_error_to_h3(42));
    assert!(r.events().contains(&Event::StreamReset { id: 4, code: Some(42) }));
}
