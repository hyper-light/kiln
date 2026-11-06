//! What kiln's API answers, packed for the browser on the way out (W21-R15, "presses answer at once":
//! the UI's JSON is most of what a page waits for). One place, [`finish`], after every route of the
//! API (`crate::api::Api`), not in each handler:
//!
//! * **Compression.** A whole body of a textual type (JSON, text, JavaScript, SVG, XML) of at least
//!   [`MIN_BODY`] bytes goes brotli-compressed to a browser that takes `br`, else gzipped to one that
//!   takes `gzip`, with `Vary: Accept-Encoding`. Left as it is: a body already encoded, a range (or a
//!   response that offers them), a streamed body (event streams, log tails, file downloads: anything
//!   whose length isn't known whole), an answer with a validator of its own (a file's: strong, so it
//!   names that body alone), `Cache-Control: no-transform`, and every request that didn't
//!   send `Accept-Encoding` (kiln's own clients, runners, scripts: what they get is unchanged).
//! * **Validators.** A `GET` answered `200` with a whole body gets a weak `ETag` from the body's
//!   SHA-256 and, unless the handler said otherwise, `Cache-Control: private, no-cache`: the browser
//!   keeps the body and asks again each time (`If-None-Match`), and an unchanged answer costs a `304`
//!   with no body. `no-cache` means no copy is used without asking, so the UI never sees stale data.
//!   The ETag is weak (RFC 9110 §8.8.1) because one is shared by the identity and compressed forms.
//!
//! The work runs on the shard when it fits a step's quantum, else on the compute pool
//! (`crate::rt::compute`): a large body never holds the shard that serves every other socket on it.

use std::io::{self, Write as _};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use http_body::{Body as _, Frame, SizeHint};

use super::server::{self as srv, Body, Request, Response};

/// The smallest body worth compressing, in bytes. Below about one TCP segment (1,460 bytes of
/// payload at Ethernet's 1,500-byte MTU, less the ~300 bytes of a typical kiln response head) the
/// answer leaves in one packet whether or not it's compressed, so compressing saves no packet and no
/// round trip; and the encodings' fixed costs (gzip's 18-byte frame, brotli's stream header,
/// `Content-Encoding` and `Vary` themselves) are a larger share of a small body. The same bound the UI's
/// static files use (`crate::ui::respond`).
pub const MIN_BODY: usize = 1024;

/// How the levels were chosen: measured in a release build on adacachy (Zen 5), 2026-11-04, on three
/// of kiln's own bodies: the production runners list (10,810 B), a list of 60 agents as
/// `/api/harness/sessions` sends them (70,529 B; both written out by `crate::http::deflate::saved`),
/// and the operations' catalog, `GET /api/ops` (149,356 B; `tests/wire_compress.rs`). Bytes, and the
/// best of 25 runs:
///
/// | codec | runners list | agents list | operations |
/// |---|---|---|---|
/// | gzip 1 | 2,555 B, 29 µs | 5,846 B, 28 µs | 44,955 B, 424 µs |
/// | gzip 4 | 2,073 B, 49 µs | 3,534 B, 104 µs | 31,501 B, 1,008 µs |
/// | **gzip 5** | 2,059 B, 52 µs | 3,451 B, 117 µs | 31,002 B, 1,333 µs |
/// | gzip 6 | 2,038 B, 58 µs | 3,173 B, 147 µs | 30,509 B, 2,272 µs |
/// | brotli 3 | 2,065 B, 34 µs | 2,860 B, 82 µs | 32,192 B, 768 µs |
/// | **brotli 4** | 2,017 B, 52 µs | 2,670 B, 284 µs | 31,287 B, 1,051 µs |
/// | brotli 5 | 1,906 B, 1,113 µs | 2,404 B, 399 µs | 29,324 B, 1,637 µs |
/// | brotli 11 | 1,765 B, 8,245 µs | 2,074 B, 32,723 µs | 26,030 B, 110,346 µs |
///
/// Each is the last level whose further bytes saved, over the three, take longer on a phone's link
/// (10 Mbit/s: 0.8 µs a byte) than its further time: gzip 5 saves 596 B over 4 (477 µs of link) for
/// 341 µs, 6 saves 792 B more (634 µs) for 975 µs; brotli 4 saves 1,143 B over 3 (914 µs) for 503 µs,
/// 5 saves 2,340 B more (1,872 µs) for 1,762 µs, about even, and pays a millisecond on every small
/// body (its hash tables), which most of the API's are. gzip matters as much as brotli: browsers
/// offer `br` only to secure origins (HTTPS, localhost), and kiln is often reached by plain HTTP.
pub const GZIP_LEVEL: u32 = 5;

/// brotli's quality for API bodies (see [`GZIP_LEVEL`]).
pub const BROTLI_QUALITY: u32 = 4;

/// brotli's largest window, 4 MiB: brotli's own default (`BROTLI_DEFAULT_WINDOW`), as the UI's static
/// files use. A body smaller than that gets the smallest window that holds it ([`brotli_window`]):
/// a larger one references nothing more and only costs memory.
const BROTLI_MAX_WINDOW: u32 = 22;

/// brotli's smallest window (`BROTLI_MIN_WINDOW_BITS`).
const BROTLI_MIN_WINDOW: u32 = 10;

/// A content coding kiln compresses with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Br,
    Gzip,
}

impl Coding {
    pub fn token(self) -> &'static str {
        match self {
            Coding::Br => "br",
            Coding::Gzip => "gzip",
        }
    }

    /// The coding to answer an `Accept-Encoding` with: the one it weighs highest of brotli and gzip
    /// (`*` standing for any it doesn't name), brotli on a tie (it's the smaller); none when it
    /// takes neither (`q=0`), or sent no header at all (RFC 9110 §12.5.3: then any coding is
    /// acceptable, but kiln's own clients send none and read only identity).
    pub fn pick(accept: Option<&str>) -> Option<Coding> {
        let accept = accept?;
        let (mut br, mut gzip, mut any) = (None::<f32>, None::<f32>, None::<f32>);
        for part in accept.split(',') {
            let mut it = part.split(';');
            let name = it.next().unwrap_or_default().trim();
            let mut q = 1.0f32;
            for p in it {
                let p = p.trim();
                if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                    q = v.trim().parse().unwrap_or(0.0);
                }
            }
            if name.eq_ignore_ascii_case("br") {
                br = Some(q);
            } else if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") {
                gzip = Some(q);
            } else if name == "*" {
                any = Some(q);
            }
        }
        let br = br.or(any).unwrap_or(0.0);
        let gzip = gzip.or(any).unwrap_or(0.0);
        match (br > 0.0, gzip > 0.0) {
            (true, true) if gzip > br => Some(Coding::Gzip),
            (true, _) => Some(Coding::Br),
            (false, true) => Some(Coding::Gzip),
            (false, false) => None,
        }
    }

    /// `data` encoded so.
    pub fn encode(self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Coding::Gzip => {
                let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 4), flate2::Compression::new(GZIP_LEVEL));
                enc.write_all(data)?;
                enc.finish()
            }
            Coding::Br => {
                let mut out = Vec::with_capacity(data.len() / 4);
                {
                    let mut w = brotli::CompressorWriter::new(&mut out, 4096, BROTLI_QUALITY, brotli_window(data.len()));
                    w.write_all(data)?;
                    w.flush()?;
                }
                Ok(out)
            }
        }
    }
}

/// The smallest brotli window that holds `len` bytes (a window of `w` bits holds `2^w - 16`), within
/// brotli's bounds.
fn brotli_window(len: usize) -> u32 {
    let mut w = BROTLI_MIN_WINDOW;
    while w < BROTLI_MAX_WINDOW && (1usize << w).saturating_sub(16) < len {
        w += 1;
    }
    w
}

/// What kind of work runs on the shard or goes to the compute pool, by its measured cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Br,
    Gzip,
    /// A WebSocket message (`super::deflate`).
    Deflate,
}

impl Work {
    /// Picoseconds a byte, measured as the levels were ([`GZIP_LEVEL`]): the slowest of the bodies at
    /// the chosen level (the operations', for both HTTP codings), with the ETag's SHA-256 (391 ps a
    /// byte) added for those: brotli 4, 7,035 and 391; gzip 5, 8,928 and 391. A WebSocket message at
    /// level 6 (`super::deflate`): 3,297 over a burst of 500 `harness` events (2,132 for one 256 KiB
    /// message). At a 250 µs quantum (kiln's step budget) that keeps about 33 KB of brotli, 26 KB of
    /// gzip and 75 KB of a message on the shard.
    fn ps_per_byte(self) -> u64 {
        match self {
            Work::Br => 7_426,
            Work::Gzip => 9_319,
            Work::Deflate => 3_297,
        }
    }
}

/// Whether `len` bytes of `work` fit in this shard's step quantum (hyper-rt's measured wake cost,
/// else its configured step budget): so it runs here; past it, the compute pool takes it. Off a
/// shard (a thread of its own, a test) it always runs where it is.
pub fn inline(work: Work, len: usize) -> bool {
    match hyper_rt::futures::step_budget_ns() {
        None => true,
        Some(quantum) => (len as u64).saturating_mul(work.ps_per_byte()) / 1000 <= quantum,
    }
}

/// What the request asked that decides how its answer goes out, read before the route takes it.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    coding: Option<Coding>,
    /// Its `If-None-Match`, for a `GET`.
    validators: Option<String>,
    get: bool,
    /// An upgrade, or a range asked: left as the handler answers.
    leave: bool,
}

impl Asked {
    pub fn of(req: &Request) -> Asked {
        let h = req.headers();
        // A HEAD's answer says what its GET's would be, unchanged.
        let leave = super::ws::is_upgrade(req) || h.contains_key(header::RANGE) || h.contains_key(header::UPGRADE) || req.method() == Method::HEAD;
        let get = req.method() == Method::GET;
        Asked {
            coding: Coding::pick(srv::header_str(req, &header::ACCEPT_ENCODING)),
            validators: if get { srv::header_str(req, &header::IF_NONE_MATCH).map(str::to_owned) } else { None },
            get,
            leave,
        }
    }
}

/// Whether a type is text that compresses: JSON, text, JavaScript, SVG, XML. Not event streams (they
/// stream), nor what's packed already (images, video, archives, fonts).
fn compressible(content_type: &str) -> bool {
    let t = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if t == "text/event-stream" {
        return false;
    }
    t.starts_with("text/") || t.ends_with("json") || t.ends_with("+json") || t.contains("javascript") || t.ends_with("xml") || t == "image/svg+xml"
}

fn has_token(h: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    h.get_all(name).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// The weak ETag of a body: its SHA-256's first 128 bits. SHA-256 (already kiln's hash) and not a
/// faster non-cryptographic one: a 304 for a changed body would show a person stale data, and a
/// body someone else can write to (a run's title) mustn't be made to collide with an older one.
pub fn etag_of(body: &[u8]) -> String {
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(body);
    format!("W/\"{}\"", hex::encode(d.get(..16).unwrap_or_default()))
}

/// Whether `If-None-Match` holds `etag`, by the weak comparison `GET` uses (RFC 9110 §13.1.2).
fn holds(validators: &str, etag: &str) -> bool {
    let bare = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    let want = bare(etag);
    validators.split(',').any(|t| t.trim() == "*" || bare(t) == want)
}

/// What [`finish`] worked out for a body.
enum Done {
    NotModified(String),
    Answer { etag: Option<String>, packed: Option<(Coding, Vec<u8>)> },
}

fn work(body: &[u8], tag: bool, validators: Option<&str>, coding: Option<Coding>) -> io::Result<Done> {
    let etag = tag.then(|| etag_of(body));
    if let (Some(e), Some(v)) = (&etag, validators)
        && holds(v, e)
    {
        return Ok(Done::NotModified(e.clone()));
    }
    let packed = match coding {
        // Sent only when it's smaller: a body that doesn't compress goes as it is.
        Some(c) => Some((c, c.encode(body)?)).filter(|(_, p)| p.len() < body.len()),
        None => None,
    };
    Ok(Done::Answer { etag, packed })
}

/// What [`finish`] does to an answer, read from its head and its body's length alone, before its body is read.
struct Plan {
    /// It gets a weak validator.
    tag: bool,
    len: usize,
    /// At least [`MIN_BODY`].
    big: bool,
    /// The coding it goes out in, if any.
    coding: Option<Coding>,
}

/// [`finish`]'s plan for `resp`, the answer to a request that asked `asked`: None where it goes as it is.
fn plan(asked: &Asked, resp: &Response) -> Option<Plan> {
    if asked.leave {
        return None;
    }
    let status = resp.status();
    if status.is_informational() || matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED | StatusCode::PARTIAL_CONTENT | StatusCode::SWITCHING_PROTOCOLS) {
        return None;
    }
    let h = resp.headers();
    // A validator of the handler's own (a file's, a package proxy's) is strong, for this body as it is:
    // compressed, it would name another (RFC 9110 §8.8.3), so that answer goes as it is.
    if h.contains_key(header::CONTENT_ENCODING) || h.contains_key(header::CONTENT_RANGE) || h.contains_key(header::ACCEPT_RANGES) || h.contains_key(header::ETAG) || has_token(h, header::CACHE_CONTROL, "no-transform") {
        return None;
    }
    if !h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(compressible) {
        return None;
    }
    let tag = asked.get && status == StatusCode::OK && !h.contains_key(header::SET_COOKIE) && !has_token(h, header::CACHE_CONTROL, "no-store");
    let len = resp.body().size_hint().exact().and_then(|n| usize::try_from(n).ok())?;
    let big = len >= MIN_BODY;
    let coding = asked.coding.filter(|_| big);
    if len == 0 || (!tag && coding.is_none() && !big) {
        return None;
    }
    Some(Plan { tag, len, big, coding })
}

/// Whether [`finish`] leaves `resp`, the answer to a request that asked `asked`, as it is: told from its head and its
/// body's length before any future is made for it, so `crate::api`'s handler makes none for an answer nothing changes
/// (goal 9, plan 01a1a87b2cec step 5: a boxed future a request on the serve path).
pub fn untouched(asked: &Asked, resp: &Response) -> bool {
    plan(asked, resp).is_none()
}

/// The API's answer to a request that asked `asked`, compressed and validated as the module says.
pub async fn finish(asked: Asked, resp: Response) -> Response {
    let Some(Plan { tag, len, big, coding }) = plan(&asked, &resp) else { return resp };
    let (mut parts, body) = resp.into_parts();
    let bytes = match whole(body, len) {
        Ok(b) => b,
        Err(body) => return Response::from_parts(parts, body),
    };
    if !tag && coding.is_none() {
        // It varies with what's accepted, though this asker takes no coding.
        vary(&mut parts.headers);
        return Response::from_parts(parts, srv::full(bytes));
    }
    let validators = asked.validators.clone();
    let cost = match coding {
        Some(Coding::Br) => Work::Br,
        _ => Work::Gzip,
    };
    let done = if inline(cost, len) {
        work(&bytes, tag, validators.as_deref(), coding)
    } else {
        let b = bytes.clone();
        match crate::rt::compute(move || work(&b, tag, validators.as_deref(), coding)).await {
            Ok(r) => r,
            Err(e) => Err(io::Error::other(e)),
        }
    };
    let cache = |h: &mut HeaderMap| {
        if !h.contains_key(header::CACHE_CONTROL) {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
        }
    };
    match done {
        Ok(Done::NotModified(etag)) => {
            let mut r = srv::status(StatusCode::NOT_MODIFIED);
            let h = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&etag) {
                h.insert(header::ETAG, v);
            }
            for name in [header::CACHE_CONTROL, header::VARY, header::EXPIRES, header::CONTENT_LOCATION] {
                for v in parts.headers.get_all(&name) {
                    h.append(name.clone(), v.clone());
                }
            }
            cache(h);
            if big {
                vary(h);
            }
            r
        }
        Ok(Done::Answer { etag, packed }) => {
            if let Some(etag) = etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
                parts.headers.insert(header::ETAG, etag);
                cache(&mut parts.headers);
            }
            if big {
                vary(&mut parts.headers);
            }
            let body = match packed {
                Some((c, p)) => {
                    parts.headers.remove(header::CONTENT_LENGTH);
                    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(c.token()));
                    srv::full(p)
                }
                None => srv::full(bytes),
            };
            Response::from_parts(parts, body)
        }
        Err(e) => {
            // Not compressed, not validated: the body as the handler made it.
            tracing::debug!(error = %e, "an answer went out as it was");
            Response::from_parts(parts, srv::full(bytes))
        }
    }
}

/// Adds `Accept-Encoding` to `Vary`, once.
fn vary(h: &mut HeaderMap) {
    if !has_token(h, header::VARY, "accept-encoding") && !has_token(h, header::VARY, "*") {
        h.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
}

/// The body's bytes when they're all there already (a body built whole, as the API's JSON is): taken
/// without waiting. A body that would have to be waited for, or comes in pieces, is given back as it
/// was (what it gave already in front), so a stream is never held up or gathered here.
fn whole(mut body: Body, len: usize) -> Result<Bytes, Body> {
    let mut cx = Context::from_waker(Waker::noop());
    match Pin::new(&mut body).poll_frame(&mut cx) {
        Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
            Ok(data) if data.len() == len => Ok(data),
            Ok(data) => Err(Prefixed::body(Frame::data(data), body)),
            Err(frame) => Err(Prefixed::body(frame, body)),
        },
        Poll::Ready(Some(Err(e))) => Err(Prefixed::failed(e)),
        Poll::Ready(None) => Err(srv::empty()),
        Poll::Pending => Err(body),
    }
}

/// A body with a frame taken off its front already, given back in front.
struct Prefixed {
    first: Option<Frame<Bytes>>,
    error: Option<io::Error>,
    rest: Option<Body>,
}

impl Prefixed {
    fn body(first: Frame<Bytes>, rest: Body) -> Body {
        use http_body_util::BodyExt as _;
        Prefixed { first: Some(first), error: None, rest: Some(rest) }.boxed()
    }

    fn failed(e: io::Error) -> Body {
        use http_body_util::BodyExt as _;
        Prefixed { first: None, error: Some(e), rest: None }.boxed()
    }
}

impl http_body::Body for Prefixed {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        if let Some(f) = this.first.take() {
            return Poll::Ready(Some(Ok(f)));
        }
        if let Some(e) = this.error.take() {
            return Poll::Ready(Some(Err(e)));
        }
        match &mut this.rest {
            Some(rest) => Pin::new(rest).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.first.is_none() && self.error.is_none() && self.rest.as_ref().is_none_or(http_body::Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        let first = self.first.as_ref().and_then(Frame::data_ref).map_or(0, |d| d.len() as u64);
        let rest = self.rest.as_ref().map_or_else(|| SizeHint::with_exact(0), http_body::Body::size_hint);
        match rest.exact() {
            Some(n) => SizeHint::with_exact(n.saturating_add(first)),
            None => {
                let mut s = SizeHint::new();
                s.set_lower(rest.lower().saturating_add(first));
                s
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read as _;

    use http_body_util::BodyExt as _;

    use super::*;

    fn get(headers: &[(&str, &str)]) -> Request {
        let mut b = http::Request::builder().method(Method::GET).uri("/api/runs");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(kiln_http::Incoming::empty()).unwrap_or_default()
    }

    async fn body_of(r: Response) -> anyhow::Result<Vec<u8>> {
        Ok(r.into_body().collect().await.map_err(|e| anyhow::anyhow!("{e}"))?.to_bytes().to_vec())
    }

    fn gunzip(b: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out)?;
        Ok(out)
    }

    fn unbrotli(b: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (mut out, mut from) = (Vec::new(), b);
        brotli::BrotliDecompress(&mut from, &mut out)?;
        Ok(out)
    }

    /// The production runners list (`tests/fixtures/production-runners.json`), as the API sends it:
    /// compact JSON.
    pub(crate) fn runners_list() -> anyhow::Result<Vec<u8>> {
        let v: serde_json::Value = serde_json::from_str(include_str!("../../tests/fixtures/production-runners.json"))?;
        Ok(serde_json::to_vec(&v)?)
    }

    fn header(r: &Response, name: header::HeaderName) -> Option<&str> {
        r.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn w21_r15_accept_encoding_picks_by_weight() {
        assert_eq!(Coding::pick(Some("gzip, deflate, br, zstd")), Some(Coding::Br));
        assert_eq!(Coding::pick(Some("gzip, deflate")), Some(Coding::Gzip));
        assert_eq!(Coding::pick(Some("br;q=0, gzip")), Some(Coding::Gzip));
        assert_eq!(Coding::pick(Some("br;q=0.5, gzip;q=0.9")), Some(Coding::Gzip));
        assert_eq!(Coding::pick(Some("br;q=1.0, gzip;q=0.8")), Some(Coding::Br));
        assert_eq!(Coding::pick(Some("*")), Some(Coding::Br));
        assert_eq!(Coding::pick(Some("*;q=0, gzip")), Some(Coding::Gzip));
        assert_eq!(Coding::pick(Some("identity")), None);
        assert_eq!(Coding::pick(Some("gzip;q=0, br;q=0")), None);
        assert_eq!(Coding::pick(None), None);
        assert_eq!(Coding::pick(Some("")), None);
    }

    #[test]
    fn w21_r15_brotli_windows_hold_the_body() {
        assert_eq!(brotli_window(0), 10);
        assert_eq!(brotli_window(1000), 10);
        assert_eq!(brotli_window(1009), 11);
        assert_eq!(brotli_window(15_332), 14);
        assert_eq!(brotli_window(usize::MAX), 22);
    }

    #[test]
    fn w21_r15_json_goes_compressed_with_a_validator() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let body = runners_list()?;
            for (accept, coding) in [("gzip, deflate, br, zstd", "br"), ("gzip", "gzip")] {
                let req = get(&[("accept-encoding", accept)]);
                let r = finish(Asked::of(&req), srv::response(StatusCode::OK, "application/json", srv::full(body.clone()))).await;
                assert_eq!(header(&r, header::CONTENT_ENCODING), Some(coding));
                assert_eq!(header(&r, header::VARY), Some("Accept-Encoding"));
                assert_eq!(header(&r, header::CACHE_CONTROL), Some("private, no-cache"));
                let etag = header(&r, header::ETAG).unwrap_or_default().to_owned();
                assert!(etag.starts_with("W/\"") && etag.len() == 36, "{etag}");
                let sent = body_of(r).await?;
                assert!(sent.len() < body.len() / 4, "{coding}: {} of {}", sent.len(), body.len());
                let back = if coding == "br" { unbrotli(&sent)? } else { gunzip(&sent)? };
                assert_eq!(back, body);
            }
            Ok(())
        })
    }

    #[test]
    fn w21_r15_an_unchanged_body_is_a_304() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let body = runners_list()?;
            let etag = etag_of(&body);
            let answer = || srv::response(StatusCode::OK, "application/json", srv::full(body.clone()));
            // The validator as sent, strong-quoted (weak comparison), and in a list: each a 304.
            for inm in [etag.clone(), etag.trim_start_matches("W/").to_owned(), format!("\"other\", {etag}"), "*".to_owned()] {
                let req = get(&[("accept-encoding", "br"), ("if-none-match", &inm)]);
                let r = finish(Asked::of(&req), answer()).await;
                assert_eq!(r.status(), StatusCode::NOT_MODIFIED, "{inm}");
                assert_eq!(header(&r, header::ETAG), Some(etag.as_str()));
                assert_eq!(header(&r, header::CACHE_CONTROL), Some("private, no-cache"));
                assert_eq!(header(&r, header::VARY), Some("Accept-Encoding"));
                assert!(header(&r, header::CONTENT_ENCODING).is_none());
                assert!(body_of(r).await?.is_empty());
            }
            // A changed body is sent whole, with its new validator.
            let req = get(&[("if-none-match", &etag)]);
            let mut changed = body.clone();
            changed.extend_from_slice(b" ");
            let r = finish(Asked::of(&req), srv::response(StatusCode::OK, "application/json", srv::full(changed.clone()))).await;
            assert_eq!(r.status(), StatusCode::OK);
            assert_ne!(header(&r, header::ETAG), Some(etag.as_str()));
            assert_eq!(body_of(r).await?, changed);
            Ok(())
        })
    }

    #[test]
    fn w21_r15_what_is_left_as_it_is() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let body = runners_list()?;
            let json = || srv::response(StatusCode::OK, "application/json", srv::full(body.clone()));
            let br = [("accept-encoding", "gzip, br")];
            // Nothing accepted (kiln's own clients): identity, but the validator and Vary still.
            let r = finish(Asked::of(&get(&[])), json()).await;
            assert!(header(&r, header::CONTENT_ENCODING).is_none());
            assert!(header(&r, header::ETAG).is_some());
            assert_eq!(header(&r, header::VARY), Some("Accept-Encoding"));
            assert_eq!(body_of(r).await?, body);
            // A tiny body: a validator, but no compression and no Vary.
            let r = finish(Asked::of(&get(&br)), srv::json(StatusCode::OK, &serde_json::json!({"ok": true}))).await;
            assert!(header(&r, header::CONTENT_ENCODING).is_none() && header(&r, header::VARY).is_none());
            assert!(header(&r, header::ETAG).is_some());
            // Already encoded, a range, ranges offered, no-transform, a packed type: untouched.
            let encoded = srv::with_header(json(), header::CONTENT_ENCODING, "gzip");
            let own_tag = srv::with_header(json(), header::ETAG, "\"v1\"");
            let ranges = srv::with_header(json(), header::ACCEPT_RANGES, "bytes");
            let no_transform = srv::with_header(json(), header::CACHE_CONTROL, "no-transform");
            let png = srv::response(StatusCode::OK, "image/png", srv::full(body.clone()));
            let stream = srv::response(StatusCode::OK, "text/event-stream", srv::full(body.clone()));
            for r in [encoded, own_tag, ranges, no_transform, png, stream] {
                let before = r.headers().clone();
                let r = finish(Asked::of(&get(&br)), r).await;
                assert_eq!(r.headers(), &before);
                assert_eq!(body_of(r).await?, body);
            }
            let r = finish(Asked::of(&get(&[("accept-encoding", "br"), ("range", "bytes=0-9")])), json()).await;
            assert!(r.headers().get(header::CONTENT_ENCODING).is_none() && r.headers().get(header::ETAG).is_none());
            // A streamed body (its length unknown) passes as it is, unread.
            let (tx, rx) = async_channel::unbounded();
            tx.send(Bytes::from(body.clone())).await?;
            drop(tx);
            let r = finish(Asked::of(&get(&br)), srv::response(StatusCode::OK, "application/json", srv::channel_body(rx))).await;
            assert!(r.headers().get(header::CONTENT_ENCODING).is_none() && r.headers().get(header::ETAG).is_none());
            assert_eq!(body_of(r).await?, body);
            // No validator where the handler set a cookie or said no-store, nor for a POST's answer;
            // a POST's answer is still compressed.
            let r = finish(Asked::of(&get(&br)), srv::with_header(json(), header::CACHE_CONTROL, "no-store")).await;
            assert!(r.headers().get(header::ETAG).is_none());
            assert_eq!(header(&r, header::CONTENT_ENCODING), Some("br"));
            let r = finish(Asked::of(&get(&br)), srv::with_header(json(), header::SET_COOKIE, "a=b")).await;
            assert!(r.headers().get(header::ETAG).is_none());
            let mut post = get(&br);
            *post.method_mut() = Method::POST;
            let r = finish(Asked::of(&post), json()).await;
            assert!(r.headers().get(header::ETAG).is_none());
            assert_eq!(header(&r, header::CONTENT_ENCODING), Some("br"));
            // A handler's own Cache-Control stands.
            let r = finish(Asked::of(&get(&br)), srv::with_header(json(), header::CACHE_CONTROL, "private, max-age=5")).await;
            assert_eq!(header(&r, header::CACHE_CONTROL), Some("private, max-age=5"));
            assert!(header(&r, header::ETAG).is_some());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: [`untouched`] tells, from an answer's head and length alone, exactly the
    /// answers [`finish`] leaves as they are, so the API makes no future for them; one it changes (a validator, a
    /// coding, `Vary`) is never among them, however small.
    #[test]
    fn g9_s5_what_finish_leaves_as_it_is_is_told_before_its_future() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let body = runners_list()?;
            let br = [("accept-encoding", "gzip, br")];
            let json = || srv::response(StatusCode::OK, "application/json", srv::full(body.clone()));
            let tiny = || srv::json(StatusCode::OK, &serde_json::json!({"ok": true}));
            let post = |headers: &[(&str, &str)]| {
                let mut r = get(headers);
                *r.method_mut() = Method::POST;
                r
            };
            let streamed = || {
                let frames = futures::stream::iter([Ok::<_, io::Error>(Frame::data(Bytes::from(body.clone())))]);
                srv::response(StatusCode::OK, "application/json", http_body_util::StreamBody::new(frames).boxed())
            };
            // Each request, its answer, and whether finish leaves that answer as it is.
            let cases = [
                ("a tiny GET answer: its validator", get(&br), tiny(), false),
                ("a GET answer: compressed and validated", get(&br), json(), false),
                ("a GET answer to a client taking no coding: validated", get(&[]), json(), false),
                ("a POST answer: compressed", post(&br), json(), false),
                ("a POST answer to a client taking no coding: Vary", post(&[]), json(), false),
                ("a tiny POST answer", post(&br), tiny(), true),
                ("a packed type", get(&br), srv::response(StatusCode::OK, "image/png", srv::full(body.clone())), true),
                ("a streamed body", get(&br), streamed(), true),
                ("no content", get(&br), srv::status(StatusCode::NO_CONTENT), true),
                ("encoded already", get(&br), srv::with_header(json(), header::CONTENT_ENCODING, "gzip"), true),
                ("a range asked", get(&[("accept-encoding", "br"), ("range", "bytes=0-9")]), json(), true),
            ];
            for (what, req, resp, leaves) in cases {
                let asked = Asked::of(&req);
                anyhow::ensure!(untouched(&asked, &resp) == leaves, "{what}: untouched should be {leaves}");
                let before = (resp.status(), resp.headers().clone());
                let after = finish(asked, resp).await;
                anyhow::ensure!(((after.status(), after.headers().clone()) == before) == leaves, "{what}: finish should {} it", if leaves { "leave" } else { "change" });
            }
            Ok(())
        })
    }

    #[test]
    fn w21_r15_a_body_in_pieces_is_given_back_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // An exact length, but two frames: given back in order, unread past the first.
            let frames = futures::stream::iter([Ok::<_, io::Error>(Frame::data(Bytes::from_static(b"{\"a\":"))), Ok(Frame::data(Bytes::from_static(b"1}")))]);
            let body = http_body_util::StreamBody::new(frames);
            let sized = Known { inner: body, len: 7 }.boxed();
            let r = finish(Asked::of(&get(&[("accept-encoding", "br")])), srv::response(StatusCode::OK, "application/json", sized)).await;
            assert!(r.headers().get(header::ETAG).is_none());
            assert_eq!(r.body().size_hint().exact(), Some(7));
            assert_eq!(body_of(r).await?, b"{\"a\":1}");
            Ok(())
        })
    }

    /// A body that says its length.
    struct Known<B> {
        inner: B,
        len: u64,
    }

    impl<B: http_body::Body<Data = Bytes, Error = io::Error> + Unpin> http_body::Body for Known<B> {
        type Data = Bytes;
        type Error = io::Error;

        fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
            let this = self.get_mut();
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(f))) => {
                    this.len = this.len.saturating_sub(f.data_ref().map_or(0, |d| d.len() as u64));
                    Poll::Ready(Some(Ok(f)))
                }
                other => other,
            }
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(self.len)
        }
    }
}
