//! HTTP/1.1 message heads (RFC 9112): parsed by httparse, the header values left in the bytes they
//! arrived in (no copy), and what the head says about the body that follows and the connection.

use bytes::{Bytes, BytesMut};
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, StatusCode, Uri, Version};

use crate::Error;
use crate::buf::{put_dec, put_hex};
use crate::error::Parse;

/// The most header lines a head may have.
pub(crate) const MAX_HEADERS: usize = 128;

/// How a message's body is delimited (RFC 9112 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// No body.
    Empty,
    /// `Content-Length` bytes.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Until the connection closes (a response with neither, or HTTP/1.0).
    Close,
}

/// A parsed request head.
pub(crate) struct RequestHead {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub framing: Framing,
    /// The client wants the connection kept for another request.
    pub keep_alive: bool,
    /// `Expect: 100-continue`.
    pub expects_continue: bool,
    /// The client asks to switch protocols (`Connection: upgrade` with `Upgrade`), or to tunnel (`CONNECT`).
    pub upgrade: bool,
}

/// A parsed response head.
pub(crate) struct ResponseHead {
    pub status: StatusCode,
    pub version: Version,
    pub headers: HeaderMap,
    pub framing: Framing,
    pub keep_alive: bool,
}

/// Where a header's name and value are in the head's bytes.
#[derive(Clone, Copy, Default)]
struct Spans {
    name: (usize, usize),
    value: (usize, usize),
}

fn span_of(base: usize, part: &[u8]) -> (usize, usize) {
    let start = (part.as_ptr() as usize).saturating_sub(base);
    (start, start + part.len())
}

thread_local! {
    /// The spans of the head being parsed, kept per thread (never zeroed, never per request).
    static SPANS: std::cell::Cell<Vec<Spans>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Header slots for httparse, left uninitialized (it writes only what it parses).
type Slots<'b> = [std::mem::MaybeUninit<httparse::Header<'b>>; MAX_HEADERS];

fn slots<'b>() -> Slots<'b> {
    [const { std::mem::MaybeUninit::uninit() }; MAX_HEADERS]
}

/// What the headers of a message say about it.
#[derive(Default)]
struct Said {
    length: Option<u64>,
    lengths_differ: bool,
    bad_length: bool,
    chunked: bool,
    /// A transfer coding other than a final `chunked`.
    other_coding: bool,
    transfer_encoding: bool,
    close: bool,
    keep_alive: bool,
    upgrade_token: bool,
    upgrade_header: bool,
    expects_continue: bool,
}

fn tokens(v: &[u8]) -> impl Iterator<Item = &[u8]> {
    v.split(|b| *b == b',').map(|t| t.trim_ascii()).filter(|t| !t.is_empty())
}

fn note(said: &mut Said, name: &HeaderName, value: &[u8]) {
    if name == header::CONTENT_LENGTH {
        // A list of the same length repeated is that length (RFC 9110 §8.6); anything else is refused.
        for t in tokens(value) {
            match std::str::from_utf8(t).ok().filter(|t| t.bytes().all(|b| b.is_ascii_digit())).and_then(|t| t.parse::<u64>().ok()) {
                Some(n) => match said.length {
                    Some(m) if m != n => said.lengths_differ = true,
                    _ => said.length = Some(n),
                },
                None => said.bad_length = true,
            }
        }
        if tokens(value).next().is_none() {
            said.bad_length = true;
        }
    } else if name == header::TRANSFER_ENCODING {
        said.transfer_encoding = true;
        for t in tokens(value) {
            // A coding after `chunked` means chunked wasn't last (RFC 9112 §6.3).
            if said.chunked {
                said.other_coding = true;
            }
            if t.eq_ignore_ascii_case(b"chunked") {
                said.chunked = true;
            } else {
                said.other_coding = true;
            }
        }
    } else if name == header::CONNECTION {
        for t in tokens(value) {
            if t.eq_ignore_ascii_case(b"close") {
                said.close = true;
            } else if t.eq_ignore_ascii_case(b"keep-alive") {
                said.keep_alive = true;
            } else if t.eq_ignore_ascii_case(b"upgrade") {
                said.upgrade_token = true;
            }
        }
    } else if name == header::UPGRADE {
        said.upgrade_header = true;
    } else if name == header::EXPECT {
        said.expects_continue |= value.trim_ascii().eq_ignore_ascii_case(b"100-continue");
    }
}

/// `head`'s bytes in `span`, sharing them.
fn piece(head: &Bytes, span: (usize, usize)) -> Bytes {
    let end = span.1.min(head.len());
    head.slice(span.0.min(end)..end)
}

/// `head` narrowed to `span`: the last piece of a head takes the head's own reference, so it costs no
/// count of its own.
fn narrowed(mut head: Bytes, span: (usize, usize)) -> Bytes {
    let end = span.1.min(head.len());
    head.truncate(end);
    bytes::Buf::advance(&mut head, span.0.min(end));
    head
}

/// The header map of a head (into `reuse`'s allocation, emptied, when one is given), its values
/// sharing `head`'s bytes.
fn header_map(head: Bytes, spans: &[Spans], said: &mut Said, reuse: Option<HeaderMap>) -> Result<HeaderMap, Error> {
    let mut headers = match reuse {
        Some(mut h) => {
            h.clear();
            h.reserve(spans.len());
            h
        }
        None => HeaderMap::with_capacity(spans.len()),
    };
    let Some((last, rest)) = spans.split_last() else { return Ok(headers) };
    for s in rest {
        let name = HeaderName::from_bytes(head.get(s.name.0..s.name.1).ok_or(Error::parse(Parse::Head))?).map_err(|_| Error::parse(Parse::Head))?;
        let value = piece(&head, s.value);
        note(said, &name, &value);
        headers.append(name, HeaderValue::from_maybe_shared(value).map_err(|_| Error::parse(Parse::Head))?);
    }
    let name = HeaderName::from_bytes(head.get(last.name.0..last.name.1).ok_or(Error::parse(Parse::Head))?).map_err(|_| Error::parse(Parse::Head))?;
    let value = narrowed(head, last.value);
    note(said, &name, &value);
    headers.append(name, HeaderValue::from_maybe_shared(value).map_err(|_| Error::parse(Parse::Head))?);
    Ok(headers)
}

fn version(minor: Option<u8>) -> Result<Version, Error> {
    match minor {
        Some(1) => Ok(Version::HTTP_11),
        Some(0) => Ok(Version::HTTP_10),
        _ => Err(Error::parse(Parse::Version)),
    }
}

/// Whether the request line at the start of `buf` is three parts, the last an HTTP-version as RFC
/// 9112 §2.3 writes one (`HTTP/` DIGIT `.` DIGIT; or as much of one as has come, the line not whole
/// yet): only then is a version httparse refuses one this server doesn't serve (505, RFC 9110
/// §15.6.6). A line of more parts or fewer, or a last part of anything else (a target with a space
/// in it: `PUT /clouds/bad name HTTP/1.1`), is no request line, which RFC 9112 §3.2 answers 400.
fn names_a_version(buf: &[u8]) -> bool {
    const SHAPE: &[u8] = b"HTTP/0.0";
    let (line, whole) = match buf.iter().position(|&b| b == b'\n') {
        Some(end) => (buf.get(..end).unwrap_or_default(), true),
        None => (buf, false),
    };
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut parts = line.split(|&b| b == b' ');
    let (Some(_), Some(_), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return false };
    let fits = version.iter().zip(SHAPE).all(|(&b, &s)| if s == b'0' { b.is_ascii_digit() } else { b == s });
    fits && if whole { version.len() == SHAPE.len() } else { version.len() <= SHAPE.len() }
}

fn head_error(e: httparse::Error) -> Error {
    match e {
        httparse::Error::TooManyHeaders => Error::parse(Parse::HeadTooLarge),
        httparse::Error::Version => Error::parse(Parse::Version),
        httparse::Error::Token => Error::parse(Parse::Method),
        _ => Error::parse(Parse::Head),
    }
}

/// Parses the request head at the start of `buf`, taking its bytes; `Ok(None)` while it's incomplete.
/// A head over `max` bytes is refused. Blank lines before a request are skipped (RFC 9112 §2.2).
/// `reuse` is a header map whose allocation the request's may take.
pub(crate) fn parse_request(buf: &mut BytesMut, max: usize, reuse: &mut Option<Box<HeaderMap>>) -> Result<Option<RequestHead>, Error> {
    let skip = buf.iter().take_while(|b| matches!(b, b'\r' | b'\n')).count();
    if skip > 0 {
        bytes::Buf::advance(buf, skip);
    }
    if buf.is_empty() {
        return Ok(None);
    }
    let mut spans = SPANS.with(std::cell::Cell::take);
    let parsed = parse_request_in(buf, max, &mut spans, reuse);
    spans.clear();
    SPANS.with(|c| c.set(spans));
    parsed
}

fn parse_request_in(buf: &mut BytesMut, max: usize, spans: &mut Vec<Spans>, reuse: &mut Option<Box<HeaderMap>>) -> Result<Option<RequestHead>, Error> {
    let mut parsed = slots();
    let mut req = httparse::Request::new(&mut []);
    let len = match req.parse_with_uninit_headers(buf, &mut parsed) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) if buf.len() >= max => return Err(Error::parse(Parse::HeadTooLarge)),
        Ok(httparse::Status::Partial) => return Ok(None),
        // No request line at all: answered 400, not 505 (RFC 9112 §3.2).
        Err(httparse::Error::Version) if !names_a_version(buf) => return Err(Error::parse(Parse::Head)),
        Err(e) => return Err(head_error(e)),
    };
    if len > max {
        return Err(Error::parse(Parse::HeadTooLarge));
    }
    let base = buf.as_ptr() as usize;
    let method = match req.method.unwrap_or_default() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        m => Method::from_bytes(m.as_bytes()).map_err(|_| Error::parse(Parse::Method))?,
    };
    let path = span_of(base, req.path.unwrap_or_default().as_bytes());
    let version = version(req.version)?;
    spans.extend(req.headers.iter().map(|h| Spans { name: span_of(base, h.name.as_bytes()), value: span_of(base, h.value) }));
    let head = buf.split_to(len).freeze();
    let uri = Uri::from_maybe_shared(if spans.is_empty() { narrowed(head.clone(), path) } else { piece(&head, path) }).map_err(|_| Error::parse(Parse::Uri))?;
    let mut said = Said::default();
    let headers = header_map(head, spans, &mut said, reuse.take().map(|b| *b))?;
    let framing = if said.transfer_encoding {
        // A request's length must be knowable: chunked, last (RFC 9112 §6.3), and never with
        // HTTP/1.0, which has no chunked coding; whatever `Content-Length` said is overridden.
        if !said.chunked || said.other_coding || version == Version::HTTP_10 {
            return Err(Error::parse(Parse::Length));
        }
        Framing::Chunked
    } else if said.bad_length || said.lengths_differ {
        return Err(Error::parse(Parse::Length));
    } else {
        match said.length {
            Some(0) | None => Framing::Empty,
            Some(n) => Framing::Length(n),
        }
    };
    let keep_alive = match version {
        Version::HTTP_11 => !said.close,
        _ => said.keep_alive && !said.close,
    } && !(said.transfer_encoding && said.length.is_some());
    let upgrade = (said.upgrade_token && said.upgrade_header) || method == Method::CONNECT;
    Ok(Some(RequestHead { method, uri, version, headers, framing, keep_alive, expects_continue: said.expects_continue && version == Version::HTTP_11, upgrade }))
}

/// Parses the response head at the start of `buf`, taking its bytes; `Ok(None)` while incomplete.
/// `method` is the request's: a response to `HEAD` (or a `2xx` to `CONNECT`) has no body.
pub(crate) fn parse_response(buf: &mut BytesMut, max: usize, method: &Method) -> Result<Option<ResponseHead>, Error> {
    let mut spans = SPANS.with(std::cell::Cell::take);
    let parsed = parse_response_in(buf, max, method, &mut spans);
    spans.clear();
    SPANS.with(|c| c.set(spans));
    parsed
}

fn parse_response_in(buf: &mut BytesMut, max: usize, method: &Method, spans: &mut Vec<Spans>) -> Result<Option<ResponseHead>, Error> {
    let mut parsed = slots();
    let mut resp = httparse::Response::new(&mut []);
    let len = match httparse::ParserConfig::default().parse_response_with_uninit_headers(&mut resp, buf, &mut parsed) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) if buf.len() >= max => return Err(Error::parse(Parse::HeadTooLarge)),
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(e) => return Err(head_error(e)),
    };
    if len > max {
        return Err(Error::parse(Parse::HeadTooLarge));
    }
    let base = buf.as_ptr() as usize;
    let status = StatusCode::from_u16(resp.code.unwrap_or(0)).map_err(|_| Error::parse(Parse::Head))?;
    let version = version(resp.version)?;
    spans.extend(resp.headers.iter().map(|h| Spans { name: span_of(base, h.name.as_bytes()), value: span_of(base, h.value) }));
    let head = buf.split_to(len).freeze();
    let mut said = Said::default();
    let headers = header_map(head, spans, &mut said, None)?;
    let bodiless = method == Method::HEAD || status.is_informational() || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED || (method == Method::CONNECT && status.is_success());
    let framing = if bodiless {
        Framing::Empty
    } else if said.transfer_encoding {
        // A response not ending chunked runs to the close (RFC 9112 §6.3).
        if said.chunked && !said.other_coding { Framing::Chunked } else { Framing::Close }
    } else if said.bad_length || said.lengths_differ {
        return Err(Error::parse(Parse::Length));
    } else {
        match said.length {
            Some(0) => Framing::Empty,
            Some(n) => Framing::Length(n),
            None => Framing::Close,
        }
    };
    let keep_alive = match version {
        Version::HTTP_11 => !said.close,
        _ => said.keep_alive && !said.close,
    } && framing != Framing::Close;
    Ok(Some(ResponseHead { status, version, headers, framing, keep_alive }))
}

/// The status line of a response.
fn status_line(out: &mut Vec<u8>, status: StatusCode) {
    out.extend_from_slice(match status.as_u16() {
        200 => b"HTTP/1.1 200 OK\r\n",
        204 => b"HTTP/1.1 204 No Content\r\n",
        101 => b"HTTP/1.1 101 Switching Protocols\r\n",
        304 => b"HTTP/1.1 304 Not Modified\r\n",
        404 => b"HTTP/1.1 404 Not Found\r\n",
        _ => {
            out.extend_from_slice(b"HTTP/1.1 ");
            out.extend_from_slice(status.as_str().as_bytes());
            out.push(b' ');
            out.extend_from_slice(status.canonical_reason().unwrap_or("Unknown").as_bytes());
            out.extend_from_slice(b"\r\n");
            return;
        }
    });
}

fn put_header(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.reserve(name.len() + value.len() + 4);
    out.extend_from_slice(name);
    out.push(b':');
    out.push(b' ');
    out.extend_from_slice(value);
    out.push(b'\r');
    out.push(b'\n');
}

/// How a response's body goes out, decided from its head, its body and the request.
pub(crate) struct ResponseFraming {
    pub framing: Framing,
    /// The connection closes after this response.
    pub close: bool,
}

/// What a response answers: the request's method and version, and whether the connection stays.
#[derive(Clone, Copy)]
pub(crate) struct Answering<'a> {
    pub method: &'a Method,
    pub version: Version,
    pub keep_alive: bool,
}

/// Encodes a response head into `out`. `body_len` is the body's exact length when known. Adds
/// `content-length` or `transfer-encoding: chunked` as the body needs, `date`, and `connection` as the
/// connection's future needs.
pub(crate) fn encode_response(out: &mut Vec<u8>, status: StatusCode, headers: &HeaderMap, body_len: Option<u64>, to: Answering<'_>, date: &[u8]) -> ResponseFraming {
    let Answering { method: req_method, version: req_version, keep_alive } = to;
    status_line(out, status);
    let mut close = !keep_alive;
    let mut has_length = None;
    let mut has_te = false;
    let mut has_date = false;
    let mut has_connection = false;
    for (name, value) in headers {
        if name == header::CONTENT_LENGTH {
            has_length = std::str::from_utf8(value.as_bytes()).ok().and_then(|v| v.trim().parse::<u64>().ok()).or(Some(u64::MAX));
        } else if name == header::TRANSFER_ENCODING {
            has_te = true;
            if req_version == Version::HTTP_10 {
                continue;
            }
        } else if name == header::DATE {
            has_date = true;
        } else if name == header::CONNECTION {
            has_connection = true;
            if tokens(value.as_bytes()).any(|t| t.eq_ignore_ascii_case(b"close")) {
                close = true;
            }
        }
        put_header(out, name.as_str().as_bytes(), value.as_bytes());
    }
    let bodiless = status.is_informational() || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
    let framing = if bodiless || *req_method == Method::HEAD || (*req_method == Method::CONNECT && status.is_success()) {
        // No body follows; a length given (the representation's, for HEAD and 304) stands, and one
        // known is said for HEAD.
        if has_length.is_none()
            && !bodiless
            && !has_te
            && *req_method == Method::HEAD
            && let Some(n) = body_len.filter(|n| *n > 0)
        {
            out.extend_from_slice(b"content-length: ");
            put_dec(out, n);
            out.extend_from_slice(b"\r\n");
        }
        Framing::Empty
    } else if let Some(n) = has_length {
        if n == u64::MAX {
            // A length that isn't a number: the body runs to the close.
            close = true;
            Framing::Close
        } else if n == 0 {
            Framing::Empty
        } else {
            Framing::Length(n)
        }
    } else if has_te && req_version != Version::HTTP_10 {
        Framing::Chunked
    } else if let Some(n) = body_len {
        out.extend_from_slice(b"content-length: ");
        put_dec(out, n);
        out.extend_from_slice(b"\r\n");
        if n == 0 { Framing::Empty } else { Framing::Length(n) }
    } else if req_version == Version::HTTP_10 {
        // HTTP/1.0 has no chunks: the body runs to the close.
        close = true;
        Framing::Close
    } else {
        out.extend_from_slice(b"transfer-encoding: chunked\r\n");
        Framing::Chunked
    };
    if !has_date {
        put_header(out, b"date", date);
    }
    if !has_connection {
        if close && status != StatusCode::SWITCHING_PROTOCOLS {
            out.extend_from_slice(b"connection: close\r\n");
        } else if req_version == Version::HTTP_10 && !close {
            out.extend_from_slice(b"connection: keep-alive\r\n");
        }
    }
    out.extend_from_slice(b"\r\n");
    ResponseFraming { framing, close }
}

/// Encodes a request head into `out`: origin-form target (the absolute URI when `absolute`), a
/// `host` from the URI when the headers have none, and the body's framing.
pub(crate) fn encode_request(out: &mut Vec<u8>, method: &Method, uri: &Uri, headers: &HeaderMap, body_len: Option<u64>, absolute: bool) -> Framing {
    out.extend_from_slice(method.as_str().as_bytes());
    out.push(b' ');
    if absolute || *method == Method::CONNECT {
        if *method == Method::CONNECT {
            out.extend_from_slice(uri.authority().map_or("", |a| a.as_str()).as_bytes());
        } else {
            out.extend_from_slice(uri.to_string().as_bytes());
        }
    } else {
        out.extend_from_slice(uri.path_and_query().map_or("/", |p| p.as_str()).as_bytes());
    }
    out.extend_from_slice(b" HTTP/1.1\r\n");
    let mut has_host = false;
    let mut has_length = None;
    let mut has_te = false;
    for (name, value) in headers {
        if name == header::HOST {
            has_host = true;
        } else if name == header::CONTENT_LENGTH {
            has_length = std::str::from_utf8(value.as_bytes()).ok().and_then(|v| v.trim().parse::<u64>().ok());
        } else if name == header::TRANSFER_ENCODING {
            has_te = true;
        }
        put_header(out, name.as_str().as_bytes(), value.as_bytes());
    }
    if !has_host && let Some(a) = uri.authority() {
        put_header(out, b"host", a.as_str().as_bytes());
    }
    let framing = if let Some(n) = has_length {
        if n == 0 { Framing::Empty } else { Framing::Length(n) }
    } else if has_te {
        Framing::Chunked
    } else {
        match body_len {
            Some(0) => {
                // A body-bearing method says its body is empty; the others say nothing.
                if matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
                    out.extend_from_slice(b"content-length: 0\r\n");
                }
                Framing::Empty
            }
            Some(n) => {
                out.extend_from_slice(b"content-length: ");
                put_dec(out, n);
                out.extend_from_slice(b"\r\n");
                Framing::Length(n)
            }
            None => {
                out.extend_from_slice(b"transfer-encoding: chunked\r\n");
                Framing::Chunked
            }
        }
    };
    out.extend_from_slice(b"\r\n");
    framing
}

/// The start of a chunk of `len` bytes.
pub(crate) fn chunk_head(out: &mut Vec<u8>, len: u64) {
    put_hex(out, len);
    out.extend_from_slice(b"\r\n");
}

/// The last chunk, with `trailers` when there are any.
pub(crate) fn last_chunk(out: &mut Vec<u8>, trailers: Option<&HeaderMap>) {
    out.extend_from_slice(b"0\r\n");
    for (name, value) in trailers.into_iter().flatten() {
        put_header(out, name.as_str().as_bytes(), value.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(s: &str) -> Result<Option<RequestHead>, Error> {
        parse_request(&mut BytesMut::from(s), 64 << 10, &mut None)
    }

    #[test]
    fn a_request_head_says_its_framing_and_its_connection() -> Result<(), Error> {
        let h = req("GET /a?b=1 HTTP/1.1\r\nHost: x\r\n\r\n")?.ok_or(Error::closed())?;
        assert_eq!((h.method, h.uri.to_string(), h.version, h.framing, h.keep_alive), (Method::GET, "/a?b=1".into(), Version::HTTP_11, Framing::Empty, true));
        let h = req("POST / HTTP/1.1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")?.ok_or(Error::closed())?;
        assert_eq!((h.framing, h.keep_alive), (Framing::Length(5), false));
        let h = req("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")?.ok_or(Error::closed())?;
        assert_eq!(h.framing, Framing::Chunked);
        let h = req("GET / HTTP/1.0\r\n\r\n")?.ok_or(Error::closed())?;
        assert!(!h.keep_alive);
        let h = req("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")?.ok_or(Error::closed())?;
        assert!(h.keep_alive);
        let h = req("GET /ws HTTP/1.1\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\r\n")?.ok_or(Error::closed())?;
        assert!(h.upgrade);
        let h = req("PUT /x HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n")?.ok_or(Error::closed())?;
        assert!(h.expects_continue);
        assert!(req("GET / HTTP/1.1\r\nHost: x\r\n")?.is_none());
        // Blank lines before a request are skipped.
        assert!(req("\r\n\r\nGET / HTTP/1.1\r\n\r\n")?.is_some());
        Ok(())
    }

    #[test]
    fn smuggling_shapes_are_refused() {
        for bad in [
            "POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 5, 6\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
            "POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET / HTTP/1.1\r\nBad Header: x\r\n\r\n",
            "GET / HTTP/2.0\r\n\r\n",
        ] {
            assert!(req(bad).is_err(), "{bad:?}");
        }
        // The same length repeated is that length; chunked over a length wins, and the connection closes.
        assert!(req("POST / HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\n").is_ok_and(|h| h.is_some_and(|h| h.framing == Framing::Length(5))));
        assert!(req("POST / HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n").is_ok_and(|h| h.is_some_and(|h| h.framing == Framing::Chunked && !h.keep_alive)));
    }

    /// A version this server doesn't serve is told apart from no request line at all: a target
    /// with a space in it, a stray word where the version goes (RFC 9112 §3.2, RFC 9110 §15.6.6).
    #[test]
    fn a_request_line_that_is_none_is_told_from_a_version_not_served() {
        let kind = |s: &str| req(s).err().map(|e| e.kind());
        for bad in ["PUT /api/clouds/bad name HTTP/1.1\r\nHost: x\r\n\r\n", "GET / FOO\r\n\r\n", "GET / HTTP/1.1 x\r\n\r\n", "GET / HTTPS/1.1\r\n\r\n"] {
            assert_eq!(kind(bad), Some(crate::error::Kind::Parse(Parse::Head)), "{bad:?}");
        }
        for unserved in ["GET / HTTP/2.0\r\n\r\n", "GET / HTTP/3.0\r\n\r\n", "GET / HTTP/2"] {
            assert_eq!(kind(unserved), Some(crate::error::Kind::Parse(Parse::Version)), "{unserved:?}");
        }
        assert!(names_a_version(b"GET / HTTP/2.0\r\n") && names_a_version(b"GET / HTTP/") && !names_a_version(b"GET / HTTP/2.0x\r\n") && !names_a_version(b"GET /a b HTTP/1.1"));
    }

    #[test]
    fn a_head_too_large_is_refused() {
        let big = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(100));
        assert!(parse_request(&mut BytesMut::from(big.as_str()), 64, &mut None).is_err());
    }

    #[test]
    fn header_values_share_the_bytes_they_arrived_in() -> Result<(), Error> {
        let mut buf = BytesMut::from("GET / HTTP/1.1\r\nX-Long: abcdefghijklmnop\r\n\r\nGET /next HTTP/1.1\r\n\r\n");
        let h = parse_request(&mut buf, 1 << 16, &mut None)?.ok_or(Error::closed())?;
        assert_eq!(h.headers.get("x-long").map(HeaderValue::as_bytes), Some(&b"abcdefghijklmnop"[..]));
        assert_eq!(&buf[..], b"GET /next HTTP/1.1\r\n\r\n");
        Ok(())
    }

    #[test]
    fn a_response_head_says_its_framing() -> Result<(), Error> {
        let mut b = BytesMut::from("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
        let h = parse_response(&mut b, 1 << 16, &Method::GET)?.ok_or(Error::closed())?;
        assert_eq!((h.status, h.framing, h.keep_alive), (StatusCode::OK, Framing::Length(3), true));
        let mut b = BytesMut::from("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n");
        assert_eq!(parse_response(&mut b, 1 << 16, &Method::HEAD)?.map(|h| h.framing), Some(Framing::Empty));
        let mut b = BytesMut::from("HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(parse_response(&mut b, 1 << 16, &Method::GET)?.map(|h| (h.framing, h.keep_alive)), Some((Framing::Close, false)));
        let mut b = BytesMut::from("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n");
        assert_eq!(parse_response(&mut b, 1 << 16, &Method::GET)?.map(|h| h.framing), Some(Framing::Empty));
        Ok(())
    }

    #[test]
    fn a_response_head_gets_its_framing_and_date() {
        let date = b"Wed, 21 Oct 2026 07:28:00 GMT";
        let mut out = Vec::new();
        let f = encode_response(&mut out, StatusCode::OK, &HeaderMap::new(), Some(13), Answering { method: &Method::GET, version: Version::HTTP_11, keep_alive: true }, date);
        assert_eq!((f.framing, f.close), (Framing::Length(13), false));
        assert_eq!(out, b"HTTP/1.1 200 OK\r\ncontent-length: 13\r\ndate: Wed, 21 Oct 2026 07:28:00 GMT\r\n\r\n");
        let mut out = Vec::new();
        let f = encode_response(&mut out, StatusCode::OK, &HeaderMap::new(), None, Answering { method: &Method::GET, version: Version::HTTP_11, keep_alive: false }, date);
        assert_eq!((f.framing, f.close), (Framing::Chunked, true));
        assert!(out.ends_with(b"transfer-encoding: chunked\r\ndate: Wed, 21 Oct 2026 07:28:00 GMT\r\nconnection: close\r\n\r\n"));
        let mut out = Vec::new();
        let f = encode_response(&mut out, StatusCode::OK, &HeaderMap::new(), None, Answering { method: &Method::GET, version: Version::HTTP_10, keep_alive: true }, date);
        assert_eq!((f.framing, f.close), (Framing::Close, true));
        let mut out = Vec::new();
        let f = encode_response(&mut out, StatusCode::NO_CONTENT, &HeaderMap::new(), Some(0), Answering { method: &Method::GET, version: Version::HTTP_11, keep_alive: true }, date);
        assert_eq!(f.framing, Framing::Empty);
        assert!(!out.windows(14).any(|w| w == b"content-length"));
    }

    #[test]
    fn a_request_head_is_written_in_origin_form() {
        let mut out = Vec::new();
        let uri: Uri = "http://example.com:8080/p?q=1".parse().unwrap_or_default();
        let f = encode_request(&mut out, &Method::POST, &uri, &HeaderMap::new(), Some(4), false);
        assert_eq!(f, Framing::Length(4));
        assert_eq!(out, b"POST /p?q=1 HTTP/1.1\r\nhost: example.com:8080\r\ncontent-length: 4\r\n\r\n");
    }
}
