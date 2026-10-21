//! Log archives, live (docs/observe.md §10.5, §13): a kiln server as a process of its own writing
//! the records it takes to object stores. An S3-compatible store in a container (RustFS unless
//! `KILN_TEST_S3_IMAGE` says: what it takes it takes by its own reading of SigV4, not kiln's);
//! Cloud Storage and Azure Blob Storage stood in for, each taking an object only with a token its
//! token service gave for its scope or audience (a service account's signed assertion; a client's
//! secret), as Google's and Microsoft's do; and an S3 stood in for that fails when asked.
//!
//! What a user relies on: every record goes to the first archive whose search takes it, once; an
//! object is Datadog's (its name, in the folder of its events' hour, its newest event's time in
//! it; its events' shape, tags when asked); kiln's own events are not archived; records come to
//! their archives though nothing compacts the store (it is cut within a while of them); a store
//! away, and the server killed meanwhile, lose nothing and write nothing twice; an archive made,
//! and the order changed, apply to the files cut after.
//!
//! The first needs Docker: `cargo test -p kiln --test observe_archives -- --ignored`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};
use sha2::Digest as _;

/// Archives looked at every half second; records cut two seconds after the first of them waits.
const FAST: [(&str, &str); 2] = [("KILN_OBSERVE_ARCHIVE_EVERY_MS", "500"), ("KILN_OBSERVE_CUT_AFTER_MS", "2000")];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------------------ services stood in for

#[derive(Debug, Clone)]
struct Req {
    method: String,
    /// The path as sent (encoded), without its query.
    path: String,
    query: BTreeMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn bearer(&self) -> &str {
        self.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("")
    }

    fn form(&self) -> BTreeMap<String, String> {
        pairs(&String::from_utf8_lossy(&self.body), true)
    }
}

struct Resp {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn reply(status: u16, v: &Value) -> Resp {
    Resp { status, content_type: "application/json", body: serde_json::to_vec(v).unwrap() }
}

fn xml(status: u16, s: &str) -> Resp {
    Resp { status, content_type: "application/xml", body: s.as_bytes().to_vec() }
}

/// `%XX` decoded; `+` a space when `plus` (a form's).
fn decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(0));
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pairs(s: &str, plus: bool) -> BTreeMap<String, String> {
    s.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((decode(p, plus), String::new()), |(k, v)| (decode(k, plus), decode(v, plus)))).collect()
}

/// RFC 3986's unreserved characters as they are, every other byte `%XX` (`/` kept in a path).
fn enc(s: &str, path: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b'/' if path => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A service on a port of its own, each request to `handle`.
struct Service {
    port: u16,
}

impl Service {
    fn start(handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        Self::start_with(std::net::TcpListener::bind("127.0.0.1:0")?, handle)
    }

    /// A service on `port` (one just found free: its address named before it starts).
    fn start_on(port: u16, handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        Self::start_with(std::net::TcpListener::bind(("127.0.0.1", port))?, handle)
    }

    fn start_with(listener: std::net::TcpListener, handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        let port = listener.local_addr()?.port();
        let handle = Arc::new(handle);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 65536];
                    let head_end = loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let mut lines = head.lines();
                    let mut words = lines.next().unwrap_or("").split_whitespace();
                    let method = words.next().unwrap_or("").to_owned();
                    let target = words.next().unwrap_or("").to_owned();
                    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let mut body = buf[head_end + 4..].to_vec();
                    while body.len() < len {
                        let Ok(n) = s.read(&mut chunk) else { break };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                    let (path, query) = target.split_once('?').map_or((target.clone(), BTreeMap::new()), |(p, q)| (p.to_owned(), pairs(q, false)));
                    let resp = handle(&Req { method, path, query, headers, body });
                    let reason = match resp.status {
                        200 => "OK",
                        201 => "Created",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        404 => "Not Found",
                        503 => "Service Unavailable",
                        _ => "Status",
                    };
                    let head = format!("HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", resp.status, resp.content_type, resp.body.len());
                    let _ = s.write_all(head.as_bytes()).and_then(|()| s.write_all(&resp.body));
                });
            }
        });
        Ok(Self { port })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

// --------------------------------------------------------- Cloud Storage and Blob Storage

const TENANT: &str = "tenant-archives";
const CLIENT: (&str, &str) = ("app-archives", "s3cret-archives");
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const GCS_TOKEN: &str = "gcs-write-token";
const AZURE_TOKEN: &str = "azure-storage-token";
const ACCOUNT: &str = "kilnacct";
const SERVICE_ACCOUNT: &str = "kiln-archives@proj-e2e.iam.gserviceaccount.com";

/// What Google's and Microsoft's stand-ins hold: objects by store and name (`gcs/<bucket>/<name>`,
/// `azure/<account>/<container>/<name>`), each one's writes, and what they refused.
#[derive(Default)]
struct Stores {
    objects: BTreeMap<String, Vec<u8>>,
    writes: BTreeMap<String, usize>,
    refused: Vec<String>,
}

/// Google's token service (a service account's assertion, RFC 7523, signed by its key, for Cloud
/// Storage's scope) and Cloud Storage's media uploads; Microsoft's Entra ID (a client's secret,
/// for Storage's audience) and Blob Storage's block blobs.
fn stores(state: Arc<Mutex<Stores>>, public_key: Vec<u8>, token_uri: String) -> impl Fn(&Req) -> Resp + Send + Sync + 'static {
    move |r: &Req| {
        let mut st = lock(&state);
        if r.path == "/token" {
            let form = r.form();
            let jwt = form.get("assertion").cloned().unwrap_or_default();
            let parts: Vec<&str> = jwt.split('.').collect();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let verified = parts.len() == 3 && {
                let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, &public_key);
                key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64.decode(parts[2]).unwrap_or_default()).is_ok()
            };
            let claims: Value = parts.get(1).and_then(|c| b64.decode(c).ok()).and_then(|c| serde_json::from_slice(&c).ok()).unwrap_or(Value::Null);
            if form.get("grant_type").map(String::as_str) != Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
                || !verified
                || claims["aud"] != token_uri.as_str()
                || claims["iss"] != SERVICE_ACCOUNT
                || claims["scope"] != GCS_SCOPE
            {
                st.refused.push(format!("a Google token: verified {verified}, claims {claims}"));
                return reply(400, &json!({ "error": "invalid_grant", "error_description": "Invalid JWT Signature." }));
            }
            return reply(200, &json!({ "access_token": GCS_TOKEN, "expires_in": 3599, "token_type": "Bearer" }));
        }
        if r.path == format!("/{TENANT}/oauth2/v2.0/token") {
            let form = r.form();
            if form.get("grant_type").map(String::as_str) != Some("client_credentials")
                || form.get("client_id").map(String::as_str) != Some(CLIENT.0)
                || form.get("client_secret").map(String::as_str) != Some(CLIENT.1)
                || form.get("scope").map(String::as_str) != Some("https://storage.azure.com/.default")
            {
                st.refused.push(format!("an Entra token: {form:?}"));
                return reply(400, &json!({ "error": "invalid_request", "error_description": "AADSTS70011: The provided request must include a 'scope' input parameter." }));
            }
            return reply(200, &json!({ "token_type": "Bearer", "expires_in": 3599, "ext_expires_in": 3599, "access_token": AZURE_TOKEN }));
        }
        if let Some(bucket) = r.path.strip_prefix("/upload/storage/v1/b/").and_then(|p| p.strip_suffix("/o")) {
            let name = r.query.get("name").cloned().unwrap_or_default();
            if r.method != "POST"
                || r.bearer() != GCS_TOKEN
                || r.query.get("uploadType").map(String::as_str) != Some("media")
                || name.is_empty()
                || r.header("content-type") != Some("application/gzip")
            {
                st.refused.push(format!("a Cloud Storage upload: {} {} {:?}", r.method, r.path, r.query));
                return reply(401, &json!({ "error": { "code": 401, "message": "Invalid Credentials" } }));
            }
            let key = format!("gcs/{}/{name}", decode(bucket, false));
            st.objects.insert(key.clone(), r.body.clone());
            *st.writes.entry(key).or_default() += 1;
            return reply(200, &json!({ "kind": "storage#object", "bucket": bucket, "name": name, "size": r.body.len().to_string() }));
        }
        if r.method == "PUT" && r.path.starts_with(&format!("/{ACCOUNT}/")) {
            let dated = r.header("x-ms-date").is_some_and(|d| d.ends_with(" GMT") && d.len() == 29);
            if r.bearer() != AZURE_TOKEN || r.header("x-ms-blob-type") != Some("BlockBlob") || r.header("x-ms-version").is_none_or(|v| v < "2017-11-09") || !dated {
                st.refused.push(format!("a blob: {} {:?}", r.path, r.headers));
                return xml(403, "<Error><Code>AuthenticationFailed</Code></Error>");
            }
            let key = format!("azure/{}", decode(r.path.trim_start_matches('/'), false));
            st.objects.insert(key.clone(), r.body.clone());
            *st.writes.entry(key).or_default() += 1;
            return Resp { status: 201, content_type: "application/xml", body: Vec::new() };
        }
        st.refused.push(format!("{} {}", r.method, r.path));
        reply(404, &json!({ "error": "not here" }))
    }
}

fn service_account(token_uri: &str) -> anyhow::Result<(String, Vec<u8>)> {
    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::signature::KeyPair as _;
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|e| anyhow::anyhow!("{e}"))?;
    let der = kp.as_der().map_err(|e| anyhow::anyhow!("{e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n"));
    let key = json!({ "type": "service_account", "project_id": "proj-e2e", "private_key_id": "archives-key-1", "private_key": pem, "client_email": SERVICE_ACCOUNT, "client_id": "4321", "token_uri": token_uri });
    Ok((key.to_string(), kp.public_key().as_ref().to_vec()))
}

// ------------------------------------------------------------------------------------- S3

const S3_IMAGE: &str = "rustfs/rustfs:latest";
const S3_KEYS: (&str, &str) = ("kilnarchives", "kiln-archives-secret-key");

/// An S3-compatible store in a container; read here with requests signed as S3 asks.
struct S3 {
    name: String,
    port: u16,
}

impl S3 {
    fn start() -> anyhow::Result<Self> {
        let port = free_port()?;
        let name = format!("kiln-archives-{}-{port}", std::process::id());
        let image = std::env::var("KILN_TEST_S3_IMAGE").unwrap_or_else(|_| S3_IMAGE.into());
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--rm", "--name", &name, "-p", &format!("127.0.0.1:{port}:9000")]);
        for (k, v) in [("RUSTFS_ACCESS_KEY", S3_KEYS.0), ("RUSTFS_SECRET_KEY", S3_KEYS.1), ("MINIO_ROOT_USER", S3_KEYS.0), ("MINIO_ROOT_PASSWORD", S3_KEYS.1)] {
            c.args(["-e", &format!("{k}={v}")]);
        }
        c.arg(&image);
        if image.contains("minio") {
            c.args(["server", "/data"]);
        }
        let out = c.output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        let s = Self { name, port };
        let start = Instant::now();
        loop {
            if s.call("GET", "/", &[], b"").is_ok_and(|(code, _)| code == 200) {
                return Ok(s);
            }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(90), "the store did not come up:\n{}", s.logs());
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A request signed as S3 asks (kiln's signer; the store checks it by its own reading).
    fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
        use kiln::observe::cloud::sigv4;
        let keys = sigv4::Keys { access_key: S3_KEYS.0.into(), secret: S3_KEYS.1.into(), token: None };
        // `common::raw` sends `Host: 127.0.0.1`: what is signed.
        let headers = [("host", "127.0.0.1")];
        let signed = sigv4::sign(&keys, "us-east-1", "s3", &sigv4::Request { method, path, query, headers: &headers, body }, time::OffsetDateTime::now_utc());
        let qs = query.iter().map(|(k, v)| format!("{}={}", enc(k, false), enc(v, false))).collect::<Vec<_>>().join("&");
        let target = if qs.is_empty() { enc(path, true) } else { format!("{}?{qs}", enc(path, true)) };
        let head: String = signed.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        common::raw(self.port, method, &target, &head, body)
    }

    fn create_bucket(&self, bucket: &str) -> anyhow::Result<()> {
        let (code, body) = self.call("PUT", &format!("/{bucket}"), &[], b"")?;
        anyhow::ensure!(code == 200, "a bucket: {code} {}", String::from_utf8_lossy(&body));
        Ok(())
    }

    /// Every object of `bucket`, by key (ListObjectsV2, page by page; then each read).
    fn objects(&self, bucket: &str) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
        let mut keys = Vec::new();
        let mut token = String::new();
        loop {
            let mut q = vec![("list-type", "2")];
            if !token.is_empty() {
                q.push(("continuation-token", token.as_str()));
            }
            let (code, body) = self.call("GET", &format!("/{bucket}"), &q, b"")?;
            let text = String::from_utf8_lossy(&body).into_owned();
            anyhow::ensure!(code == 200, "listing {bucket}: {code} {text}");
            keys.extend(between(&text, "<Key>", "</Key>").into_iter().map(str::to_owned));
            match between(&text, "<NextContinuationToken>", "</NextContinuationToken>").first() {
                Some(t) if text.contains("<IsTruncated>true</IsTruncated>") => token = (*t).to_owned(),
                _ => break,
            }
        }
        let mut out = BTreeMap::new();
        for k in keys {
            let (code, body) = self.call("GET", &format!("/{bucket}/{k}"), &[], b"")?;
            anyhow::ensure!(code == 200, "{k}: {code}");
            out.insert(k, body);
        }
        Ok(out)
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "40", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for S3 {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    s.split(open).skip(1).filter_map(|p| p.split(close).next()).collect()
}

/// An S3 stood in for: each PUT taken whose payload's hash is the one signed and whose
/// authorization is SigV4's for S3 naming it, but those of the objects it was told to fail: S3's
/// SlowDown (503) until told otherwise.
#[derive(Default)]
struct Flaky {
    objects: BTreeMap<String, Vec<u8>>,
    /// Each key's PUTs taken.
    puts: BTreeMap<String, usize>,
    /// Objects whose keys hold this fail; none: none do.
    failing: Option<String>,
    refused: usize,
    bad: Vec<String>,
}

const FLAKY_KEYS: (&str, &str) = ("AKIDARCHIVES", "flaky-secret");

fn flaky(state: Arc<Mutex<Flaky>>) -> impl Fn(&Req) -> Resp + Send + Sync + 'static {
    move |r: &Req| {
        let mut st = lock(&state);
        let hash = hex::encode(sha2::Sha256::digest(&r.body));
        let authz = r.header("authorization").unwrap_or("");
        let signed = authz.split("SignedHeaders=").nth(1).and_then(|s| s.split(',').next()).unwrap_or("");
        let named = authz.starts_with(&format!("AWS4-HMAC-SHA256 Credential={}/", FLAKY_KEYS.0)) && authz.contains("/us-east-1/s3/aws4_request");
        if r.method != "PUT"
            || r.header("x-amz-content-sha256") != Some(hash.as_str())
            || !named
            || !signed.split(';').any(|h| h == "x-amz-content-sha256")
            || r.header("content-type") != Some("application/gzip")
        {
            st.bad.push(format!("{} {} {authz}", r.method, r.path));
            return xml(400, "<Error><Code>InvalidRequest</Code></Error>");
        }
        let key = decode(r.path.trim_start_matches('/'), false);
        if st.failing.as_deref().is_some_and(|f| key.contains(f)) {
            st.refused += 1;
            return xml(503, "<Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>");
        }
        st.objects.insert(key.clone(), r.body.clone());
        *st.puts.entry(key).or_default() += 1;
        xml(200, "")
    }
}

// --------------------------------------------------------------------------------- records

/// A record sent, its `seq` its own.
#[derive(Debug, Clone)]
struct Sent {
    seq: i64,
    ts: i64,
    service: &'static str,
    status: &'static str,
    host: String,
    env: &'static str,
    code: u16,
    message: String,
}

impl Sent {
    /// Record `seq`, of three hours from `t0` back (each a third of them), a few with what JSON
    /// escapes in their message.
    fn new(seq: i64, t0: i64) -> Self {
        let service = ["web", "api", "db"][(seq / 3 % 3) as usize];
        let status = if seq % 5 == 0 {
            "error"
        } else if seq % 7 == 0 {
            "warn"
        } else {
            "info"
        };
        let code = if status == "error" { 500 } else { 200 };
        let message = if seq % 50 == 0 { format!("line {seq}\n\t\"quoted\" \\ ünï ☃ \u{1}") } else { format!("GET /{service}/{seq} {code}") };
        Self { seq, ts: t0 - (seq % 3) * 3_600_000 - seq * 13, service, status, host: format!("host-{}", seq % 4), env: if seq % 2 == 0 { "prod" } else { "staging" }, code, message }
    }

    fn json(&self) -> Value {
        json!({ "message": self.message, "timestamp": self.ts, "status": self.status, "service": self.service, "hostname": self.host, "source": "app", "attributes": { "seq": self.seq, "env": self.env, "http": { "status_code": self.code, "method": "GET" } } })
    }
}

fn send(f: &Fleet, records: &[Value]) -> anyhow::Result<()> {
    for chunk in records.chunks(500) {
        let start = Instant::now();
        loop {
            let (code, b) = f.request("POST", "/api/observe/v1/logs", Some(&Value::from(chunk.to_vec())))?;
            if code == 202 {
                break;
            }
            anyhow::ensure!(code == 503 && start.elapsed() < Duration::from_secs(30), "the intake: {code} {}", String::from_utf8_lossy(&b));
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(())
}

/// Waits until `done`, or fails saying `what` with the fleet's logs.
fn until(f: &Fleet, what: &str, within: Duration, done: &dyn Fn() -> bool) -> anyhow::Result<()> {
    let start = Instant::now();
    while !done() {
        anyhow::ensure!(start.elapsed() < within, "{what}: never\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// A call answered 2xx: its body as JSON (null if none).
fn ok(f: &Fleet, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let (code, b) = f.request(method, path, body)?;
    anyhow::ensure!((200..300).contains(&code), "{method} {path}: {code} {}", String::from_utf8_lossy(&b));
    Ok(if b.is_empty() { Value::Null } else { serde_json::from_slice(&b)? })
}

fn refused(f: &Fleet, method: &str, path: &str, body: &Value) -> anyhow::Result<(u16, String)> {
    let (code, b) = f.request(method, path, Some(body))?;
    Ok((code, String::from_utf8_lossy(&b).into_owned()))
}

fn events_of(gz: &[u8]) -> Vec<Value> {
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz).read_to_string(&mut text).unwrap();
    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn rfc3339_ms(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap();
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second(), t.millisecond())
}

/// An object as Datadog's archives hold one: `<path>/dt=<YYYYMMDD>/hour=<HH>/archive_<HHmmss>.<SSSS>.<uuid>.json.gz`,
/// every event of it of that day and hour, the name's time its newest event's.
fn check_object(path: &str, key: &str, events: &[Value]) -> anyhow::Result<()> {
    let rest = key.strip_prefix(&format!("{path}/")).ok_or_else(|| anyhow::anyhow!("{key} is not under {path}/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    anyhow::ensure!(parts.len() == 3, "{key}: three parts past its path");
    let day = parts[0].strip_prefix("dt=").ok_or_else(|| anyhow::anyhow!("{key}: dt="))?;
    let hour = parts[1].strip_prefix("hour=").ok_or_else(|| anyhow::anyhow!("{key}: hour="))?;
    let name = parts[2].strip_prefix("archive_").and_then(|n| n.strip_suffix(".json.gz")).ok_or_else(|| anyhow::anyhow!("{key}: archive_….json.gz"))?;
    anyhow::ensure!(name.len() == 11 + 1 + 36, "{key}: HHmmss.SSSS.<uuid>");
    let (time, uuid) = (&name[..11], &name[12..]);
    anyhow::ensure!(uuid.as_bytes()[14] == b'8' && uuid.split('-').map(str::len).collect::<Vec<_>>() == [8, 4, 4, 4, 12], "{key}: a UUID of version 8");
    anyhow::ensure!(!events.is_empty(), "{key}: no events");
    let mut newest = String::new();
    for e in events {
        let date = e["date"].as_str().unwrap_or("");
        anyhow::ensure!(date.len() == 24 && date.ends_with('Z'), "{key}: a date {date}");
        anyhow::ensure!(format!("{}{}{}", &date[0..4], &date[5..7], &date[8..10]) == day && &date[11..13] == hour, "{key}: an event of {date}");
        newest = newest.max(date.to_owned());
    }
    let want = format!("{}{}{}.{}0", &newest[11..13], &newest[14..16], &newest[17..19], &newest[20..23]);
    anyhow::ensure!(time == want, "{key}: named {time}, its newest event {newest}");
    Ok(())
}

/// Each event read against the record it is of: Datadog's reserved attributes at the top, the rest
/// under `attributes` (dotted ones as objects), tags (the record's unified service tags) when its
/// archive asks, an `_id` of its time. The event's `seq`.
fn check_event(e: &Value, sent: &BTreeMap<i64, Sent>, tags: bool) -> anyhow::Result<i64> {
    let seq: i64 = e["attributes"]["seq"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("no seq: {e}"))?;
    let r = sent.get(&seq).ok_or_else(|| anyhow::anyhow!("seq {seq} was not sent"))?;
    anyhow::ensure!(e["date"] == rfc3339_ms(r.ts).as_str(), "seq {seq}: date {} for {}", e["date"], r.ts);
    anyhow::ensure!(e["message"] == r.message.as_str() && e["service"] == r.service && e["host"] == r.host.as_str() && e["source"] == "app" && e["status"] == r.status, "seq {seq}: {e}");
    let a = &e["attributes"];
    anyhow::ensure!(a["http"] == json!({ "status_code": r.code.to_string(), "method": "GET" }) && a["env"] == r.env, "seq {seq}'s attributes: {a}");
    let want_tags = if tags { json!([format!("service:{}", r.service), format!("host:{}", r.host), "source:app", format!("env:{}", r.env)]) } else { json!([]) };
    anyhow::ensure!(e["tags"] == want_tags, "seq {seq}'s tags: {}", e["tags"]);
    let id = base64::engine::general_purpose::STANDARD.decode(e["_id"].as_str().unwrap_or(""))?;
    anyhow::ensure!(id.len() == 18, "seq {seq}: an _id of 18 bytes");
    let ms = id[..6].iter().fold(0i64, |n, b| (n << 8) | i64::from(*b));
    anyhow::ensure!(ms == r.ts, "seq {seq}: its _id's time {ms}, its own {}", r.ts);
    Ok(seq)
}

// ----------------------------------------------------------------------------------- scenarios

/// Three archives in order (errors to Cloud Storage, `service:web` to Blob Storage with tags, the
/// rest to S3): every record sent goes to the first whose search takes it, once; each object is
/// Datadog's, its events their records; none of kiln's own events; though nothing compacts the
/// store. The API says each is working, what it wrote, and kiln's metrics count it alike.
#[test]
#[ignore = "starts a kiln server as a process, an S3-compatible store in Docker, and services that stand in for Google's and Microsoft's"]
fn every_record_goes_once_to_the_first_archive_that_takes_it_written_as_datadog_writes_them() -> anyhow::Result<()> {
    let s3 = S3::start()?;
    s3.create_bucket("kiln-archive")?;
    let port = free_port()?;
    let token_uri = format!("http://127.0.0.1:{port}/token");
    let (key, public) = service_account(&token_uri)?;
    let held = Arc::new(Mutex::new(Stores::default()));
    let svc = Service::start_on(port, stores(held.clone(), public, token_uri))?;
    let f = Fleet::with_env(|_| String::new(), &FAST)?;
    let started = now_ms();

    // Cloud integrations that only write: paused, they read nothing.
    ok(&f, "PUT", "/api/observe/v1/clouds/store", Some(&json!({ "kind": "aws", "access_key_id": S3_KEYS.0, "secret_access_key": S3_KEYS.1, "paused": true })))?;
    ok(&f, "PUT", "/api/observe/v1/clouds/google", Some(&json!({ "kind": "gcp", "service_account": key, "paused": true })))?;
    ok(
        &f,
        "PUT",
        "/api/observe/v1/clouds/microsoft",
        Some(&json!({ "kind": "azure", "tenant_id": TENANT, "client_id": CLIENT.0, "client_secret": CLIENT.1, "authority": svc.url(), "paused": true })),
    )?;

    // What is refused, saying why: a cloud of the wrong kind, one not there, a search that does not
    // parse.
    let gcs_with = |cloud: &str| json!({ "name": "x", "destination": { "type": "gcs", "bucket": "b", "integration": { "cloud": cloud } } });
    for (body, says) in [
        (gcs_with("store"), "aws integration"),
        (gcs_with("nowhere"), "no cloud integration"),
        (json!({ "name": "x", "query": "(", "destination": { "type": "gcs", "bucket": "b", "integration": { "cloud": "google" } } }), "its query"),
    ] {
        let (code, text) = refused(&f, "POST", "/api/observe/v1/archives", &body)?;
        anyhow::ensure!(code == 400 && text.contains(says), "{body}: {code} {text}");
    }

    // Errors to Cloud Storage (made as Datadog's API makes one), the web's to Blob Storage with
    // tags, the rest to S3.
    let errors = ok(
        &f,
        "POST",
        "/api/observe/v1/archives",
        Some(&json!({ "data": { "type": "archives", "attributes": {
        "name": "errors", "query": "status:error", "include_tags": false, "rehydration_tags": ["team:obs"],
        "destination": { "type": "gcs", "bucket": "kiln-errors", "path": "/errors", "integration": { "cloud": "google", "client_email": SERVICE_ACCOUNT, "project_id": "proj-e2e" }, "endpoint": svc.url() } } } })),
    )?;
    let web = ok(
        &f,
        "POST",
        "/api/observe/v1/archives",
        Some(&json!({ "name": "web", "query": "service:web", "include_tags": true,
        "destination": { "type": "azure", "storage_account": ACCOUNT, "container": "logs", "path": "web", "integration": { "cloud": "microsoft" }, "endpoint": format!("{}/{ACCOUNT}", svc.url()) } })),
    )?;
    let rest = ok(
        &f,
        "POST",
        "/api/observe/v1/archives",
        Some(&json!({ "name": "rest",
        "destination": { "type": "s3", "bucket": "kiln-archive", "path": "/all/logs/", "region": "us-east-1", "storage_class": "STANDARD", "integration": { "cloud": "store" }, "endpoint": s3.url() } })),
    )?;
    anyhow::ensure!(
        errors["destination"]["integration"]["client_email"] == SERVICE_ACCOUNT && errors["rehydration_tags"] == json!(["team:obs"]) && errors["state"] == "UNKNOWN",
        "Datadog's fields kept: {errors}"
    );
    let ids: Vec<String> = [&errors, &web, &rest].iter().map(|a| a["id"].as_str().unwrap_or("").to_owned()).collect();
    anyhow::ensure!(ok(&f, "GET", "/api/observe/v1/archive_order", None)?["archive_ids"] == json!(ids), "the order they were made in");
    let (code, text) = refused(&f, "POST", "/api/observe/v1/archives", &json!({ "name": "web", "destination": { "type": "gcs", "bucket": "b", "integration": { "cloud": "google" } } }))?;
    anyhow::ensure!(code == 409, "a name taken: {code} {text}");

    // Six hundred records over three hours, and kiln's own events among them.
    let t0 = now_ms() - 60_000;
    let sent: BTreeMap<i64, Sent> = (0..600).map(|i| (i, Sent::new(i, t0))).collect();
    let mut body: Vec<Value> = sent.values().map(Sent::json).collect();
    body.extend((0..5).map(|i| json!({ "message": format!("kiln's own event {i}"), "timestamp": t0, "status": "error", "service": "web", "hostname": "kiln", "source": "kiln.event", "attributes": { "seq": 10_000 + i } })));
    send(&f, &body)?;
    let first = |r: &Sent| {
        if r.status == "error" {
            0
        } else if r.service == "web" {
            1
        } else {
            2
        }
    };
    let want: Vec<BTreeSet<i64>> = (0..3).map(|a| sent.values().filter(|r| first(r) == a).map(|r| r.seq).collect()).collect();

    // Each archive's objects, by key under its bucket or container.
    let read = || -> anyhow::Result<Vec<BTreeMap<String, Vec<u8>>>> {
        let st = lock(&held);
        let of = |prefix: &str| st.objects.iter().filter_map(|(k, v)| k.strip_prefix(prefix).map(|k| (k.to_owned(), v.clone()))).collect::<BTreeMap<_, _>>();
        let (g, a) = (of("gcs/kiln-errors/"), of(&format!("azure/{ACCOUNT}/logs/")));
        drop(st);
        Ok(vec![g, a, s3.objects("kiln-archive")?])
    };
    let seqs = |objects: &BTreeMap<String, Vec<u8>>| -> Vec<i64> { objects.values().flat_map(|b| events_of(b)).filter_map(|e| e["attributes"]["seq"].as_str().and_then(|s| s.parse().ok())).collect() };
    let start = Instant::now();
    let objects = loop {
        let objects = read()?;
        let counts: Vec<usize> = objects.iter().map(|o| seqs(o).len()).collect();
        if counts.iter().zip(&want).all(|(n, w)| *n >= w.len()) {
            break objects;
        }
        anyhow::ensure!(
            start.elapsed() < Duration::from_secs(120),
            "the archives hold {counts:?} of {:?}; refused {:?}\n{}",
            want.iter().map(BTreeSet::len).collect::<Vec<_>>(),
            lock(&held).refused,
            f.logs()
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    // A few rounds more: nothing more comes.
    std::thread::sleep(Duration::from_secs(4));
    let again = read()?;
    anyhow::ensure!(
        again.iter().map(|o| o.keys().cloned().collect::<Vec<_>>()).collect::<Vec<_>>() == objects.iter().map(|o| o.keys().cloned().collect::<Vec<_>>()).collect::<Vec<_>>(),
        "objects came after every record had"
    );

    // Every record in the first archive that takes it, once; each object Datadog's.
    let paths = ["errors", "web", "all/logs"];
    let mut all_ids = BTreeSet::new();
    for (a, objects) in objects.iter().enumerate() {
        let mut got = Vec::new();
        for (key, body) in objects {
            let events = events_of(body);
            check_object(paths[a], key, &events)?;
            for e in &events {
                anyhow::ensure!(e["source"] != "kiln.event", "kiln's own event archived: {e}");
                got.push(check_event(e, &sent, a == 1)?);
                anyhow::ensure!(all_ids.insert(e["_id"].as_str().unwrap_or("").to_owned()), "an _id twice: {e}");
            }
        }
        let distinct: BTreeSet<i64> = got.iter().copied().collect();
        anyhow::ensure!(distinct.len() == got.len(), "archive {} holds a record twice", paths[a]);
        anyhow::ensure!(distinct == want[a], "archive {} holds {} records, wanted {}", paths[a], distinct.len(), want[a].len());
    }
    let st = lock(&held);
    anyhow::ensure!(st.refused.is_empty(), "the stand-ins refused: {:?}", st.refused);
    anyhow::ensure!(st.writes.values().all(|n| *n == 1), "an object written twice: {:?}", st.writes.iter().filter(|(_, n)| **n > 1).collect::<Vec<_>>());
    drop(st);

    // The API: each working, what it wrote, nothing left to write.
    let start = Instant::now();
    loop {
        let list = ok(&f, "GET", "/api/observe/v1/archives", None)?;
        let archives = list["archives"].as_array().cloned().unwrap_or_default();
        let good = archives.len() == 3
            && archives
                .iter()
                .zip(&want)
                .all(|(a, w)| a["state"] == "WORKING" && a["status"]["events"].as_u64() == Some(w.len() as u64) && a["status"]["pending"] == 0 && a["status"]["error"].is_null());
        if good {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "the archives as the API says: {list}");
        std::thread::sleep(Duration::from_millis(500));
    }
    // kiln's count of what each wrote: kiln.archives.events, by archive.
    let q =
        ok(&f, "POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:kiln.archives.events{*} by {archive}.as_count()", "from": started - 60_000, "to": now_ms() + 60_000, "step": 10_000 })))?;
    let counted: BTreeMap<String, f64> = q["series"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| (at(s, &["tags", "archive"]).as_str().unwrap_or("").to_owned(), at(s, &["values"]).as_array().into_iter().flatten().filter_map(Value::as_f64).sum()))
        .collect();
    for (name, w) in ["errors", "web", "rest"].iter().zip(&want) {
        anyhow::ensure!(counted.get(*name).copied() == Some(w.len() as f64), "kiln.archives.events of {name}: {counted:?}");
    }
    Ok(())
}

/// One archive, its store failing part way through a file: the parts written stay written (none
/// written again), the file after it is written while the archive waits on the one it failed, the
/// archive says it is failing (an event says so, not archived); the server killed meanwhile, and a
/// second archive made first in order, every record is written once: those of the files begun
/// before to the first archive, those cut after to the second.
#[test]
#[ignore = "starts a kiln server as a process, and an S3 stood in for that fails"]
fn a_store_away_and_the_server_killed_lose_nothing_and_write_nothing_twice() -> anyhow::Result<()> {
    let st = Arc::new(Mutex::new(Flaky::default()));
    let store = Service::start(flaky(st.clone()))?;
    let mut f = Fleet::with_env(|_| String::new(), &FAST)?;
    ok(&f, "PUT", "/api/observe/v1/clouds/s3", Some(&json!({ "kind": "aws", "access_key_id": FLAKY_KEYS.0, "secret_access_key": FLAKY_KEYS.1, "paused": true })))?;
    let s3 = |name: &str, bucket: &str, path: &str| json!({ "name": name, "query": "*", "destination": { "type": "s3", "bucket": bucket, "path": path, "region": "us-east-1", "integration": { "cloud": "s3" }, "endpoint": store.url() } });
    let first = ok(&f, "POST", "/api/observe/v1/archives", Some(&s3("first", "first", "a")))?;
    let first_id = first["id"].as_str().unwrap_or("").to_owned();
    let t0 = now_ms() - 60_000;
    let mut sent: BTreeMap<i64, Sent> = (0..900).map(|i| (i, Sent::new(i, t0))).collect();
    // Records 900 to 949: of the newest hour alone.
    sent.extend((900..950).map(|i| (i, Sent { ts: t0 - (i - 900) * 7, ..Sent::new(i, t0) })));
    let batch = |r: std::ops::Range<i64>| r.map(|i| sent[&i].json()).collect::<Vec<Value>>();
    // The seqs each bucket holds, and its objects' keys.
    let held = |bucket: &str| -> (Vec<i64>, Vec<String>) {
        let st = lock(&st);
        let objects: Vec<(&String, &Vec<u8>)> = st.objects.iter().filter(|(k, _)| k.starts_with(&format!("{bucket}/"))).collect();
        let seqs = objects.iter().flat_map(|(_, b)| events_of(b)).filter_map(|e| e["attributes"]["seq"].as_str().and_then(|s| s.parse().ok())).collect();
        (seqs, objects.iter().map(|(k, _)| (*k).clone()).collect())
    };

    // A first batch, archived whole.
    send(&f, &batch(0..300))?;
    until(&f, "the first batch archived", Duration::from_secs(60), &|| held("first").0.len() >= 300)?;

    // The store fails the objects of the second batch's middle hour (its file is a part an hour,
    // three hours of records, written oldest first): that file is written in part.
    let date = rfc3339_ms(t0 - 3_600_000);
    let folder = format!("dt={}{}{}/hour={}", &date[0..4], &date[5..7], &date[8..10], &date[11..13]);
    anyhow::ensure!((900..950).all(|i| !rfc3339_ms(sent[&i].ts).starts_with(&date[..13])), "the newest hour's records are of another hour than the middle one");
    lock(&st).failing = Some(folder);
    send(&f, &batch(300..600))?;
    until(&f, "the archive failing", Duration::from_secs(90), &|| {
        ok(&f, "GET", &format!("/api/observe/v1/archives/{first_id}"), None).is_ok_and(|a| a["state"] == "FAILING" && a["status"]["error"].as_str().is_some_and(|e| e.contains("503")))
    })?;
    let (seqs, _) = held("first");
    anyhow::ensure!(seqs.len() > 300 && seqs.len() < 600 && lock(&st).refused > 0, "a part of the second batch written before the store failed: {} records", seqs.len());
    let said = |result: &str| {
        ok(
            &f,
            "POST",
            "/api/observe/v1/logs/search",
            Some(&json!({ "query": format!("source:kiln.event @event.type:archive.write @result:{result} @archive:first"), "from": t0 - 60_000, "to": now_ms() + 60_000, "limit": 5 })),
        )
        .map(|v| v["records"].as_array().map_or(0, Vec::len))
        .unwrap_or(0)
    };
    until(&f, "an event saying it is failing", Duration::from_secs(30), &|| said("failing") == 1)?;

    // A file after it (cut apart: the newest hour's records alone) is written beside the one it
    // failed, as it is tried again, and the archive stays where it was.
    std::thread::sleep(Duration::from_secs(3));
    send(&f, &batch(900..950))?;
    until(&f, "the file after it written while the archive fails", Duration::from_secs(90), &|| {
        let seqs: BTreeSet<i64> = held("first").0.into_iter().collect();
        (900..950).all(|i| seqs.contains(&i))
    })?;
    let a = ok(&f, "GET", &format!("/api/observe/v1/archives/{first_id}"), None)?;
    anyhow::ensure!(a["state"] == "FAILING" && a["status"]["pending"].as_u64().is_some_and(|n| n >= 2), "still failing, both files pending: {a}");

    // A second archive, made first in order (as Datadog's API orders them): the files begun keep
    // their routing.
    let second = ok(&f, "POST", "/api/observe/v1/archives", Some(&s3("second", "second", "b")))?;
    let second_id = second["id"].as_str().unwrap_or("").to_owned();
    let order = ok(&f, "PUT", "/api/observe/v1/archive_order", Some(&json!({ "data": { "type": "archive_order", "attributes": { "archive_ids": [second_id, first_id] } } })))?;
    anyhow::ensure!(order["archive_ids"] == json!([second_id, first_id]), "the order: {order}");

    // The server killed; the store back; the server again, and a third batch.
    f.kill_server();
    lock(&st).failing = None;
    f.start_server()?;
    send(&f, &batch(600..900))?;
    until(&f, "every batch archived", Duration::from_secs(120), &|| held("first").0.len() >= 650 && held("second").0.len() >= 300)?;
    std::thread::sleep(Duration::from_secs(4));

    let (first_seqs, _) = held("first");
    let (second_seqs, _) = held("second");
    let once = |seqs: &[i64]| seqs.iter().copied().collect::<BTreeSet<i64>>().len() == seqs.len();
    anyhow::ensure!(once(&first_seqs) && once(&second_seqs), "a record written twice");
    anyhow::ensure!(first_seqs.iter().copied().collect::<BTreeSet<_>>() == (0..600).chain(900..950).collect(), "the first archive: the batches before the second archive, every record");
    anyhow::ensure!(second_seqs.iter().copied().collect::<BTreeSet<_>>() == (600..900).collect(), "the second archive: the third batch, every record");
    let st = lock(&st);
    anyhow::ensure!(st.bad.is_empty(), "requests not signed as S3 asks: {:?}", st.bad);
    anyhow::ensure!(st.puts.values().all(|n| *n == 1), "an object written again: {:?}", st.puts.iter().filter(|(_, n)| **n > 1).collect::<Vec<_>>());
    for (key, body) in &st.objects {
        let (bucket, rest) = key.split_once('/').unwrap_or_default();
        let path = if bucket == "first" { "a" } else { "b" };
        check_object(path, rest, &events_of(body))?;
    }
    drop(st);
    let list = ok(&f, "GET", "/api/observe/v1/archives", None)?;
    anyhow::ensure!(list["archives"].as_array().is_some_and(|a| a.iter().all(|x| x["state"] == "WORKING")), "both working again: {list}");
    Ok(())
}
