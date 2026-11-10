//! A viewer's packed snapshot (`?z=gzip`) made of pieces deflated once (goal 10, plan 01a1a87b2dae
//! step 3). A snapshot was gzipped whole on every attach: a long session's ten megabytes of JSON took
//! most of a second, and the frames its viewer waited on waited that long too (goal 4's lab). Now each
//! sealed chunk of a copy's events (`super::copies::Chunk`) keeps its events' texts deflated once, a
//! [`Segment`], and a snapshot is one gzip member put together from them, as pigz puts its pieces
//! together: each segment is raw deflate that ends byte-aligned on a sync flush and is never final, the
//! few bytes between them (a frame's head and end) go as stored blocks, an empty final block ends the
//! stream, and the trailer's CRC-32 is the segments' and those bytes' combined (`crc32fast`'s
//! `combine`). Any gzip decoder reads it as the one member it is (`DecompressionStream("gzip")` in the
//! UI, `flate2::read::GzDecoder` in the tests).

use std::io::{self, Write as _};

use bytes::Bytes;
use flate2::Compression;

/// What a piece of a gzip stream holds, deflated: raw deflate ending on a sync flush (so byte-aligned
/// and never final), with the CRC-32 and length of what it holds.
pub struct Segment {
    pub deflated: Bytes,
    pub crc: u32,
    pub len: u64,
}

/// The most a stored block holds (RFC 1951 3.2.4: its length is 16 bits).
const STORED_MAX: usize = u16::MAX as usize;

/// An empty stored block that's final: what ends a stream of pieces that each end on a sync flush.
const END: [u8; 5] = [1, 0, 0, 0xff, 0xff];

#[cfg(test)]
thread_local! {
    /// Segments inflated on this thread (goal 10, plan 01a1a87b2dae step 5's tests: what a read inflated).
    pub(crate) static INFLATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// What a segment holds, inflated again (goal 11 reads an event's text out of its chunk's so): its
/// raw deflate ended with [`END`], checked against the length and CRC-32 it says it holds.
pub fn inflate(s: &Segment) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    #[cfg(test)]
    INFLATED.with(|n| n.set(n.get() + 1));
    let mut out = Vec::with_capacity(usize::try_from(s.len).unwrap_or(0));
    flate2::read::DeflateDecoder::new(s.deflated.as_ref().chain(END.as_slice())).read_to_end(&mut out)?;
    if u64::try_from(out.len()).ok() != Some(s.len) || crc32fast::hash(&out) != s.crc {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a segment that doesn't hold what it says"));
    }
    Ok(out)
}

/// `parts`, one after the other, deflated at `level` into a [`Segment`].
pub fn deflate<'a>(level: Compression, parts: impl IntoIterator<Item = &'a [u8]>) -> io::Result<Segment> {
    let mut d = Deflating::new(level);
    for part in parts {
        d.part(part)?;
    }
    d.finish()
}

/// A [`Segment`] made a part at a time, over as many calls as its maker likes ([`deflate`] makes one in
/// one): the packer's, a sealed chunk an event at a time, giving its compute thread up between any two
/// (goal 10, plan 01a1a87b2dae step 3: `super::copies`). The parts fed the same way make the same bytes,
/// however the calls fall.
pub struct Deflating {
    c: flate2::write::DeflateEncoder<Vec<u8>>,
    crc: crc32fast::Hasher,
    len: u64,
}

impl Deflating {
    pub fn new(level: Compression) -> Self {
        Self { c: flate2::write::DeflateEncoder::new(Vec::new(), level), crc: crc32fast::Hasher::new(), len: 0 }
    }

    /// `part`, after the parts before it.
    pub fn part(&mut self, part: &[u8]) -> io::Result<()> {
        self.crc.update(part);
        self.len = self.len.saturating_add(u64::try_from(part.len()).unwrap_or(u64::MAX));
        self.c.write_all(part)
    }

    /// The segment of the parts fed, flushed to a byte boundary.
    pub fn finish(mut self) -> io::Result<Segment> {
        // W21-R15: miniz may spend the first sync call draining pending output, and flate2
        // then drains with FlushNone. Flush again after that drain so the backend writes the
        // byte-aligned sync boundary. flush_finish takes the bytes without a final block.
        self.c.flush()?;
        let mut out = self.c.flush_finish()?;
        if !out.ends_with(&[0, 0, 0xff, 0xff]) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a deflate segment without its sync flush boundary"));
        }
        // What it holds and no more: a segment is kept as long as its chunk (goal 11, plan 01a1a87b2e56 step 2),
        // and the buffer it was deflated into grew by doubling.
        out.shrink_to_fit();
        Ok(Segment { deflated: Bytes::from(out), crc: self.crc.finalize(), len: self.len })
    }
}

/// A gzip member put together from pieces ([`Segment`]s, stored bytes, bytes deflated as they come).
pub struct Gzip {
    out: Vec<u8>,
    crc: crc32fast::Hasher,
    len: u64,
}

impl Gzip {
    /// A member about `room` bytes long, its header written.
    pub fn new(room: usize) -> Self {
        let mut out = Vec::with_capacity(room.saturating_add(32));
        // ID1 ID2, CM deflate, no flags, no time, no extra flags, OS unknown (RFC 1952 2.3).
        out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
        Self { out, crc: crc32fast::Hasher::new(), len: 0 }
    }

    /// A segment, as it was deflated.
    pub fn segment(&mut self, s: &Segment) {
        self.out.extend_from_slice(&s.deflated);
        self.crc.combine(&crc32fast::Hasher::new_with_initial_len(s.crc, s.len));
        self.len = self.len.wrapping_add(s.len);
    }

    /// `bytes` as they are, in stored blocks (a frame's few bytes around a segment).
    pub fn stored(&mut self, bytes: &[u8]) {
        for block in bytes.chunks(STORED_MAX) {
            let n = u16::try_from(block.len()).unwrap_or(u16::MAX);
            // Not final, stored, to the byte boundary the last piece ended on; its length and that
            // length's complement.
            self.out.push(0);
            self.out.extend_from_slice(&n.to_le_bytes());
            self.out.extend_from_slice(&(!n).to_le_bytes());
            self.out.extend_from_slice(block);
        }
        self.crc.update(bytes);
        self.len = self.len.wrapping_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    /// `bytes` deflated now, at `level`.
    pub fn deflate(&mut self, level: Compression, bytes: &[u8]) -> io::Result<()> {
        if !bytes.is_empty() {
            self.segment(&deflate(level, [bytes])?);
        }
        Ok(())
    }

    /// What it's made so far that hasn't been taken (its header with the first): a member sent as it's made
    /// goes out a piece at a time (goal 11, plan 01a1a87b2e56 step 5), [`Gzip::finish`] giving the rest.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// The bytes it's made since what was last taken.
    pub fn made(&self) -> usize {
        self.out.len()
    }

    /// The member: an empty final stored block, then the trailer (the CRC-32 and the length of all it
    /// holds, modulo 2^32).
    pub fn finish(mut self) -> Vec<u8> {
        self.out.extend_from_slice(&END);
        self.out.extend_from_slice(&self.crc.finalize().to_le_bytes());
        self.out.extend_from_slice(&u32::try_from(self.len & u64::from(u32::MAX)).unwrap_or(u32::MAX).to_le_bytes());
        self.out
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// W21-R15: this generated prefix first fills the old 4,096-byte output buffer at the end
    /// of a one-byte part. Finishing then used to drain pending bytes without aligning the stream.
    #[test]
    fn w21_r15_pending_output_is_drained_before_the_alignment_flush() -> io::Result<()> {
        let input: Vec<u8> = (0u64..).flat_map(|q| format!("{:016x}", q.wrapping_mul(0x9E37_79B9_7F4A_7C15)).into_bytes()).take(74_892).collect();
        let segment = deflate(Compression::default(), input.chunks(1))?;
        assert!(segment.deflated.ends_with(&[0, 0, 0xff, 0xff]), "every segment ends byte-aligned on a sync flush");
        assert_eq!(inflate(&segment)?, input, "the segment retains every input byte");
        let mut member = Gzip::new(segment.deflated.len());
        member.stored(b"before\n");
        member.segment(&segment);
        member.stored(b"\nafter");
        assert_eq!(gunzip(&member.finish())?, [b"before\n".as_slice(), input.as_slice(), b"\nafter"].concat());
        Ok(())
    }

    fn gunzip(b: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out)?;
        Ok(out)
    }

    fn whole(b: &[u8]) -> io::Result<Vec<u8>> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(b)?;
        gz.finish()
    }

    /// Goal 10, plan 01a1a87b2dae step 3: a member put together from segments deflated apart, stored
    /// bytes between them and bytes deflated as they came reads back, with flate2's decoder, as the
    /// same bytes the old whole-gzip snapshot held, its CRC and length checked by the decoder; and a
    /// segment's own CRC and length are those of what it holds.
    #[test]
    fn g10_s3_a_member_put_together_reads_back_as_the_whole_gzip_did() -> io::Result<()> {
        let event = |n: usize| format!(r#"{{"id":"e{n}","seq":{n},"body":{{"text":"{}"}}}}"#, "the retry loop reads its deadline ".repeat(n % 9 + 1));
        let texts: Vec<String> = (1..=70).map(event).collect();
        let joined = texts.join(",");
        let mut parts: Vec<&[u8]> = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                parts.push(b",");
            }
            parts.push(t.as_bytes());
        }
        let seg = deflate(Compression::default(), parts)?;
        assert_eq!((seg.crc, seg.len), (crc32fast::hash(joined.as_bytes()), u64::try_from(joined.len()).unwrap_or(0)));
        assert_eq!(inflate(&seg)?, joined.as_bytes(), "a segment inflates to what it holds");
        let head = br#"{"t":"open","info":{"id":"s"},"rev":3}"#;
        let mut gz = Gzip::new(seg.deflated.len());
        gz.deflate(Compression::fast(), &[head.as_slice(), b"\n"].concat())?;
        gz.stored(br#"{"t":"events","events":["#);
        gz.segment(&seg);
        gz.stored(b"]}\n");
        // A second copy of the same segment: a piece is taken as it is however often.
        gz.stored(br#"{"t":"events","events":["#);
        gz.segment(&seg);
        gz.stored(b"]}\n");
        gz.deflate(Compression::fast(), br#"{"t":"synced"}"#)?;
        let packed = gz.finish();
        let mut want = Vec::new();
        want.extend_from_slice(head);
        want.push(b'\n');
        for _ in 0..2 {
            want.extend_from_slice(br#"{"t":"events","events":["#);
            want.extend_from_slice(joined.as_bytes());
            want.extend_from_slice(b"]}\n");
        }
        want.extend_from_slice(br#"{"t":"synced"}"#);
        assert_eq!(gunzip(&packed)?, gunzip(&whole(&want)?)?, "the same bytes as the whole gzip");
        assert_eq!(gunzip(&packed)?, want);
        assert!(packed.len() * 4 < want.len(), "it packs: {} of {}", packed.len(), want.len());
        Ok(())
    }

    /// The member the UI's e2e check unpacks in the browsers, with the browser's own decoder the agent
    /// page uses (ui/e2e/packed-snapshot.spec.ts, `DecompressionStream("gzip")`): made here as a
    /// snapshot's is, a frame deflated as it came, then a chunk's events between their frame's stored
    /// head and end, twice, then `synced`. `KILN_BLESS=1` writes it again after a change to how one is
    /// put together.
    fn fixture() -> io::Result<(Vec<u8>, String)> {
        let texts: Vec<String> = (1..=70).map(|n| format!(r#"{{"id":"e{n}","seq":{n},"kind":"assistant","text":"{n}: {}"}}"#, "the viewer's snapshot comes whole ".repeat(n % 5 + 1))).collect();
        let joined = texts.join(",");
        let seg = deflate(Compression::default(), [joined.as_bytes()])?;
        let head = r#"{"t":"open","info":{"id":"fixture"},"born":7,"rev":70}"#;
        let mut gz = Gzip::new(seg.deflated.len() * 2);
        gz.deflate(Compression::fast(), format!("{head}\n").as_bytes())?;
        let mut text = format!("{head}\n");
        for _ in 0..2 {
            gz.stored(br#"{"t":"events","events":["#);
            gz.segment(&seg);
            gz.stored(b"]}\n");
            text.push_str(&format!(r#"{{"t":"events","events":[{joined}]}}"#));
            text.push('\n');
        }
        gz.deflate(Compression::fast(), br#"{"t":"synced"}"#)?;
        text.push_str(r#"{"t":"synced"}"#);
        Ok((gz.finish(), text))
    }

    #[test]
    fn g10_s3_the_browsers_fixture_is_what_the_packer_makes() -> io::Result<()> {
        let (packed, text) = fixture()?;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/e2e/fixtures");
        if std::env::var_os("KILN_BLESS").is_some() {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("packed-snapshot.gz"), &packed)?;
            std::fs::write(dir.join("packed-snapshot.txt"), &text)?;
        }
        assert_eq!(gunzip(&packed)?, text.as_bytes());
        assert!(std::fs::read(dir.join("packed-snapshot.gz"))? == packed, "ui/e2e/fixtures/packed-snapshot.gz isn't what the packer makes now: KILN_BLESS=1 writes it again");
        assert_eq!(std::fs::read_to_string(dir.join("packed-snapshot.txt"))?, text);
        Ok(())
    }

    /// Stored bytes past a stored block's 65,535 go in as many blocks as they take; nothing at all is a
    /// member of nothing.
    #[test]
    fn stored_bytes_of_any_length_and_nothing_read_back() -> io::Result<()> {
        let big: Vec<u8> = (0..200_000u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let mut gz = Gzip::new(big.len());
        gz.stored(&big);
        gz.deflate(Compression::fast(), b"")?;
        gz.segment(&deflate(Compression::fast(), std::iter::empty::<&[u8]>())?);
        assert_eq!(gunzip(&gz.finish())?, big);
        assert_eq!(gunzip(&Gzip::new(0).finish())?, Vec::<u8>::new());
        Ok(())
    }
}
