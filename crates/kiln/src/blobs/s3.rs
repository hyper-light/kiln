//! Any S3-compatible store behind kiln-seams' `Blobs` (CP-T10.1, docs/blobs.md): AWS S3, MinIO
//! and the rest. S3's REST API, signed here with Signature Version 4
//! (`crate::ci::integrations::aws`), over kiln's HTTP client.
//!
//! - **Where.** An object at key `k` is S3's `<prefix>/o/<k> `, a space after it; an upload under
//!   way is joined at `<prefix>/u/<id>` and copied into place when it completes. A prefix is at most
//!   60 bytes, so every kiln key (960 bytes at most) fits S3's 1024.
//! - **The space after.** A store that keeps keys as paths can't list an object and another under
//!   it: MinIO keeps `p/x` but leaves it out of every listing once `p` is there (seen 2026-10-21,
//!   MinIO RELEASE.2026-09-22, with boto3 as with kiln). With a space after each key, `p ` and
//!   `p/x ` are apart, and as a space is the lowest character a kiln key may hold, S3's order of the
//!   keys is kiln's.
//! - **Etags.** S3's ETag is an MD5, or a digest of parts' MD5s, not the content's SHA-256 kiln
//!   names an object by: kiln keeps the SHA-256 with the object (`x-amz-meta-kiln-sha256`) and sends
//!   it as `x-amz-checksum-sha256` too, so S3 checks what it got. `If-Match` is judged against head's
//!   SHA-256 and then sent as S3's `If-Match` of the ETag head saw, so a write between the two is
//!   refused, not lost.
//! - **Uploads.** An object's SHA-256 is metadata, fixed when an upload starts, and S3 makes no
//!   SHA-256 of a whole from parts. So parts go to an upload at `<prefix>/u/<id>`; completing joins
//!   them there, takes the SHA-256 (as the parts came, in order, or by reading the joined object back
//!   once), copies it to its key with the SHA-256 (CopyObject, or UploadPartCopy past 5 GiB), all
//!   within S3, and deletes the joined one. The key appears once, whole. An upload's id carries its
//!   key, where it's joined, and S3's upload id. An abort asks S3 for the upload's parts first:
//!   MinIO answers an abort of an upload that isn't there 204, as if it were.
//! - **Objects from elsewhere** (a presigned put, another S3 client) carry no SHA-256: the first
//!   head or get reads one whole, once, and keeps its SHA-256 with it (copied onto itself, only while
//!   it's still what was read).
//! - **Time.** Head, get and list say S3's Last-Modified, to the second; a put's answer says the
//!   Date of S3's answer.
//! - **Retries.** A request S3 answers `503 SlowDown` is sent again, three times at most, a
//!   backoff apart; a conditional write S3 answers `409 ConditionalRequestConflict` (another under
//!   way) likewise. Anything else unanswered is `Unavailable`, never guessed.
//! - **Tasks.** Each exchange with S3 runs as a task of its own on kiln's runtime, so a caller may
//!   wait on it from any future (`join_all`, `select`), whose wakers kiln's sockets don't take.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::blobs::{self as seam, Access, Blobs, Cond, Listed, Listing, Meta, Method, Put, Range, Refusal};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ci::integrations::aws::{self, Credentials, Signable};
use crate::http::client::{Client, Method as Http, Roots};

/// `[blobs.s3]`: the S3-compatible store kiln keeps its bytes in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where it answers: `https://s3.us-east-1.amazonaws.com`, `http://127.0.0.1:9000`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Where kiln's objects go in the bucket: at most 60 bytes, no leading or trailing `/`.
    pub prefix: String,
    /// Path-style addressing (`<endpoint>/<bucket>/<key>`), as MinIO and most S3-compatible stores
    /// take it; else virtual-hosted (`<bucket>.<host>/<key>`).
    pub path_style: bool,
    /// The access key's id (else `AWS_ACCESS_KEY_ID`).
    pub access_key_id: String,
    /// A file holding the secret access key (else `AWS_SECRET_ACCESS_KEY`): never in kiln.toml.
    pub secret_access_key_file: Option<PathBuf>,
    /// A file holding a session token, for temporary credentials (else `AWS_SESSION_TOKEN`).
    pub session_token_file: Option<PathBuf>,
    /// PEM certificates to trust for an HTTPS endpoint the public roots don't vouch for.
    pub ca_file: Option<PathBuf>,
    /// Make the bucket at start when it isn't there.
    pub create_bucket: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: String::new(),
            prefix: "kiln".into(),
            path_style: true,
            access_key_id: String::new(),
            secret_access_key_file: None,
            session_token_file: None,
            ca_file: None,
            create_bucket: false,
        }
    }
}

/// The longest prefix: S3's 1024-byte keys, less kiln's 960, `/o/` and the space after.
pub const PREFIX_MAX: usize = 60;
/// The most a single CopyObject copies (S3's); past it, UploadPartCopy.
const COPY_MAX: u64 = 5 << 30;
/// What an UploadPartCopy copies at a time.
const COPY_PART: u64 = 1 << 30;
/// What a read-back reads at a time.
const READ_PIECE: u64 = 8 << 20;
/// The longest answer read that isn't an object: an error, a listing of 1000 keys of 1 KiB.
const DOCUMENT_MAX: u64 = 4 << 20;
/// Tries of a request S3 asks to have again (`503 SlowDown`, `409 ConditionalRequestConflict`).
const TRIES: u32 = 4;
/// An upload left this long is let go by [`S3Blobs::collect`].
pub const UPLOAD_TTL: Duration = Duration::from_secs(2 * 86_400);
/// What a request waits for its answer's head; a body's reading is bounded by its size.
const ANSWER: Duration = Duration::from_secs(120);
/// The SHA-256 of nothing: an empty body's.
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the S3 store holds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The largest object, and part.
    pub object: u64,
    /// The smallest a part but the last may be (S3's).
    pub part_min: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { object: super::OBJECT_MAX, part_min: seam::PART_MIN }
    }
}

/// An error S3 answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct S3Error {
    status: u16,
    code: String,
    message: String,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S3 answered {} {}: {}", self.status, self.code, self.message)
    }
}

/// What S3 answered: its status, its headers (names in lower case) and its body.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// An answer that failed, or one S3 answered 200 with an error after (as it may, for a copy or
    /// a completion that took a while).
    fn failed(&self) -> bool {
        !self.ok() || self.body.starts_with(b"<?xml") && self.text().contains("<Error>")
    }

    fn error(&self) -> S3Error {
        let text = self.text();
        let code = aws::xml_text(&text, "Code").unwrap_or_else(|| match self.status {
            404 => "NotFound".into(),
            412 => "PreconditionFailed".into(),
            416 => "InvalidRange".into(),
            s => format!("HTTP {s}"),
        });
        S3Error { status: self.status, code, message: aws::xml_text(&text, "Message").unwrap_or_default() }
    }
}

/// What a request sends (its bytes and their SHA-256, hex), and the most of its answer read.
struct Payload<'a> {
    bytes: Bytes,
    sha: &'a str,
    limit: u64,
}

impl Payload<'static> {
    /// Nothing sent; at most `limit` of the answer read.
    fn none(limit: u64) -> Self {
        Self { bytes: Bytes::new(), sha: EMPTY_SHA, limit }
    }
}

/// What head says of an object, S3's own ETag beside kiln's SHA-256 (none: not written through
/// kiln yet).
#[derive(Debug, Clone)]
struct Found {
    size: u64,
    modified_ms: i64,
    sha: Option<String>,
    etag: String,
}

pub struct S3Blobs {
    https: bool,
    /// `host[:port]`, as the requests' Host header says it (the bucket's, virtual-hosted).
    authority: String,
    /// `/<bucket>` path-style, else empty.
    base: String,
    bucket: String,
    region: String,
    /// `<prefix>/`, or empty.
    root: String,
    creds: Credentials,
    client: Arc<Client>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    following: super::Following,
}

impl std::fmt::Debug for S3Blobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Blobs").field("authority", &self.authority).field("bucket", &self.bucket).field("root", &self.root).field("creds", &self.creds).finish()
    }
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn unavailable(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(e.to_string())
}

/// A secret from a file named, else from the environment.
fn secret(file: Option<&PathBuf>, env: &str) -> anyhow::Result<Option<String>> {
    match file {
        Some(p) => Ok(Some(std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?.trim().to_owned())),
        None => Ok(std::env::var(env).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())),
    }
}

/// AWS's timestamp of `ms`: `20150830T123600Z`.
fn amz_date(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// An HTTP date (`Wed, 21 Oct 2026 07:28:00 GMT`) in milliseconds since the epoch.
pub fn http_date_ms(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let words: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, hms, _] = words.as_slice() else { return None };
    let m = MONTHS.iter().position(|x| x == month)?;
    let month = time::Month::try_from(u8::try_from(m.checked_add(1)?).ok()?).ok()?;
    let mut t = hms.split(':').map(|p| p.parse::<u8>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let date = time::Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()?;
    date.with_hms(h, mi, se).ok()?.assume_utc().unix_timestamp().checked_mul(1000)
}

/// An ISO 8601 time (`2026-10-21T07:28:00.123Z`) in milliseconds since the epoch, to the second.
fn iso_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).ok()?;
    t.unix_timestamp().checked_mul(1000)
}

/// An ETag without its quotes.
fn bare(etag: &str) -> String {
    etag.trim().trim_matches('"').to_owned()
}

/// A query string in Signature Version 4's canonical form: sorted, each part encoded.
fn query_string(q: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = q.iter().map(|(k, v)| (aws::encode(k, false), aws::encode(v, false))).collect();
    pairs.sort();
    pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// An upload's id: its key, where it's joined, and S3's id for it.
fn upload_id(key: &str, place: &str, s3: &str) -> String {
    format!("{}~{place}~{s3}", hex::encode(key.as_bytes()))
}

/// What an upload's id says: its key, where it's joined, and S3's id (`None`: not one of ours).
fn parse_upload(id: &str) -> Option<(String, String, String)> {
    let mut parts = id.splitn(3, '~');
    let key = String::from_utf8(hex::decode(parts.next()?).ok()?).ok()?;
    let place = parts.next()?;
    let s3 = parts.next()?;
    (!place.is_empty() && place.bytes().all(|b| b.is_ascii_hexdigit()) && !s3.is_empty()).then(|| (key, place.to_owned(), s3.to_owned()))
}

/// CompleteMultipartUpload's document.
fn complete_xml(parts: &[(u32, String)]) -> String {
    let mut x = String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (n, etag) in parts {
        x.push_str(&format!("<Part><PartNumber>{n}</PartNumber><ETag>\"{}\"</ETag></Part>", bare(etag)));
    }
    x.push_str("</CompleteMultipartUpload>");
    x
}

impl S3Blobs {
    /// The store `cfg` names, its credentials read now.
    pub fn new(cfg: &Config, clock: Arc<dyn Clock>, limits: Limits) -> anyhow::Result<Self> {
        let (scheme, rest) = cfg.endpoint.split_once("://").ok_or_else(|| anyhow::anyhow!("[blobs.s3] endpoint {:?} isn't a URL", cfg.endpoint))?;
        let https = match scheme {
            "https" => true,
            "http" => false,
            other => anyhow::bail!("[blobs.s3] endpoint: {other}:// isn't http or https"),
        };
        let host = rest.trim_end_matches('/');
        anyhow::ensure!(!host.is_empty() && !host.contains('/'), "[blobs.s3] endpoint {:?}: a scheme and a host, no path", cfg.endpoint);
        anyhow::ensure!(!cfg.bucket.is_empty() && cfg.bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'), "[blobs.s3] bucket {:?} isn't a bucket's name", cfg.bucket);
        let prefix = cfg.prefix.trim_matches('/');
        anyhow::ensure!(prefix.len() <= PREFIX_MAX, "[blobs.s3] prefix is {} bytes: at most {PREFIX_MAX}", prefix.len());
        if !prefix.is_empty() {
            seam::check_key(prefix).map_err(|e| anyhow::anyhow!("[blobs.s3] prefix {prefix:?}: {e}"))?;
        }
        let (authority, base) = if cfg.path_style { (host.to_owned(), format!("/{}", cfg.bucket)) } else { (format!("{}.{host}", cfg.bucket), String::new()) };
        let access = if cfg.access_key_id.is_empty() { std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default() } else { cfg.access_key_id.clone() };
        let secret_key = secret(cfg.secret_access_key_file.as_ref(), "AWS_SECRET_ACCESS_KEY")?.unwrap_or_default();
        anyhow::ensure!(!access.is_empty() && !secret_key.is_empty(), "[blobs.s3] needs an access key: access_key_id and secret_access_key_file (or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY)");
        let session_token = secret(cfg.session_token_file.as_ref(), "AWS_SESSION_TOKEN")?;
        let roots = match &cfg.ca_file {
            Some(p) => Roots::Pem(std::fs::read(p).map_err(|e| anyhow::anyhow!("[blobs.s3] ca_file {}: {e}", p.display()))?),
            None => Roots::WebPki,
        };
        Ok(Self {
            https,
            authority,
            base,
            bucket: cfg.bucket.clone(),
            region: if cfg.region.is_empty() { "us-east-1".into() } else { cfg.region.clone() },
            root: if prefix.is_empty() { String::new() } else { format!("{prefix}/") },
            creds: Credentials { access_key_id: access, secret_access_key: secret_key, session_token, expires_at: None },
            client: Arc::new(Client::new(roots, None)?),
            clock,
            limits,
            following: super::Following::default(),
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Where key `key`'s object is in the bucket: a space after it (the module's note).
    fn object(&self, key: &str) -> String {
        format!("{}o/{key} ", self.root)
    }

    /// Where the objects whose keys start with `prefix` are.
    fn objects(&self, prefix: &str) -> String {
        format!("{}o/{prefix}", self.root)
    }

    /// Where upload place `place` is joined.
    fn joined(&self, place: &str) -> String {
        format!("{}u/{place}", self.root)
    }

    /// The path a request goes to: the bucket's (`s3key` none) or an object's.
    fn path(&self, s3key: Option<&str>) -> String {
        match s3key {
            Some(k) => format!("{}/{}", self.base, seam::encode_key(k)),
            None => format!("{}/", self.base),
        }
    }

    fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    /// Signs and sends one request, as a task of its own; once again a backoff apart while S3
    /// answers `503 SlowDown`, or `409` to a conditional write.
    async fn send(&self, method: Http, s3key: Option<&str>, query: &[(&str, String)], headers: &[(String, String)], payload: Payload<'_>) -> Result<Answer, Refusal> {
        let Payload { bytes: body, sha: body_sha, limit } = payload;
        let path = self.path(s3key);
        let query: Vec<(String, String)> = query.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
        let qs = query_string(&query);
        let url = format!("{}://{}{path}{}{qs}", self.scheme(), self.authority, if qs.is_empty() { "" } else { "?" });
        let conditional = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("if-none-match") || k.eq_ignore_ascii_case("if-match"));
        let method_name = match method {
            Http::Get => "GET",
            Http::Put => "PUT",
            Http::Post => "POST",
            Http::Delete => "DELETE",
            Http::Head => "HEAD",
            Http::Patch => "PATCH",
        };
        let mut wait = Duration::from_millis(100);
        for attempt in 1..=TRIES {
            let amz = amz_date(self.clock.unix_ms());
            let mut signed: Vec<(String, String)> = headers.to_vec();
            signed.push(("host".into(), self.authority.clone()));
            signed.push(("x-amz-date".into(), amz.clone()));
            signed.push(("x-amz-content-sha256".into(), body_sha.to_owned()));
            if let Some(t) = &self.creds.session_token {
                signed.push(("x-amz-security-token".into(), t.clone()));
            }
            let auth = aws::authorization(&self.creds, &self.region, "s3", &amz, &Signable { method: method_name, path: &path, query: &query, headers: &signed, payload_sha256: body_sha }, false);
            let (client, url, body) = (Arc::clone(&self.client), url.clone(), body.clone());
            let answer = crate::rt::spawn(async move {
                let mut req = client.request(method, &url).timeout(ANSWER).header("authorization", &auth);
                for (k, v) in signed.iter().filter(|(k, _)| k != "host") {
                    req = req.header(k, v);
                }
                if matches!(method, Http::Put | Http::Post) {
                    req = req.body(body);
                }
                let resp = req.send().await?;
                let (status, headers) = (resp.status(), resp.headers());
                let body = resp.bytes(usize::try_from(limit).unwrap_or(usize::MAX)).await?;
                Ok::<_, anyhow::Error>(Answer { status, headers, body: body.to_vec() })
            })
            .await;
            let answer = match answer {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => return Err(Refusal::Unavailable(format!("S3 at {}: {e:#}", self.authority))),
                Err(_) => return Err(Refusal::Unavailable(format!("S3 at {}: the request's task ended", self.authority))),
            };
            let again = answer.status == 503 || (conditional && answer.status == 409);
            if !again || attempt == TRIES {
                return Ok(answer);
            }
            crate::rt::sleep(wait).await;
            wait = wait.saturating_mul(4);
        }
        Err(Refusal::Unavailable("S3 asked to wait, every time".into()))
    }

    /// A request with no body, its answer's body read up to [`DOCUMENT_MAX`].
    async fn ask(&self, method: Http, s3key: Option<&str>, query: &[(&str, String)], headers: &[(String, String)]) -> Result<Answer, Refusal> {
        self.send(method, s3key, query, headers, Payload::none(DOCUMENT_MAX)).await
    }

    /// Makes the bucket, when it isn't there.
    pub async fn ensure_bucket(&self) -> Result<(), Refusal> {
        let a = self.ask(Http::Head, None, &[], &[]).await?;
        if a.ok() {
            return Ok(());
        }
        if a.status != 404 {
            return Err(unavailable(a.error()));
        }
        let body = if self.region == "us-east-1" {
            Vec::new()
        } else {
            format!("<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>", self.region).into_bytes()
        };
        let sha = crate::util::sha256_hex(&body);
        let a = self.send(Http::Put, None, &[], &[], Payload { bytes: Bytes::from(body), sha: &sha, limit: DOCUMENT_MAX }).await?;
        if a.ok() || a.error().code == "BucketAlreadyOwnedByYou" { Ok(()) } else { Err(unavailable(a.error())) }
    }

    /// What head says of the object at S3 key `s3key`.
    async fn found(&self, key: &str, s3key: &str) -> Result<Found, Refusal> {
        let a = self.ask(Http::Head, Some(s3key), &[], &[]).await?;
        if a.status == 404 {
            return Err(Refusal::NotFound(key.to_owned()));
        }
        if !a.ok() {
            return Err(unavailable(a.error()));
        }
        Ok(Found {
            size: a.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0),
            modified_ms: a.header("last-modified").and_then(http_date_ms).unwrap_or(0),
            sha: a.header("x-amz-meta-kiln-sha256").map(str::to_ascii_lowercase).filter(|s| seam::check_sha256(s).is_ok()),
            etag: a.header("etag").map(bare).unwrap_or_default(),
        })
    }

    /// The object at `key` as kiln has it, and S3's ETag of it: an object from elsewhere has its
    /// SHA-256 taken first.
    async fn meta(&self, key: &str) -> Result<(Meta, String), Refusal> {
        let s3key = self.object(key);
        let f = self.found(key, &s3key).await?;
        match f.sha {
            Some(sha) => Ok((Meta { key: key.to_owned(), size: f.size, etag: sha, modified_ms: f.modified_ms }, f.etag)),
            None => self.adopt(key, &s3key).await,
        }
    }

    /// Takes in an object written without kiln's SHA-256 (a presigned put, another client): reads it
    /// whole, once, and keeps its SHA-256 with it, copied onto itself while it's still what was read.
    async fn adopt(&self, key: &str, s3key: &str) -> Result<(Meta, String), Refusal> {
        for _ in 0..3 {
            let f = self.found(key, s3key).await?;
            if f.size > self.limits.object {
                return Err(Refusal::TooLarge { what: "an object to take in", size: f.size, limit: self.limits.object });
            }
            let a = self.send(Http::Get, Some(s3key), &[], &[("if-match".to_owned(), format!("\"{}\"", f.etag))], Payload::none(self.limits.object)).await?;
            match a.status {
                404 => return Err(Refusal::NotFound(key.to_owned())),
                412 => continue,
                _ if !a.ok() => return Err(unavailable(a.error())),
                _ => {}
            }
            let bytes = a.body;
            let size = len64(bytes.len());
            let sha = crate::rt::blocking(move || hex::encode(Sha256::digest(&bytes))).await.map_err(|e| unavailable(format!("{e:#}")))?;
            let headers = vec![
                ("x-amz-copy-source".to_owned(), format!("/{}/{}", self.bucket, seam::encode_key(s3key))),
                ("x-amz-copy-source-if-match".to_owned(), format!("\"{}\"", f.etag)),
                ("x-amz-metadata-directive".to_owned(), "REPLACE".to_owned()),
                ("x-amz-meta-kiln-sha256".to_owned(), sha.clone()),
            ];
            let a = self.ask(Http::Put, Some(s3key), &[], &headers).await?;
            if a.status == 412 || a.error().code == "PreconditionFailed" && a.failed() {
                continue;
            }
            if a.status == 404 {
                return Err(Refusal::NotFound(key.to_owned()));
            }
            if a.failed() {
                return Err(unavailable(format!("S3 kept no SHA-256 with {key}: {}", a.error())));
            }
            let text = a.text();
            let modified = aws::xml_text(&text, "LastModified").as_deref().and_then(iso_ms).unwrap_or(0);
            let etag = aws::xml_text(&text, "ETag").map(|e| bare(&e)).unwrap_or(f.etag);
            return Ok((Meta { key: key.to_owned(), size, etag: sha, modified_ms: modified }, etag));
        }
        Err(Refusal::Unavailable(format!("{key} kept changing while its SHA-256 was taken")))
    }

    /// The SHA-256 of the object at `s3key` (`size` bytes), read back a piece at a time.
    async fn read_back(&self, s3key: &str, size: u64) -> Result<String, Refusal> {
        let mut sha = Sha256::new();
        let mut at = 0u64;
        while at < size {
            let end = at.saturating_add(READ_PIECE.saturating_sub(1)).min(size.saturating_sub(1));
            let a = self.send(Http::Get, Some(s3key), &[], &[("range".into(), format!("bytes={at}-{end}"))], Payload::none(READ_PIECE)).await?;
            if !a.ok() {
                return Err(unavailable(a.error()));
            }
            let piece = a.body;
            if piece.is_empty() {
                return Err(Refusal::Unavailable(format!("S3 gave no bytes at {at} of {s3key}")));
            }
            at = at.saturating_add(len64(piece.len()));
            sha = crate::rt::blocking(move || {
                sha.update(&piece);
                sha
            })
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
        }
        Ok(hex::encode(sha.finalize()))
    }

    /// Copies the joined upload at `from` to `to` with its SHA-256: CopyObject, or UploadPartCopy
    /// past 5 GiB. The copy's modified time.
    async fn copy_into_place(&self, from: &str, to: &str, size: u64, sha: &str) -> Result<i64, Refusal> {
        let source = format!("/{}/{}", self.bucket, seam::encode_key(from));
        if size <= COPY_MAX {
            let headers = vec![
                ("x-amz-copy-source".to_owned(), source),
                ("x-amz-metadata-directive".to_owned(), "REPLACE".to_owned()),
                ("x-amz-meta-kiln-sha256".to_owned(), sha.to_owned()),
            ];
            let a = self.ask(Http::Put, Some(to), &[], &headers).await?;
            if a.failed() {
                return Err(unavailable(format!("S3 didn't copy the upload into place: {}", a.error())));
            }
            return Ok(aws::xml_text(&a.text(), "LastModified").as_deref().and_then(iso_ms).unwrap_or(0));
        }
        let a = self.ask(Http::Post, Some(to), &[("uploads", String::new())], &[("x-amz-meta-kiln-sha256".to_owned(), sha.to_owned())]).await?;
        if a.failed() {
            return Err(unavailable(a.error()));
        }
        let id = aws::xml_text(&a.text(), "UploadId").ok_or_else(|| Refusal::Unavailable("S3 gave no upload id for the copy".into()))?;
        let mut parts = Vec::new();
        let (mut at, mut n) = (0u64, 1u32);
        while at < size {
            let end = at.saturating_add(COPY_PART.saturating_sub(1)).min(size.saturating_sub(1));
            let headers = vec![("x-amz-copy-source".to_owned(), source.clone()), ("x-amz-copy-source-range".to_owned(), format!("bytes={at}-{end}"))];
            let a = self.ask(Http::Put, Some(to), &[("partNumber", n.to_string()), ("uploadId", id.clone())], &headers).await?;
            if a.failed() {
                let _ = self.ask(Http::Delete, Some(to), &[("uploadId", id.clone())], &[]).await;
                return Err(unavailable(format!("S3 didn't copy part {n} into place: {}", a.error())));
            }
            parts.push((n, aws::xml_text(&a.text(), "ETag").map(|e| bare(&e)).unwrap_or_default()));
            at = end.saturating_add(1);
            n = n.saturating_add(1);
        }
        let xml = complete_xml(&parts);
        let sha_xml = crate::util::sha256_hex(xml.as_bytes());
        let a = self.send(Http::Post, Some(to), &[("uploadId", id.clone())], &[], Payload { bytes: Bytes::from(xml), sha: &sha_xml, limit: DOCUMENT_MAX }).await?;
        if a.failed() {
            let _ = self.ask(Http::Delete, Some(to), &[("uploadId", id)], &[]).await;
            return Err(unavailable(format!("S3 didn't join the copy: {}", a.error())));
        }
        Ok(self.found(to, to).await?.modified_ms)
    }

    /// The parts S3 holds of an upload: number, ETag and size, in order.
    async fn parts(&self, id: &str, place: &str, s3id: &str) -> Result<Vec<(u32, String, u64)>, Refusal> {
        let mut out = Vec::new();
        let mut marker = String::new();
        for _ in 0..11 {
            let mut q = vec![("uploadId", s3id.to_owned()), ("max-parts", "1000".to_owned())];
            if !marker.is_empty() {
                q.push(("part-number-marker", marker.clone()));
            }
            let a = self.ask(Http::Get, Some(&self.joined(place)), &q, &[]).await?;
            if !a.ok() {
                let e = a.error();
                return Err(if e.code == "NoSuchUpload" || e.status == 404 { Refusal::NoSuchUpload(id.to_owned()) } else { unavailable(e) });
            }
            let text = a.text();
            for p in aws::xml_all(&text, "Part") {
                let n = aws::xml_text(p, "PartNumber").and_then(|v| v.parse().ok()).unwrap_or(0);
                let etag = aws::xml_text(p, "ETag").map(|e| bare(&e)).unwrap_or_default();
                let size = aws::xml_text(p, "Size").and_then(|v| v.parse().ok()).unwrap_or(0);
                out.push((n, etag, size));
            }
            if aws::xml_text(&text, "IsTruncated").as_deref() != Some("true") {
                return Ok(out);
            }
            marker = aws::xml_text(&text, "NextPartNumberMarker").unwrap_or_default();
            if marker.is_empty() {
                return Ok(out);
            }
        }
        Ok(out)
    }

    /// Uses a presigned URL as its holder would (what a fixture or a test does).
    pub async fn redeem(&self, url: &str, method: Method, body: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Refusal> {
        let (client, url, limit) = (Arc::clone(&self.client), url.to_owned(), self.limits.object);
        let answer = crate::rt::spawn(async move {
            let req = match method {
                Method::Get => client.request(Http::Get, &url),
                Method::Put => client.request(Http::Put, &url).body(body.unwrap_or_default()),
            };
            let resp = req.timeout(ANSWER).send().await?;
            let (status, headers) = (resp.status(), resp.headers());
            let body = resp.bytes(usize::try_from(limit).unwrap_or(usize::MAX)).await?;
            Ok::<_, anyhow::Error>(Answer { status, headers, body: body.to_vec() })
        })
        .await;
        let a = match answer {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => return Err(unavailable(format!("{e:#}"))),
            Err(_) => return Err(Refusal::Unavailable("the request's task ended".into())),
        };
        if a.ok() {
            return Ok(match method {
                Method::Get => Some(a.body),
                Method::Put => None,
            });
        }
        let e = a.error();
        Err(match a.status {
            404 => Refusal::NotFound(e.message),
            403 if e.message.to_ascii_lowercase().contains("expired") => Refusal::Expired,
            _ => Refusal::Forbidden(e.to_string()),
        })
    }

    /// Lets go of uploads begun over [`UPLOAD_TTL`] ago, and what they left joined (a crash between
    /// joining and copying into place).
    pub async fn collect(&self) -> anyhow::Result<usize> {
        let before = self.clock.unix_ms().saturating_sub(i64::try_from(UPLOAD_TTL.as_millis()).unwrap_or(i64::MAX));
        let place = format!("{}u/", self.root);
        let mut gone = 0usize;
        let a = self.ask(Http::Get, None, &[("uploads", String::new()), ("prefix", place.clone())], &[]).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if a.ok() {
            let text = a.text();
            for u in aws::xml_all(&text, "Upload") {
                let (Some(key), Some(id)) = (aws::xml_text(u, "Key"), aws::xml_text(u, "UploadId")) else { continue };
                if aws::xml_text(u, "Initiated").as_deref().and_then(iso_ms).is_some_and(|t| t < before) {
                    let _ = self.ask(Http::Delete, Some(&key), &[("uploadId", id)], &[]).await;
                    gone = gone.saturating_add(1);
                }
            }
        }
        let a = self.ask(Http::Get, None, &[("list-type", "2".to_owned()), ("prefix", place), ("max-keys", "1000".to_owned())], &[]).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if a.ok() {
            let text = a.text();
            for c in aws::xml_all(&text, "Contents") {
                let Some(key) = aws::xml_text(c, "Key") else { continue };
                if aws::xml_text(c, "LastModified").as_deref().and_then(iso_ms).is_some_and(|t| t < before) {
                    let _ = self.ask(Http::Delete, Some(&key), &[], &[]).await;
                    gone = gone.saturating_add(1);
                }
            }
        }
        Ok(gone)
    }
}

impl Blobs for S3Blobs {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let size = len64(bytes.len());
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
            }
            if let Some(want) = &put.sha256 {
                seam::check_sha256(want)?;
            }
            let bytes = Bytes::from(bytes);
            let b = bytes.clone();
            let digest = crate::rt::blocking(move || Sha256::digest(&b)).await.map_err(|e| unavailable(format!("{e:#}")))?;
            let sha = hex::encode(digest);
            if let Some(want) = put.sha256.as_ref().filter(|w| !w.eq_ignore_ascii_case(&sha)) {
                return Err(Refusal::BadChecksum { want: want.clone(), got: sha });
            }
            let s3key = self.object(key);
            for _ in 0..3 {
                let mut headers = vec![
                    ("x-amz-meta-kiln-sha256".to_owned(), sha.clone()),
                    ("x-amz-checksum-sha256".to_owned(), base64::engine::general_purpose::STANDARD.encode(digest)),
                    ("content-type".to_owned(), "application/octet-stream".to_owned()),
                ];
                match &put.cond {
                    Cond::Always => {}
                    Cond::IfNoneMatch => headers.push(("if-none-match".to_owned(), "*".to_owned())),
                    Cond::IfMatch(want) => {
                        let (now, etag) = match self.meta(key).await {
                            Ok(m) => m,
                            Err(Refusal::NotFound(_)) => return Err(Refusal::PreconditionFailed(key.to_owned())),
                            Err(e) => return Err(e),
                        };
                        if &now.etag != want {
                            return Err(Refusal::PreconditionFailed(key.to_owned()));
                        }
                        headers.push(("if-match".to_owned(), format!("\"{etag}\"")));
                    }
                }
                let a = self.send(Http::Put, Some(&s3key), &[], &headers, Payload { bytes: bytes.clone(), sha: &sha, limit: DOCUMENT_MAX }).await?;
                if a.ok() {
                    let modified = a.header("date").and_then(http_date_ms).unwrap_or_else(|| self.clock.unix_ms().div_euclid(1000).saturating_mul(1000));
                    return Ok(Meta { key: key.to_owned(), size, etag: sha, modified_ms: modified });
                }
                let e = a.error();
                match (a.status, e.code.as_str(), &put.cond) {
                    // The object changed between head and put: judged again against what's there now.
                    (412, _, Cond::IfMatch(_)) => {}
                    (412 | 409, _, _) => return Err(Refusal::PreconditionFailed(key.to_owned())),
                    (_, "BadDigest" | "XAmzContentSHA256Mismatch" | "XAmzContentChecksumMismatch" | "InvalidDigest", _) => return Err(Refusal::BadChecksum { want: sha, got: e.message }),
                    _ => return Err(unavailable(e)),
                }
            }
            Err(Refusal::PreconditionFailed(key.to_owned()))
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let s3key = self.object(key);
            if let Some(r) = range
                && r.end.is_some_and(|e| e < r.start)
            {
                let (m, _) = self.meta(key).await?;
                return Err(Refusal::InvalidRange { size: m.size });
            }
            for _ in 0..2 {
                let headers: Vec<(String, String)> = match range {
                    Some(Range { start, end: Some(end) }) => vec![("range".into(), format!("bytes={start}-{end}"))],
                    Some(Range { start, end: None }) => vec![("range".into(), format!("bytes={start}-"))],
                    None => Vec::new(),
                };
                let a = self.send(Http::Get, Some(&s3key), &[], &headers, Payload::none(self.limits.object)).await?;
                match a.status {
                    404 => return Err(Refusal::NotFound(key.to_owned())),
                    416 => {
                        let (m, _) = self.meta(key).await?;
                        return Err(Refusal::InvalidRange { size: m.size });
                    }
                    _ if !a.ok() => return Err(unavailable(a.error())),
                    _ => {}
                }
                let Some(sha) = a.header("x-amz-meta-kiln-sha256").map(str::to_ascii_lowercase).filter(|s| seam::check_sha256(s).is_ok()) else {
                    // From elsewhere: its SHA-256 taken in, then read again.
                    self.adopt(key, &s3key).await?;
                    continue;
                };
                let partial = a.status == 206;
                let modified = a.header("last-modified").and_then(http_date_ms).unwrap_or(0);
                let total = if partial { a.header("content-range").and_then(|v| v.rsplit('/').next()).and_then(|v| v.parse().ok()).unwrap_or(0) } else { len64(a.body.len()) };
                let meta = Meta { key: key.to_owned(), size: total, etag: sha, modified_ms: modified };
                let bytes = a.body;
                let Some(range) = range else {
                    // Read whole, it's checked against its name.
                    let etag = meta.etag.clone();
                    let (b, ok) = crate::rt::blocking(move || {
                        let ok = hex::encode(Sha256::digest(&bytes)) == etag;
                        (bytes, ok)
                    })
                    .await
                    .map_err(|e| unavailable(format!("{e:#}")))?;
                    if !ok {
                        return Err(Refusal::Unavailable(format!("{key} is damaged: its bytes don't hash to its etag")));
                    }
                    return Ok((meta, b));
                };
                let (first, last) = seam::byte_range(range, meta.size)?;
                if partial {
                    let want = last.saturating_sub(first).saturating_add(1);
                    if len64(bytes.len()) != want {
                        return Err(Refusal::Unavailable(format!("S3 gave {} of the {want} bytes asked of {key}", bytes.len())));
                    }
                    return Ok((meta, bytes));
                }
                // S3 gave it whole (some stores do, for a range of a small object): the range taken here.
                let (s, e) = (usize::try_from(first).unwrap_or(usize::MAX), usize::try_from(last).unwrap_or(usize::MAX));
                let part = bytes.get(s..=e).map(<[u8]>::to_vec).ok_or(Refusal::InvalidRange { size: meta.size })?;
                return Ok((meta, part));
            }
            Err(Refusal::Unavailable(format!("{key} kept changing while it was read")))
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.meta(key).await.map(|(m, _)| m)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let a = self.ask(Http::Delete, Some(&self.object(key)), &[], &[]).await?;
            if a.ok() || a.status == 404 { Ok(()) } else { Err(unavailable(a.error())) }
        })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        Box::pin(async move {
            let limit = seam::list_limit(limit)?;
            // One more than asked: the key that is the prefix but its last space (`p` for `p `) is
            // stored as `p `, so S3 lists it too, and it's left out below.
            let mut q = vec![("list-type", "2".to_owned()), ("prefix", self.objects(prefix)), ("max-keys", limit.saturating_add(1).min(seam::LIST_MAX).to_string())];
            if let Some(a) = after {
                q.push(("start-after", self.object(a)));
            }
            let a = self.ask(Http::Get, None, &q, &[]).await?;
            if !a.ok() {
                return Err(unavailable(a.error()));
            }
            let text = a.text();
            let strip = self.objects("");
            let mut objects = Vec::new();
            for c in aws::xml_all(&text, "Contents") {
                let Some(k) = aws::xml_text(c, "Key") else { continue };
                let Some(key) = k.strip_prefix(&strip).and_then(|k| k.strip_suffix(' ')).filter(|k| k.starts_with(prefix)) else { continue };
                objects.push(Listed {
                    key: key.to_owned(),
                    size: aws::xml_text(c, "Size").and_then(|v| v.parse().ok()).unwrap_or(0),
                    modified_ms: aws::xml_text(c, "LastModified").as_deref().and_then(iso_ms).unwrap_or(0),
                });
            }
            let more = objects.len() > limit;
            objects.truncate(limit);
            let truncated = more || aws::xml_text(&text, "IsTruncated").as_deref() == Some("true");
            Ok(Listing { objects, truncated })
        })
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let place = hex::encode(rand::random::<[u8; 16]>());
            let a = self.ask(Http::Post, Some(&self.joined(&place)), &[("uploads", String::new())], &[]).await?;
            if a.failed() {
                return Err(unavailable(a.error()));
            }
            let s3 = aws::xml_text(&a.text(), "UploadId").ok_or_else(|| Refusal::Unavailable("S3 gave no upload id".into()))?;
            let id = upload_id(key, &place, &s3);
            self.following.start(&id);
            Ok(id)
        })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_part(part)?;
            let size = len64(bytes.len());
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "a part", size, limit: self.limits.object });
            }
            let (_, place, s3id) = parse_upload(upload).ok_or_else(|| Refusal::NoSuchUpload(upload.to_owned()))?;
            let bytes = Arc::new(bytes);
            let b = Arc::clone(&bytes);
            let sha = crate::rt::blocking(move || hex::encode(Sha256::digest(b.as_slice()))).await.map_err(|e| unavailable(format!("{e:#}")))?;
            let body = Bytes::from(bytes.as_ref().clone());
            let a = self.send(Http::Put, Some(&self.joined(&place)), &[("partNumber", part.to_string()), ("uploadId", s3id)], &[], Payload { bytes: body, sha: &sha, limit: DOCUMENT_MAX }).await?;
            if !a.ok() {
                let e = a.error();
                return Err(if e.code == "NoSuchUpload" || e.status == 404 { Refusal::NoSuchUpload(upload.to_owned()) } else { unavailable(e) });
            }
            let etag = a.header("etag").map(bare).filter(|e| !e.is_empty()).ok_or_else(|| Refusal::Unavailable("S3 gave the part no ETag".into()))?;
            self.following.part(upload, part, bytes).await;
            Ok(etag)
        })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let (key, place, s3id) = parse_upload(upload).ok_or_else(|| Refusal::NoSuchUpload(upload.to_owned()))?;
            let have = self.parts(upload, &place, &s3id).await?;
            seam::check_parts(&parts)?;
            let last = parts.len().saturating_sub(1);
            let mut size = 0u64;
            for (i, (n, etag)) in parts.iter().enumerate() {
                let (_, have_etag, part_size) = have.iter().find(|(m, _, _)| m == n).ok_or_else(|| Refusal::BadParts(format!("part {n} wasn't uploaded")))?;
                if bare(etag) != *have_etag {
                    return Err(Refusal::BadParts(format!("part {n}'s etag doesn't match")));
                }
                if i < last && *part_size < len64(self.limits.part_min) {
                    return Err(Refusal::BadParts(format!("part {n} is {part_size} bytes: every part but the last is at least {}", self.limits.part_min)));
                }
                size = size.saturating_add(*part_size);
            }
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
            }
            let joined = self.joined(&place);
            let xml = complete_xml(&parts);
            let sha_xml = crate::util::sha256_hex(xml.as_bytes());
            let a = self.send(Http::Post, Some(&joined), &[("uploadId", s3id.clone())], &[], Payload { bytes: Bytes::from(xml), sha: &sha_xml, limit: DOCUMENT_MAX }).await?;
            if a.failed() {
                let e = a.error();
                return Err(match e.code.as_str() {
                    "NoSuchUpload" => Refusal::NoSuchUpload(upload.to_owned()),
                    "InvalidPart" | "InvalidPartOrder" | "EntityTooSmall" | "MalformedXML" => Refusal::BadParts(format!("{}: {}", e.code, e.message)),
                    _ => Refusal::Unavailable(format!("S3 didn't join the upload: {e}")),
                });
            }
            let sha = match self.following.finish(upload, &parts) {
                Some(s) => s,
                None => self.read_back(&joined, size).await?,
            };
            let modified = self.copy_into_place(&joined, &self.object(&key), size, &sha).await?;
            if !self.ask(Http::Delete, Some(&joined), &[], &[]).await.is_ok_and(|a| a.ok()) {
                tracing::debug!(target: "kiln.blobs", event = "put", key = %key, "an upload's joined object stays until collected");
            }
            Ok(Meta { key, size, etag: sha, modified_ms: modified })
        })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let (_, place, s3id) = parse_upload(upload).ok_or_else(|| Refusal::NoSuchUpload(upload.to_owned()))?;
            self.following.forget(upload);
            // MinIO answers an abort of an upload that isn't there 204: its parts are asked first.
            self.parts(upload, &place, &s3id).await?;
            let a = self.ask(Http::Delete, Some(&self.joined(&place)), &[("uploadId", s3id)], &[]).await?;
            if a.ok() {
                return Ok(());
            }
            let e = a.error();
            Err(if e.code == "NoSuchUpload" || e.status == 404 { Refusal::NoSuchUpload(upload.to_owned()) } else { unavailable(e) })
        })
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let secs = seam::lifetime_secs(expires)?;
            let amz = amz_date(self.clock.unix_ms());
            let date = amz.get(..8).unwrap_or(&amz);
            let scope = format!("{date}/{}/s3/aws4_request", self.region);
            // A put's length is signed: S3 refuses a body of any other.
            let (m, headers) = match access {
                Access::Get => ("GET", vec![("host".to_owned(), self.authority.clone())]),
                Access::Put { length } => ("PUT", vec![("content-length".to_owned(), length.to_string()), ("host".to_owned(), self.authority.clone())]),
            };
            let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
            let mut query = vec![
                ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
                ("X-Amz-Credential".to_owned(), format!("{}/{scope}", self.creds.access_key_id)),
                ("X-Amz-Date".to_owned(), amz.clone()),
                ("X-Amz-Expires".to_owned(), secs.to_string()),
                ("X-Amz-SignedHeaders".to_owned(), signed_headers),
            ];
            if let Some(t) = &self.creds.session_token {
                query.push(("X-Amz-Security-Token".to_owned(), t.clone()));
            }
            let path = self.path(Some(&self.object(key)));
            let (canonical, _) = aws::canonical_request(&Signable { method: m, path: &path, query: &query, headers: &headers, payload_sha256: "UNSIGNED-PAYLOAD" }, false);
            let to_sign = format!("AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}", crate::util::sha256_hex(canonical.as_bytes()));
            let signature = crate::util::hmac_hex(&aws::signing_key(&self.creds.secret_access_key, date, &self.region, "s3"), to_sign.as_bytes());
            Ok(format!("{}://{}{path}?{}&X-Amz-Signature={signature}", self.scheme(), self.authority, query_string(&query)))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kiln_seams::blobs::BlobsFixture;

    /// The largest object the fixture's store takes: small, so the suite can pass it.
    const FIXTURE_OBJECT: u64 = 24 << 20;

    /// An S3 store for a test, from `KILN_TEST_S3`: `<endpoint> <bucket> <access key id> <a file
    /// holding the secret>` (the S3 lab's MinIO on fedora, docs/blobs.md).
    pub(crate) fn from_env(prefix: &str, limits: Limits) -> Option<anyhow::Result<S3Blobs>> {
        let spec = std::env::var("KILN_TEST_S3").ok()?;
        let words: Vec<&str> = spec.split_whitespace().collect();
        let [endpoint, bucket, access, secret_file] = words.as_slice() else { return Some(Err(anyhow::anyhow!("KILN_TEST_S3: <endpoint> <bucket> <access key id> <secret file>"))) };
        let cfg = Config {
            endpoint: (*endpoint).to_owned(),
            bucket: (*bucket).to_owned(),
            prefix: prefix.to_owned(),
            access_key_id: (*access).to_owned(),
            secret_access_key_file: Some(PathBuf::from(secret_file)),
            ..Config::default()
        };
        Some(S3Blobs::new(&cfg, crate::rt::clock(), limits))
    }

    pub(crate) struct Fixture {
        pub blobs: S3Blobs,
    }

    impl BlobsFixture for Fixture {
        fn blobs(&self) -> &dyn Blobs {
            &self.blobs
        }
        fn part_min(&self) -> usize {
            self.blobs.limits.part_min
        }
        fn object_max(&self) -> u64 {
            FIXTURE_OBJECT
        }
        fn resolution(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
            // An S3's clock isn't the test's: its time is waited out.
            Box::pin(crate::rt::sleep(d))
        }
        fn redeem<'a>(&'a self, url: &'a str, method: Method, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>> {
            Box::pin(self.blobs.redeem(url, method, body))
        }
    }

    /// CP-T10.1, AC-T10.1.2 (and T-T10.1.1, its BLOBS-58: a conditional create racing others):
    /// an S3 store passes every clause of the Blobs suite, each case under a
    /// prefix of its own in one bucket. Runs where `KILN_TEST_S3` names an S3 (the S3 lab: MinIO
    /// in a container on fedora, docs/blobs.md): `cargo test -p kiln --lib blobs::s3 -- --ignored`.
    #[test]
    #[ignore = "needs an S3: KILN_TEST_S3 (docs/blobs.md, \"The suite against MinIO\")"]
    fn an_s3_store_passes_the_blobs_suite() -> anyhow::Result<()> {
        let run = format!("conformance/{}", hex::encode(rand::random::<[u8; 6]>()));
        let report = crate::rt::block_on(async move {
            let mut n = 0u32;
            crate::blobs::tests::run_suite(
                || {
                    n = n.saturating_add(1);
                    let prefix = format!("{run}/{n}");
                    async move {
                        let blobs = from_env(&prefix, Limits { object: FIXTURE_OBJECT, ..Limits::default() }).ok_or_else(|| anyhow::anyhow!("KILN_TEST_S3 isn't set"))??;
                        blobs.ensure_bucket().await?;
                        Ok(Fixture { blobs })
                    }
                },
                Duration::from_secs(300),
            )
            .await
        })?;
        let passed = report.into_result().map_err(|e| anyhow::anyhow!(e))?;
        assert!(passed >= 150, "{passed} cases");
        Ok(())
    }

    /// The dates S3 answers, read to the millisecond and the second.
    #[test]
    fn s3_s_dates_read_as_s3_writes_them() {
        assert_eq!(http_date_ms("Wed, 21 Oct 2026 07:28:00 GMT"), Some(1_792_567_680_000));
        assert_eq!(iso_ms("2026-10-21T07:28:00.999Z"), Some(1_792_567_680_000));
        assert_eq!(http_date_ms("not a date"), None);
        assert_eq!(amz_date(1_792_567_680_999), "20261021T072800Z");
    }

    /// An upload's id carries its key, where it's joined and S3's id, and nothing else passes for one.
    #[test]
    fn an_upload_id_says_its_key_place_and_s3_id() {
        let id = upload_id("a/ü key", "00ff", "s3~id.with-odd_chars");
        assert_eq!(parse_upload(&id), Some(("a/ü key".to_owned(), "00ff".to_owned(), "s3~id.with-odd_chars".to_owned())));
        for bad in ["", "no-such-upload", "zz~00ff~x", "61~~x", "61~00ff~"] {
            assert_eq!(parse_upload(bad), None, "{bad:?}");
        }
    }

    /// A space after every key keeps S3's order of the keys kiln's: a key and one that goes on from
    /// it, and every character a key may hold after it.
    #[test]
    fn a_space_after_each_key_keeps_kiln_s_order() {
        let mut keys = ["p", "p/x", "p/x/y", "pa", "p ", "p!", "a", "a/b", "a&", "a0", "é", "Z", "p  q"].map(str::to_owned).to_vec();
        keys.sort();
        let mut spaced: Vec<String> = keys.iter().map(|k| format!("{k} ")).collect();
        spaced.sort();
        assert_eq!(spaced.iter().map(|k| k.trim_end_matches(' ').to_owned()).collect::<Vec<_>>().len(), keys.len());
        assert!(spaced.iter().zip(&keys).all(|(s, k)| s.strip_suffix(' ') == Some(k.as_str())), "{spaced:?} vs {keys:?}");
    }
}
