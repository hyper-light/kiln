//! The trail (docs/ci.md W27-R09), the pure part: one append-only log of every signed statement
//! kiln takes, in the order it takes them.
//!
//! - **An entry** names the statement it records by its digest (SHA-256 over its PAE:
//!   [`super::statement::digest`]), its kind (the statement's payload type, or what kiln checked
//!   for what isn't one), the things it's about by digest, when kiln took it, its index from 0, and
//!   the hash of the entry before it. The entries are a hash chain, as gittuf's reference state log
//!   chains its entries: one edited, dropped, inserted or moved breaks it where it happened
//!   ([`check`]).
//! - **The tree.** The entries are the leaves of an RFC 6962 Merkle tree ([`root`]), an entry's
//!   hash its leaf hash: an entry's inclusion in a tree of n, and a tree's consistency with an
//!   earlier one, are proven in at most ceil(log2 n) + 1 hashes ([`inclusion`], [`consistency`],
//!   checked by RFC 9162's algorithms in [`verify_inclusion`] and [`verify_consistency`]). A chain
//!   forged whole, every hash made again, still doesn't make the root kiln signed ([`check_root`]).
//! - **A checkpoint** is a C2SP checkpoint in a signed note ([`Checkpoint`]): the trail's origin
//!   (`kiln://<org>/trail`, its key's name too), its size and its root, signed by kiln's server
//!   with its P-256 key (signed-note's type 0x02, its key ID the hash of its public key, as Rekor's
//!   checkpoints are), under a certificate that verifies to kiln's root ([`signed`],
//!   [`verify_note`]). kiln signs no checkpoint that doesn't extend the one before ([`extends`]),
//!   and signs one once entries have waited long enough or come in number enough ([`due`]).
//! - **What an entry says** of its statement comes from the statement ([`about`]). What kiln checks
//!   that isn't a statement itself (a pushed commit's signature, what a host verified, a
//!   revocation) goes in as kiln's own statement of it ([`PAYLOAD_CHECKED`],
//!   [`PAYLOAD_REVOCATION`]), unsigned: the checkpoints over the trail vouch for it.

use base64::Engine as _;
use const_oid::db::rfc5912::{ID_EC_PUBLIC_KEY, SECP_256_R_1};
use der::asn1::ObjectIdentifier;
use der::{Decode as _, Encode as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::chain::{self, Actor, Identity, Revocation, Verified};
use super::statement::{Statement, Subject};

/// A SHA-256 hash.
pub type Hash = [u8; 32];

/// What the first entry names as the hash before it.
pub const ZERO: Hash = [0; 32];

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// One entry of the trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its place in the trail, from 0.
    pub index: u64,
    /// The hash of the entry before it ([`ZERO`] for the first).
    pub prev: Hash,
    /// The statement it records, by digest (`sha256:<hex>`).
    pub statement: String,
    /// What kind of statement it is: its payload type, or what kiln checked (`commit`).
    pub kind: String,
    /// What it's about, each by digest (`sha256:<hex>`, `gitCommit:<hex>`).
    pub subjects: Vec<String>,
    /// When kiln took it (unix seconds): the time it's checked at again.
    pub taken_at: i64,
}

impl Entry {
    /// What's hashed (the leaf's data, as a tiled log's entry bundles carry it): a line each,
    /// under a version. No field holds a newline ([`next`] and [`check`] refuse one that does), so
    /// no two entries encode alike.
    pub fn encoding(&self) -> Vec<u8> {
        let mut s = format!("kiln trail entry v1\n{}\n{}\n{}\n{}\n{}\n", self.index, chain::hex(&self.prev), self.statement, self.kind, self.taken_at);
        for subject in &self.subjects {
            s.push_str(subject);
            s.push('\n');
        }
        s.into_bytes()
    }

    /// Its hash: RFC 6962's leaf hash of its encoding, which the next entry names and the tree is
    /// built over.
    pub fn hash(&self) -> Hash {
        leaf(&self.encoding())
    }

    /// An entry read back from its encoding (an entry bundle's), or why it isn't one kiln writes.
    pub fn parse(data: &[u8]) -> Result<Entry, Break> {
        let bad = |why: &str| Break::Malformed { index: 0, why: why.to_owned() };
        let text = std::str::from_utf8(data).map_err(|_| bad("it isn't UTF-8"))?;
        let body = text.strip_suffix('\n').ok_or_else(|| bad("it doesn't end in a newline"))?;
        let mut lines = body.split('\n');
        let (Some("kiln trail entry v1"), Some(index), Some(prev), Some(statement), Some(kind), Some(taken_at)) = (lines.next(), lines.next(), lines.next(), lines.next(), lines.next(), lines.next()) else {
            return Err(bad("it isn't version 1 of kiln's trail entries"));
        };
        let index: u64 = index.parse().map_err(|_| bad("its index isn't a number"))?;
        let prev = unhex(prev).and_then(|p| <Hash>::try_from(p.as_slice()).ok()).ok_or_else(|| bad("its prev isn't a SHA-256 in hex"))?;
        let taken_at: i64 = taken_at.parse().map_err(|_| bad("when kiln took it isn't a number"))?;
        let e = Entry { index, prev, statement: statement.to_owned(), kind: kind.to_owned(), subjects: lines.map(str::to_owned).collect(), taken_at };
        match e.malformed() {
            Some(why) => Err(Break::Malformed { index, why }),
            None if e.encoding() != data => Err(Break::Malformed { index, why: "it isn't written as kiln writes it".into() }),
            None => Ok(e),
        }
    }

    /// Why it isn't an entry kiln writes, when it isn't.
    fn malformed(&self) -> Option<String> {
        let line = |s: &str| !s.is_empty() && !s.chars().any(char::is_control);
        if !self.statement.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Some(format!("its statement {:?} isn't a digest (sha256:<64 hex>)", self.statement));
        }
        if !line(&self.kind) {
            return Some(format!("its kind {:?} is empty, or more than a line", self.kind));
        }
        if let Some(bad) = self.subjects.iter().find(|s| !line(s) || !s.contains(':')) {
            return Some(format!("its subject {bad:?} isn't a digest (<algorithm>:<hex>)"));
        }
        if self.index == 0 && self.prev != ZERO {
            return Some("it's the first entry, yet it names an entry before it".to_owned());
        }
        None
    }
}

/// The entry that records `statement` after `last` (none: the trail's first), or why it can't be
/// one: kiln writes no entry [`check`] would refuse, whatever a statement's payload type says.
pub fn next(last: Option<&Entry>, statement: &str, kind: &str, subjects: Vec<String>, taken_at: i64) -> Result<Entry, Break> {
    let e = Entry { index: last.map_or(0, |e| e.index.saturating_add(1)), prev: last.map_or(ZERO, Entry::hash), statement: statement.to_owned(), kind: kind.to_owned(), subjects, taken_at };
    match e.malformed() {
        Some(why) => Err(Break::Malformed { index: e.index, why }),
        None => Ok(e),
    }
}

/// Where a run of entries doesn't hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Break {
    #[error("entry {index} isn't one kiln writes: {why}")]
    Malformed { index: u64, why: String },
    #[error("the trail starts at entry {index}, not entry 0")]
    Start { index: u64 },
    #[error("entry {index} comes right after entry {after}: entry {missing} is missing")]
    Missing { index: u64, after: u64, missing: u64 },
    #[error("there are two entries {index}: one was inserted")]
    Inserted { index: u64 },
    #[error("entry {index} comes after entry {after}: the trail is out of order")]
    OutOfOrder { index: u64, after: u64 },
    #[error("entry {index} doesn't hash to what entry {next} names")]
    Edited { index: u64, next: u64 },
    #[error("the trail's first {size} entries don't make the root its checkpoint names")]
    Root { size: u64 },
}

/// Whether `entries` hold as a run of the trail after `before` (none: from its start): each well
/// formed, numbered one after the other, each naming the hash of the one before. The first link
/// that doesn't hold is named.
pub fn check(before: Option<&Entry>, entries: &[Entry]) -> Result<(), Break> {
    let mut last = before;
    for (i, e) in entries.iter().enumerate() {
        if let Some(why) = e.malformed() {
            return Err(Break::Malformed { index: e.index, why });
        }
        match last {
            None if e.index != 0 => return Err(Break::Start { index: e.index }),
            None => {}
            Some(p) => {
                let want = p.index.saturating_add(1);
                if e.index > want {
                    // The one it skips comes later: moved, not dropped.
                    let later = entries.iter().skip(i + 1).any(|x| x.index == want);
                    return Err(if later { Break::OutOfOrder { index: e.index, after: p.index } } else { Break::Missing { index: e.index, after: p.index, missing: want } });
                }
                if e.index == p.index {
                    return Err(Break::Inserted { index: e.index });
                }
                if e.index < want {
                    return Err(Break::OutOfOrder { index: e.index, after: p.index });
                }
                if e.prev != p.hash() {
                    return Err(Break::Edited { index: p.index, next: e.index });
                }
            }
        }
        last = Some(e);
    }
    Ok(())
}

/// Whether `entries`, the trail from its start, hold, and their first `checkpoint.size` make the
/// root it names: a chain forged whole (each hash made again) doesn't, nor does a last entry
/// changed, which no entry after it names.
pub fn check_root(entries: &[Entry], checkpoint: &Checkpoint) -> Result<(), Break> {
    check(None, entries)?;
    let size = usize::try_from(checkpoint.size).unwrap_or(usize::MAX);
    let leaves: Vec<Hash> = entries.iter().take(size).map(Entry::hash).collect();
    if leaves.len() != size || root(&leaves) != checkpoint.root {
        return Err(Break::Root { size: checkpoint.size });
    }
    Ok(())
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

fn sha(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    <Hash>::try_from(h.finalize().as_slice()).unwrap_or(ZERO)
}

/// RFC 6962's hash of a leaf: SHA-256 of 0x00 and its data.
pub fn leaf(data: &[u8]) -> Hash {
    sha(&[&[0], data])
}

fn node(left: &Hash, right: &Hash) -> Hash {
    sha(&[&[1], left, right])
}

/// The largest power of two smaller than `n` (more than 1).
fn split(n: usize) -> usize {
    let mut k = 1;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

fn split64(n: u64) -> u64 {
    let mut k = 1;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// The root of the tree over `leaves` (RFC 6962's Merkle Tree Hash): SHA-256 of nothing for none.
pub fn root(leaves: &[Hash]) -> Hash {
    match leaves {
        [] => sha(&[]),
        [one] => *one,
        _ => {
            let (l, r) = leaves.split_at(split(leaves.len()));
            node(&root(l), &root(r))
        }
    }
}

/// The proof that leaf `index` is in the tree over `leaves` (RFC 6962's audit path); none for an
/// index past its end.
pub fn inclusion(leaves: &[Hash], index: usize) -> Option<Vec<Hash>> {
    inclusion_by(leaves.len() as u64, index as u64, &mut of_leaves(leaves))
}

/// RFC 6962's hash of the leaves `lo..hi` of `leaves`.
fn of_leaves(leaves: &[Hash]) -> impl FnMut(u64, u64) -> Option<Hash> + '_ {
    |lo, hi| leaves.get(usize::try_from(lo).ok()?..usize::try_from(hi).ok()?).map(root)
}

/// [`inclusion`] in a tree of `size`, its hashes from `mth(lo, hi)`, RFC 6962's hash of its leaves
/// `lo..hi` (from its tiles, say: [`mth_from`]); none for an index past its end, or a hash not
/// given.
pub fn inclusion_by(size: u64, index: u64, mth: &mut impl FnMut(u64, u64) -> Option<Hash>) -> Option<Vec<Hash>> {
    fn walk(m: u64, lo: u64, hi: u64, mth: &mut impl FnMut(u64, u64) -> Option<Hash>, path: &mut Vec<Hash>) -> Option<()> {
        if hi - lo <= 1 {
            return Some(());
        }
        let k = split64(hi - lo);
        if m < k {
            walk(m, lo, lo + k, mth, path)?;
            path.push(mth(lo + k, hi)?);
        } else {
            walk(m - k, lo + k, hi, mth, path)?;
            path.push(mth(lo, lo + k)?);
        }
        Some(())
    }
    if index >= size {
        return None;
    }
    let mut path = Vec::new();
    walk(index, 0, size, mth, &mut path)?;
    Some(path)
}

/// Whether `proof` shows `leaf` at `index` of a tree of `size` whose root is `root` (RFC 9162,
/// 2.1.3.2).
pub fn verify_inclusion(leaf: &Hash, index: u64, size: u64, proof: &[Hash], root: &Hash) -> bool {
    if index >= size {
        return false;
    }
    let (mut f, mut s) = (index, size - 1);
    let mut r = *leaf;
    for p in proof {
        if s == 0 {
            return false;
        }
        if f & 1 == 1 || f == s {
            r = node(p, &r);
            while f & 1 == 0 && f != 0 {
                f >>= 1;
                s >>= 1;
            }
        } else {
            r = node(&r, p);
        }
        f >>= 1;
        s >>= 1;
    }
    s == 0 && r == *root
}

/// The proof that the tree over `leaves` extends the tree over its first `old` (RFC 6962's
/// consistency proof); none for an `old` past its end.
pub fn consistency(leaves: &[Hash], old: usize) -> Option<Vec<Hash>> {
    consistency_by(leaves.len() as u64, old as u64, &mut of_leaves(leaves))
}

/// [`consistency`] of a tree of `size` with its first `old`, its hashes from `mth(lo, hi)` as
/// [`inclusion_by`] has them.
pub fn consistency_by(size: u64, old: u64, mth: &mut impl FnMut(u64, u64) -> Option<Hash>) -> Option<Vec<Hash>> {
    fn sub(m: u64, lo: u64, hi: u64, whole: bool, mth: &mut impl FnMut(u64, u64) -> Option<Hash>, proof: &mut Vec<Hash>) -> Option<()> {
        if m == hi - lo {
            if !whole {
                proof.push(mth(lo, hi)?);
            }
            return Some(());
        }
        let k = split64(hi - lo);
        if m <= k {
            sub(m, lo, lo + k, whole, mth, proof)?;
            proof.push(mth(lo + k, hi)?);
        } else {
            sub(m - k, lo + k, hi, false, mth, proof)?;
            proof.push(mth(lo, lo + k)?);
        }
        Some(())
    }
    if old > size {
        return None;
    }
    let mut proof = Vec::new();
    if old > 0 {
        sub(old, 0, size, true, mth, &mut proof)?;
    }
    Some(proof)
}

/// How many hashes a full tile holds (C2SP tlog-tiles): a subtree of height 8.
pub const TILE: u64 = 256;

/// A tile of the tree, or an entry bundle (C2SP tlog-tiles): its level, its index on that level,
/// and its width, [`TILE`] for a full one. At level 0 its hashes are leaves'; above, each is the
/// hash of a full tile at the level below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub level: u8,
    pub index: u64,
    pub width: u64,
}

impl Tile {
    /// Its path under the log's prefix: `tile/<L>/<N>[.p/<W>]`, an entry bundle's
    /// `tile/entries/<N>[.p/<W>]`; `<N>` in 3-digit elements, all but the last after an `x`.
    pub fn path(&self, entries: bool) -> String {
        let mut elements = vec![self.index % 1000];
        let mut n = self.index / 1000;
        while n > 0 {
            elements.push(n % 1000);
            n /= 1000;
        }
        let last = elements.len().saturating_sub(1);
        let n: Vec<String> = elements.iter().rev().enumerate().map(|(i, e)| if i < last { format!("x{e:03}") } else { format!("{e:03}") }).collect();
        let head = if entries { "tile/entries".to_owned() } else { format!("tile/{}", self.level) };
        let partial = if self.width < TILE { format!(".p/{}", self.width) } else { String::new() };
        format!("{head}/{}{partial}", n.join("/"))
    }

    /// A tile's path read back (after the log's prefix): the tile, and whether it's an entry
    /// bundle. One written otherwise than [`Tile::path`] writes it isn't read.
    pub fn parse(path: &str) -> Option<(Tile, bool)> {
        // Read loosely, then held to how it's written: no leading zeroes, no signs.
        let rest = path.strip_prefix("tile/")?;
        let (entries, level, rest) = match rest.strip_prefix("entries/") {
            Some(r) => (true, 0u8, r),
            None => {
                let (l, r) = rest.split_once('/')?;
                (false, l.parse::<u8>().ok().filter(|l| *l <= 63)?, r)
            }
        };
        let (n, width) = match rest.split_once(".p/") {
            Some((n, w)) => (n, w.parse::<u64>().ok().filter(|w| (1..TILE).contains(w))?),
            None => (rest, TILE),
        };
        let mut index: u64 = 0;
        for e in n.split('/') {
            let e = e.strip_prefix('x').unwrap_or(e);
            index = index.checked_mul(1000)?.checked_add(e.parse::<u64>().ok().filter(|_| e.len() == 3 && e.bytes().all(|b| b.is_ascii_digit()))?)?;
        }
        let t = Tile { level, index, width };
        (t.path(entries) == path).then_some((t, entries))
    }

    /// The leaves its hashes cover: from its first to past its last.
    pub fn span(&self) -> Option<(u64, u64)> {
        let per = TILE.checked_pow(u32::from(self.level))?;
        let first = self.index.checked_mul(TILE)?;
        Some((first.checked_mul(per)?, first.checked_add(self.width)?.checked_mul(per)?))
    }
}

/// How wide tile `index` at `level` is in a tree of `size`: [`TILE`] while it's full, the partial
/// width `floor(size / 256^level) mod 256` for the one the tree ends in, none past it (or empty).
pub fn width_at(size: u64, level: u8, index: u64) -> Option<u64> {
    let per = TILE.checked_pow(u32::from(level))?;
    let hashes = size / per;
    let full = hashes / TILE;
    match index.cmp(&full) {
        std::cmp::Ordering::Less => Some(TILE),
        std::cmp::Ordering::Equal => Some(hashes % TILE).filter(|w| *w > 0),
        std::cmp::Ordering::Greater => None,
    }
}

/// RFC 6962's hash of leaves `lo..hi` of a tree of `size`, from its tiles: `tile(t)` gives tile
/// `t`'s hashes, all `t.width` of them. The range splits as the tree does from its root, so its
/// left parts are whole subtrees, read from a tile and hashed up within it. None for a range the
/// tree hasn't, or a tile not given.
pub fn mth_from(size: u64, lo: u64, hi: u64, tile: &mut impl FnMut(Tile) -> Option<Vec<Hash>>) -> Option<Hash> {
    let n = hi.checked_sub(lo).filter(|n| *n > 0 && hi <= size)?;
    if n.is_power_of_two() && lo.is_multiple_of(n) {
        // A whole subtree of height h: 2^(h mod 8) hashes of one tile at level h / 8.
        let height = n.trailing_zeros();
        let level = u8::try_from(height / 8).ok()?;
        let count = 1u64 << (height % 8);
        let first = (lo / n).checked_mul(count)?;
        let t = Tile { level, index: first / TILE, width: width_at(size, level, first / TILE)? };
        let at = first % TILE;
        if at + count > t.width {
            return None;
        }
        let hashes = tile(t)?;
        return hashes.get(usize::try_from(at).ok()?..usize::try_from(at + count).ok()?).map(root);
    }
    let k = split64(n);
    Some(node(&mth_from(size, lo, lo + k, tile)?, &mth_from(size, lo + k, hi, tile)?))
}

/// Tile `t`'s hashes from its bytes (32 each); none when they aren't `t.width` of them.
pub fn hashes_of(t: &Tile, bytes: &[u8]) -> Option<Vec<Hash>> {
    let want = usize::try_from(t.width).ok()?.checked_mul(32)?;
    (bytes.len() == want).then(|| bytes.as_chunks::<32>().0.to_vec())
}

/// An entry bundle (C2SP tlog-tiles): each entry's data, a big-endian 16-bit length before it;
/// none when one is longer than 65,535 bytes.
pub fn bundle(entries: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for e in entries {
        out.extend(u16::try_from(e.len()).ok()?.to_be_bytes());
        out.extend(e);
    }
    Some(out)
}

/// An entry bundle read back: each entry's data; none when it's cut short.
pub fn unbundle(mut bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some((len, rest)) = bytes.split_first_chunk::<2>() {
        let (entry, rest) = rest.split_at_checked(usize::from(u16::from_be_bytes(*len)))?;
        out.push(entry.to_vec());
        bytes = rest;
    }
    bytes.is_empty().then_some(out)
}

/// Whether `proof` shows the tree of `second` leaves (root `second_root`) extends the tree of its
/// first `first` (root `first_root`) (RFC 9162, 2.1.4.2; the empty tree is extended by every one).
pub fn verify_consistency(first: u64, second: u64, first_root: &Hash, second_root: &Hash, proof: &[Hash]) -> bool {
    if first > second {
        return false;
    }
    if first == second {
        return proof.is_empty() && first_root == second_root;
    }
    if first == 0 {
        return proof.is_empty();
    }
    if proof.is_empty() {
        return false;
    }
    let mut path: Vec<Hash> = Vec::with_capacity(proof.len() + 1);
    if first.is_power_of_two() {
        path.push(*first_root);
    }
    path.extend_from_slice(proof);
    let (mut f, mut s) = (first - 1, second - 1);
    while f & 1 == 1 {
        f >>= 1;
        s >>= 1;
    }
    let Some((head, rest)) = path.split_first() else { return false };
    let (mut fr, mut sr) = (*head, *head);
    for c in rest {
        if s == 0 {
            return false;
        }
        if f & 1 == 1 || f == s {
            fr = node(c, &fr);
            sr = node(c, &sr);
            while f & 1 == 0 && f != 0 {
                f >>= 1;
                s >>= 1;
            }
        } else {
            sr = node(&sr, c);
        }
        f >>= 1;
        s >>= 1;
    }
    fr == *first_root && sr == *second_root && s == 0
}

/// The origin of org `org`'s trail: what its checkpoints name it, and their key's name.
pub fn origin(org: &str) -> String {
    format!("kiln://{org}/trail")
}

/// A checkpoint of the trail: what kiln signs of it at a size (C2SP tlog-checkpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// The trail's name: [`origin`].
    pub origin: String,
    /// How many entries it covers.
    pub size: u64,
    /// The root of the tree over them.
    pub root: Hash,
}

impl Checkpoint {
    /// The checkpoint of `entries`, the trail from its start.
    pub fn of(origin: &str, entries: &[Entry]) -> Checkpoint {
        let leaves: Vec<Hash> = entries.iter().map(Entry::hash).collect();
        Checkpoint { origin: origin.to_owned(), size: leaves.len() as u64, root: root(&leaves) }
    }

    /// Its note text: the origin, the size, and the root in base64, a line each.
    pub fn text(&self) -> String {
        format!("{}\n{}\n{}\n", self.origin, self.size, B64.encode(self.root))
    }

    /// A checkpoint's note text read back: its origin, size and root, then any extension lines,
    /// which are ignored.
    pub fn parse(text: &str) -> Result<Checkpoint, Refusal> {
        let bad = |why: &str| Refusal::Malformed(why.to_owned());
        let body = text.strip_suffix('\n').ok_or_else(|| bad("a note's text ends in a newline"))?;
        let lines: Vec<&str> = body.split('\n').collect();
        let [origin, size, root, ..] = lines.as_slice() else { return Err(bad("a checkpoint has three lines: origin, size, root")) };
        if lines.iter().any(|l| l.is_empty()) {
            return Err(bad("a checkpoint has no empty lines"));
        }
        if !size.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad("its size isn't a number"));
        }
        let size: u64 = size.parse().map_err(|_| bad("its size isn't a number"))?;
        let root = B64.decode(root).ok().and_then(|r| <Hash>::try_from(r.as_slice()).ok()).ok_or_else(|| bad("its root isn't a SHA-256 in base64"))?;
        Ok(Checkpoint { origin: (*origin).to_owned(), size, root })
    }
}

/// Why a checkpoint isn't signed or taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("not a checkpoint kiln reads: {0}")]
    Malformed(String),
    #[error("it's a checkpoint of {origin}, not of {want}")]
    Elsewhere { origin: String, want: String },
    #[error("the checkpoint's signing key: {0}")]
    Key(String),
    #[error("kiln couldn't sign the checkpoint: {0}")]
    Sign(String),
    #[error("the checkpoint has no signature as {0} by its certificate's key")]
    Unsigned(String),
    #[error(transparent)]
    Signer(#[from] chain::Refusal),
    #[error("the checkpoint is signed by {by}, not by {org}'s server")]
    NotTheServer { by: String, org: String },
    #[error("checkpoint {new} doesn't extend checkpoint {old}: {why}")]
    Inconsistent { old: u64, new: u64, why: String },
}

/// Whether `name` can name a log or a note's key (C2SP tlog-cosignature): not empty, at most 255
/// bytes, and no spaces, plus signs or control characters.
fn nameable(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(|c| c.is_whitespace() || c == '+' || c.is_control())
}

/// The public key (its SubjectPublicKeyInfo, DER) a checkpoint is signed with, from its
/// certificate: an ECDSA P-256 key, as signed-note's type 0x02 takes (it hashes with SHA-256
/// whatever the curve, so kiln signs with no other).
fn signing_key(cert_der: &[u8]) -> Result<Vec<u8>, Refusal> {
    let cert = x509_cert::Certificate::from_der(cert_der).map_err(|e| Refusal::Key(format!("its certificate doesn't parse: {e}")))?;
    let spki = &cert.tbs_certificate.subject_public_key_info;
    let curve = spki.algorithm.parameters.as_ref().and_then(|p| p.decode_as::<ObjectIdentifier>().ok());
    if spki.algorithm.oid != ID_EC_PUBLIC_KEY || curve != Some(SECP_256_R_1) {
        return Err(Refusal::Key("it isn't ECDSA P-256, which a checkpoint's signature (signed-note's type 0x02) is made with".into()));
    }
    spki.to_der().map_err(|e| Refusal::Key(e.to_string()))
}

/// The key ID of an ECDSA key's note signatures: the first four bytes of SHA-256 over its public
/// key, as signed-note has it for type 0x02 (other types hash the key's name and type first).
fn key_id(spki: &[u8]) -> [u8; 4] {
    let h = sha(&[spki]);
    [h[0], h[1], h[2], h[3]]
}

/// A signature line of a signed note: its key's name and ID, and the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSignature {
    pub name: String,
    pub key_id: [u8; 4],
    pub signature: Vec<u8>,
}

/// A signed note read (C2SP signed-note): its text, through the newline before its last blank
/// line, and its signature lines after it.
pub fn open(note: &str) -> Result<(&str, Vec<NoteSignature>), Refusal> {
    let bad = |why: &str| Refusal::Malformed(why.to_owned());
    if note.chars().any(|c| c < ' ' && c != '\n') {
        return Err(bad("a note has no control characters but newlines"));
    }
    let split = note.rfind("\n\n").ok_or_else(|| bad("a signed note is its text, a blank line and its signatures"))?;
    let text = note.get(..=split).unwrap_or_default();
    let lines = note.get(split + 2..).unwrap_or_default();
    if lines.is_empty() {
        return Err(bad("it has no signatures"));
    }
    if !lines.ends_with('\n') {
        return Err(bad("its last signature line doesn't end in a newline"));
    }
    let signatures = lines
        .split_terminator('\n')
        .map(|line| {
            let (name, sig) = line.strip_prefix("\u{2014} ").and_then(|l| l.split_once(' ')).filter(|(n, _)| nameable(n)).ok_or_else(|| bad("a signature line is \u{2014}, its key's name and its signature, a space apart"))?;
            let blob = B64.decode(sig).map_err(|_| bad("a signature isn't base64"))?;
            let (id, signature) = blob.split_at_checked(4).filter(|(_, s)| !s.is_empty()).ok_or_else(|| bad("a signature is cut short"))?;
            Ok(NoteSignature { name: name.to_owned(), key_id: <[u8; 4]>::try_from(id).unwrap_or_default(), signature: signature.to_vec() })
        })
        .collect::<Result<Vec<_>, Refusal>>()?;
    Ok((text, signatures))
}

/// `checkpoint` as a signed note: its text, a blank line and its signature line under its origin's
/// name, `sign` signing the text as kiln's server (it says the signature and the certificate it
/// verifies under, as kiln's `sign_own` does). kiln signs only a checkpoint that extends the last
/// one it signed, `last` with the consistency proof from it (none: the trail's first): it never
/// signs two that contradict each other.
pub fn signed(checkpoint: &Checkpoint, last: Option<(&Checkpoint, &[Hash])>, sign: impl FnOnce(&[u8]) -> Result<(Vec<u8>, Vec<u8>), String>) -> Result<String, Refusal> {
    if let Some((last, proof)) = last {
        extends(last, checkpoint, proof)?;
    }
    let name = &checkpoint.origin;
    if !nameable(name) {
        return Err(Refusal::Malformed(format!("{name:?} can't name a log: it's empty, over 255 bytes, or has spaces, plus signs or control characters")));
    }
    let text = checkpoint.text();
    let (signature, cert) = sign(text.as_bytes()).map_err(Refusal::Sign)?;
    let mut blob = key_id(&signing_key(&cert)?).to_vec();
    blob.extend(signature);
    Ok(format!("{text}\n\u{2014} {name} {}\n", B64.encode(blob)))
}

/// The checkpoint a signed note says, and who signed it, read as org `org`'s trail: its signature
/// under its origin's name by the key of `chain`'s first certificate, which verifies to `root` at
/// `at` with `revocations` as `org`'s server, the only signer of its trail.
pub fn verify_note(note: &str, org: &str, chain: &[&[u8]], root: &[u8], revocations: &[Revocation], at: i64) -> Result<(Checkpoint, Verified), Refusal> {
    let (text, signatures) = open(note)?;
    let checkpoint = Checkpoint::parse(text)?;
    let want = origin(org);
    if checkpoint.origin != want {
        return Err(Refusal::Elsewhere { origin: checkpoint.origin, want });
    }
    let leaf = chain.first().ok_or_else(|| Refusal::Key("no certificate for it".into()))?;
    let id = key_id(&signing_key(leaf)?);
    let signature = signatures.iter().find(|s| s.name == want && s.key_id == id).ok_or_else(|| Refusal::Unsigned(want.clone()))?;
    let v = chain::verify_signed(chain, root, revocations, at, text.as_bytes(), &signature.signature)?;
    if v.identity != Identity::new(org, Actor::Server) {
        return Err(Refusal::NotTheServer { by: v.identity.to_string(), org: org.to_owned() });
    }
    Ok((checkpoint, v))
}

/// Whether checkpoint `new` extends checkpoint `old`, `proof` its consistency proof: kiln signs
/// none that doesn't.
pub fn extends(old: &Checkpoint, new: &Checkpoint, proof: &[Hash]) -> Result<(), Refusal> {
    let refuse = |why: &str| Err(Refusal::Inconsistent { old: old.size, new: new.size, why: why.to_owned() });
    if old.origin != new.origin {
        return refuse("they're of different trails");
    }
    if new.size < old.size {
        return refuse("it's smaller");
    }
    if !verify_consistency(old.size, new.size, &old.root, &new.root, proof) {
        return refuse("its tree doesn't contain the earlier one's");
    }
    Ok(())
}

/// Whether kiln signs a checkpoint now: entries came after the last one it signed (at `signed`
/// entries, 0 before the first), and `every` of them did, or the oldest, taken at `oldest`, has
/// waited `minutes`. No entry waits longer for a checkpoint, nor among more than `every`.
pub fn due(size: u64, signed: u64, oldest: Option<i64>, now: i64, every: u64, minutes: u64) -> bool {
    let waiting = size.saturating_sub(signed);
    let wait = i64::try_from(minutes.saturating_mul(60)).unwrap_or(i64::MAX);
    waiting > 0 && (waiting >= every.max(1) || oldest.is_some_and(|t| now.saturating_sub(t) >= wait))
}

/// The most subjects an entry names ([`about`]).
pub const MAX_SUBJECTS: usize = 256;

/// What a trail entry says of a statement, from its payload type and payload: its digest (SHA-256
/// over its PAE, [`super::statement::digest`]), its kind and its subjects. Its kind is an in-toto
/// statement's predicate type (W18's attestations), else its payload type, on one line however it
/// came; its subjects are what its payload names by digest (kiln's statements and in-toto's alike),
/// each `<algorithm>:<hex>`, leaving out any that isn't one.
pub fn about(payload_type: &str, payload: &[u8]) -> (String, String, Vec<String>) {
    let digest = super::statement::digest(payload_type, payload);
    let value: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let kind = value.get("predicateType").and_then(Value::as_str).filter(|k| !k.is_empty()).unwrap_or(payload_type);
    let kind: String = kind.escape_debug().take(256).collect();
    let mut subjects: Vec<String> = value
        .get("subject")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("digest").and_then(Value::as_object))
        .flatten()
        .filter_map(|(alg, hex)| hex.as_str().filter(|h| alg.len() <= 32 && alg.chars().all(|c| c.is_ascii_alphanumeric()) && !alg.is_empty() && is_hex(h)).map(|h| format!("{alg}:{}", h.to_ascii_lowercase())))
        .collect();
    subjects.sort();
    subjects.dedup();
    // At most MAX_SUBJECTS: an entry fits an entry bundle's 64 KiB; its statement names them all.
    subjects.truncate(MAX_SUBJECTS);
    (digest, if kind.is_empty() { "unknown".to_owned() } else { kind }, subjects)
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// kiln's own statement of what it checked, where what it checked isn't a statement itself: a
/// pushed commit's signature ([`checked_commit`]), what a repository's host verified of a commit
/// ([`checked_by_host`]). Unsigned: the checkpoints over the trail vouch for it.
pub const PAYLOAD_CHECKED: &str = "application/vnd.kiln.checked+json";
/// kiln's own statement of a revocation it recorded ([`revocation`]), unsigned as
/// [`PAYLOAD_CHECKED`]'s are.
pub const PAYLOAD_REVOCATION: &str = "application/vnd.kiln.revocation+json";

/// A subject named by one digest, when `hex` is one of `algorithm`'s (lowercased).
fn subject(name: &str, algorithm: &str, hex: &str) -> Option<Subject> {
    let hex = hex.to_ascii_lowercase();
    let length = match algorithm {
        "gitCommit" => hex.len() == 40 || hex.len() == 64,
        "sha256" => hex.len() == 64,
        _ => true,
    };
    (is_hex(&hex) && length).then(|| Subject { name: name.to_owned(), digest: [(algorithm.to_owned(), hex)].into(), ..Subject::default() })
}

/// kiln's statement that it checked the signature of commit `sha` as a push to `refname` of
/// `repo` brought it: the signature by its digest (`signature`, the SHA-256 of the signature as the
/// commit carries it) where it has one, and what kiln found: `seen` (`unsigned`, `unverified` or
/// `by`) and its words (why it isn't verified; the identity that signed).
pub fn checked_commit(repo: &str, refname: &str, sha: &str, signature: Option<&str>, seen: &str, words: Option<&str>) -> Statement {
    let mut s = Statement::new(json!({ "check": "signature", "repo": repo, "ref": refname, "seen": seen, "words": words }));
    s.subject = commit_and_signature(sha, signature);
    s
}

/// kiln's statement that it made commit `sha` of `repo` and signed it as `by` (its server's or
/// CD's identity), the signature by its digest as [`checked_commit`] names one.
pub fn made_commit(repo: &str, sha: &str, signature: Option<&str>, by: &str) -> Statement {
    let mut s = Statement::new(json!({ "check": "made", "repo": repo, "by": by }));
    s.subject = commit_and_signature(sha, signature);
    s
}

fn commit_and_signature(sha: &str, signature: Option<&str>) -> Vec<Subject> {
    subject("commit", "gitCommit", sha).into_iter().chain(signature.and_then(|d| subject("signature", "sha256", d))).collect()
}

/// kiln's statement of what host `host` (`github`, `gitlab`, `bitbucket`) said it verified of
/// commit `sha` of `repo`, as kiln read it at `read_at`: its state, why, the signer it named and
/// when it verified.
#[allow(clippy::too_many_arguments)]
pub fn checked_by_host(repo: &str, host: &str, sha: &str, state: &str, reason: Option<&str>, signer: Option<&str>, verified_at: Option<i64>, read_at: i64) -> Statement {
    let mut s = Statement::new(json!({ "check": "host", "repo": repo, "host": host, "state": state, "reason": reason, "signer": signer, "verified_at": verified_at, "read_at": read_at }));
    s.subject = subject("commit", "gitCommit", sha).into_iter().collect();
    s
}

/// kiln's statement of a revocation: the certificate (`serial`) or key (`key`, the SHA-256 of its
/// SubjectPublicKeyInfo) it names, whose they were, from when (`at`), why and by whom.
pub fn revocation(serial: Option<&str>, key: Option<&str>, subject_of: &str, at: i64, reason: &str, by: &str) -> Statement {
    let mut s = Statement::new(json!({ "serial": serial, "key": key, "subject": subject_of, "at": at, "reason": reason, "by": by }));
    s.subject = serial.and_then(|v| subject("certificate", "x509Serial", v)).into_iter().chain(key.and_then(|k| subject("key", "sha256", k))).collect();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SignatureAlgorithm, SigningKey as _, date_time_ymd};

    /// Inside every test leaf's validity (2026-01-01 to 2027-01-01).
    const AT: i64 = 1_790_000_000;
    const ORG: &str = "hyper-light";
    const SERVER: &str = "kiln://hyper-light/server";

    fn hex(h: &Hash) -> String {
        chain::hex(h)
    }

    /// A trail of `n` entries, each a statement of its own.
    fn trail(n: u64) -> anyhow::Result<Vec<Entry>> {
        let mut t: Vec<Entry> = Vec::new();
        for i in 0..n {
            let statement = format!("sha256:{}", chain::hex(&sha(&[&i.to_be_bytes()])));
            let e = next(t.last(), &statement, "application/vnd.kiln.custody+json", vec![format!("gitCommit:{:040x}", i + 1)], AT + i as i64)?;
            t.push(e);
        }
        Ok(t)
    }

    fn leaves(t: &[Entry]) -> Vec<Hash> {
        t.iter().map(Entry::hash).collect()
    }

    fn ceil_log2(n: usize) -> usize {
        (usize::BITS - n.saturating_sub(1).leading_zeros()) as usize
    }

    #[test]
    fn w27_ac06_an_edited_dropped_or_reordered_entry_is_found() -> anyhow::Result<()> {
        let t = trail(10)?;
        assert_eq!(check(None, &t), Ok(()), "a whole trail holds");
        let cp = Checkpoint::of(&origin(ORG), &t);
        assert_eq!(check_root(&t, &cp), Ok(()));
        // An entry's subject edited: the next entry no longer names its hash.
        let mut edited = t.clone();
        if let Some(e) = edited.get_mut(4) {
            e.subjects = vec!["gitCommit:ffffffffffffffffffffffffffffffffffffffff".into()];
        }
        let found = check(None, &edited);
        assert_eq!(found, Err(Break::Edited { index: 4, next: 5 }));
        assert_eq!(found.err().map(|b| b.to_string()).as_deref(), Some("entry 4 doesn't hash to what entry 5 names"));
        // One dropped.
        let mut dropped = t.clone();
        dropped.remove(6);
        let found = check(None, &dropped);
        assert_eq!(found, Err(Break::Missing { index: 7, after: 5, missing: 6 }));
        assert_eq!(found.err().map(|b| b.to_string()).as_deref(), Some("entry 7 comes right after entry 5: entry 6 is missing"));
        // Two moved.
        let mut moved = t.clone();
        moved.swap(3, 4);
        assert_eq!(check(None, &moved), Err(Break::OutOfOrder { index: 4, after: 2 }));
        // One inserted beside entry 6, naming entry 5's hash as entry 6 does; one inserted with
        // a wrong hash before it.
        let (head, tail) = t.split_at(6);
        let six = next(head.last(), &format!("sha256:{}", "a".repeat(64)), "commit", vec!["gitCommit:0000000000000000000000000000000000000abc".into()], AT)?;
        let inserted: Vec<Entry> = head.iter().cloned().chain([six.clone()]).chain(tail.iter().cloned()).collect();
        let found = check(None, &inserted);
        assert_eq!(found, Err(Break::Inserted { index: 6 }));
        assert_eq!(found.err().map(|b| b.to_string()).as_deref(), Some("there are two entries 6: one was inserted"));
        let wrong = Entry { prev: ZERO, ..six };
        let inserted: Vec<Entry> = head.iter().cloned().chain([wrong]).chain(tail.iter().cloned()).collect();
        assert_eq!(check(None, &inserted), Err(Break::Edited { index: 5, next: 6 }));
        // A chain forged whole from entry 4 on (an entry inserted and every hash after it made
        // again): it holds as a chain, but not against the root kiln signed.
        let (kept, _) = t.split_at(4);
        let mut forged = kept.to_vec();
        for i in 4..10u64 {
            let e = next(forged.last(), &format!("sha256:{}", "e".repeat(64)), "commit", vec![format!("gitCommit:{i:040x}")], AT + 100)?;
            forged.push(e);
        }
        assert_eq!(check(None, &forged), Ok(()));
        assert_eq!(check_root(&forged, &cp), Err(Break::Root { size: 10 }));
        // The last entry changed: no entry names it, the root does.
        let mut last = t.clone();
        if let Some(e) = last.last_mut() {
            e.taken_at += 1;
        }
        assert_eq!(check(None, &last), Ok(()));
        assert_eq!(check_root(&last, &cp), Err(Break::Root { size: 10 }));
        // Fewer entries than the checkpoint covers.
        assert_eq!(check_root(t.get(..9).unwrap_or_default(), &cp), Err(Break::Root { size: 10 }));
        // A run checked after the entry before it; one that starts elsewhere; one malformed.
        let (head, tail) = t.split_at(3);
        assert_eq!(check(head.last(), tail), Ok(()));
        assert_eq!(check(None, tail), Err(Break::Start { index: 3 }));
        let mut bad = t.clone();
        if let Some(e) = bad.get_mut(2) {
            e.statement = "sha256:not-a-digest".into();
        }
        assert!(matches!(check(None, &bad), Err(Break::Malformed { index: 2, .. })));
        // An entry that would break the trail is never written: a payload type of two lines.
        let refused = next(t.last(), &format!("sha256:{}", "b".repeat(64)), "application/x\nforged", vec![], AT);
        assert!(matches!(refused, Err(Break::Malformed { index: 10, .. })), "{refused:?}");
        Ok(())
    }

    #[test]
    fn w27_r09_the_tree_is_rfc_6962s() {
        // RFC 6962's test leaves and the roots of their first 1, 2, 3 and 8, and the empty tree.
        let data: [&[u8]; 8] = [b"", b"\x00", b"\x10", b"\x20\x21", b"\x30\x31", b"\x40\x41\x42\x43", b"\x50\x51\x52\x53\x54\x55\x56\x57", b"\x60\x61\x62\x63\x64\x65\x66\x67\x68\x69\x6a\x6b\x6c\x6d\x6e\x6f"];
        let l: Vec<Hash> = data.iter().map(|d| leaf(d)).collect();
        assert_eq!(hex(&root(&[])), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let known = [
            (1, "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d"),
            (2, "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125"),
            (3, "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77"),
            (8, "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328"),
        ];
        for (n, want) in known {
            assert_eq!(hex(&root(l.get(..n).unwrap_or_default())), want, "the root of the first {n}");
        }
    }

    #[test]
    fn w27_r09_proofs_verify_and_stay_logarithmic() -> anyhow::Result<()> {
        // One trail of 10,000: the trail of n is its first n.
        let all = leaves(&trail(10_000)?);
        let mut longest = (0, 0);
        let mut proven = |n: usize, at: &[usize], olds: &[usize]| -> anyhow::Result<()> {
            let l = all.get(..n).unwrap_or_default();
            anyhow::ensure!(l.len() == n, "a trail of {n}");
            let r = root(l);
            let bound = ceil_log2(n) + 1;
            for &m in at {
                let leaf = l.get(m).copied().unwrap_or(ZERO);
                let p = inclusion(l, m).unwrap_or_default();
                anyhow::ensure!(verify_inclusion(&leaf, m as u64, n as u64, &p, &r), "leaf {m} of {n}");
                anyhow::ensure!(p.len() <= bound, "leaf {m} of {n}: {} hashes, over {bound}", p.len());
                longest.0 = longest.0.max(p.len());
                if !p.is_empty() {
                    let mut wrong = p.clone();
                    if let Some(h) = wrong.first_mut() {
                        h[0] ^= 1;
                    }
                    anyhow::ensure!(!verify_inclusion(&leaf, m as u64, n as u64, &wrong, &r), "a changed proof, leaf {m} of {n}");
                    anyhow::ensure!(!verify_inclusion(&leaf, (m as u64 + 1) % n as u64, n as u64, &p, &r), "another index, leaf {m} of {n}");
                }
                anyhow::ensure!(!verify_inclusion(&leaf, n as u64, n as u64, &p, &r), "an index past the end, {n}");
            }
            for &m in olds {
                let p = consistency(l, m).unwrap_or_default();
                let old = root(l.get(..m).unwrap_or_default());
                anyhow::ensure!(verify_consistency(m as u64, n as u64, &old, &r, &p), "{m} to {n}");
                anyhow::ensure!(p.len() <= bound, "{m} to {n}: {} hashes, over {bound}", p.len());
                longest.1 = longest.1.max(p.len());
                if m > 0 && m < n {
                    anyhow::ensure!(!verify_consistency(m as u64, n as u64, &leaf(b"another"), &r, &p), "another old root, {m} to {n}");
                    let mut wrong = p.clone();
                    if let Some(h) = wrong.last_mut() {
                        h[31] ^= 1;
                    }
                    anyhow::ensure!(!verify_consistency(m as u64, n as u64, &old, &r, &wrong), "a changed proof, {m} to {n}");
                }
            }
            Ok(())
        };
        // Every leaf and every earlier size of the trees of 1 to 64.
        for n in 1..=64usize {
            let every: Vec<usize> = (0..n).collect();
            let olds: Vec<usize> = (0..=n).collect();
            proven(n, &every, &olds)?;
        }
        // Up to 10,000: each side of every power of two, its ends, its middle.
        for n in [100usize, 127, 128, 129, 255, 256, 257, 1_000, 1_023, 1_024, 1_025, 4_095, 4_096, 4_097, 8_191, 8_192, 8_193, 9_999, 10_000] {
            let pow = split(n);
            let at = [0, 1, pow - 1, pow, n / 2, n - 2, n - 1];
            let olds = [0, 1, 2, n / 3, pow - 1, pow, pow + 1, n - 1, n];
            proven(n, &at, &olds.map(|m| m.min(n)))?;
        }
        assert!(longest.0 <= ceil_log2(10_000) + 1 && longest.1 <= ceil_log2(10_000) + 1, "{longest:?}");
        assert!(inclusion(&all, 10_000).is_none() && consistency(&all, 10_001).is_none());
        Ok(())
    }

    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    fn ca() -> anyhow::Result<Ca> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, ORG);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let cert = p.self_signed(&key)?;
        Ok(Ca { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
    }

    /// A certificate from `ca` for `uri` with a key of `alg`, and the key.
    fn signer(ca: &Ca, uri: &str, alg: &'static SignatureAlgorithm) -> anyhow::Result<(Vec<u8>, KeyPair)> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2026, 1, 1);
        p.not_after = date_time_ymd(2027, 1, 1);
        let key = KeyPair::generate_for(alg)?;
        let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
        Ok((der, key))
    }

    /// What signs as `cert`'s holder, as kiln's `sign_own` signs: the signature and the certificate.
    fn signs_as<'a>(cert: &'a [u8], key: &'a KeyPair) -> impl FnOnce(&[u8]) -> Result<(Vec<u8>, Vec<u8>), String> + 'a {
        move |m: &[u8]| Ok((key.sign(m).map_err(|e| e.to_string())?, cert.to_vec()))
    }

    #[test]
    fn w27_r09_a_checkpoint_never_contradicts_the_one_before() -> anyhow::Result<()> {
        let ca = ca()?;
        let (server, key) = signer(&ca, SERVER, &rcgen::PKCS_ECDSA_P256_SHA256)?;
        let t = trail(12)?;
        let (first, _) = t.split_at(10);
        let cp10 = Checkpoint::of(&origin(ORG), first);
        let note = signed(&cp10, None, signs_as(&server, &key))?;
        assert!(note.starts_with(&format!("kiln://hyper-light/trail\n10\n{}\n\n\u{2014} kiln://hyper-light/trail ", B64.encode(cp10.root))) && note.ends_with('\n'), "{note}");
        let (read, by) = verify_note(&note, ORG, &[&server], &ca.der, &[], AT)?;
        assert_eq!((read, by.identity), (cp10.clone(), Identity::new(ORG, Actor::Server)));
        // Twelve extend ten, and kiln signs them; a trail changed at entry 3 doesn't, however it's
        // hashed again, and kiln won't sign it.
        let cp12 = Checkpoint::of(&origin(ORG), &t);
        let l = leaves(&t);
        let proof = consistency(&l, 10).unwrap_or_default();
        assert_eq!(extends(&cp10, &cp12, &proof), Ok(()));
        let note12 = signed(&cp12, Some((&cp10, &proof)), signs_as(&server, &key))?;
        assert_eq!(verify_note(&note12, ORG, &[&server], &ca.der, &[], AT)?.0, cp12);
        let mut rehashed: Vec<Entry> = Vec::new();
        for (i, e) in t.iter().enumerate() {
            let taken_at = if i == 3 { e.taken_at + 60 } else { e.taken_at };
            let n = next(rehashed.last(), &e.statement, &e.kind, e.subjects.clone(), taken_at)?;
            rehashed.push(n);
        }
        let forged = Checkpoint::of(&origin(ORG), &rehashed);
        let proof = consistency(&leaves(&rehashed), 10).unwrap_or_default();
        assert!(matches!(extends(&cp10, &forged, &proof), Err(Refusal::Inconsistent { old: 10, new: 12, .. })));
        let refused = signed(&forged, Some((&cp10, &proof)), signs_as(&server, &key));
        assert!(matches!(refused, Err(Refusal::Inconsistent { old: 10, new: 12, .. })), "{refused:?}");
        assert_eq!(refused.err().map(|r| r.to_string()).as_deref(), Some("checkpoint 12 doesn't extend checkpoint 10: its tree doesn't contain the earlier one's"));
        assert!(extends(&cp12, &cp10, &[]).is_err(), "a smaller one never extends");
        // A note changed after it was signed; one by someone else; one of another trail; one
        // whose signature isn't the certificate's key's; a key signed-note's type 0x02 isn't.
        let edited = note.replacen("\n10\n", "\n11\n", 1);
        assert!(matches!(verify_note(&edited, ORG, &[&server], &ca.der, &[], AT), Err(Refusal::Signer(chain::Refusal::BadSignature { .. }))));
        let (sam, sam_key) = signer(&ca, "kiln://hyper-light/person/sam", &rcgen::PKCS_ECDSA_P256_SHA256)?;
        let by_sam = signed(&cp10, None, signs_as(&sam, &sam_key))?;
        assert!(matches!(verify_note(&by_sam, ORG, &[&sam], &ca.der, &[], AT), Err(Refusal::NotTheServer { .. })));
        assert_eq!(verify_note(&by_sam, ORG, &[&server], &ca.der, &[], AT).err(), Some(Refusal::Unsigned(origin(ORG))));
        assert!(matches!(verify_note(&note, "other-org", &[&server], &ca.der, &[], AT), Err(Refusal::Elsewhere { .. })));
        let (ed, ed_key) = signer(&ca, SERVER, &rcgen::PKCS_ED25519)?;
        assert!(matches!(signed(&cp10, None, signs_as(&ed, &ed_key)), Err(Refusal::Key(_))));
        // The checkpoint's text reads back; one that isn't three lines, or isn't a note, doesn't.
        assert_eq!(Checkpoint::parse(&cp12.text()), Ok(cp12));
        assert!(Checkpoint::parse("kiln://hyper-light/trail\nten\n").is_err());
        assert!(Checkpoint::parse("kiln://hyper-light/trail\n+10\nAAAA\n").is_err());
        assert!(open("kiln://hyper-light/trail\n10\n").is_err());
        Ok(())
    }

    #[test]
    fn w27_r09_the_tree_reads_as_tiles() -> anyhow::Result<()> {
        // Paths as C2SP tlog-tiles writes them, and read back; none written otherwise.
        let t = |level, index, width| Tile { level, index, width };
        assert_eq!(t(0, 1_234_067, TILE).path(false), "tile/0/x001/x234/067");
        assert_eq!(t(2, 0, 7).path(false), "tile/2/000.p/7");
        assert_eq!(t(0, 1000, TILE).path(true), "tile/entries/x001/000");
        for (tile, entries) in [(t(0, 1_234_067, TILE), false), (t(2, 0, 7), false), (t(0, 1000, 255), true), (t(63, 5, 1), false)] {
            assert_eq!(Tile::parse(&tile.path(entries)), Some((tile, entries)));
        }
        for bad in ["tile/0/1", "tile/01/000", "tile/0/x000/001", "tile/0/000.p/0", "tile/0/000.p/256", "tile/0/000.p/07", "tile/64/000", "tile/0/001/002", "tile/entries/000.p/+5", "tile/+1/000"] {
            assert_eq!(Tile::parse(bad), None, "{bad}");
        }
        // The spec's examples: trees of 70,000 and of 256.
        assert_eq!((width_at(70_000, 0, 272), width_at(70_000, 0, 273), width_at(70_000, 0, 274)), (Some(256), Some(112), None));
        assert_eq!((width_at(70_000, 1, 0), width_at(70_000, 1, 1), width_at(70_000, 2, 0)), (Some(256), Some(17), Some(1)));
        assert_eq!((width_at(256, 0, 0), width_at(256, 0, 1), width_at(256, 1, 0)), (Some(256), None, Some(1)));

        // A tree of 70,000 as tiles: each hash at level l the root of 256^l leaves.
        let all = leaves(&trail(70_000)?);
        let size = all.len() as u64;
        let mut made: std::collections::HashMap<(u8, u64, u64), Vec<Hash>> = std::collections::HashMap::new();
        let mut tile = |t: Tile| -> Option<Vec<Hash>> {
            if let Some(h) = made.get(&(t.level, t.index, t.width)) {
                return Some(h.clone());
            }
            let per = TILE.pow(u32::from(t.level));
            let (first, _) = t.span()?;
            let hashes: Vec<Hash> = (0..t.width).map(|i| all.get(usize::try_from(first + i * per).ok()?..usize::try_from(first + (i + 1) * per).ok()?).map(root)).collect::<Option<_>>()?;
            made.insert((t.level, t.index, t.width), hashes.clone());
            Some(hashes)
        };
        for (lo, hi) in [(0, 70_000), (0, 65_536), (65_536, 70_000), (300, 301), (256, 512), (1000, 69_999)] {
            assert_eq!(mth_from(size, lo, hi, &mut tile), all.get(lo as usize..hi as usize).map(root), "{lo}..{hi}");
        }
        assert_eq!(mth_from(size, 0, 70_001, &mut tile), None, "past the tree's end");
        for m in [0u64, 255, 256, 65_535, 65_536, 69_999] {
            let from_tiles = inclusion_by(size, m, &mut |lo, hi| mth_from(size, lo, hi, &mut tile));
            assert_eq!(from_tiles, inclusion(&all, m as usize), "leaf {m}");
        }
        for old in [1u64, 1000, 65_536, 69_999] {
            assert_eq!(consistency_by(size, old, &mut |lo, hi| mth_from(size, lo, hi, &mut tile)), consistency(&all, old as usize), "{old} to 70,000");
        }

        // An entry bundle: each entry's encoding, read back whole, each hashing to its leaf.
        let t5 = trail(5)?;
        let data: Vec<Vec<u8>> = t5.iter().map(Entry::encoding).collect();
        let bytes = bundle(&data).ok_or_else(|| anyhow::anyhow!("a bundle"))?;
        let back: Vec<Entry> = unbundle(&bytes).ok_or_else(|| anyhow::anyhow!("read back"))?.iter().map(|d| Entry::parse(d)).collect::<Result<_, _>>()?;
        assert_eq!(back, t5);
        assert!(unbundle(bytes.get(..bytes.len() - 1).unwrap_or_default()).is_none(), "cut short");
        assert!(bundle(&[vec![0; 65_536]]).is_none(), "an entry too long for a bundle");
        // Written otherwise than kiln writes it: not an entry.
        let odd = String::from_utf8(data.first().cloned().unwrap_or_default())?.replacen("kiln trail entry v1\n0\n0000", "kiln trail entry v1\n0\n0Z00", 1);
        assert!(Entry::parse(odd.as_bytes()).is_err());
        Ok(())
    }

    #[test]
    fn w27_r09_a_checkpoint_comes_due_by_count_or_by_time() {
        let now = AT;
        assert!(!due(0, 0, None, now, 4, 10), "nothing to sign");
        assert!(!due(3, 0, Some(now - 10), now, 4, 10), "three waiting, ten seconds");
        assert!(due(4, 0, Some(now - 10), now, 4, 10), "four waiting");
        assert!(due(5, 4, Some(now - 600), now, 4, 10), "one waiting ten minutes");
        assert!(!due(5, 4, Some(now - 599), now, 4, 10), "one waiting a second less");
        assert!(!due(4, 4, Some(now - 10_000), now, 4, 10), "none waiting since the last");
        assert!(due(1, 0, Some(now), now, 0, 10), "every 0 is every entry");
    }

    #[test]
    fn w27_r09_an_entry_says_what_its_statement_is_about() -> anyhow::Result<()> {
        use super::super::statement;
        // kiln's own words: its payload type, the commit and its signature.
        let checked = checked_commit("hyper-light/kiln", "refs/heads/main", &"a".repeat(40), Some(&"B".repeat(64)), "by", Some("kiln://hyper-light/person/sam"));
        let payload = statement::payload(&checked)?;
        let (digest, kind, subjects) = about(PAYLOAD_CHECKED, &payload);
        assert_eq!(digest, statement::digest(PAYLOAD_CHECKED, &payload));
        assert_eq!((kind.as_str(), subjects), (PAYLOAD_CHECKED, vec![format!("gitCommit:{}", "a".repeat(40)), format!("sha256:{}", "b".repeat(64))]));
        let host = statement::payload(&checked_by_host("hyper-light/kiln", "github", &"c".repeat(40), "valid", Some("valid"), Some("sam"), Some(AT), AT))?;
        assert_eq!(about(PAYLOAD_CHECKED, &host).2, vec![format!("gitCommit:{}", "c".repeat(40))]);
        // An in-toto statement (W18's): its predicate type; its subjects lowercased; one that
        // isn't a digest left out.
        let intoto = json!({ "_type": "https://in-toto.io/Statement/v1", "subject": [{ "name": "image", "digest": { "sha256": "CC".repeat(32) } }, { "name": "x", "digest": { "sha256": "zz\n" } }], "predicateType": "https://slsa.dev/provenance/v1", "predicate": {} });
        let (_, kind, subjects) = about("application/vnd.in-toto+json", &serde_json::to_vec(&intoto)?);
        assert_eq!((kind.as_str(), subjects), ("https://slsa.dev/provenance/v1", vec![format!("sha256:{}", "c".repeat(64))]));
        // A payload type of two lines is one, its newline written out: the entry is written.
        let (d, kind, _) = about("application/x\nforged", b"{}");
        assert_eq!(kind, "application/x\\nforged");
        assert!(next(None, &d, &kind, vec![], AT).is_ok());
        // A revocation: its serial lowercased; one that isn't hex named in its words alone.
        let r = statement::payload(&revocation(Some("0A1B"), Some(&"d".repeat(64)), "fedora-2", AT, "runner forgotten", "ada"))?;
        assert_eq!(about(PAYLOAD_REVOCATION, &r).2, vec![format!("sha256:{}", "d".repeat(64)), "x509Serial:0a1b".to_owned()]);
        let odd = revocation(Some("not hex"), None, "fedora-2", AT, "key lost", "sam");
        assert!(odd.subject.is_empty() && statement::payload(&odd).is_ok());
        Ok(())
    }

    /// Rekor's public log's checkpoint and key, as https://rekor.sigstore.dev/api/v1/log and
    /// /api/v1/log/publicKey gave them on 2026-10-30: a type 0x02 signed note from outside kiln.
    const REKOR_NOTE: &str = "rekor.sigstore.dev - 1193050959916656506\n3484845866\nvC+7KDx/H0/nu8aZsqO02V2jsX/UaS77jEP4AgEx2VI=\n\n\u{2014} rekor.sigstore.dev wNI9ajBFAiEAoO79I1HUQKh1t7dLFxeKnuTfDKcVQiFVJoPy+vNriFUCICfqXTpqtA5JqD7JBoajh4pYtXOXLV5hx5rARNxxBQcY\n";
    const REKOR_KEY: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE2G2Y+2tabdTV5BcGiBIx0a9fAFwrkBbmLSGtks4L3qX6yYY0zufBnhC8Ur/iy55GhWP/9A/bY2LhC30M9+RYtw==";

    /// Whether a C2SP verifier holding only `spki` (an ECDSA P-256 key, DER) takes `name`'s
    /// signature on `note`, as transparency-dev's does: its key ID, then its signature over the text.
    fn c2sp_verifies(note: &str, name: &str, spki: &[u8]) -> anyhow::Result<bool> {
        let (text, signatures) = open(note)?;
        let point = x509_cert::spki::SubjectPublicKeyInfoOwned::from_der(spki)?.subject_public_key.raw_bytes().to_vec();
        let key = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point);
        Ok(signatures.iter().any(|s| s.name == name && s.key_id == key_id(spki) && key.verify(text.as_bytes(), &s.signature).is_ok()))
    }

    #[test]
    fn w27_r09_a_checkpoint_is_a_signed_note_as_rekors_are() -> anyhow::Result<()> {
        // Rekor's note opens to the text it signed, under the key ID its key hashes to.
        let rekor = B64.decode(REKOR_KEY)?;
        assert_eq!(key_id(&rekor), [0xc0, 0xd2, 0x3d, 0x6a]);
        assert!(c2sp_verifies(REKOR_NOTE, "rekor.sigstore.dev", &rekor)?);
        assert!(!c2sp_verifies(&REKOR_NOTE.replacen("3484845866", "3484845867", 1), "rekor.sigstore.dev", &rekor)?);
        let cp = Checkpoint::parse(open(REKOR_NOTE)?.0)?;
        assert_eq!((cp.origin.as_str(), cp.size, hex(&cp.root).as_str()), ("rekor.sigstore.dev - 1193050959916656506", 3_484_845_866, "bc2fbb283c7f1f4fe7bbc699b2a3b4d95da3b17fd4692efb8c43f8020131d952"));
        // kiln's checkpoint, as a verifier holding only the server's public key checks it.
        let ca = ca()?;
        let (server, key) = signer(&ca, SERVER, &rcgen::PKCS_ECDSA_P256_SHA256)?;
        let note = signed(&Checkpoint::of(&origin(ORG), &trail(3)?), None, signs_as(&server, &key))?;
        assert!(c2sp_verifies(&note, &origin(ORG), &signing_key(&server)?)?);
        Ok(())
    }
}
