//! A step's log by digest (docs/ci.md W27-R05): what a leg hands on as its log, checked against
//! what kiln keeps of it. The leg's runner folds each chunk it sends into a chain, and the server
//! keeps each chunk's digest as it takes it (`<step>.digests`, beside the log), past the log's size
//! cap too. So a log kiln cut short still checks, and one that lost a chunk on the way, or was
//! changed since, says so rather than reading as the leg's.
//!
//! - **The chain** ([`Chain`]): `h₀ = SHA-256("kiln/log/v1")`, `hᵢ = SHA-256(hᵢ₋₁ ‖ SHA-256(chunkᵢ))`,
//!   with the chunks' count and bytes. Chunks are as the runner sent them, in order.
//! - **The pieces** ([`Piece`]): each chunk the server took, its length and digest, and where it's
//!   kept in the step's log, or that it isn't (past the cap). A retry's mark starts the next
//!   attempt's ([`parse`]): each attempt is another leg, with a chain of its own.
//! - **The check** ([`check`]): the pieces' chain against the one the leg signed, then each kept
//!   piece against the bytes at its place in the log.
//!
//! No clock, disk or network: the log's bytes and its pieces are arguments.

use sha2::{Digest as _, Sha256};

/// What the chain starts from, naming it: a chain of another scheme never equals one of this.
pub const SCHEME: &str = "kiln/log/v1";
/// A chain's head as a digest in a subject's digest set (in-toto's): never filed as `sha256`,
/// which it isn't.
pub const DIGEST: &str = "kiln-log-v1";

/// A step's log as its chunks' chain: its head, and how many chunks and bytes it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chain {
    pub head: [u8; 32],
    pub chunks: u64,
    pub bytes: u64,
}

impl Default for Chain {
    fn default() -> Self {
        Self { head: sha256(SCHEME.as_bytes()), chunks: 0, bytes: 0 }
    }
}

impl Chain {
    /// The chain of no chunks.
    pub fn new() -> Self {
        Self::default()
    }

    /// The chain of `chunks`, in order.
    pub fn of<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut c = Self::new();
        for chunk in chunks {
            c.push(chunk);
        }
        c
    }

    /// Folds `chunk` in, after those before it; its digest back.
    pub fn push(&mut self, chunk: &[u8]) -> [u8; 32] {
        let digest = sha256(chunk);
        self.push_digest(chunk.len() as u64, &digest);
        digest
    }

    /// Folds in a chunk known by its length and digest.
    pub fn push_digest(&mut self, len: u64, digest: &[u8; 32]) {
        let mut h = Sha256::new();
        h.update(self.head);
        h.update(digest);
        self.head = to_32(h.finalize().as_slice());
        self.chunks += 1;
        self.bytes = self.bytes.saturating_add(len);
    }

    /// Its head, as hex.
    pub fn head_hex(&self) -> String {
        hex(&self.head)
    }

    /// A chain known by its head (hex, as [`Chain::head_hex`] writes it) and its counts.
    pub fn from_hex(head: &str, chunks: u64, bytes: u64) -> Option<Self> {
        Some(Self { head: unhex(head)?, chunks, bytes })
    }
}

/// A chunk the server took of a step's log: where in the log it's kept (`None`: it isn't, past the
/// log's size cap, or the log couldn't be written), its length and its digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub at: Option<u64>,
    pub len: u64,
    pub digest: [u8; 32],
}

/// A chunk's digest, as the chain folds it in.
pub fn digest(chunk: &[u8]) -> [u8; 32] {
    sha256(chunk)
}

/// A piece's line in `<step>.digests`: `<offset> <length> <sha256>`, `-` for the offset of one
/// that isn't kept.
pub fn line(p: &Piece) -> String {
    let at = p.at.map_or_else(|| "-".to_owned(), |a| a.to_string());
    format!("{at} {} {}", p.len, hex(&p.digest))
}

/// The line that ends an attempt's pieces in `<step>.digests`: a retry in place writes the next
/// attempt's into the same log (docs/ci.md W05-R28).
pub fn boundary(why: &str) -> String {
    format!("# {}", why.replace(['\r', '\n'], " "))
}

/// `<step>.digests` read: each attempt's pieces, oldest attempt first (none written: one
/// attempt of none). A line torn by a crash, or not a piece's, is left out.
pub fn parse(text: &str) -> Vec<Vec<Piece>> {
    let mut attempts = vec![Vec::new()];
    for l in text.lines() {
        if l.starts_with('#') {
            attempts.push(Vec::new());
            continue;
        }
        let mut f = l.split(' ');
        let (Some(at), Some(len), Some(digest), None) = (f.next(), f.next(), f.next(), f.next()) else { continue };
        let at = match at {
            "-" => None,
            n => match n.parse() {
                Ok(n) => Some(n),
                Err(_) => continue,
            },
        };
        let (Ok(len), Some(digest)) = (len.parse(), unhex(digest)) else { continue };
        if let Some(last) = attempts.last_mut() {
            last.push(Piece { at, len, digest });
        }
    }
    attempts
}

/// How a step's log as kiln keeps it stands to the chain its leg signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The log the leg sent: `kept` bytes of it in the step's log, and `dropped` chunks known by
    /// their digests alone (past the log's size cap).
    Verified { kept: u64, dropped: u64 },
    /// Piece `piece` (from 0), kept at `at`, isn't the bytes kiln took: the log was changed since.
    Altered { piece: u64, at: u64 },
    /// kiln took `have` of the `want` chunks the leg sent: some were lost on the way.
    Incomplete { have: u64, want: u64 },
    /// What kiln took isn't what the leg sent: another chain of as many chunks or more.
    Differs,
}

/// `log` (a step's log as kiln keeps it) and `pieces` (the attempt's, from its `<step>.digests`)
/// against `signed`, the chain its leg signed.
pub fn check(log: &[u8], pieces: &[Piece], signed: &Chain) -> Verdict {
    let mut took = Chain::new();
    for p in pieces {
        took.push_digest(p.len, &p.digest);
    }
    if took.chunks < signed.chunks {
        return Verdict::Incomplete { have: took.chunks, want: signed.chunks };
    }
    if took != *signed {
        return Verdict::Differs;
    }
    let (mut kept, mut dropped) = (0u64, 0u64);
    for (i, p) in pieces.iter().enumerate() {
        let Some(at) = p.at else {
            dropped += 1;
            continue;
        };
        let bytes = usize::try_from(at).ok().zip(usize::try_from(p.len).ok()).and_then(|(a, n)| log.get(a..a.checked_add(n)?));
        if bytes.is_none_or(|b| sha256(b) != p.digest) {
            return Verdict::Altered { piece: i as u64, at };
        }
        kept = kept.saturating_add(p.len);
    }
    Verdict::Verified { kept, dropped }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    to_32(Sha256::digest(bytes).as_slice())
}

fn to_32(bytes: &[u8]) -> [u8; 32] {
    <[u8; 32]>::try_from(bytes).unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (o, pair) in out.iter_mut().zip(s.as_bytes().chunks(2)) {
        *o = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a server keeps of `chunks` under a `cap` on the log's bytes, as `Logs::append` does:
    /// the log (with the cap's notice in place of the chunk that crossed it) and the pieces.
    fn kept(chunks: &[&[u8]], cap: usize) -> (Vec<u8>, Vec<Piece>) {
        let notice = b"[kiln] log size limit reached";
        let (mut log, mut pieces, mut capped) = (Vec::new(), Vec::new(), false);
        for c in chunks {
            let digest = sha256(c);
            let len = c.len() as u64;
            if capped || log.len() + c.len() > cap {
                if !capped {
                    log.extend_from_slice(notice);
                }
                capped = true;
                pieces.push(Piece { at: None, len, digest });
                continue;
            }
            pieces.push(Piece { at: Some(log.len() as u64), len, digest });
            log.extend_from_slice(c);
        }
        (log, pieces)
    }

    const SENT: [&[u8]; 4] = [b"$ cargo test\n", b"   Compiling kiln-ci\n", b"test logs ... ok\n", b"test result: ok. 1 passed\n"];

    /// W27-R05: the log a leg sent, as kiln keeps it, verifies against the leg's chain.
    #[test]
    fn w27_r05_a_log_verifies_as_its_leg_sent_it() {
        let signed = Chain::of(SENT);
        let (log, pieces) = kept(&SENT, 1 << 20);
        assert_eq!(check(&log, &pieces, &signed), Verdict::Verified { kept: log.len() as u64, dropped: 0 });
        assert_eq!((signed.chunks, signed.bytes), (4, SENT.iter().map(|c| c.len() as u64).sum()));
    }

    /// W27-R05: a byte changed in the log kiln keeps is found, and where.
    #[test]
    fn w27_r05_a_changed_byte_is_altered() {
        let [first, second, ..] = SENT;
        let signed = Chain::of(SENT);
        let (mut log, pieces) = kept(&SENT, 1 << 20);
        let at = first.len() + second.len() + 5;
        if let Some(b) = log.get_mut(at) {
            *b ^= 1;
        }
        assert_eq!(check(&log, &pieces, &signed), Verdict::Altered { piece: 2, at: (first.len() + second.len()) as u64 });
        // A log cut short is altered as well: its last piece's bytes are gone.
        let short = log.get(..log.len() - 3).unwrap_or_default();
        assert!(matches!(check(short, &pieces, &signed), Verdict::Altered { piece: 2 | 3, .. }));
    }

    /// W27-R05: chunks lost on the way leave kiln fewer than the leg signed; one swapped for
    /// another, or reordered, is another log.
    #[test]
    fn w27_r05_lost_swapped_or_reordered_chunks_dont_verify() {
        let [first, second, third, fourth] = SENT;
        let signed = Chain::of(SENT);
        let lost = [first, second, fourth];
        let (log, pieces) = kept(&lost, 1 << 20);
        assert_eq!(check(&log, &pieces, &signed), Verdict::Incomplete { have: 3, want: 4 });
        let swapped = [first, second, b"test logs ... FAILED\n".as_slice(), fourth];
        let (log, pieces) = kept(&swapped, 1 << 20);
        assert_eq!(check(&log, &pieces, &signed), Verdict::Differs);
        let reordered = [second, first, third, fourth];
        assert_ne!(Chain::of(reordered), signed);
        // Another scheme's chain of the same chunks never equals this one.
        let mut other = Chain { head: sha256(b"kiln/log/v0"), chunks: 0, bytes: 0 };
        for c in SENT {
            other.push(c);
        }
        assert_ne!(other.head, signed.head);
    }

    /// W27-R05: a log kiln cut short at its size cap still verifies: what it kept by its bytes,
    /// the rest by its digests.
    #[test]
    fn w27_r05_a_capped_log_still_verifies() {
        let [first, second, ..] = SENT;
        let signed = Chain::of(SENT);
        let cap = first.len() + second.len() + 2;
        let (log, pieces) = kept(&SENT, cap);
        assert!(log.ends_with(b"[kiln] log size limit reached"));
        assert_eq!(check(&log, &pieces, &signed), Verdict::Verified { kept: (first.len() + second.len()) as u64, dropped: 2 });
    }

    /// `<step>.digests`, written and read: each attempt's pieces after a retry's mark, and a torn
    /// or foreign line left out.
    #[test]
    fn w27_r05_digests_lines_read_back_by_attempt() {
        let [first, ..] = SENT;
        let (_, pieces) = kept(&SENT, first.len() + 1);
        let mut text: String = pieces.iter().map(|p| line(p) + "\n").collect();
        text.push_str(&boundary("attempt 2\nbegins"));
        text.push('\n');
        text.push_str(&line(&Piece { at: Some(70), len: 3, digest: sha256(b"abc") }));
        text.push_str("\nnot a piece\n12 3 abc\n");
        text.push_str(line(&Piece { at: Some(73), len: 1, digest: sha256(b"d") }).get(..20).unwrap_or_default());
        let attempts = parse(&text);
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts.first(), Some(&pieces));
        assert_eq!(attempts.get(1).map(Vec::as_slice), Some([Piece { at: Some(70), len: 3, digest: sha256(b"abc") }].as_slice()));
        assert!(pieces.iter().any(|p| p.at.is_none()), "a piece past the cap is kept as `-`");
        assert_eq!(parse(""), vec![Vec::new()]);
        assert!(!boundary("a\r\nb").contains('\n'));
    }
}
