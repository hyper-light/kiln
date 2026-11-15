//! A connection's seal (docs/design/hex-quic.md Q13; the review's E-6): the keys each message of the node's wire that
//! carries a secret is sealed under before it reaches Hex's lanes ([`hex_core::wire::seal`]), so what hyper-quic's send
//! buffer and hyper-transport's receive pool hold of it, which Hex can't wipe, is ciphertext.
//!
//! AES-256-GCM, a key each way, each the connection's TLS exporter's (RFC 8446 §7.5) under Hex's own label and its
//! way's context: both ends derive each way's key alike, the two ways differ, and every key is its connection's alone.
//! A way's nonce is a 96-bit counter, RFC 8446 §5.3's construction (a 64-bit sequence number padded to the left with
//! zeros), taken atomically, never twice, and refused before it would wrap. A key seals no more than AES-GCM's limit in
//! RFC 8446 §5.5, refused past it as a spent nonce is: a connection whose key is spent carries no more secrets, and its
//! client dials again, the new connection's TLS session giving fresh keys. A receiver opens with the nonce the message
//! carries, in whatever order messages come: their order and replay are the connection's TLS's to hold. The seal is no
//! second channel security, the connection's TLS being that: it keeps a secret out of buffers Hex can't wipe.

use std::sync::atomic::{AtomicU64, Ordering};

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use hex_core::wire::{self, NONCE_BYTES, SealFault, TAG_BYTES};

use crate::Refused;

// The label, both ways' contexts and the key's length: every input to a key, here alone.
/// The exporter's label (RFC 8446 §7.5; RFC 5705 §4: an application's own, beginning "EXPORTER"): Hex's wire's seal,
/// version 1. A change to how a key is derived or used names another.
const LABEL: &[u8] = b"EXPORTER-hex-wire-seal-v1";
/// The context of the way from the side that dialed, the connection's TLS client, to the side that accepted.
const FROM_DIALER: &[u8] = b"from the dialer";
/// The context of the way from the side that accepted, the connection's TLS server, to the side that dialed.
const FROM_ACCEPTOR: &[u8] = b"from the acceptor";
/// A way's key: AES-256's 256 bits (FIPS 197 §5).
const KEY_BYTES: usize = 32;
/// AES's block: 128 bits (FIPS 197 §5), what a key's limit counts.
const BLOCK_BYTES: usize = 16;
/// The AES blocks one key seals at most: AES-GCM's limit in RFC 8446 §5.5, "up to 2^24.5 full-size records (about 24
/// million) ... while keeping a safety margin of approximately 2^-57 for Authenticated Encryption (AE) security", which
/// rests on Luykx and Paterson's AEBounds analysis ("Limits on Authenticated Encryption Use in TLS", 2017). A full record
/// is 2^14 bytes, 2^10 blocks, so 2^34.5 blocks: its floor, √(2^69).
const LIMIT: u128 = (1u128 << 69).isqrt();

/// Which side of its connection an end is: the one that dialed, its TLS client, or the one that accepted, its server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Dialer,
    Acceptor,
}

/// One connection's seal, as one end holds it: the key it seals under, the key its peer seals under, the next nonce it
/// seals with, and the AES blocks it has sealed, to [`LIMIT`].
pub struct Keys {
    sending: LessSafeKey,
    receiving: LessSafeKey,
    next: AtomicU64,
    sealed: AtomicU64,
}

impl Keys {
    /// The keys of a connection whose `side` this end is, each way's from `export`: the connection's TLS exporter,
    /// filling its buffer with the keying material of the label and context it is given.
    pub(crate) fn derive(
        side: Side,
        mut export: impl FnMut(&[u8], &[u8], &mut [u8]) -> Result<(), Refused>,
    ) -> Result<Self, Refused> {
        let (mine, theirs) = match side {
            Side::Dialer => (FROM_DIALER, FROM_ACCEPTOR),
            Side::Acceptor => (FROM_ACCEPTOR, FROM_DIALER),
        };
        Ok(Self {
            sending: key(&mut export, mine)?,
            receiving: key(&mut export, theirs)?,
            next: AtomicU64::new(0),
            sealed: AtomicU64::new(0),
        })
    }

    /// Keys that have sealed `blocks` AES blocks already: a test's of a key near its limit.
    #[cfg(test)]
    pub(crate) fn having_sealed(mut self, blocks: u64) -> Self {
        self.sealed = AtomicU64::new(blocks);
        self
    }

    /// Keys whose next nonce is `next`: a test's of a way near its last.
    #[cfg(test)]
    pub(crate) fn at(mut self, next: u64) -> Self {
        self.next = AtomicU64::new(next);
        self
    }

    /// Charges a seal of `body` to its key's [`LIMIT`], atomically, so seals made at once never pass it together: the AES
    /// blocks GCM runs for it, its body's and one more for its tag (NIST SP 800-38D §7.1: GCTR over the body from J0's
    /// successor, and J0 itself for the tag). Refused, with nothing charged, where they would pass the limit.
    fn charge(&self, body: &[u8]) -> Result<(), SealFault> {
        let blocks = body
            .len()
            .div_ceil(BLOCK_BYTES)
            .checked_add(1)
            .and_then(|blocks| u64::try_from(blocks).ok())
            .ok_or(SealFault::Spent)?;
        self.sealed
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |sealed| {
                sealed
                    .checked_add(blocks)
                    .filter(|total| u128::from(*total) <= LIMIT)
            })
            .map(drop)
            .map_err(|_| SealFault::Spent)
    }

    /// The next nonce this end seals with, taken atomically, so no two seals take one however many seal at once: its
    /// count big-endian, padded to the left with zeros to the nonce's 96 bits (RFC 8446 §5.3). Refused once the last is
    /// taken, so none repeats.
    fn nonce(&self) -> Result<[u8; NONCE_BYTES], SealFault> {
        let taken = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| SealFault::Spent)?;
        let mut nonce = [0u8; NONCE_BYTES];
        if let Some((_, count)) = nonce.split_last_chunk_mut::<{ size_of::<u64>() }>() {
            *count = taken.to_be_bytes();
        }
        Ok(nonce)
    }
}

/// One way's key: `LABEL`'s keying material under `context` from `export`, as AES-256-GCM's key, the material wiped
/// once the key holds it.
fn key(
    export: &mut impl FnMut(&[u8], &[u8], &mut [u8]) -> Result<(), Refused>,
    context: &[u8],
) -> Result<LessSafeKey, Refused> {
    let mut material = [0u8; KEY_BYTES];
    let made = export(LABEL, context, &mut material).and_then(|()| {
        UnboundKey::new(&AES_256_GCM, &material).map_err(|_| Refused::Configuration {
            what: "a connection's seal",
            why: "AES-256-GCM refused the key its connection's exporter gave".to_owned(),
        })
    });
    hex_api::lease::wipe_in_place(&mut material);
    made.map(LessSafeKey::new)
}

impl wire::Seal for Keys {
    fn seal(
        &self,
        aad: &[u8],
        body: &[u8],
        sealed: &mut [u8],
    ) -> Result<([u8; NONCE_BYTES], [u8; TAG_BYTES]), SealFault> {
        self.charge(body)?;
        let nonce = self.nonce()?;
        let mut tag = [0u8; TAG_BYTES];
        self.sending
            .seal_out_of_place_scatter(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                body,
                sealed,
                &[],
                &mut tag,
            )
            .map_err(|_| SealFault::Cipher {
                what: "a body to seal",
            })?;
        Ok((nonce, tag))
    }

    fn open(
        &self,
        aad: &[u8],
        nonce: &[u8; NONCE_BYTES],
        sealed: &[u8],
        tag: &[u8; TAG_BYTES],
        body: &mut [u8],
    ) -> Result<(), SealFault> {
        self.receiving
            .open_separate_gather(
                Nonce::assume_unique_for_key(*nonce),
                Aad::from(aad),
                sealed,
                tag,
                body,
            )
            .map_err(|_| SealFault::Broken)
    }
}

#[cfg(test)]
mod tests;
