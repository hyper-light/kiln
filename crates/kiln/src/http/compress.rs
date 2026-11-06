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
//!   The ETag is weak (RFC 9110 §8.8.1) because one is shared by the identity and compressed forms. A
//!   weak ETag the handler set is taken as that validator, its body not digested again: a read model's,
//!   made once for what it answers ([`weak_etag`]; goal 9, plan 01a1a87b2cec step 5).
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
/// stream), nor what's packed already (images, video, archives, fonts). Its name compared without regard
/// to ASCII case where it is, not lowered into a copy first (goal 9, plan 01a1a87b2cec step 5: a string
/// made for every answer of the API, twice).
fn compressible(content_type: &str) -> bool {
    let t = content_type.split(';').next().unwrap_or_default().trim().as_bytes();
    let is = |name: &str| t.eq_ignore_ascii_case(name.as_bytes());
    let starts = |head: &str| t.get(..head.len()).is_some_and(|h| h.eq_ignore_ascii_case(head.as_bytes()));
    let ends = |tail: &str| t.len().checked_sub(tail.len()).and_then(|at| t.get(at..)).is_some_and(|e| e.eq_ignore_ascii_case(tail.as_bytes()));
    let holds = |part: &str| t.windows(part.len()).any(|w| w.eq_ignore_ascii_case(part.as_bytes()));
    if is("text/event-stream") {
        return false;
    }
    // `+json` ends in `json`.
    starts("text/") || ends("json") || holds("javascript") || ends("xml") || is("image/svg+xml")
}

/// Whether an ETag is weak (`W/`, RFC 9110 §8.8.3).
fn weak(etag: &HeaderValue) -> bool {
    etag.as_bytes().starts_with(b"W/")
}

fn has_token(h: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    h.get_all(name).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// The bytes of the SHA-256 a weak ETag names (its first 128 bits), and so twice as many hex digits.
pub const ETAG_DIGEST: usize = 16;

/// A weak ETag's bytes: `W/"`, the digest's hex digits, `"`.
const ETAG_LEN: usize = r#"W/""#.len() + 2 * ETAG_DIGEST + r#"""#.len();

/// What a weak ETag names of `body`: its SHA-256's first 128 bits. SHA-256 (already kiln's hash) and not a
/// faster non-cryptographic one: a 304 for a changed body would show a person stale data, and a body someone
/// else can write to (a run's title) mustn't be made to collide with an older one. A read model that keeps
/// what it answers as parts digests each part once and names the whole by the digest of their digests
/// ([`weak_etag`]; goal 9, plan 01a1a87b2cec step 5: `crate::harness::copies::Copies::list_tagged`).
pub fn digest(body: &[u8]) -> [u8; ETAG_DIGEST] {
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(body);
    let mut first = [0u8; ETAG_DIGEST];
    if let Some(head) = d.get(..ETAG_DIGEST) {
        first.copy_from_slice(head);
    }
    first
}

/// The weak ETag naming `digest`, as its bytes: `W/"`, its hex digits, `"`. Written into one buffer of its
/// length, which its header value takes as it is (goal 9, plan 01a1a87b2cec step 5: the digest's hex, then
/// `W/"…"` of it, then the header value's copy, three allocations an answer).
fn etag_bytes_of(digest: &[u8; ETAG_DIGEST]) -> Vec<u8> {
    let mut hexed = [0u8; 2 * ETAG_DIGEST];
    // Twice the digest's bytes always fill the room.
    let _ = hex::encode_to_slice(digest, &mut hexed);
    let mut tag = Vec::with_capacity(ETAG_LEN);
    tag.extend_from_slice(br#"W/""#);
    tag.extend_from_slice(&hexed);
    tag.push(b'"');
    tag
}

/// The weak ETag of a body, as its bytes ([`digest`], [`etag_bytes_of`]).
fn etag_bytes(body: &[u8]) -> Vec<u8> {
    etag_bytes_of(&digest(body))
}

/// The weak ETag naming `digest` as a header value ([`etag_bytes_of`]): what a read model sets on an answer it
/// made the digest of, which [`finish`] then takes as the answer's validator, not digesting its body again.
pub fn weak_etag(digest: &[u8; ETAG_DIGEST]) -> Option<HeaderValue> {
    HeaderValue::from_maybe_shared(Bytes::from(etag_bytes_of(digest))).ok()
}

/// The weak ETag of a body ([`etag_bytes`]), as text.
pub fn etag_of(body: &[u8]) -> String {
    String::from_utf8(etag_bytes(body)).unwrap_or_default()
}

/// The weak ETag of a body ([`etag_bytes`]) as a header value: its buffer taken as it is, never copied (it's
/// as long as it was made, so its bytes are that allocation), and checked as a header value's are.
fn etag_value(body: &[u8]) -> Option<HeaderValue> {
    HeaderValue::from_maybe_shared(Bytes::from(etag_bytes(body))).ok()
}

/// Whether `If-None-Match` holds `etag`, by the weak comparison `GET` uses (RFC 9110 §13.1.2): each compared
/// where it is, nothing copied.
fn holds(validators: &str, etag: &str) -> bool {
    fn bare(t: &str) -> &str {
        t.trim().trim_start_matches("W/")
    }
    let want = bare(etag);
    validators.split(',').any(|t| t.trim() == "*" || bare(t) == want)
}

/// What [`finish`] worked out for a body.
enum Done {
    NotModified(HeaderValue),
    Answer { etag: Option<HeaderValue>, packed: Option<(Coding, Vec<u8>)> },
}

/// The answer's validator and coding worked out for `body`: its weak ETag where it gets one (`tag`), the one its
/// handler gave (`given`, a read model's, [`weak_etag`]) else its body's digest's; a 304 where `validators` (its
/// request's `If-None-Match`) hold it; else its body compressed in `coding`, where that's smaller.
fn work(body: &[u8], tag: bool, given: Option<HeaderValue>, validators: Option<&str>, coding: Option<Coding>) -> io::Result<Done> {
    let etag = if tag { given.or_else(|| etag_value(body)) } else { None };
    let unchanged = match (&etag, validators) {
        (Some(e), Some(v)) => e.to_str().is_ok_and(|e| holds(v, e)),
        _ => false,
    };
    if unchanged && let Some(e) = etag {
        return Ok(Done::NotModified(e));
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
    // A strong validator of the handler's own (a file's, a package proxy's) is for this body as it is: compressed,
    // it would name another (RFC 9110 §8.8.3), so that answer goes as it is. A weak one (a read model's for what it
    // answers, [`weak_etag`]) is shared by the identity and compressed forms, as finish's own: the answer's
    // validator (goal 9, plan 01a1a87b2cec step 5).
    let strong = h.get(header::ETAG).is_some_and(|v| !weak(v));
    if h.contains_key(header::CONTENT_ENCODING) || h.contains_key(header::CONTENT_RANGE) || h.contains_key(header::ACCEPT_RANGES) || strong || has_token(h, header::CACHE_CONTROL, "no-transform") {
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
/// body's length alone, before its body is read ([`finish_here`] gives such an answer back at once, as it came).
pub fn untouched(asked: &Asked, resp: &Response) -> bool {
    plan(asked, resp).is_none()
}

/// The API's answer to a request that asked `asked`, compressed and validated as the module says.
pub async fn finish(asked: Asked, resp: Response) -> Response {
    match finish_here(asked, resp) {
        Finished::Now(resp) => resp,
        Finished::Later(later) => finish_later(later).await,
    }
}

/// [`finish`]'s answer as far as it goes here: done, or (a body long to hash or compress) left for the compute
/// pool ([`finish_later`]).
pub enum Finished {
    Now(Response),
    Later(Later),
}

/// An answer [`finish_here`] left for the compute pool, with what it worked out of it.
pub struct Later {
    parts: http::response::Parts,
    bytes: Bytes,
    tag: bool,
    big: bool,
    coding: Option<Coding>,
    given: Option<HeaderValue>,
    validators: Option<String>,
}

/// [`finish`] as far as it goes without waiting: an answer it leaves as it is, or works out here (its body fits a
/// step's quantum, [`inline`]), done; one whose body's work doesn't fit, left for [`finish_later`]. So `crate::api`'s
/// handler makes a future for an answer only where its work goes to the compute pool (goal 9, plan 01a1a87b2cec
/// step 5: every answer finish changed was a boxed future, its work done in that future's first poll).
pub fn finish_here(asked: Asked, resp: Response) -> Finished {
    let Some(Plan { tag, len, big, coding }) = plan(&asked, &resp) else { return Finished::Now(resp) };
    let (mut parts, body) = resp.into_parts();
    let bytes = match whole(body, len) {
        Ok(b) => b,
        Err(body) => return Finished::Now(Response::from_parts(parts, body)),
    };
    if !tag && coding.is_none() {
        // It varies with what's accepted, though this asker takes no coding.
        vary(&mut parts.headers);
        return Finished::Now(Response::from_parts(parts, srv::full(bytes)));
    }
    let cost = match coding {
        Some(Coding::Br) => Work::Br,
        _ => Work::Gzip,
    };
    // A weak validator its handler made for it (a read model's), taken as its own, where it gets one.
    let given = if tag { parts.headers.remove(header::ETAG) } else { None };
    // Named by its handler and asked for in no coding, its body leaves nothing to work but its validators' compare:
    // finished here, whatever its length (goal 9, plan 01a1a87b2cec step 3: the sessions' list, 38 KB in kiln-lab's
    // realistic profile and named as it's joined, went to the compute pool for that at each read).
    if (given.is_none() || coding.is_some()) && !inline(cost, len) {
        return Finished::Later(Later { parts, bytes, tag, big, coding, given, validators: asked.validators });
    }
    let done = work(&bytes, tag, given, asked.validators.as_deref(), coding);
    Finished::Now(answered(parts, bytes, big, done))
}

/// The rest of [`finish`] for an answer [`finish_here`] left: its body's work on the compute pool.
pub async fn finish_later(later: Later) -> Response {
    let Later { parts, bytes, tag, big, coding, given, validators } = later;
    let b = bytes.clone();
    let done = match crate::rt::compute(move || work(&b, tag, given, validators.as_deref(), coding)).await {
        Ok(r) => r,
        Err(e) => Err(io::Error::other(e)),
    };
    answered(parts, bytes, big, done)
}

/// The answer of `parts` and `bytes` as [`work`] worked it out (`done`).
fn answered(mut parts: http::response::Parts, bytes: Bytes, big: bool, done: io::Result<Done>) -> Response {
    let cache = |h: &mut HeaderMap| {
        if !h.contains_key(header::CACHE_CONTROL) {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
        }
    };
    match done {
        Ok(Done::NotModified(etag)) => {
            let mut r = srv::status(StatusCode::NOT_MODIFIED);
            let h = r.headers_mut();
            h.insert(header::ETAG, etag);
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
            if let Some(etag) = etag {
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

    /// Goal 9, plan 01a1a87b2cec step 5: what finishing a small answer allocates, and that it's done here, no future
    /// made. A GET answered 200 with a body under a quantum's work (/healthz's "ok", a tiny JSON) is validated with
    /// two allocations: its ETag, written in place and taken whole by its header value, and its body boxed again
    /// (it was a boxed future, the type lowered into a copy twice, the digest's hex, `W/"…"` of it and the header
    /// value's copy before). One it leaves as it is allocates nothing. The ETag and the 304 are as they were.
    #[test]
    fn g9_s5_a_small_answer_is_finished_here_in_two_allocations() -> anyhow::Result<()> {
        let finished = |asked: Asked, resp: Response| -> anyhow::Result<(Response, u64)> {
            let before = crate::os::counted::made().blocks;
            let done = finish_here(asked, resp);
            let made = crate::os::counted::made().blocks.saturating_sub(before);
            match done {
                Finished::Now(r) => Ok((r, made)),
                Finished::Later(_) => anyhow::bail!("a small answer was left for the compute pool"),
            }
        };
        let br = [("accept-encoding", "gzip, br")];
        for (what, resp, body) in [("/healthz's", srv::text(StatusCode::OK, "ok"), b"ok".to_vec()), ("a tiny JSON", srv::json(StatusCode::OK, &serde_json::json!({"ok": true})), br#"{"ok":true}"#.to_vec())] {
            let (r, made) = finished(Asked::of(&get(&br)), resp)?;
            anyhow::ensure!(made == 2, "{what}: {made} allocations");
            anyhow::ensure!(header(&r, header::ETAG) == Some(etag_of(&body).as_str()), "{what}: {:?}", header(&r, header::ETAG));
            anyhow::ensure!(header(&r, header::CACHE_CONTROL) == Some("private, no-cache") && header(&r, header::VARY).is_none(), "{what}");
            let sent = crate::rt::block_on(body_of(r))?;
            anyhow::ensure!(sent == body, "{what}: {sent:?}");
        }
        // Its ETag as it was written before: `W/"`, the hex of the first 16 bytes of its SHA-256, `"`.
        {
            use sha2::Digest as _;
            let d = sha2::Sha256::digest(b"ok");
            anyhow::ensure!(etag_of(b"ok") == format!("W/\"{}\"", hex::encode(d.get(..16).unwrap_or_default())), "{}", etag_of(b"ok"));
        }
        // Left as it is (a tiny answer to a POST): nothing allocated.
        let mut post = get(&br);
        *post.method_mut() = Method::POST;
        let (r, made) = finished(Asked::of(&post), srv::json(StatusCode::OK, &serde_json::json!({"ok": true})))?;
        anyhow::ensure!(made == 0 && r.headers().get(header::ETAG).is_none(), "untouched: {made} allocations");
        // Asked again with its ETag: a 304, the validator compared where it is.
        let etag = etag_of(b"ok");
        for inm in [etag.clone(), etag.trim_start_matches("W/").to_owned(), format!("\"other\", {etag}"), "*".to_owned()] {
            let (r, _) = finished(Asked::of(&get(&[("if-none-match", &inm)])), srv::text(StatusCode::OK, "ok"))?;
            anyhow::ensure!(r.status() == StatusCode::NOT_MODIFIED && header(&r, header::ETAG) == Some(etag.as_str()), "{inm}: {}", r.status());
        }
        let (r, _) = finished(Asked::of(&get(&[("if-none-match", "W/\"other\"")])), srv::text(StatusCode::OK, "ok"))?;
        anyhow::ensure!(r.status() == StatusCode::OK, "another validator: {}", r.status());
        // Types told without regard to case, as they were lowered before.
        for (t, yes) in [("Application/JSON; charset=utf-8", true), ("application/problem+json", true), ("TEXT/plain", true), ("text/event-stream", false), ("Text/Event-Stream", false), ("application/javascript", true), ("image/svg+xml", true), ("application/xml", true), ("image/png", false), ("", false)] {
            anyhow::ensure!(compressible(t) == yes, "{t}: {}", compressible(t));
        }
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a weak ETag its handler set (a read model's, made once for what it answers) is
    /// the answer's validator: kept as it is (its body isn't digested: it isn't the body's digest here), the body still
    /// compressed for one that takes a coding, `Cache-Control` and `Vary` as finish's own validator gets them, a 304
    /// for a request that holds it; finished in one allocation, its body's box. A strong one is left as it is, as
    /// before; a weak one on an answer that gets no validator (a POST's) too.
    #[test]
    fn g9_s5_a_handlers_weak_etag_is_the_answers_validator() -> anyhow::Result<()> {
        let made = weak_etag(&[7; ETAG_DIGEST]).ok_or_else(|| anyhow::anyhow!("no header value"))?;
        let named = made.to_str()?.to_owned();
        anyhow::ensure!(named == format!("W/\"{}\"", "07".repeat(ETAG_DIGEST)), "{named}");
        let answer = |made: &HeaderValue, b: &[u8]| {
            let mut r = srv::response(StatusCode::OK, "application/json", srv::full(b.to_vec()));
            r.headers_mut().insert(header::ETAG, made.clone());
            r
        };
        {
            let (made, named) = (made.clone(), named.clone());
            crate::rt::block_on(async move {
                let body = runners_list()?;
                let r = finish(Asked::of(&get(&[("accept-encoding", "br")])), answer(&made, &body)).await;
                anyhow::ensure!(header(&r, header::ETAG) == Some(named.as_str()), "kept: {:?}", header(&r, header::ETAG));
                anyhow::ensure!(header(&r, header::CONTENT_ENCODING) == Some("br") && header(&r, header::VARY) == Some("Accept-Encoding"), "compressed");
                anyhow::ensure!(header(&r, header::CACHE_CONTROL) == Some("private, no-cache"), "{:?}", header(&r, header::CACHE_CONTROL));
                anyhow::ensure!(unbrotli(&body_of(r).await?)? == body, "the body as it was");
                let r = finish(Asked::of(&get(&[("if-none-match", &named)])), answer(&made, &body)).await;
                anyhow::ensure!(r.status() == StatusCode::NOT_MODIFIED && header(&r, header::ETAG) == Some(named.as_str()), "a 304: {}", r.status());
                let r = finish(Asked::of(&get(&[("if-none-match", &etag_of(&body))])), answer(&made, &body)).await;
                anyhow::ensure!(r.status() == StatusCode::OK, "its body's own digest isn't its validator: {}", r.status());
                // A strong one: the answer as it was.
                let mut strong = srv::response(StatusCode::OK, "application/json", srv::full(body.clone()));
                strong.headers_mut().insert(header::ETAG, HeaderValue::from_static("\"v1\""));
                let before = strong.headers().clone();
                let r = finish(Asked::of(&get(&[("accept-encoding", "br")])), strong).await;
                anyhow::ensure!(r.headers() == &before, "a strong validator's answer as it was");
                // A POST's answer gets no validator: its handler's stays where it set it, its body compressed.
                let mut post = get(&[("accept-encoding", "br")]);
                *post.method_mut() = Method::POST;
                let r = finish(Asked::of(&post), answer(&made, &body)).await;
                anyhow::ensure!(header(&r, header::ETAG) == Some(named.as_str()) && header(&r, header::CONTENT_ENCODING) == Some("br"), "a POST's");
                Ok(())
            })?;
        }
        // A small one finished here: its body's box alone.
        let resp = answer(&made, br#"{"ok":true}"#);
        let asked = Asked::of(&get(&[]));
        let before = crate::os::counted::made().blocks;
        let done = finish_here(asked, resp);
        let blocks = crate::os::counted::made().blocks.saturating_sub(before);
        let Finished::Now(r) = done else { anyhow::bail!("a small answer was left for the compute pool") };
        anyhow::ensure!(blocks == 1 && header(&r, header::ETAG) == Some(named.as_str()), "{blocks} allocations");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an answer its handler named (a weak ETag of its own) and asked for in no coding
    /// leaves its body nothing to work but its validators' compare: finished here on its shard whatever its length,
    /// its validator and a 304 as before, where one longer than a step's gzip went to the compute pool for that. Asked
    /// for in a coding, or not named, a body that long still goes there.
    #[test]
    fn g9_s3_a_named_answer_in_no_coding_is_finished_here_whatever_its_length() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // Shape: twice what a step's quantum gzips at its measured rate (`Work::Gzip`), so only its naming keeps it here.
            let quantum = usize::try_from(hyper_rt::futures::step_budget_ns().ok_or_else(|| anyhow::anyhow!("not on a shard"))?)?;
            let long = 2 * quantum * 1000 / usize::try_from(Work::Gzip.ps_per_byte())?;
            let body: Vec<u8> = std::iter::once(b'[').chain(std::iter::repeat_n(b'1', long)).chain(*b"]").collect();
            let made = weak_etag(&digest(&body)).ok_or_else(|| anyhow::anyhow!("no header value"))?;
            let named = |b: &[u8]| {
                let mut r = srv::response(StatusCode::OK, "application/json", srv::full(b.to_vec()));
                r.headers_mut().insert(header::ETAG, made.clone());
                r
            };
            let Finished::Now(r) = finish_here(Asked::of(&get(&[])), named(&body)) else { anyhow::bail!("a named answer in no coding was left for the compute pool") };
            anyhow::ensure!(header(&r, header::ETAG) == made.to_str().ok() && r.status() == StatusCode::OK, "its own validator: {:?}", header(&r, header::ETAG));
            anyhow::ensure!(body_of(r).await? == body, "the body as it was");
            let Finished::Now(r) = finish_here(Asked::of(&get(&[("if-none-match", made.to_str()?)])), named(&body)) else { anyhow::bail!("a revalidation was left for the compute pool") };
            anyhow::ensure!(r.status() == StatusCode::NOT_MODIFIED, "a 304: {}", r.status());
            anyhow::ensure!(matches!(finish_here(Asked::of(&get(&[("accept-encoding", "gzip")])), named(&body)), Finished::Later(_)), "one to gzip goes to the compute pool");
            let unnamed = srv::response(StatusCode::OK, "application/json", srv::full(body.clone()));
            anyhow::ensure!(matches!(finish_here(Asked::of(&get(&[])), unnamed), Finished::Later(_)), "one to digest goes to the compute pool");
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
