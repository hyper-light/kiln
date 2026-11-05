//! The attack suite (goal 4 step 8, plan 01a1a832eb597110a6dcfda227981604): hyper-quic clients
//! attack the door over the in-memory network, and hyper-measure's allocator counts what the
//! door holds (its code alone: the clients' and the network's allocations aren't counted).
//!
//! Every attack runs at 1x, 10x and 100x volume against one connection and against [`MANY`] at
//! once, and each cell checks: the live bytes a connection holds beyond its idle baseline are at
//! most the bound B the door computes for it (before its session, or after), and they grow by at
//! most 4 KiB from the 10x run to the 100x one. Each cell is printed (`attack: ...`) so goal 10
//! can cite it. Attacks that end their connection (a connection error, a malformed session) run
//! once per connection at each volume against one connection, re-dialling, and at 1x against
//! [`MANY`]: a second offence on a connection that's gone is no attack.

use std::time::Duration;

use super::conn::Event;
use super::door::DoorConfig;
use super::qpack::{Encoder, put_int};
use super::testnet::{Browser, Echo, Net, open_session};
use super::tests::{CHROME, control, headers, v, wt_connect};
use super::{capsule, frame, put_frame, stream_type, varint};

/// Connections attacked at once in the wide cells.
const MANY: usize = 1000;
/// The growth allowed from 10x to 100x, per connection.
const GROWTH: i64 = 4096;
/// Virtual time a connection settles for before it's measured (past its handshake's records).
const SETTLE: Duration = Duration::from_secs(3);

/// One cell of the report.
#[derive(Debug, Clone, Copy)]
struct Cell {
    conns: usize,
    volume: u32,
    per_conn: i64,
    peak_per_conn: i64,
}

/// Where an attack starts: a connection with no session, or an admitted one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    Pre,
    Post,
}

fn cfg() -> DoorConfig {
    let mut c = DoorConfig::door();
    c.max_pre_session = MANY + 16;
    c.max_connections = MANY + 16;
    // No Retry in these runs (the address-validation attack sets it back).
    c.retry_after = usize::MAX;
    c
}

/// `n` browsers connected (their control streams open with Chrome's settings), sessions admitted
/// when `start` says so; their CONNECT streams' ids.
fn connect(net: &mut Net<Echo>, n: usize, start: Start) -> Vec<(usize, u64)> {
    let first = net.browsers.len();
    for _ in 0..n {
        net.dial();
    }
    prepare(net, &(first..first + n).collect::<Vec<_>>(), start)
}

/// Brings browsers `which` (dialled) to `start`.
fn prepare(net: &mut Net<Echo>, which: &[usize], start: Start) -> Vec<(usize, u64)> {
    net.until(1_000_000, |net| which.iter().all(|&i| net.browsers.get(i).is_some_and(Browser::connected)));
    let accepted = |net: &Net<Echo>| net.events.iter().filter(|(_, e)| matches!(e, Event::Accepted(_))).count();
    let want = accepted(net) + which.len();
    let mut out = Vec::new();
    for &i in which {
        let Some(b) = net.b(i) else { continue };
        if start == Start::Post {
            if let Some(id) = open_session(b, CHROME, b"/wt") {
                out.push((i, id));
            }
        } else {
            let ctl = b.open(false).unwrap_or_default();
            b.write(ctl, &control(CHROME));
            out.push((i, 0));
        }
    }
    if start == Start::Post {
        net.until(1_000_000, |net| accepted(net) >= want);
    }
    net.run_for(Duration::from_millis(200));
    out
}

/// Runs `unit` (one unit of the attack, on one browser) `volume` units per connection, the net
/// moving between rounds, and settles.
fn drive(net: &mut Net<Echo>, conns: &[(usize, u64)], units: u32, unit: &mut impl FnMut(&mut Browser, u64, u32)) {
    for round in 0..units {
        for &(i, sid) in conns {
            if let Some(b) = net.b(i) {
                unit(b, sid, round);
            }
        }
        net.run_for(Duration::from_millis(2));
    }
    net.run_for(Duration::from_millis(300));
}

/// An attack's cells: against one connection and against [`MANY`], at 1x, 10x and 100x `unit`s.
fn attack(name: &str, source: &str, start: Start, units: u32, ends_connection: bool, unit: impl FnMut(&mut Browser, u64, u32)) {
    attack_on(name, source, start, units, ends_connection, 0, unit);
}

/// [`attack`] on a path that reorders `reorder_ppm` of its datagrams.
fn attack_on(name: &str, source: &str, start: Start, units: u32, ends_connection: bool, reorder_ppm: u64, mut unit: impl FnMut(&mut Browser, u64, u32)) {
    let bound = cfg().bound(start == Start::Post) as i64;
    let mut cells = Vec::new();
    for conns in [1usize, MANY] {
        let net = Net::new(cfg(), Echo, 0x5eed ^ conns as u64);
        assert!(net.is_some(), "no network");
        let Some(mut net) = net else { return };
        net.reorder_ppm = reorder_ppm;
        let volumes: &[u32] = if ends_connection && conns > 1 { &[1] } else { &[1, 10, 100] };
        let mut set = connect(&mut net, conns, start);
        // Settled first: what the handshakes held (their keys, their packets' records) is gone
        // before the count begins, so it isn't counted as the attack's.
        net.run_for(SETTLE);
        net.measure();
        net.run_for(Duration::from_millis(50));
        let (base, _) = net.door_live();
        let calls0 = hyper_measure::alloc::read().allocations;
        let mut done = 0u32;
        for &vol in volumes {
            if ends_connection {
                // Each offence on a connection of its own, one after another.
                for _ in done..vol {
                    drive(&mut net, &set, units, &mut unit);
                    if conns == 1 {
                        // The attacker moves on to a connection of its own: the last one ends.
                        // From its own address again: an attacker of one address (the
                        // endpoint's per-address memories, 256 addresses each, are the
                        // endpoint's bound, not a connection's).
                        let which: Vec<usize> = set.iter().map(|&(i, _)| i).collect();
                        for &i in &which {
                            net.hang_up(i);
                        }
                        net.run_for(SETTLE);
                        for &i in &which {
                            net.redial(i);
                        }
                        set = prepare(&mut net, &which, start);
                        net.run_for(SETTLE);
                    }
                }
            } else {
                drive(&mut net, &set, units * (vol - done), &mut unit);
            }
            done = vol;
            let (live, peak) = net.door_live();
            let per_conn = (live - base) / conns as i64;
            let cell = Cell { conns, volume: vol, per_conn, peak_per_conn: (peak - base) / conns as i64 };
            let (open, pre) = net.door.counts();
            let refused: u64 = net.door.ids().iter().filter_map(|&i| net.door.h3(i)).map(|h| h.stats.refused + h.stats.buffered_rejected + h.stats.datagrams_dropped).sum();
            let calls = hyper_measure::alloc::read().allocations - calls0;
            println!(
                "attack: {name} ({source}) {} conns {}x: {} B/conn beyond idle (peak {} B/conn), B = {bound} B [door: {open} open, {pre} without a session, {refused} refusals, {} datagrams dropped, {calls} allocations]",
                cell.conns, cell.volume, cell.per_conn, cell.peak_per_conn, net.door.stats.datagrams_dropped
            );
            cells.push(cell);
        }
        drop(net);
        hyper_measure::alloc::end();
    }
    for c in &cells {
        assert!(c.per_conn <= bound, "{name}: {c:?} past B = {bound}");
    }
    for conns in [1usize, MANY] {
        let at = |v: u32| cells.iter().find(|c| c.conns == conns && c.volume == v).map(|c| c.per_conn);
        if let (Some(ten), Some(hundred)) = (at(10), at(100)) {
            assert!(hundred - ten <= GROWTH, "{name}: {conns} conns grew {} B from 10x to 100x", hundred - ten);
        }
    }
}

/// A request stream carrying `frame` and ended.
fn request(b: &mut Browser, frame: &[u8]) {
    if let Some(id) = b.open(true) {
        b.write(id, frame);
        b.finish(id);
    }
}

/// A HEADERS frame of `lines` (the section prefix added).
fn section(lines: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    Encoder::begin(&mut block);
    block.extend_from_slice(lines);
    let mut out = Vec::new();
    put_frame(&mut out, frame::HEADERS, &block);
    out
}

/// FoxIO's count accordion at the largest frame the door reads: distinct 1-byte literal names.
fn count_accordion() -> Vec<u8> {
    let mut lines = Vec::new();
    let mut i = 0u32;
    while lines.len() + 8 < super::DOOR_BUDGET.bytes {
        put_int(&mut lines, 0x20, 3, 1);
        lines.push(b'a' + (i % 26) as u8);
        put_int(&mut lines, 0x00, 7, 0);
        i += 1;
    }
    section(&lines)
}

/// FoxIO's static accordion: 1-byte references to the largest static entry they reach.
fn static_accordion() -> Vec<u8> {
    section(&vec![0xc0 | 58; super::DOOR_BUDGET.bytes - 8])
}

/// A Huffman value expanding 8:5, as long as a frame the door reads.
fn huffman_accordion() -> Vec<u8> {
    let mut v = Vec::new();
    crate::h2::hpack::huffman::encode(&vec![b'e'; super::DOOR_BUDGET.bytes * 8 / 5 - 64], &mut v);
    let mut lines = Vec::new();
    put_int(&mut lines, 0x50, 4, 2);
    put_int(&mut lines, 0x80, 7, v.len() as u64);
    lines.extend_from_slice(&v);
    section(&lines)
}

// ------------------------------------------------------------------- HTTP/3 and QPACK

#[test]
fn foxio_count_accordion_leaves_memory_flat() {
    let f = count_accordion();
    attack("QPACK count accordion", "FoxIO 2026-10-20", Start::Pre, 1, false, |b, _, _| request(b, &f));
}

#[test]
fn foxio_static_accordion_leaves_memory_flat() {
    let f = static_accordion();
    attack("QPACK static accordion", "FoxIO 2026-10-20", Start::Pre, 1, false, |b, _, _| request(b, &f));
}

#[test]
fn huffman_expansion_leaves_memory_flat() {
    let f = huffman_accordion();
    attack("QPACK Huffman expansion", "RFC 7541 §5.2, FoxIO", Start::Pre, 1, false, |b, _, _| request(b, &f));
}

#[test]
fn lsquic_many_headers_frames_on_one_stream_leave_memory_flat() {
    let get = headers(&[(b":method", b"GET"), (b":scheme", b"https"), (b":authority", b"a"), (b":path", b"/")]);
    let many: Vec<u8> = std::iter::repeat_n(get.clone(), 100).flatten().collect();
    attack("many HEADERS frames on one stream", "FoxIO on lsquic", Start::Pre, 1, false, |b, _, _| request(b, &many));
}

#[test]
fn trailers_on_the_connect_stream_end_the_session() {
    let trailers = headers(&[(b"x-trailer", b"1")]);
    attack("trailers on the CONNECT stream", "RFC 9114 §4.1", Start::Post, 1, true, |b, sid, _| {
        b.write(sid, &trailers);
    });
}

#[test]
fn encoder_stream_inserts_are_a_connection_error() {
    attack("QPACK encoder-stream inserts", "FoxIO, RFC 9204 §4.3", Start::Pre, 1, true, |b, _, _| {
        if let Some(id) = b.open(false) {
            let mut bytes = v(stream_type::ENCODER);
            bytes.extend_from_slice(&[0x3f, 0xe1, 0x1f]);
            bytes.extend(std::iter::repeat_n([0xc0 | 17, 0x01, b'x'], 1000).flatten());
            b.write(id, &bytes);
        }
    });
}

#[test]
fn cve_2026_42530_duplicate_critical_streams_are_a_connection_error() {
    attack("duplicate control/encoder/decoder streams", "CVE-2026-42530, RFC 9114 §6.2.1", Start::Pre, 1, true, |b, _, _| {
        for kind in [stream_type::ENCODER, stream_type::ENCODER, stream_type::DECODER, stream_type::DECODER] {
            if let Some(id) = b.open(false) {
                b.write(id, &v(kind));
            }
        }
    });
}

// ------------------------------------------------------------------- capsules

#[test]
fn cve_2026_57497_a_huge_unknown_capsule_leaves_memory_flat() {
    let mut started = std::collections::BTreeSet::new();
    let chunk = vec![0u8; 16 << 10];
    attack("one huge unknown capsule", "CVE-2026-57497", Start::Post, 4, false, move |b, sid, _| {
        if started.insert(b.addr) {
            let mut head = Vec::new();
            varint::put(&mut head, frame::DATA);
            varint::put(&mut head, varint::MAX);
            varint::put(&mut head, 0x1f * 9 + 0x29);
            varint::put(&mut head, varint::MAX - 16);
            b.write(sid, &head);
        }
        b.write(sid, &chunk);
    });
}

#[test]
fn many_tiny_capsules_leave_memory_flat() {
    let mut caps = Vec::new();
    for i in 0..2000u64 {
        varint::put(&mut caps, 0x1f * (i % 50) + 0x29);
        varint::put(&mut caps, i % 2);
        if i % 2 == 1 {
            caps.push(0);
        }
    }
    let mut f = Vec::new();
    put_frame(&mut f, frame::DATA, &caps);
    attack("many tiny capsules", "RFC 9297 §3.2", Start::Post, 1, false, |b, sid, _| {
        b.write(sid, &f);
    });
}

#[test]
fn an_oversized_close_message_ends_the_session() {
    let mut big = Vec::new();
    varint::put(&mut big, capsule::kind::CLOSE_SESSION);
    varint::put(&mut big, 4 + 4096);
    big.extend(vec![b'x'; 4100]);
    let mut f = Vec::new();
    put_frame(&mut f, frame::DATA, &big);
    attack("oversized WT_CLOSE_SESSION", "draft-14 §6", Start::Post, 1, true, |b, sid, _| {
        b.write(sid, &f);
    });
}

// ------------------------------------------------------------------- streams

#[test]
fn a_flood_of_unknown_uni_streams_leaves_memory_flat() {
    attack("unknown uni stream flood", "RFC 9114 §6.2", Start::Pre, 8, false, |b, _, round| {
        if let Some(id) = b.open(false) {
            let mut bytes = v(0x1f * u64::from(round % 100) + 0x21);
            bytes.extend(vec![1u8; 512]);
            b.write(id, &bytes);
            b.finish(id);
        }
    });
}

#[test]
fn a_pre_session_webtransport_stream_flood_leaves_memory_flat() {
    attack("pre-session WT stream flood", "draft-14 §4.6", Start::Pre, 8, false, |b, _, _| {
        if let Some(id) = b.open(false) {
            let mut bytes = v(stream_type::WEBTRANSPORT);
            bytes.extend(v(0));
            bytes.extend(vec![1u8; 512]);
            b.write(id, &bytes);
        }
    });
}

#[test]
fn cve_2023_44487_rapid_reset_on_bidi_streams() {
    let part = wt_connect(b"/wt");
    let part = part.get(..part.len() / 2).unwrap_or_default().to_vec();
    attack("Rapid Reset, bidi", "CVE-2023-44487", Start::Post, 4, false, move |b, _, _| {
        if let Some(id) = b.open(true) {
            b.write(id, &part);
            b.reset(id, 0x10c);
        }
    });
}

#[test]
fn cve_2023_44487_rapid_reset_on_uni_streams() {
    attack("Rapid Reset, uni", "CVE-2023-44487", Start::Post, 4, false, |b, sid, _| {
        if let Some(id) = b.open(false) {
            let mut bytes = v(stream_type::WEBTRANSPORT);
            bytes.extend(v(sid));
            bytes.extend(vec![1u8; 256]);
            b.write(id, &bytes);
            b.reset(id, 0);
        }
    });
}

#[test]
fn exhausting_max_streams_leaves_memory_flat() {
    attack("every stream the limit allows, held open", "RFC 9000 §21.8", Start::Post, 1, false, |b, sid, _| {
        while let Some(id) = b.open(false) {
            let mut bytes = v(stream_type::WEBTRANSPORT);
            bytes.extend(v(sid));
            b.write(id, &bytes);
        }
        while let Some(id) = b.open(true) {
            let mut bytes = v(frame::WT_STREAM);
            bytes.extend(v(sid));
            b.write(id, &bytes);
        }
    });
}

#[test]
fn stop_sending_floods_leave_memory_flat() {
    // Eight a round, so the 10x run already meets the stream limit: a stopped stream the peer
    // hasn't ended stays open (its receive side is the peer's to end), and what such streams hold
    // is bounded by the limit, which is what the cells show.
    attack("STOP_SENDING on the door's echo streams", "RFC 9000 §19.5", Start::Post, 8, false, |b, sid, _| {
        if let Some(id) = b.open(true) {
            let mut bytes = v(frame::WT_STREAM);
            bytes.extend(v(sid));
            bytes.extend(vec![2u8; 2048]);
            b.write(id, &bytes);
            b.stop(id, 7);
        }
    });
}

// ------------------------------------------------------------------- QUIC

#[test]
fn one_byte_out_of_order_stream_frames_leave_memory_flat() {
    // The client writes one byte a packet on a reordering path: one-byte STREAM frames, out of
    // order (hyper-quic's assembler defragments and bounds them, RFC 9000 §21.7).
    let mut streams = std::collections::BTreeMap::new();
    attack_on("one-byte STREAM frames, reordered", "RFC 9000 §21.7", Start::Post, 8, false, 300_000, move |b, sid, _| {
        let id = *streams.entry(b.addr).or_insert_with(|| {
            let id = b.open(true).unwrap_or_default();
            let mut head = v(frame::WT_STREAM);
            head.extend(v(sid));
            b.write(id, &head);
            id
        });
        b.write(id, &[0x2a]);
    });
}

#[test]
fn datagrams_for_no_session_are_dropped() {
    attack("datagrams for no session", "RFC 9297 §2", Start::Pre, 16, false, |b, _, round| {
        let mut d = v(u64::from(round) * 4 + 1);
        d.extend(vec![0u8; 1000]);
        b.datagram(&d);
    });
}

#[test]
fn slowloris_a_connect_dribbled_a_byte_at_a_time() {
    // All of a CONNECT but its last byte, a byte a round, then held there with PINGs: the
    // connection never gets a session, and costs its pre-session bound at most.
    let whole = wt_connect(b"/wt");
    let mut at = std::collections::BTreeMap::new();
    attack("CONNECT a byte at a time", "RFC 9000 §21.6", Start::Pre, 16, false, move |b, _, _| {
        let n = at.entry(b.addr).or_insert(0usize);
        if *n == 0 {
            let _ = b.open(true);
        }
        match whole.get(*n) {
            Some(byte) if *n + 1 < whole.len() => {
                b.write(0, &[*byte]);
                *n += 1;
            }
            _ => {
                if let Some(c) = b.c() {
                    c.ping();
                }
            }
        }
    });
}

#[test]
fn slowloris_sessions_kept_alive_with_pings_alone() {
    attack("sessions kept alive by PINGs", "RFC 9000 §21.6", Start::Post, 16, false, |b, _, _| {
        if let Some(c) = b.c() {
            c.ping();
        }
    });
}

#[test]
fn rfc9000_8_the_door_sends_at_most_three_times_what_an_unvalidated_address_sent() {
    // Spoofed sources: clients whose answers never come back (their datagrams from the door are
    // lost), so their addresses are never validated.
    let Some(mut net) = Net::new(cfg(), Echo, 99) else { return };
    let mut spoofed = Vec::new();
    for _ in 0..50 {
        if let Some(i) = net.dial()
            && let Some(b) = net.b(i)
        {
            spoofed.push(b.addr);
        }
    }
    net.black_holes = spoofed.clone();
    net.run_for(Duration::from_secs(30));
    let mut worst = 0.0f64;
    for a in &spoofed {
        let sent = net.door_sent.get(a).copied().unwrap_or(0) as f64;
        let got = net.door_received.get(a).copied().unwrap_or(0).max(1) as f64;
        worst = worst.max(sent / got);
    }
    println!("attack: bytes before address validation (RFC 9000 §8): worst sent/received {worst:.3} over {} spoofed sources", spoofed.len());
    assert!(worst <= 3.0, "{worst}");
}

#[test]
fn the_idle_cost_of_a_connection_and_of_a_session() {
    for start in [Start::Pre, Start::Post] {
        let Some(mut net) = Net::new(cfg(), Echo, 3) else { return };
        net.measure();
        let n = 200;
        connect(&mut net, n, start);
        net.run_for(SETTLE);
        let (live, _) = net.door_live();
        hyper_measure::alloc::end();
        let what = if start == Start::Pre { "a connection with no session" } else { "an admitted session" };
        println!("attack: idle cost of {what}: {} B each ({n} of them; kiln-http's HTTP/1.1 868 B, HTTP/2 2,487 B)", live / n as i64);
        assert!(live > 0);
    }
}

#[test]
fn long_lived_sessions_settle_to_a_steady_size() {
    // Goal 4 step 10's soak, in memory: admitted sessions that each echo a bidi stream every 15
    // seconds (inside the 30-second idle timeout) for 30 virtual minutes. What a session holds
    // grows with its first streams (hyper-quic keeps each freed stream's state for reuse, up to
    // the stream limit) and then holds: the minutes print as
    // `soak-in-memory:` so the soak's resident memory can be read against them.
    let Some(mut net) = Net::new(cfg(), Echo, 11) else { return };
    let n = 50usize;
    let set = connect(&mut net, n, Start::Post);
    net.run_for(SETTLE);
    net.measure();
    let mut lives = Vec::new();
    for minute in 0..30u32 {
        for quarter in 0..4 {
            for &(i, sid) in &set {
                if let Some(b) = net.b(i)
                    && let Some(id) = b.open(true)
                {
                    let mut bytes = v(frame::WT_STREAM);
                    bytes.extend(v(sid));
                    bytes.extend_from_slice(b"ping");
                    b.write(id, &bytes);
                    b.finish(id);
                    b.watch(id);
                }
            }
            net.run_for(Duration::from_secs(15));
            let _ = quarter;
        }
        let (live, _) = net.door_live();
        lives.push(live / n as i64);
        println!("soak-in-memory: minute {minute}: {} B a session beyond its idle size", live / n as i64);
    }
    hyper_measure::alloc::end();
    // Four streams a minute: the 65 bidi streams the limit allows are reached by minute 16, and
    // from there a session holds what it holds (each stream's kept state, `STREAM_STATE`).
    let (at16, end) = (lives.get(16).copied().unwrap_or(0), lives.last().copied().unwrap_or(0));
    println!("soak-in-memory: minute 16 {at16} B, minute 29 {end} B a session");
    let c = cfg();
    assert!(end <= ((c.post.bidi_streams + c.post.uni_streams) as usize * super::door::STREAM_STATE) as i64, "{end}");
    assert!(end - at16 <= GROWTH, "a session grew {} B from minute 16 to 29", end - at16);
}
