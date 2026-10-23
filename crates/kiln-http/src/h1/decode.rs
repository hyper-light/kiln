//! Body decoders: a `Content-Length` body, a chunked one (RFC 9112 §7.1), or one that runs to the
//! close. Each takes what it can from the read buffer, data without a copy.

use bytes::{Buf as _, Bytes, BytesMut};
use http::HeaderMap;

use crate::Error;
use crate::error::Parse;

/// What a decoder took from the buffer.
#[derive(Debug)]
pub(crate) enum Decoded {
    Data(Bytes),
    /// The body ended (with trailers, if it had them).
    End(Option<HeaderMap>),
    /// Nothing whole yet: read more.
    More,
}

/// The longest chunk-size line, extensions included.
const MAX_SIZE_LINE: usize = 4 << 10;
/// The most bytes of chunk extensions a body may carry in all.
const MAX_EXTENSIONS: usize = 64 << 10;
/// The largest trailer section.
const MAX_TRAILERS: usize = 16 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The chunk size's hex digits.
    Size,
    /// Whitespace after the size, before extensions.
    SizeSpace,
    /// Chunk extensions, ignored.
    Extension,
    /// The size line's LF.
    SizeLf,
    Data,
    DataCr,
    DataLf,
    /// The trailer section, to its blank line.
    Trailers,
    Done,
}

#[derive(Debug)]
pub(crate) enum Decoder {
    Length(u64),
    Chunked {
        step: Step,
        size: u64,
        digits: u8,
        line: usize,
        extensions: usize,
        trailers: Vec<u8>,
    },
    /// To the end of the stream (a response with no length).
    Close,
}

impl Decoder {
    pub(crate) fn length(n: u64) -> Decoder {
        Decoder::Length(n)
    }

    pub(crate) fn chunked() -> Decoder {
        Decoder::Chunked { step: Step::Size, size: 0, digits: 0, line: 0, extensions: 0, trailers: Vec::new() }
    }

    /// Whether the body is done.
    pub(crate) fn is_done(&self) -> bool {
        match self {
            Decoder::Length(n) => *n == 0,
            Decoder::Chunked { step, .. } => *step == Step::Done,
            Decoder::Close => false,
        }
    }

    /// The stream ended: the body ends with it if it runs to the close; otherwise it broke off.
    pub(crate) fn at_eof(&mut self) -> Result<Decoded, Error> {
        match self {
            Decoder::Close => {
                *self = Decoder::Length(0);
                Ok(Decoded::End(None))
            }
            _ if self.is_done() => Ok(Decoded::End(None)),
            _ => Err(Error::closed()),
        }
    }

    /// Takes the next piece of the body from `buf`.
    pub(crate) fn decode(&mut self, buf: &mut BytesMut) -> Result<Decoded, Error> {
        match self {
            Decoder::Length(remaining) => {
                if *remaining == 0 {
                    return Ok(Decoded::End(None));
                }
                if buf.is_empty() {
                    return Ok(Decoded::More);
                }
                let n = usize::try_from(*remaining).unwrap_or(usize::MAX).min(buf.len());
                *remaining -= n as u64;
                Ok(Decoded::Data(buf.split_to(n).freeze()))
            }
            Decoder::Close => {
                if buf.is_empty() {
                    return Ok(Decoded::More);
                }
                Ok(Decoded::Data(buf.split().freeze()))
            }
            Decoder::Chunked { step, size, digits, line, extensions, trailers } => loop {
                if *step == Step::Data {
                    if buf.is_empty() {
                        return Ok(Decoded::More);
                    }
                    let n = usize::try_from(*size).unwrap_or(usize::MAX).min(buf.len());
                    *size -= n as u64;
                    if *size == 0 {
                        *step = Step::DataCr;
                    }
                    return Ok(Decoded::Data(buf.split_to(n).freeze()));
                }
                if *step == Step::Done {
                    return Ok(Decoded::End(None));
                }
                if *step == Step::Trailers {
                    // Byte by byte to the blank line ending the section.
                    let Some(&b) = buf.first() else { return Ok(Decoded::More) };
                    buf.advance(1);
                    trailers.push(b);
                    if trailers.len() > MAX_TRAILERS {
                        return Err(Error::parse(Parse::Chunk));
                    }
                    if trailers.as_slice() == b"\r\n" {
                        *step = Step::Done;
                        return Ok(Decoded::End(None));
                    }
                    if trailers.ends_with(b"\r\n\r\n") {
                        *step = Step::Done;
                        let parsed = parse_trailers(trailers)?;
                        return Ok(Decoded::End(Some(parsed)));
                    }
                    continue;
                }
                let Some(&b) = buf.first() else { return Ok(Decoded::More) };
                buf.advance(1);
                *line += 1;
                if *line > MAX_SIZE_LINE {
                    return Err(Error::parse(Parse::Chunk));
                }
                match (*step, b) {
                    (Step::Size, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F') => {
                        *digits += 1;
                        if *digits > 16 {
                            return Err(Error::parse(Parse::Chunk));
                        }
                        let d = match b {
                            b'0'..=b'9' => b - b'0',
                            b'a'..=b'f' => b - b'a' + 10,
                            _ => b - b'A' + 10,
                        };
                        *size = (*size << 4) | u64::from(d);
                    }
                    (Step::Size | Step::SizeSpace, b' ' | b'\t') if *digits > 0 => *step = Step::SizeSpace,
                    (Step::Size | Step::SizeSpace, b';') if *digits > 0 => *step = Step::Extension,
                    (Step::Size | Step::SizeSpace | Step::Extension, b'\r') if *digits > 0 => *step = Step::SizeLf,
                    (Step::Extension, b'\n') => return Err(Error::parse(Parse::Chunk)),
                    (Step::Extension, _) => {
                        *extensions += 1;
                        if *extensions > MAX_EXTENSIONS {
                            return Err(Error::parse(Parse::Chunk));
                        }
                    }
                    (Step::SizeLf, b'\n') => {
                        *line = 0;
                        *digits = 0;
                        *step = if *size == 0 { Step::Trailers } else { Step::Data };
                    }
                    (Step::DataCr, b'\r') => *step = Step::DataLf,
                    (Step::DataLf, b'\n') => {
                        *line = 0;
                        *step = Step::Size;
                    }
                    _ => return Err(Error::parse(Parse::Chunk)),
                }
            },
        }
    }
}

fn parse_trailers(raw: &[u8]) -> Result<HeaderMap, Error> {
    let mut parsed = [httparse::EMPTY_HEADER; 32];
    match httparse::parse_headers(raw, &mut parsed) {
        Ok(httparse::Status::Complete((_, headers))) => {
            let mut map = HeaderMap::with_capacity(headers.len());
            for h in headers {
                let name = http::HeaderName::from_bytes(h.name.as_bytes()).map_err(|_| Error::parse(Parse::Chunk))?;
                let value = http::HeaderValue::from_bytes(h.value).map_err(|_| Error::parse(Parse::Chunk))?;
                map.append(name, value);
            }
            Ok(map)
        }
        _ => Err(Error::parse(Parse::Chunk)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes `wire` fed `step` bytes at a time: the data, and the trailers.
    fn decode(wire: &[u8], step: usize) -> Result<(Vec<u8>, Option<HeaderMap>), Error> {
        let mut d = Decoder::chunked();
        let mut buf = BytesMut::new();
        let mut data = Vec::new();
        let mut fed = 0;
        loop {
            match d.decode(&mut buf)? {
                Decoded::Data(b) => data.extend_from_slice(&b),
                Decoded::End(t) => return Ok((data, t)),
                Decoded::More => {
                    let next = wire.get(fed..(fed + step).min(wire.len())).unwrap_or_default();
                    if next.is_empty() {
                        return Err(Error::closed());
                    }
                    buf.extend_from_slice(next);
                    fed += next.len();
                }
            }
        }
    }

    #[test]
    fn chunked_bodies_decode_however_they_arrive() -> Result<(), Error> {
        let wire = b"5\r\nhello\r\n6;name=value\r\n world\r\n0\r\nx-sum: 11\r\n\r\n";
        for step in [1, 2, 3, 7, wire.len()] {
            let (data, trailers) = decode(wire, step)?;
            assert_eq!(data, b"hello world");
            assert_eq!(trailers.as_ref().and_then(|t| t.get("x-sum")).map(http::HeaderValue::as_bytes), Some(&b"11"[..]));
        }
        assert_eq!(decode(b"A\r\n0123456789\r\n0\r\n\r\n", 4)?.0, b"0123456789");
        Ok(())
    }

    #[test]
    fn malformed_chunks_are_refused() {
        for bad in [&b"5\nhello\r\n0\r\n\r\n"[..], b"5\r\nhelloX\r\n0\r\n\r\n", b"x\r\n", b"\r\n", b"11111111111111111\r\n", b"5 5\r\nhello\r\n"] {
            assert!(decode(bad, 1).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn a_length_body_takes_only_its_length() -> Result<(), Error> {
        let mut d = Decoder::length(3);
        let mut buf = BytesMut::from(&b"abcGET"[..]);
        assert!(matches!(d.decode(&mut buf)?, Decoded::Data(b) if &b[..] == b"abc"));
        assert!(matches!(d.decode(&mut buf)?, Decoded::End(None)));
        assert_eq!(&buf[..], b"GET");
        Ok(())
    }
}
