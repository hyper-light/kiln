//! QPACK at table capacity 0 (goal 4 step 2): FoxIO's five mistakes one test each, the accordion
//! shapes, the dynamic-table cases, and the bounds on what a decode holds, counted by
//! hyper-measure's allocator.

use super::*;
use crate::h2::hpack::huffman as hf;
use crate::h3::DOOR_BUDGET;
use hyper_measure::alloc;

type Fields = Vec<(Vec<u8>, Vec<u8>)>;

fn all(section: &[u8], budget: Budget) -> Result<Fields, Refused<()>> {
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    decode(section, budget, &mut scratch, |n, v| {
        out.push((n.to_vec(), v.to_vec()));
        Ok::<(), ()>(())
    })?;
    Ok(out)
}

/// Fields handed over before the decode stopped, and how it ended.
fn until(section: &[u8], budget: Budget) -> (usize, Result<Decoded, Refused<()>>) {
    let mut n = 0usize;
    let mut scratch = Vec::new();
    let r = decode(section, budget, &mut scratch, |_, _| {
        n += 1;
        Ok::<(), ()>(())
    });
    (n, r)
}

fn section(lines: &[u8]) -> Vec<u8> {
    let mut s = vec![0x00, 0x00];
    s.extend_from_slice(lines);
    s
}

const BIG: Budget = Budget { bytes: 4096, fields: 1 << 20 };

#[test]
fn rfc9204_static_table_has_its_99_entries() {
    assert_eq!(table::STATIC.len(), 99);
    assert_eq!(table::STATIC.first(), Some(&(&b":authority"[..], &b""[..])));
    assert_eq!(table::STATIC.get(15), Some(&(&b":method"[..], &b"CONNECT"[..])));
    assert_eq!(table::STATIC.get(25), Some(&(&b":status"[..], &b"200"[..])));
    assert_eq!(table::STATIC.get(90), Some(&(&b"origin"[..], &b""[..])));
    assert_eq!(table::STATIC.get(98), Some(&(&b"x-frame-options"[..], &b"sameorigin"[..])));
}

#[test]
fn rfc9204_b1_literal_with_static_name_reference_decodes() -> Result<(), Refused<()>> {
    // RFC 9204 Appendix B.1: ":path: /index.html" as a literal with a static name reference.
    let s = [0x00, 0x00, 0x51, 0x0b, b'/', b'i', b'n', b'd', b'e', b'x', b'.', b'h', b't', b'm', b'l'];
    assert_eq!(all(&s, BIG)?, vec![(b":path".to_vec(), b"/index.html".to_vec())]);
    Ok(())
}

#[test]
fn rfc9204_huffman_names_and_values_decode() -> Result<(), Refused<()>> {
    let (mut name, mut value) = (Vec::new(), Vec::new());
    hf::encode(b"sec-webtransport-http3-draft02", &mut name);
    hf::encode(b"1", &mut value);
    let mut lines = Vec::new();
    put_int(&mut lines, 0x28, 3, name.len() as u64);
    lines.extend_from_slice(&name);
    put_int(&mut lines, 0x80, 7, value.len() as u64);
    lines.extend_from_slice(&value);
    let mut path = Vec::new();
    hf::encode(b"/wt?ticket=abc", &mut path);
    put_int(&mut lines, 0x50, 4, 1);
    put_int(&mut lines, 0x80, 7, path.len() as u64);
    lines.extend_from_slice(&path);
    assert_eq!(all(&section(&lines), BIG)?, vec![(b"sec-webtransport-http3-draft02".to_vec(), b"1".to_vec()), (b":path".to_vec(), b"/wt?ticket=abc".to_vec())]);
    Ok(())
}

#[test]
fn what_the_encoder_writes_the_decoder_reads() -> Result<(), Refused<()>> {
    let mut s = Vec::new();
    Encoder::begin(&mut s);
    Encoder::field(&mut s, b":status", b"200");
    Encoder::field(&mut s, b":status", b"501");
    Encoder::field(&mut s, b"sec-webtransport-http3-draft", b"draft02");
    Encoder::field(&mut s, b"content-length", b"0");
    // Indexed :status 200 is one byte; 501 names :status by reference.
    assert_eq!(s.get(2), Some(&(0xc0 | 25)));
    assert_eq!(
        all(&s, BIG)?,
        vec![
            (b":status".to_vec(), b"200".to_vec()),
            (b":status".to_vec(), b"501".to_vec()),
            (b"sec-webtransport-http3-draft".to_vec(), b"draft02".to_vec()),
            (b"content-length".to_vec(), b"0".to_vec())
        ]
    );
    Ok(())
}

// ------------------------------------------------------------------ FoxIO's five mistakes

/// Repeated 1-byte references to the largest static entry a 1-byte line reaches (index 58,
/// strict-transport-security with preload: 25 + 44 + 32 = 101 bytes counted each; FoxIO's static
/// variant. The 6-bit prefix reaches indexes 0-62 in one byte).
fn static_accordion(lines: usize) -> Vec<u8> {
    section(&vec![0xc0 | 58; lines])
}

/// A static line's counted size.
const STATIC_LINE: usize = 101;

#[test]
fn foxio_1_a_small_frame_that_decodes_past_the_budget_is_refused() {
    // 200 encoded bytes that decode to 200 * 101 = 20,200: the frame is small, its fields aren't.
    let s = static_accordion(200);
    assert!(s.len() < BIG.bytes);
    let (n, r) = until(&s, BIG);
    assert_eq!(r, Err(Refused::TooLarge));
    // Refused at the first field past the budget: every field handed over fit.
    assert_eq!(n, BIG.bytes / STATIC_LINE);
}

#[test]
fn foxio_2_the_refusal_comes_before_the_rest_of_the_section_is_decoded() {
    // Past the budget, then bytes that would be QPACK_DECOMPRESSION_FAILED (a post-base
    // reference) and an invalid Huffman string: the decode never reaches them.
    let mut s = static_accordion(BIG.bytes / STATIC_LINE + 1);
    s.extend_from_slice(&[0x10, 0x5f, 0x0a, 0x81, 0xff]);
    assert_eq!(until(&s, BIG).1, Err(Refused::TooLarge));
    // The same tail inside the budget is the failure it is.
    let mut ok = static_accordion(1);
    ok.extend_from_slice(&[0x10]);
    assert_eq!(until(&ok, BIG).1, Err(Refused::Failed));
}

#[test]
fn foxio_3_a_section_that_fits_only_without_the_32_byte_overhead_is_refused() {
    // 40 fields of a 1-byte name and a 69-byte value: 70 bytes each, 2,800 without the overhead
    // (under 4,096), 4,080 with it (under) for 40; 41 of them are 4,182 (over).
    let mut lines = Vec::new();
    for i in 0..41u8 {
        put_int(&mut lines, 0x20, 3, 1);
        lines.push(b'a' + (i % 26));
        put_int(&mut lines, 0x00, 7, 69);
        lines.extend_from_slice(&[b'v'; 69]);
    }
    let s = section(&lines);
    let (n, r) = until(&s, BIG);
    assert_eq!(r, Err(Refused::TooLarge));
    assert_eq!(n, 40);
    const { assert!(41 * 70 < BIG.bytes, "it would fit if the overhead were left out") };
}

#[test]
fn foxio_4_the_default_configuration_enforces_a_limit() {
    // The door's budget is a constant, finite and at most 16 KiB: there is no "unset".
    const { assert!(DOOR_BUDGET.bytes > 0 && DOOR_BUDGET.bytes <= 16 << 10) };
    const { assert!(DOOR_BUDGET.fields > 0 && DOOR_BUDGET.fields <= 256) };
    let s = static_accordion(DOOR_BUDGET.bytes / STATIC_LINE + 1);
    assert_eq!(until(&s, DOOR_BUDGET).1, Err(Refused::TooLarge));
}

#[test]
fn foxio_5_the_advertised_limit_is_the_one_enforced() {
    // The SETTINGS the door sends say SETTINGS_MAX_FIELD_SECTION_SIZE (0x06) = the budget it
    // decodes with; a section of exactly that size decodes, one byte more doesn't.
    let settings = crate::h3::settings_payload(&crate::h3::Settings::door());
    let mut pos = 0;
    let mut advertised = None;
    while let Some((id, n)) = crate::h3::varint::decode(settings.get(pos..).unwrap_or_default()) {
        pos += n;
        let Some((v, m)) = crate::h3::varint::decode(settings.get(pos..).unwrap_or_default()) else { break };
        pos += m;
        if id == 0x06 {
            advertised = Some(v);
        }
    }
    assert_eq!(advertised, Some(DOOR_BUDGET.bytes as u64));
    // One literal field of exactly the budget: name "x", value of budget - 33 bytes.
    let exact = |value: usize| {
        let mut lines = Vec::new();
        put_int(&mut lines, 0x20, 3, 1);
        lines.push(b'x');
        put_int(&mut lines, 0x00, 7, value as u64);
        lines.extend(std::iter::repeat_n(b'v', value));
        section(&lines)
    };
    let fits = DOOR_BUDGET.bytes - 1 - FIELD_OVERHEAD;
    assert_eq!(until(&exact(fits), DOOR_BUDGET).1.map(|d| d.size), Ok(DOOR_BUDGET.bytes));
    assert_eq!(until(&exact(fits + 1), DOOR_BUDGET).1, Err(Refused::TooLarge));
}

// ------------------------------------------------------------------ accordions and the table

/// A maximum-length frame of distinct 1-byte literal names (FoxIO's count amplification: about
/// 160,000 headers in 1 MB).
fn count_accordion(bytes: usize) -> Vec<u8> {
    let mut lines = Vec::new();
    let mut i = 0u32;
    while lines.len() + 4 < bytes {
        put_int(&mut lines, 0x20, 3, 1);
        lines.push(b'a' + (i % 26) as u8);
        put_int(&mut lines, 0x00, 7, 0);
        i += 1;
    }
    section(&lines)
}

/// A value of `n` 'e's Huffman-coded (5 bits each: the most a string expands, 8:5).
fn huffman_accordion(n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    hf::encode(&vec![b'e'; n], &mut v);
    let mut lines = Vec::new();
    put_int(&mut lines, 0x50, 4, 2); // age
    put_int(&mut lines, 0x80, 7, v.len() as u64);
    lines.extend_from_slice(&v);
    section(&lines)
}

#[test]
fn foxio_count_amplification_stops_at_the_field_cap_and_the_budget() {
    let s = count_accordion(1 << 20);
    let (n, r) = until(&s, Budget { bytes: 1 << 30, fields: 64 });
    assert_eq!((n, r), (64, Err(Refused::TooLarge)));
    let (n, r) = until(&s, BIG);
    assert_eq!((n, r), (BIG.bytes / 33, Err(Refused::TooLarge)));
}

#[test]
fn huffman_expansion_is_refused_before_it_is_decoded_past_the_budget() {
    let s = huffman_accordion(1 << 16);
    assert_eq!(until(&s, BIG).1, Err(Refused::TooLarge));
    // Within the budget it decodes exactly.
    let small = huffman_accordion(100);
    assert_eq!(all(&small, BIG).map(|f| f.first().map(|(_, v)| v.len())), Ok(Some(100)));
}

#[test]
fn dynamic_table_references_are_connection_errors() {
    let cases: [&[u8]; 6] = [
        // A nonzero Required Insert Count.
        &[0x01, 0x00],
        &[0xff, 0x01, 0x00],
        // Indexed, dynamic (T = 0).
        &[0x00, 0x00, 0x80],
        // Literal with a dynamic name reference (T = 0).
        &[0x00, 0x00, 0x40, 0x00],
        // Post-base indexed, and literal with a post-base name reference.
        &[0x00, 0x00, 0x10],
        &[0x00, 0x00, 0x00, 0x00],
    ];
    for c in cases {
        assert_eq!(until(c, BIG).1, Err(Refused::Failed), "{c:02x?}");
    }
    // An index past the static table, a string past the section, bad Huffman, an overlong integer.
    let more: [&[u8]; 4] = [&[0x00, 0x00, 0xff, 0x24], &[0x00, 0x00, 0x51, 0x0b, b'/'], &[0x00, 0x00, 0x51, 0x81, 0x00], &[0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]];
    for c in more {
        assert_eq!(until(c, BIG).1, Err(Refused::Failed), "{c:02x?}");
    }
}

#[test]
fn rfc9204_4_3_the_encoder_stream_may_only_set_capacity_zero() {
    assert_eq!(encoder_stream(&[0x20, 0x20]), Ok(()));
    // Set Dynamic Table Capacity 1, and 31+ (a multi-byte one).
    assert_eq!(encoder_stream(&[0x21]), Err(code::ENCODER_STREAM_ERROR));
    assert_eq!(encoder_stream(&[0x3f, 0x01]), Err(code::ENCODER_STREAM_ERROR));
    // Inserts (static name reference, literal name) and a duplicate.
    for b in [0xc0u8, 0x80, 0x40, 0x00] {
        assert_eq!(encoder_stream(&[b, 0x00]), Err(code::ENCODER_STREAM_ERROR));
    }
}

#[test]
fn rfc9204_4_4_the_decoder_stream_may_only_cancel_streams() {
    let mut d = DecoderStream::default();
    // Stream Cancellation of stream 4, then of a large id split across reads.
    assert_eq!(d.read(&[0x44, 0x7f]), Ok(()));
    assert_eq!(d.read(&[0xff, 0xff]), Ok(()));
    assert_eq!(d.read(&[0x01, 0x40]), Ok(()));
    // Section Acknowledgment, Insert Count Increment.
    assert_eq!(DecoderStream::default().read(&[0x84]), Err(code::DECODER_STREAM_ERROR));
    assert_eq!(DecoderStream::default().read(&[0x01]), Err(code::DECODER_STREAM_ERROR));
    // An integer longer than any stream id.
    let mut d = DecoderStream::default();
    assert_eq!(d.read(&[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]), Err(code::DECODER_STREAM_ERROR));
}

// ------------------------------------------------------------------ what a decode holds

/// One decode, counted: the bytes it holds after (the door keeps none of a refused section),
/// and its peak.
fn counted(s: &[u8], budget: Budget) -> (i64, i64) {
    assert!(alloc::installed());
    alloc::begin();
    let mut scratch = Vec::new();
    let r = decode(s, budget, &mut scratch, |_, _| Ok::<(), ()>(()));
    drop(scratch);
    let c = alloc::end();
    std::hint::black_box(r.is_ok());
    (c.live, c.peak)
}

#[test]
fn accordions_retain_nothing_and_peak_within_the_budget() {
    let vectors: Vec<(&str, Vec<u8>)> = vec![
        ("static 1-byte references to entry 58, 1 MiB", static_accordion(1 << 20)),
        ("distinct 1-byte literal names, 1 MiB", count_accordion(1 << 20)),
        ("Huffman 8:5 expansion, 1 MiB decoded", huffman_accordion(1 << 20)),
        ("static references within the budget", static_accordion(DOOR_BUDGET.bytes / STATIC_LINE)),
        ("one field of exactly the budget", huffman_accordion(DOOR_BUDGET.bytes - 3 - 32)),
    ];
    for (what, v) in &vectors {
        let (live, peak) = counted(v, DOOR_BUDGET);
        let ratio = live as f64 / v.len() as f64;
        println!("qpack {what}: {} B encoded, retained {live} B (ratio {ratio:.4}), peak {peak} B", v.len());
        assert!(live <= v.len() as i64, "{what}: retained {live} of {} encoded", v.len());
        assert!(peak <= 20_480, "{what}: peak {peak} B");
        assert!(peak <= DOOR_BUDGET.bytes as i64 + 1024, "{what}: peak {peak} B past the budget and fixed state");
    }
}

#[test]
fn random_sections_never_panic() {
    // A fixed-seed xorshift: every outcome is an error value or fields within the budget.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut scratch = Vec::new();
    for _ in 0..20_000 {
        let n = (next() % 64) as usize;
        let mut s: Vec<u8> = (0..n).map(|_| next() as u8).collect();
        if next() % 2 == 0 {
            s.splice(0..0, [0x00, 0x00]);
        }
        let mut size = 0usize;
        let r = decode(&s, BIG, &mut scratch, |n, v| {
            size += n.len() + v.len() + 32;
            Ok::<(), ()>(())
        });
        assert!(size <= BIG.bytes);
        if let Ok(d) = r {
            assert_eq!(d.size, size);
        }
    }
}
