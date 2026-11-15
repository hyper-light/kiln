//! A connection's seal, on keys a stand-in exporter gives (docs/design/hex-quic.md Q13; the review's E-6; AC-T7.4.1):
//! its label and contexts, its two ways, its nonces under concurrent sealing, its tamper refusals and its last nonce.
//! The keys a real connection's TLS session gives are `crate::tests`' (HEX-QUIC-32).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use hex_core::wire::{self, Seal, SealFault};

use super::*;

/// Each label and context an exporter was asked for, in order.
type Asked = Arc<Mutex<Vec<(Vec<u8>, Vec<u8>)>>>;

/// What a connection's TLS exporter gives (RFC 8446 §7.5) as both its ends see it: one session's keying material for each
/// label and context, the same at each end, another for another session, each ask recorded.
#[derive(Clone)]
struct Exporter {
    session: u8,
    asked: Asked,
}

impl Exporter {
    fn new(session: u8) -> Self {
        Self {
            session,
            asked: Arc::default(),
        }
    }

    fn keys(&self, side: Side) -> Keys {
        let me = self.clone();
        Keys::derive(side, move |label, context, out| {
            use sha2::Digest;
            me.asked
                .lock()
                .unwrap()
                .push((label.to_vec(), context.to_vec()));
            let made = sha2::Sha256::new()
                .chain_update([me.session])
                .chain_update(label)
                .chain_update(context)
                .finalize();
            out.copy_from_slice(&made[..out.len()]);
            Ok(())
        })
        .unwrap()
    }
}

/// Seals `body` with `keys` under `aad`: the nonce, the sealed body and the tag.
fn sealed(keys: &Keys, aad: &[u8], body: &[u8]) -> ([u8; NONCE_BYTES], Vec<u8>, [u8; TAG_BYTES]) {
    let mut out = vec![0u8; body.len()];
    let (nonce, tag) = keys.seal(aad, body, &mut out).unwrap();
    (nonce, out, tag)
}

/// Opens what `sealed` made, with `keys`.
fn opened(
    keys: &Keys,
    aad: &[u8],
    (nonce, body, tag): &([u8; NONCE_BYTES], Vec<u8>, [u8; TAG_BYTES]),
) -> Result<Vec<u8>, SealFault> {
    let mut out = vec![0u8; body.len()];
    keys.open(aad, nonce, body, tag, &mut out).map(|()| out)
}

/// Q13, ROOT's note (1), 2026-11-14: do derive a connection's keys at its dialer and at its acceptor from one TLS session,
/// and at a dialer of another session. Expect each key asked of the exporter under the label `EXPORTER-hex-wire-seal-v1`
/// alone and the two ways' contexts alone, each end asking both, 32 bytes each (AES-256's key); what either end seals the
/// other opens; what an end seals it can't open itself, the two ways' keys differing; and what one session's dialer seals
/// another session's acceptor can't open.
#[test]
fn q13_both_ends_derive_each_ways_key_alike_and_the_two_ways_apart() {
    let session = Exporter::new(1);
    let (dialer, acceptor) = (session.keys(Side::Dialer), session.keys(Side::Acceptor));
    let asked = session.asked.lock().unwrap().clone();
    let labels: BTreeSet<&[u8]> = asked.iter().map(|(label, _)| label.as_slice()).collect();
    assert_eq!(labels, BTreeSet::from([&b"EXPORTER-hex-wire-seal-v1"[..]]));
    let contexts: Vec<&[u8]> = asked
        .iter()
        .map(|(_, context)| context.as_slice())
        .collect();
    assert_eq!(
        contexts,
        vec![FROM_DIALER, FROM_ACCEPTOR, FROM_ACCEPTOR, FROM_DIALER],
        "each end its own way's first"
    );
    assert_eq!(KEY_BYTES, AES_256_GCM.key_len());
    let (aad, body) = (b"header".as_slice(), b"a lease's value".as_slice());
    let up = sealed(&dialer, aad, body);
    assert_eq!(opened(&acceptor, aad, &up).unwrap(), body);
    let down = sealed(&acceptor, aad, body);
    assert_eq!(opened(&dialer, aad, &down).unwrap(), body);
    assert_ne!(up.1, down.1, "the two ways' keys differ");
    assert_eq!(opened(&dialer, aad, &up), Err(SealFault::Broken));
    assert_eq!(opened(&acceptor, aad, &down), Err(SealFault::Broken));
    let other = Exporter::new(2).keys(Side::Acceptor);
    assert_eq!(opened(&other, aad, &up), Err(SealFault::Broken));
}

/// Q13, ROOT's note (2), 2026-11-14: do seal from 16 threads at once with one end's keys, 1,000 bodies each, and open each
/// at the other end. Expect 16,000 nonces, every one its own, each the counter's value padded to 96 bits (its first four
/// bytes zero), the counter at 16,000 after, and every body opened.
#[test]
fn q13_a_ways_nonce_is_never_taken_twice_by_many_sealing_at_once() {
    let session = Exporter::new(3);
    let dialer = Arc::new(session.keys(Side::Dialer));
    let acceptor = session.keys(Side::Acceptor);
    let (threads, each) = (16u64, 1_000u64);
    let made: Vec<_> = (0..threads)
        .map(|t| {
            let dialer = Arc::clone(&dialer);
            std::thread::spawn(move || {
                (0..each)
                    .map(|i| {
                        let body = (t * each + i).to_le_bytes();
                        (body, sealed(&dialer, b"aad", &body))
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    let nonces: BTreeSet<[u8; NONCE_BYTES]> = made.iter().map(|(_, (nonce, ..))| *nonce).collect();
    assert_eq!(nonces.len() as u64, threads * each);
    assert!(nonces.iter().all(|nonce| nonce[..4] == [0; 4]));
    assert_eq!(dialer.next.load(Ordering::Relaxed), threads * each);
    for (body, seal) in &made {
        assert_eq!(opened(&acceptor, b"aad", seal).unwrap(), body);
    }
}

/// Q13 (AC-T7.4.1): do flip each bit of a sealed body's nonce, body and tag, and of the bytes bound to it, and open each.
/// Expect each refused `Broken`, and the body as sealed opened only as it was.
#[test]
fn q13_a_seal_tampered_with_is_refused_typed() {
    let session = Exporter::new(4);
    let (dialer, acceptor) = (session.keys(Side::Dialer), session.keys(Side::Acceptor));
    let aad = b"HEXW header bound".to_vec();
    let seal = sealed(&dialer, &aad, b"refresh token");
    assert_eq!(opened(&acceptor, &aad, &seal).unwrap(), b"refresh token");
    for bit in 0..NONCE_BYTES * 8 {
        let mut changed = seal.clone();
        changed.0[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(opened(&acceptor, &aad, &changed), Err(SealFault::Broken));
    }
    for bit in 0..seal.1.len() * 8 {
        let mut changed = seal.clone();
        changed.1[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(opened(&acceptor, &aad, &changed), Err(SealFault::Broken));
    }
    for bit in 0..TAG_BYTES * 8 {
        let mut changed = seal.clone();
        changed.2[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(opened(&acceptor, &aad, &changed), Err(SealFault::Broken));
    }
    for bit in 0..aad.len() * 8 {
        let mut bound = aad.clone();
        bound[bit / 8] ^= 1 << (bit % 8);
        assert_eq!(opened(&acceptor, &bound, &seal), Err(SealFault::Broken));
    }
}

/// Q13 (AC-T7.4.1): a way's nonces end before any repeats. Do seal with keys whose next nonce is the last the counter
/// takes (its greatest value less one: taking it leaves the counter at its greatest), then seal again. Expect the first
/// sealed under that nonce and opened, and the second refused `Spent` with nothing sealed, again on every later seal; and
/// through the wire's seal, a lease's message refused so.
#[test]
fn q13_a_way_whose_nonces_are_spent_seals_nothing_more() {
    let session = Exporter::new(5);
    let dialer = session.keys(Side::Dialer).at(u64::MAX - 1);
    let acceptor = session.keys(Side::Acceptor);
    let first = sealed(&dialer, b"aad", b"one");
    assert_eq!(
        first.0,
        [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]
    );
    assert_eq!(opened(&acceptor, b"aad", &first).unwrap(), b"one");
    let mut out = [0u8; 3];
    for _ in 0..3 {
        assert_eq!(dialer.seal(b"aad", b"two", &mut out), Err(SealFault::Spent));
    }
    assert_eq!(out, [0; 3], "nothing sealed");
    let leased = wire::request(
        1,
        &wire::Request::Leased {
            run: 1,
            answer: Err(hex_api::lease::LeaseRefused::Ended),
        },
        wire::MAX_MESSAGE,
    )
    .unwrap();
    assert_eq!(
        wire::seal(&leased, &dialer),
        Err(wire::Fault::Seal(SealFault::Spent))
    );
}

/// Q13, RFC 8446 §5.5 (ROOT, 2026-11-14; AC-T7.4.1): a key seals no more than AES-GCM's limit there, 2^24.5 full records
/// of 2^14 bytes, 2^34.5 AES blocks, for an AE margin of about 2^-57 (Luykx and Paterson's AEBounds analysis, on which
/// §5.5 rests). Do seal at an end whose key has sealed the limit's floor less two blocks: a 16-byte body, one block and one
/// more for its tag, which brings it to the limit; then a 1-byte body. Expect the first sealed and opened, and the second
/// refused `Spent` with nothing sealed and no nonce taken, again on every later seal.
#[test]
fn q13_a_key_seals_no_more_than_aes_gcms_limit() {
    // ⌊2^34.5⌋ = ⌊√(2^69)⌋: 2^24.5 records of 2^14 bytes, 2^10 blocks of 16 bytes each.
    let limit = u64::try_from((1u128 << 69).isqrt()).unwrap();
    let session = Exporter::new(6);
    let dialer = session.keys(Side::Dialer).having_sealed(limit - 2);
    let acceptor = session.keys(Side::Acceptor);
    let first = sealed(&dialer, b"aad", &[7; 16]);
    assert_eq!(opened(&acceptor, b"aad", &first).unwrap(), [7; 16]);
    let mut out = [0u8; 1];
    for _ in 0..3 {
        assert_eq!(dialer.seal(b"aad", &[1], &mut out), Err(SealFault::Spent));
    }
    assert_eq!(out, [0], "nothing sealed");
    assert_eq!(
        dialer.next.load(Ordering::Relaxed),
        1,
        "the first seal's nonce alone taken"
    );
}
