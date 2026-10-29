//! `multipart/form-data` (RFC 7578; its parts delimited as RFC 2046 §5.1 says), read in place: what
//! Datadog's profilers upload (an `event` part, and a part for each profile). Each part's name and
//! file name from its `Content-Disposition`, its type, its body borrowed; every size bounded, and
//! every malformed body an error, never a guess.

use memchr::memmem::Finder;

/// Parts a body may hold, and a part's header block's bytes, at the most.
pub const MAX_PARTS: usize = 64;
pub const MAX_HEADERS: usize = 8 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MultipartError {
    #[error("no boundary: a body begins with its first delimiter")]
    NoDelimiter,
    #[error("a part's headers end nowhere, or past {MAX_HEADERS} bytes")]
    Headers,
    #[error("a part ends nowhere: its closing delimiter is missing")]
    Unclosed,
    #[error("a part without a name (`Content-Disposition: form-data; name=…`)")]
    NoName,
    #[error("more than {MAX_PARTS} parts")]
    TooMany,
    #[error("a header not UTF-8")]
    Text,
}

/// A part: its form field's name, its file name where it has one, its type, its body and where its
/// body begins in the whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part<'a> {
    pub name: &'a str,
    pub filename: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    pub at: usize,
}

/// The `boundary` parameter of a `multipart/form-data` content type (quoted or a token); none where
/// the type is another.
pub fn boundary(content_type: &str) -> Option<&str> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"')).filter(|b| !b.is_empty() && b.len() <= 70)
    })
}

/// The parts of `body`, delimited by `boundary`, in order (a preamble before the first delimiter
/// and an epilogue after the last, RFC 2046's, passed over).
pub fn parts<'a>(body: &'a [u8], boundary: &str) -> Result<Vec<Part<'a>>, MultipartError> {
    let mut delimiter = Vec::with_capacity(boundary.len() + 4);
    delimiter.extend_from_slice(b"\r\n--");
    delimiter.extend_from_slice(boundary.as_bytes());
    let next = Finder::new(&delimiter);
    // The first delimiter may begin the body (no CRLF before it).
    let first = delimiter.get(2..).unwrap_or_default();
    let mut at = if body.starts_with(first) { first.len() } else { next.find(body).ok_or(MultipartError::NoDelimiter)? + delimiter.len() };
    let mut out = Vec::new();
    loop {
        let rest = body.get(at..).ok_or(MultipartError::Unclosed)?;
        // `--` after a delimiter closes the body.
        if rest.starts_with(b"--") {
            return Ok(out);
        }
        // Transport padding (spaces, tabs) then CRLF.
        let skip = rest.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        let rest = rest.get(skip..).and_then(|r| r.strip_prefix(b"\r\n")).ok_or(MultipartError::Unclosed)?;
        let end = Finder::new(b"\r\n\r\n").find(rest.get(..rest.len().min(MAX_HEADERS + 4)).unwrap_or(rest)).ok_or(MultipartError::Headers)?;
        let headers = std::str::from_utf8(rest.get(..end).unwrap_or_default()).map_err(|_| MultipartError::Text)?;
        let content = rest.get(end + 4..).unwrap_or_default();
        let len = next.find(content).ok_or(MultipartError::Unclosed)?;
        if out.len() == MAX_PARTS {
            return Err(MultipartError::TooMany);
        }
        let (mut name, mut filename, mut content_type) = (None, None, None);
        for line in headers.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-disposition") {
                for p in v.split(';').skip(1) {
                    let Some((pk, pv)) = p.split_once('=') else { continue };
                    let pv = pv.trim().trim_matches('"');
                    match pk.trim() {
                        k if k.eq_ignore_ascii_case("name") => name = Some(pv),
                        k if k.eq_ignore_ascii_case("filename") => filename = Some(pv),
                        _ => {}
                    }
                }
            } else if k.eq_ignore_ascii_case("content-type") {
                content_type = Some(v);
            }
        }
        out.push(Part { name: name.ok_or(MultipartError::NoName)?, filename, content_type, body: content.get(..len).unwrap_or_default(), at: body.len() - content.len() });
        // Past this part's body and its delimiter.
        at = body.len() - content.len() + len + delimiter.len();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A profiler's upload as dd-trace-go writes it (Go's `mime/multipart`): its attachments, then
    /// `event`; a quoted boundary; a preamble; a body that is binary.
    #[test]
    fn a_profiler_s_upload_is_read() {
        let ct = "multipart/form-data; boundary=\"b0und4ry\"";
        let b = boundary(ct).unwrap();
        assert_eq!(b, "b0und4ry");
        let mut body = b"preamble\r\n--b0und4ry\r\nContent-Disposition: form-data; name=\"cpu.pprof\"; filename=\"cpu.pprof\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
        body.extend_from_slice(&[0x1f, 0x8b, 0, 13, 10, 45, 45]);
        body.extend_from_slice(
            b"\r\n--b0und4ry\r\nContent-Disposition: form-data; name=\"event\"; filename=\"event.json\"\r\nContent-Type: application/json\r\n\r\n{\"family\":\"go\"}\r\n--b0und4ry--\r\nepilogue",
        );
        let ps = parts(&body, b).unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!((ps[0].name, ps[0].filename, ps[0].body), ("cpu.pprof", Some("cpu.pprof"), &[0x1f, 0x8b, 0, 13, 10, 45, 45][..]));
        assert_eq!(body.get(ps[0].at..ps[0].at + ps[0].body.len()), Some(ps[0].body), "its place in the whole");
        assert_eq!((ps[1].name, ps[1].content_type, ps[1].body), ("event", Some("application/json"), &b"{\"family\":\"go\"}"[..]));
    }

    /// Refused: another type; no delimiter; a part never closed; one without a name; headers that
    /// never end.
    #[test]
    fn a_malformed_body_is_refused() {
        assert_eq!(boundary("application/json"), None);
        assert_eq!(parts(b"nothing here", "x"), Err(MultipartError::NoDelimiter));
        assert_eq!(parts(b"--x\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nbody", "x"), Err(MultipartError::Unclosed));
        assert_eq!(parts(b"--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n--x--", "x"), Err(MultipartError::NoName));
        assert_eq!(parts(b"--x\r\nContent-Disposition: form-data; name=\"a\"", "x"), Err(MultipartError::Headers));
    }
}
