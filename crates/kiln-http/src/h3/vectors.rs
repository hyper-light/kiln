//! The browsers' recorded handshakes (goal 4 step 1, `tests/vectors/webtransport/`), replayed: each
//! parses as HTTP/3 frames and QPACK field lines with Required Insert Count 0 (step 2), each
//! decodes to its fields within the door's budget, and each gets its session admitted under the
//! dialect it speaks (step 3). The budget is fixed from them here.

use super::conn::{Admission, Connect, Event};
use super::qpack::{self, Budget, FIELD_OVERHEAD};
use super::tests::Rig;
use super::{DOOR_BUDGET, Draft, frame, setting, varint};

/// A recorded browser: who, its dialect, and what it sent, stream by stream, in order.
pub(crate) struct Vector {
    pub browser: String,
    pub draft: Draft,
    pub streams: Vec<(u64, Vec<u8>, bool)>,
    pub datagrams: Vec<Vec<u8>>,
}

fn hex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

pub(crate) fn parse(text: &str) -> Option<Vector> {
    let mut v = Vector { browser: String::new(), draft: Draft::Draft02, streams: Vec::new(), datagrams: Vec::new() };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut w = line.split_whitespace();
        match w.next()? {
            "browser" => v.browser = w.collect::<Vec<_>>().join(" "),
            "draft" => {
                v.draft = match w.next()? {
                    "draft-02" => Draft::Draft02,
                    "draft-07" => Draft::Draft07,
                    _ => Draft::Draft14,
                }
            }
            "stream" => {
                let id = w.next()?.parse().ok()?;
                // `-` is no bytes (a FIN alone).
                let bytes = match w.next().unwrap_or_default() {
                    "-" => Vec::new(),
                    h => hex(h)?,
                };
                v.streams.push((id, bytes, w.next() == Some("fin")));
            }
            "datagram" => v.datagrams.push(hex(w.next()?)?),
            _ => return None,
        }
    }
    Some(v)
}

/// Every recorded browser.
pub(crate) fn all() -> Vec<Vector> {
    [
        include_str!("../../tests/vectors/webtransport/chromium-156.txt"),
        include_str!("../../tests/vectors/webtransport/firefox-157.txt"),
        include_str!("../../tests/vectors/webtransport/safari-26.4.txt"),
    ].iter().filter_map(|t| parse(t)).collect()
}

/// The first HEADERS frame's payload on stream 0 (the CONNECT's field section).
fn connect_section(v: &Vector) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = v.streams.iter().filter(|(id, _, _)| *id == 0).flat_map(|(_, b, _)| b.clone()).collect();
    let (t, n) = varint::decode(&bytes)?;
    let (len, m) = varint::decode(bytes.get(n..)?)?;
    (t == frame::HEADERS).then(|| bytes.get(n + m..n + m + len as usize).map(<[u8]>::to_vec))?
}

/// The settings on a browser's control stream.
fn settings(v: &Vector) -> Option<Vec<(u64, u64)>> {
    let (_, b, _) = v.streams.iter().find(|(id, b, _)| id % 4 == 2 && b.first() == Some(&0))?;
    let rest = b.get(1..)?;
    let (_, n) = varint::decode(rest)?;
    let (len, m) = varint::decode(rest.get(n..)?)?;
    let mut p = rest.get(n + m..n + m + len as usize)?;
    let mut out = Vec::new();
    while !p.is_empty() {
        let (id, a) = varint::decode(p)?;
        let (val, c) = varint::decode(p.get(a..)?)?;
        out.push((id, val));
        p = p.get(a + c..)?;
    }
    Some(out)
}

/// Room for the ticket and principal in a CONNECT's path: `?ticket=` and 43 characters (32
/// bytes, base64url), `&u=` and a principal of up to 160.
pub(crate) const TICKET_ROOM: usize = 8 + 43 + 3 + 160;

#[test]
fn step1_each_vector_names_its_browser_its_dialect_and_what_it_needs() {
    let vs = all();
    assert_eq!(vs.len(), 3, "chromium, firefox and safari");
    for v in &vs {
        let s = settings(v).unwrap_or_default();
        println!("vector: {} speaks {}; its SETTINGS {:x?}", v.browser, v.draft.name(), s);
        // Draft-02's browsers need SETTINGS_ENABLE_WEBTRANSPORT and H3_DATAGRAM from the server.
        match v.draft {
            Draft::Draft02 => assert!(s.contains(&(setting::ENABLE_WEBTRANSPORT, 1))),
            _ => assert!(s.contains(&(setting::WT_MAX_SESSIONS, 1)) || s.contains(&(setting::WEBTRANSPORT_MAX_SESSIONS_DRAFT07, 1))),
        }
        assert!(s.contains(&(setting::H3_DATAGRAM, 1)) || s.contains(&(setting::H3_DATAGRAM_DRAFT04, 1)));
    }
}

#[test]
fn step2_each_vector_decodes_to_its_fields_with_no_dynamic_table() {
    let mut largest = 0usize;
    let mut most_fields = 0usize;
    for v in all() {
        let section = connect_section(&v).unwrap_or_default();
        // Required Insert Count 0: the browsers took the door's table capacity of 0.
        assert_eq!(section.first(), Some(&0x00), "{}", v.browser);
        let mut fields = Vec::new();
        let mut scratch = Vec::new();
        let d = qpack::decode(&section, Budget { bytes: 1 << 20, fields: 1 << 10 }, &mut scratch, |n, val| {
            fields.push((String::from_utf8_lossy(n).into_owned(), String::from_utf8_lossy(val).into_owned()));
            Ok::<(), ()>(())
        });
        assert!(d.is_ok(), "{}: {d:?}", v.browser);
        let size: usize = fields.iter().map(|(n, val)| n.len() + val.len() + FIELD_OVERHEAD).sum();
        println!("vector: {} CONNECT: {} B encoded, {} fields, {size} B decoded (names, values, 32 each)", v.browser, section.len(), fields.len());
        for (n, val) in &fields {
            println!("vector:   {n}: {val} ({} B)", n.len() + val.len() + FIELD_OVERHEAD);
        }
        // The fields each browser sent, decoded exactly (the lab page's URL and origin).
        let has = |n: &str, val: &str| fields.iter().any(|(a, b)| a == n && b == val);
        assert!(has(":protocol", "webtransport") && has(":method", "CONNECT") && has(":scheme", "https"), "{}", v.browser);
        assert!(has(":authority", "127.0.0.1:18443"), "{}", v.browser);
        assert!(has(":path", &format!("/wt?ticket={}", "t".repeat(43))), "{}", v.browser);
        assert!(has("origin", "http://127.0.0.1:18480"), "{}", v.browser);
        assert_eq!(fields.iter().any(|(n, _)| n == "sec-webtransport-http3-draft02"), v.draft == Draft::Draft02, "{}", v.browser);
        largest = largest.max(size);
        most_fields = most_fields.max(fields.len());
    }
    // The budget: four times the largest real CONNECT, plus room for a ticket, at most 16 KiB.
    let budget = 4 * largest + TICKET_ROOM;
    println!("vector: largest CONNECT {largest} B; budget 4 x {largest} + {TICKET_ROOM} = {budget} B; the door's {} B", DOOR_BUDGET.bytes);
    assert!(DOOR_BUDGET.bytes >= budget, "the door's budget is below what browsers send");
    const { assert!(DOOR_BUDGET.bytes <= 16 << 10) };
    assert!(DOOR_BUDGET.fields >= 4 * most_fields, "{most_fields} fields");
}

#[test]
fn step3_each_vector_gets_its_session_under_its_dialect() {
    for v in all() {
        let mut r = Rig::new();
        for (id, bytes, fin) in &v.streams {
            r.t.push(*id, bytes);
            if *fin {
                r.t.fin(*id);
            }
            r.readable(*id);
        }
        let ev = r.events();
        let accepted: Vec<&Connect> = ev.iter().filter_map(|e| if let Event::Accepted(c) = e { Some(c) } else { None }).collect();
        assert_eq!(accepted.len(), 1, "{}: {ev:?}", v.browser);
        assert_eq!(accepted.first().map(|c| c.draft), Some(v.draft), "{}", v.browser);
        assert!(r.asked.len() == 1 && r.answer == Admission::Accept);
        // The bidi and uni streams joined the session, and its close was read.
        assert!(ev.iter().any(|e| matches!(e, Event::Stream { bidi: true, .. })), "{}", v.browser);
        assert!(ev.iter().any(|e| matches!(e, Event::Stream { bidi: false, .. })), "{}", v.browser);
        assert!(ev.iter().any(|e| matches!(e, Event::Closed { code: 7, .. })), "{}: {ev:?}", v.browser);
        assert_eq!(r.closed(), None, "{}", v.browser);
        // The answer carried what draft-02 asks.
        let resp = super::tests::response(r.t.written(0)).unwrap_or_default();
        assert!(resp.contains(&(b":status".to_vec(), b"200".to_vec())));
        if v.draft == Draft::Draft02 {
            assert!(resp.contains(&(b"sec-webtransport-http3-draft".to_vec(), b"draft02".to_vec())));
        }
        // The datagram is the session's.
        for d in &v.datagrams {
            r.t.dgram_in.push_back(bytes::Bytes::copy_from_slice(d));
        }
    }
}
