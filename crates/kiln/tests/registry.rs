//! kiln's OCI registry (docs/ci.md W12-R01, W12-AC01), spoken to as the OCI distribution-spec
//! conformance suite speaks to a registry, in its four parts:
//! - **pull**: manifests by tag and digest, blobs, ranges, what isn't there;
//! - **push**: monolithic and chunked uploads (a chunk out of order refused), a wrong digest
//!   refused, mounts from another repository, a manifest whose blobs aren't there refused;
//! - **content discovery**: tags in pages, the referrers API (and its filter), the catalog;
//! - **content management**: deleting a tag, a manifest and a blob.
//!
//! And its auth: the challenge, Docker's token flow, anonymous pull where it's allowed.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;
use common::{Options, start};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// A response: its status, headers (names lowercase) and body.
struct Got {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Got {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn code(&self) -> String {
        self.json().pointer("/errors/0/code").and_then(Value::as_str).unwrap_or_default().to_owned()
    }
}

fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<Got> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no response head"))?;
    let (h, rest) = raw.split_at(split);
    let text = String::from_utf8_lossy(h).into_owned();
    let mut lines = text.lines();
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect::<HashMap<_, _>>();
    let mut body = rest.get(4..).unwrap_or_default().to_vec();
    // Chunked bodies (no Content-Length): joined.
    if headers.get("transfer-encoding").is_some_and(|t| t.contains("chunked")) {
        let mut out = Vec::new();
        let mut at = 0usize;
        while let Some(end) = body.get(at..).and_then(|b| b.windows(2).position(|w| w == b"\r\n")) {
            let size = usize::from_str_radix(String::from_utf8_lossy(body.get(at..at + end).unwrap_or_default()).trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            out.extend_from_slice(body.get(at + end + 2..at + end + 2 + size).unwrap_or_default());
            at += end + 2 + size + 2;
        }
        body = out;
    }
    Ok(Got { status, headers, body })
}

fn digest(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// An image manifest of `config` and `layers` (and maybe a subject and an artifact type).
fn manifest(config: &[u8], layers: &[&[u8]], subject: Option<(&str, usize)>, artifact_type: Option<&str>) -> Vec<u8> {
    let mut m = json!({
        "schemaVersion": 2, "mediaType": MANIFEST,
        "config": { "mediaType": artifact_type.map_or("application/vnd.oci.image.config.v1+json", |_| "application/vnd.oci.empty.v1+json"), "digest": digest(config), "size": config.len() },
        "layers": layers.iter().map(|l| json!({ "mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest(l), "size": l.len() })).collect::<Vec<_>>(),
    });
    if let Some(o) = m.as_object_mut() {
        if let Some((d, n)) = subject {
            o.insert("subject".into(), json!({ "mediaType": MANIFEST, "digest": d, "size": n }));
        }
        if let Some(t) = artifact_type {
            o.insert("artifactType".into(), json!(t));
            o.insert("annotations".into(), json!({ "org.example": "yes" }));
        }
    }
    serde_json::to_vec(&m).unwrap_or_default()
}

#[test]
fn w12_ac01_conformance() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { extra: "[ci.registry]\nanonymous_pull = [\"acme/public\"]\n", ..Options::default() })?;
    let port = s.port;

    // The challenge, then Docker's token flow: Basic with a kiln token as the password.
    let g = send(port, "GET", "/v2/", &[], b"")?;
    assert_eq!(g.status, 401);
    assert!(g.header("www-authenticate").starts_with("Bearer realm=") && g.header("www-authenticate").contains("/v2/token"), "{:?}", g.headers);
    assert_eq!(g.header("docker-distribution-api-version"), "registry/2.0");
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", s.admin)));
    let g = send(port, "GET", "/v2/token?service=kiln&scope=repository:acme/app:pull,push,delete&scope=repository:acme/other:pull,push&scope=repository:acme/public:pull,push", &[("Authorization", &basic)], b"")?;
    assert_eq!(g.status, 200, "{}", String::from_utf8_lossy(&g.body));
    let token = g.json().get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let auth = format!("Bearer {token}");
    let a = [("Authorization", auth.as_str())];
    assert_eq!(send(port, "GET", "/v2/", &a, b"")?.status, 200);
    let bad = send(port, "GET", "/v2/acme/app/tags/list", &[("Authorization", "Bearer kreg.e30.00")], b"")?;
    assert_eq!((bad.status, bad.code()), (401, "UNAUTHORIZED".to_owned()), "a forged token");
    // OAuth2's form, which containerd and BuildKit ask with first: a password grant gets the same
    // token; a refresh grant (kiln gives no refresh tokens) is refused as they expect, so they ask
    // with GET; wrong credentials get nothing.
    let form = [("Content-Type", "application/x-www-form-urlencoded")];
    let g = send(port, "POST", "/v2/token", &form, format!("grant_type=password&service=kiln&client_id=containerd-client&username=admin&password={}&scope=repository%3Aacme%2Fapp%3Apull%2Cpush", s.admin).as_bytes())?;
    assert_eq!(g.status, 200, "{}", String::from_utf8_lossy(&g.body));
    assert_eq!(g.json().get("scope").and_then(Value::as_str), Some("repository:acme/app:pull,push"), "{}", String::from_utf8_lossy(&g.body));
    let oauth = format!("Bearer {}", g.json().get("access_token").and_then(Value::as_str).unwrap_or_default());
    assert_eq!(send(port, "GET", "/v2/acme/app/tags/list", &[("Authorization", oauth.as_str())], b"")?.status, 404, "the token works (no such repository yet)");
    assert_eq!(send(port, "POST", "/v2/token", &form, b"grant_type=refresh_token&refresh_token=x&service=kiln")?.status, 400);
    assert_eq!(send(port, "POST", "/v2/token", &form, b"grant_type=password&username=admin&password=wrong&service=kiln")?.status, 401);

    // Push, monolithic: a config and a layer.
    let (config, layer) = (br#"{"architecture":"amd64","os":"linux"}"#.to_vec(), b"layer one\n".repeat(1000));
    for blob in [&config, &layer] {
        let g = send(port, "POST", &format!("/v2/acme/app/blobs/uploads/?digest={}", digest(blob)), &[a[0], ("Content-Type", "application/octet-stream")], blob)?;
        assert_eq!(g.status, 201, "{}", String::from_utf8_lossy(&g.body));
        assert_eq!(g.header("location"), format!("/v2/acme/app/blobs/{}", digest(blob)));
    }
    // A wrong digest is refused.
    let g = send(port, "POST", &format!("/v2/acme/app/blobs/uploads/?digest={}", digest(b"other")), &a, b"not other")?;
    assert_eq!((g.status, g.code()), (400, "DIGEST_INVALID".to_owned()));

    // Push, chunked: a chunk out of order refused, the rest in order, then the digest.
    let big = b"0123456789".repeat(300_000);
    let g = send(port, "POST", "/v2/acme/app/blobs/uploads/", &a, b"")?;
    assert_eq!(g.status, 202);
    let loc = g.header("location").to_owned();
    assert!(loc.starts_with("/v2/acme/app/blobs/uploads/") && g.header("range") == "0-0" && !g.header("docker-upload-uuid").is_empty(), "{:?}", g.headers);
    let (first, second) = big.split_at(1_000_000);
    let g = send(port, "PATCH", &loc, &[a[0], ("Content-Type", "application/octet-stream"), ("Content-Range", "0-999999")], first)?;
    assert_eq!((g.status, g.header("range")), (202, "0-999999"), "{:?}", g.headers);
    let g = send(port, "PATCH", &loc, &[a[0], ("Content-Range", "5-10")], b"oops!!")?;
    assert_eq!(g.status, 416, "a chunk that doesn't follow");
    let g = send(port, "GET", &loc, &a, b"")?;
    assert_eq!((g.status, g.header("range")), (204, "0-999999"));
    let g = send(port, "PUT", &format!("{loc}?digest={}", digest(&big)), &[a[0], ("Content-Type", "application/octet-stream")], second)?;
    assert_eq!(g.status, 201, "{}", String::from_utf8_lossy(&g.body));
    assert_eq!(g.header("docker-content-digest"), digest(&big));
    assert_eq!(send(port, "GET", &loc, &a, b"")?.status, 404, "a finished upload is gone");

    // A manifest whose blob isn't there is refused; with its blobs, it's taken by tag.
    let missing = manifest(&config, &[b"never pushed"], None, None);
    let g = send(port, "PUT", "/v2/acme/app/manifests/broken", &[a[0], ("Content-Type", MANIFEST)], &missing)?;
    assert_eq!((g.status, g.code()), (400, "MANIFEST_BLOB_UNKNOWN".to_owned()));
    let m = manifest(&config, &[&layer, &big], None, None);
    let md = digest(&m);
    let g = send(port, "PUT", "/v2/acme/app/manifests/v1", &[a[0], ("Content-Type", MANIFEST)], &m)?;
    assert_eq!((g.status, g.header("docker-content-digest")), (201, md.as_str()), "{}", String::from_utf8_lossy(&g.body));
    assert_eq!(send(port, "PUT", &format!("/v2/acme/app/manifests/{md}"), &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201, "by its digest");
    let g = send(port, "PUT", &format!("/v2/acme/app/manifests/{}", digest(b"x")), &[a[0], ("Content-Type", MANIFEST)], &m)?;
    assert_eq!((g.status, g.code()), (400, "DIGEST_INVALID".to_owned()), "a digest that isn't the manifest's");
    for tag in ["v2", "latest", "a1"] {
        assert_eq!(send(port, "PUT", &format!("/v2/acme/app/manifests/{tag}"), &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201);
    }

    // Pull: by tag and digest, HEAD, blobs, a range; what isn't there.
    for reference in ["v1", md.as_str()] {
        let g = send(port, "GET", &format!("/v2/acme/app/manifests/{reference}"), &[a[0], ("Accept", MANIFEST)], b"")?;
        assert_eq!((g.status, g.header("content-type"), g.header("docker-content-digest")), (200, MANIFEST, md.as_str()));
        assert_eq!(g.body, m);
    }
    let g = send(port, "HEAD", "/v2/acme/app/manifests/v1", &a, b"")?;
    assert_eq!((g.status, g.header("content-length").parse::<usize>().unwrap_or(0)), (200, m.len()));
    let g = send(port, "GET", &format!("/v2/acme/app/blobs/{}", digest(&big)), &a, b"")?;
    assert!(g.status == 200 && g.body == big, "the chunked blob, whole");
    let g = send(port, "GET", &format!("/v2/acme/app/blobs/{}", digest(&layer)), &[a[0], ("Range", "bytes=10-19")], b"")?;
    assert_eq!((g.status, g.body.as_slice()), (206, layer.get(10..20).unwrap_or_default()));
    let g = send(port, "HEAD", &format!("/v2/acme/app/blobs/{}", digest(&config)), &a, b"")?;
    assert_eq!((g.status, g.header("docker-content-digest")), (200, digest(&config).as_str()));
    let g = send(port, "GET", &format!("/v2/acme/app/blobs/{}", digest(b"none")), &a, b"")?;
    assert_eq!((g.status, g.code()), (404, "BLOB_UNKNOWN".to_owned()));
    let g = send(port, "GET", "/v2/acme/app/manifests/nope", &a, b"")?;
    assert_eq!((g.status, g.code()), (404, "MANIFEST_UNKNOWN".to_owned()));
    // Another repository hasn't got the blob until it's mounted there.
    assert_eq!(send(port, "GET", &format!("/v2/acme/other/blobs/{}", digest(&layer)), &a, b"")?.status, 404);
    let g = send(port, "POST", &format!("/v2/acme/other/blobs/uploads/?mount={}&from=acme/app", digest(&layer)), &a, b"")?;
    assert_eq!(g.status, 201, "a mount");
    assert_eq!(send(port, "GET", &format!("/v2/acme/other/blobs/{}", digest(&layer)), &a, b"")?.status, 200);
    let g = send(port, "POST", &format!("/v2/acme/other/blobs/uploads/?mount={}&from=acme/app", digest(b"nothing")), &a, b"")?;
    assert_eq!(g.status, 202, "nothing to mount: an upload starts");

    // Content discovery: tags in pages, referrers and their filter, the catalog.
    let g = send(port, "GET", "/v2/acme/app/tags/list", &a, b"")?;
    assert_eq!(g.json(), json!({ "name": "acme/app", "tags": ["a1", "latest", "v1", "v2"] }));
    let g = send(port, "GET", "/v2/acme/app/tags/list?n=2", &a, b"")?;
    assert_eq!(g.json().get("tags"), Some(&json!(["a1", "latest"])));
    assert!(g.header("link").contains("last=latest") && g.header("link").contains("rel=\"next\""), "{:?}", g.headers);
    let g = send(port, "GET", "/v2/acme/app/tags/list?n=2&last=latest", &a, b"")?;
    assert_eq!(g.json().get("tags"), Some(&json!(["v1", "v2"])));
    let empty = b"{}".to_vec();
    assert_eq!(send(port, "POST", &format!("/v2/acme/app/blobs/uploads/?digest={}", digest(&empty)), &a, &empty)?.status, 201);
    let sig = manifest(&empty, &[&config], Some((&md, m.len())), Some("application/vnd.example.signature"));
    let sbom = manifest(&empty, &[&config], Some((&md, m.len())), Some("application/vnd.example.sbom"));
    for r in [&sig, &sbom] {
        let g = send(port, "PUT", &format!("/v2/acme/app/manifests/{}", digest(r)), &[a[0], ("Content-Type", MANIFEST)], r)?;
        assert_eq!((g.status, g.header("oci-subject")), (201, md.as_str()), "{}", String::from_utf8_lossy(&g.body));
    }
    let g = send(port, "GET", &format!("/v2/acme/app/referrers/{md}"), &a, b"")?;
    let refs = g.json();
    assert_eq!((g.status, g.header("content-type")), (200, "application/vnd.oci.image.index.v1+json"));
    assert_eq!(refs.get("manifests").and_then(Value::as_array).map(Vec::len), Some(2), "{refs}");
    assert_eq!(refs.pointer("/manifests/0/annotations/org.example"), Some(&json!("yes")));
    let g = send(port, "GET", &format!("/v2/acme/app/referrers/{md}?artifactType=application/vnd.example.sbom"), &a, b"")?;
    assert_eq!((g.header("oci-filters-applied"), g.json().pointer("/manifests/0/digest").and_then(Value::as_str)), ("artifactType", Some(digest(&sbom).as_str())));
    assert_eq!(send(port, "GET", &format!("/v2/acme/app/referrers/{}", digest(b"q")), &a, b"")?.json().get("manifests"), Some(&json!([])));
    let g = send(port, "GET", "/v2/_catalog", &[("Authorization", &basic)], b"")?;
    assert!(g.json().get("repositories").is_some_and(|r| r.to_string().contains("acme/app") && r.to_string().contains("acme/other")), "{}", g.json());

    // Anonymous pull where it's allowed, and nowhere else; no anonymous push.
    assert_eq!(send(port, "PUT", "/v2/acme/public/manifests/v1", &[("Content-Type", MANIFEST)], &m)?.status, 401);
    for blob in [&config, &layer, &big] {
        send(port, "POST", &format!("/v2/acme/public/blobs/uploads/?digest={}", digest(blob)), &a, blob)?;
    }
    assert_eq!(send(port, "PUT", "/v2/acme/public/manifests/v1", &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201);
    assert_eq!(send(port, "GET", "/v2/acme/public/manifests/v1", &[], b"")?.status, 200, "anonymous pull of acme/public");
    let g = send(port, "GET", "/v2/acme/app/manifests/v1", &[], b"")?;
    assert!(g.status == 401 && g.header("www-authenticate").contains("scope=\"repository:acme/app:pull\""), "{:?}", g.headers);

    // Content management: a tag, a manifest, a blob.
    assert_eq!(send(port, "DELETE", "/v2/acme/app/manifests/latest", &a, b"")?.status, 202);
    assert_eq!(send(port, "GET", "/v2/acme/app/manifests/latest", &a, b"")?.status, 404);
    assert_eq!(send(port, "GET", "/v2/acme/app/manifests/v1", &a, b"")?.status, 200, "the manifest stays");
    assert_eq!(send(port, "DELETE", &format!("/v2/acme/app/manifests/{md}"), &a, b"")?.status, 202);
    assert_eq!(send(port, "GET", "/v2/acme/app/manifests/v1", &a, b"")?.status, 404, "its tags went with it");
    assert_eq!(send(port, "DELETE", &format!("/v2/acme/app/blobs/{}", digest(&layer)), &a, b"")?.status, 202);
    assert_eq!(send(port, "GET", &format!("/v2/acme/app/blobs/{}", digest(&layer)), &a, b"")?.status, 404);
    assert_eq!(send(port, "GET", &format!("/v2/acme/other/blobs/{}", digest(&layer)), &a, b"")?.status, 200, "another repository keeps its own");
    Ok(())
}

/// The OCI distribution-spec conformance suite itself (W12-AC01), when `KILN_OCI_CONFORMANCE` names
/// its test binary (`go test -c` of opencontainers/distribution-spec's `conformance/`): pull, push,
/// content discovery and content management, against a server of its own.
#[test]
fn w12_ac01_official_suite() -> anyhow::Result<()> {
    let Ok(suite) = std::env::var("KILN_OCI_CONFORMANCE") else {
        eprintln!("KILN_OCI_CONFORMANCE isn't set: the official suite doesn't run here");
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    // Its JUnit and HTML report, kept where `KILN_OCI_REPORT` says.
    let report = std::env::var("KILN_OCI_REPORT").map_or_else(|_| dir.path().join("report"), std::path::PathBuf::from);
    std::fs::create_dir_all(&report)?;
    let o = std::process::Command::new(&suite)
        .env("OCI_ROOT_URL", format!("http://127.0.0.1:{}", s.port))
        .env("OCI_NAMESPACE", "acme/conformance")
        .env("OCI_CROSSMOUNT_NAMESPACE", "acme/conformance-cross")
        .env("OCI_USERNAME", "admin")
        .env("OCI_PASSWORD", &s.admin)
        .env("OCI_TEST_PULL", "1")
        .env("OCI_TEST_PUSH", "1")
        .env("OCI_TEST_CONTENT_DISCOVERY", "1")
        .env("OCI_TEST_CONTENT_MANAGEMENT", "1")
        .env("OCI_HIDE_SKIPPED_WORKFLOWS", "0")
        // A mount without `from` starts an upload: kiln mounts only from a repository named.
        .env("OCI_AUTOMATIC_CROSSMOUNT", "0")
        .env("OCI_REPORT_DIR", &report)
        .current_dir(dir.path())
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let tail: String = out.lines().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    assert!(o.status.success(), "the conformance suite failed:\n{tail}\n{}", s.log());
    println!("{tail}");
    Ok(())
}

/// What the fake upstream was asked: (method, path).
type Asked = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// A fake upstream registry with Docker's token challenge, serving `library/alpine:3.20` (an image
/// of one layer); down while `down` is set.
fn upstream(config: Vec<u8>, layer: Vec<u8>, manifest: Vec<u8>, down: std::sync::Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<(u16, Asked)> {
    use std::io::BufRead as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let asked: Asked = std::sync::Arc::default();
    let seen = asked.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = std::io::BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.trim().eq_ignore_ascii_case("authorization")
                {
                    auth = v.trim().to_owned();
                }
            }
            if let Ok(mut a) = seen.lock() {
                a.push((method.clone(), path.clone()));
            }
            let mut s = reader.into_inner();
            if down.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = write!(s, "HTTP/1.1 503 Down\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let p = path.split('?').next().unwrap_or_default();
            let (status, ctype, body, extra): (u16, &str, Vec<u8>, String) = if p == "/token" {
                (200, "application/json", br#"{"token":"up-t","expires_in":300}"#.to_vec(), String::new())
            } else if p == format!("/storage/{}", digest(&layer)) {
                // Storage's links are signed, as a CDN's are: no credentials.
                (if auth.is_empty() { 200 } else { 400 }, "application/octet-stream", layer.clone(), String::new())
            } else if auth != "Bearer up-t" {
                (401, "application/json", b"{}".to_vec(), format!("WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"up\",scope=\"repository:library/alpine:pull\"\r\n"))
            } else if p == "/v2/library/alpine/manifests/3.20" || p == format!("/v2/library/alpine/manifests/{}", digest(&manifest)) {
                (200, MANIFEST, manifest.clone(), format!("Docker-Content-Digest: {}\r\n", digest(&manifest)))
            } else if p == format!("/v2/library/alpine/blobs/{}", digest(&config)) {
                (200, "application/octet-stream", config.clone(), String::new())
            } else if p == format!("/v2/library/alpine/blobs/{}", digest(&layer)) {
                // Blobs come from storage elsewhere, as Docker Hub's do.
                (307, "text/plain", Vec::new(), format!("Location: http://127.0.0.1:{port}/storage/{}\r\n", digest(&layer)))
            } else {
                (404, "application/json", br#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#.to_vec(), String::new())
            };
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    Ok((port, asked))
}

/// Pull-through (W12-AC02, R06): an image pulled twice through kiln's mirror of an upstream (with
/// its token challenge and a redirect to storage for a blob) asks the upstream for its blobs once;
/// the mirror's own address serves it at the root, as Docker's `registry-mirrors` pulls; with the
/// upstream down, kiln's copy is served and says so; nothing is pushed to a mirror.
#[test]
fn w12_ac02_pull_through() -> anyhow::Result<()> {
    let (config, layer) = (br#"{"architecture":"amd64","os":"linux"}"#.to_vec(), b"alpine's layer\n".repeat(5000));
    let m = manifest(&config, &[&layer], None, None);
    let down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (up, asked) = upstream(config.clone(), layer.clone(), m.clone(), down.clone())?;
    let root_port = common::free_port()?;
    let dir = tempfile::tempdir()?;
    let extra = format!("[[ci.registry.mirrors]]\nupstream = \"docker.io\"\nurl = \"http://127.0.0.1:{up}\"\nlisten = \"127.0.0.1:{root_port}\"\nanonymous = true\ntag_ttl = 0\n");
    let s = start(dir.path(), &Options { extra: &extra, ..Options::default() })?;
    let port = s.port;
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", s.admin)));
    let a = [("Authorization", basic.as_str())];
    let blob_asks = |d: &str| asked.lock().map(|l| l.iter().filter(|(_, p)| p.contains(d)).count()).unwrap_or(0);

    let mut after_first = (0, 0);
    for round in 0..2 {
        if round == 1 {
            after_first = (blob_asks(&digest(&config)), blob_asks(&format!("blobs/{}", digest(&layer))));
            assert!(after_first.0 > 0 && after_first.1 > 0, "the first pull asked the upstream");
        }
        let g = send(port, "GET", "/v2/docker.io/library/alpine/manifests/3.20", &a, b"")?;
        assert_eq!((g.status, g.header("docker-content-digest")), (200, digest(&m).as_str()), "round {round}: {}\n{}", String::from_utf8_lossy(&g.body), s.log());
        for blob in [&config, &layer] {
            let g = send(port, "GET", &format!("/v2/docker.io/library/alpine/blobs/{}", digest(blob)), &a, b"")?;
            assert!(g.status == 200 && g.body == *blob, "round {round}: {} {}", g.status, String::from_utf8_lossy(&g.body));
        }
    }
    assert_eq!((blob_asks(&digest(&config)), blob_asks(&format!("blobs/{}", digest(&layer)))), after_first, "the second pull's blobs came from kiln: {:?}", asked.lock().map(|l| l.clone()).unwrap_or_default());

    // At the mirror's own address, as Docker's registry-mirrors pulls: the upstream's names, anonymously.
    let g = send(root_port, "GET", "/v2/library/alpine/manifests/3.20", &[], b"")?;
    assert_eq!(g.status, 200, "{}", String::from_utf8_lossy(&g.body));
    let g = send(root_port, "GET", &format!("/v2/library/alpine/blobs/{}", digest(&layer)), &[], b"")?;
    assert!(g.status == 200 && g.body == layer);

    // The upstream down: kiln's copy, saying so; a manifest kiln never had is a 502.
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    let g = send(port, "GET", "/v2/docker.io/library/alpine/manifests/3.20", &a, b"")?;
    assert!(g.status == 200 && g.header("warning").contains("out of reach"), "{} {:?}", g.status, g.headers);
    let g = send(port, "GET", "/v2/docker.io/library/busybox/manifests/1", &a, b"")?;
    assert_eq!(g.status, 502);

    // Nothing is pushed to a mirror.
    let g = send(port, "PUT", "/v2/docker.io/library/alpine/manifests/mine", &[a[0], ("Content-Type", MANIFEST)], &m)?;
    assert_eq!((g.status, g.code()), (403, "DENIED".to_owned()));
    Ok(())
}

fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// A tag pushed to kiln's registry is a `registry_package` event (W12-R07): the workflow of the
/// repository the name is under that listens for it runs, with the package in its event.
#[test]
fn w12_r07_registry_package() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    std::fs::write(
        repo.join(".github/workflows/pkg.yml"),
        "on:\n  registry_package:\n    types: [published]\njobs:\n  seen:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: echo \"published ${{ github.event.registry_package.package_version.container_metadata.tag.name }} as ${{ github.event.action }}\"\n",
    )?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/pkg\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "github_workflows = \"on\"", extra: &extra })?;
    let port = s.port;
    // The repository known (its first poll adopts what it has), then an image pushed under its name.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/pkg") && body.contains("last_polled_at") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", s.admin)));
    let a = [("Authorization", basic.as_str())];
    let (config, layer) = (b"{}".to_vec(), b"a layer".to_vec());
    for blob in [&config, &layer] {
        assert_eq!(send(port, "POST", &format!("/v2/acme/pkg/blobs/uploads/?digest={}", digest(blob)), &a, blob)?.status, 201);
    }
    let m = manifest(&config, &[&layer], None, None);
    assert_eq!(send(port, "PUT", "/v2/acme/pkg/manifests/v1", &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201);
    // The run, to its end, and what it saw.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/pkg&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!((run.get("event").and_then(Value::as_str), run.get("conclusion").and_then(Value::as_str)), (Some("registry_package"), Some("success")), "{run}");
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert!(log.contains("published v1 as published"), "{log}");
    // Pushing the same digest to the tag again is no new event; a new digest under it is `updated`.
    assert_eq!(send(port, "PUT", "/v2/acme/pkg/manifests/v1", &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201);
    std::thread::sleep(Duration::from_secs(2));
    let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/pkg&limit=5", &s.admin, None)?;
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).map(Vec::len), Some(1), "{body}");
    Ok(())
}

/// Jobs' registry credentials (W12-R03): in a leg, Docker asks `docker-credential-kiln` (kiln's own
/// binary on the leg's PATH) and gets the job's token for kiln's registry and the integration's for
/// another; the leg's Docker config names kiln for each; a registry it has nothing for is "none".
#[test]
fn w12_r03_job_credentials() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "creds\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/creds\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    // The engine runs `.kiln/` workflows (v1 keeps them otherwise).
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let port = s.port;
    let (st, body) = common::http(port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "oci", "name": "ghcr", "config": { "registry": "ghcr.io", "username": "bot" }, "secret": "ghp_fake_secret", "scope": "server" })))?;
    anyhow::ensure!((200..300).contains(&st), "the integration: {st} {body}");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/creds") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        r#"name: creds
on: { push: {} }
jobs:
  creds:
    runs-on: [self-hosted, local]
    integrations: [ghcr]
    steps:
      - run: |
          set -e
          echo "$KILN_REGISTRY_HOST" | docker-credential-kiln get | grep -q '"Username":"kiln"'
          echo own-ok
          echo ghcr.io | docker-credential-kiln get | grep -q '"Username":"bot"'
          echo ghcr.io | docker-credential-kiln get | grep -q '"Secret":"ghp_fake_secret"'
          echo ghcr-ok
          grep -q '"ghcr.io":"kiln"' "$DOCKER_CONFIG/config.json"
          echo config-ok
          if echo quay.io | docker-credential-kiln get; then exit 1; fi
          echo none-ok
"#,
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "creds"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/creds&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("success"), "{run}\n{log}");
    for said in ["own-ok", "ghcr-ok", "config-ok", "none-ok"] {
        assert!(log.contains(said), "{said}: {log}");
    }
    assert!(!log.contains("ghp_fake_secret"), "the token is masked: {log}");
    Ok(())
}

/// An attestation of an image in kiln's registry is attached to it (docs/ci.md W18, W12-R01): the
/// referrers API lists kiln's signed provenance as a Sigstore bundle about the image, and `cosign`
/// (its container, when the runner has its image) verifies it with the server's key.
#[test]
fn w18_attestation_on_image() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "attested\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[ci.registry]\nanonymous_pull = [\"acme/app\"]\n\n[[repos]]\nname = \"acme/app\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let port = s.port;
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", s.admin)));
    let a = [("Authorization", basic.as_str())];
    // An image in kiln's registry.
    let (config, layer) = (br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{}}"#.to_vec(), b"a layer".to_vec());
    for blob in [&config, &layer] {
        assert_eq!(send(port, "POST", &format!("/v2/acme/app/blobs/uploads/?digest={}", digest(blob)), &a, blob)?.status, 201);
    }
    let m = manifest(&config, &[&layer], None, None);
    let md = digest(&m);
    assert_eq!(send(port, "PUT", "/v2/acme/app/manifests/v1", &[a[0], ("Content-Type", MANIFEST)], &m)?.status, 201);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/app") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    // A job attests it.
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        format!("name: attest\non: {{ push: {{}} }}\njobs:\n  attest:\n    runs-on: [self-hosted, local]\n    steps:\n      - uses: kiln/attest@v1\n        with:\n          subject-name: 127.0.0.1:{port}/acme/app\n          subject-digest: {md}\n"),
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "attest"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let referrers = loop {
        let g = send(port, "GET", &format!("/v2/acme/app/referrers/{md}?artifactType=application/vnd.dev.sigstore.bundle.v0.3%2Bjson"), &a, b"")?;
        let v = g.json();
        if v.get("manifests").and_then(Value::as_array).is_some_and(|l| !l.is_empty()) {
            break v;
        }
        let (_, runs) = common::http(port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        anyhow::ensure!(std::time::Instant::now() < deadline, "nothing attached: {v}\n{runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(referrers.pointer("/manifests/0/annotations/dev.sigstore.bundle.predicateType").and_then(Value::as_str), Some("https://slsa.dev/provenance/v1"), "{referrers}");
    // The bundle itself, through the referrer's manifest.
    let rd = referrers.pointer("/manifests/0/digest").and_then(Value::as_str).unwrap_or_default().to_owned();
    let rm = send(port, "GET", &format!("/v2/acme/app/manifests/{rd}"), &a, b"")?.json();
    assert_eq!(rm.pointer("/subject/digest").and_then(Value::as_str), Some(md.as_str()), "{rm}");
    let bd = rm.pointer("/layers/0/digest").and_then(Value::as_str).unwrap_or_default().to_owned();
    let bundle = send(port, "GET", &format!("/v2/acme/app/blobs/{bd}"), &a, b"")?;
    assert!(String::from_utf8_lossy(&bundle.body).contains("dsseEnvelope"), "{}", String::from_utf8_lossy(&bundle.body));
    // cosign, where its image is: kiln's key verifies the bundle found through the referrers API
    // as an attestation of the image's manifest (whose digest is the subject's).
    let cosign = "ghcr.io/sigstore/cosign/cosign:v2.4.1";
    if std::process::Command::new("docker").args(["image", "inspect", cosign]).output().is_ok_and(|o| o.status.success()) {
        let (_, pem) = common::http(port, "GET", "/_kiln/attestation-key.pem", &s.admin, None)?;
        std::fs::write(dir.path().join("key.pem"), pem)?;
        std::fs::write(dir.path().join("root.json"), "{\"mediaType\":\"application/vnd.dev.sigstore.trustedroot+json;version=0.1\"}")?;
        std::fs::write(dir.path().join("bundle.json"), &bundle.body)?;
        std::fs::write(dir.path().join("manifest.json"), &m)?;
        let o = std::process::Command::new("docker")
            .args(["run", "--rm", "--network", "host", "-v"])
            .arg(format!("{}:/w", dir.path().display()))
            .args([cosign, "verify-blob-attestation", "--key", "/w/key.pem", "--bundle", "/w/bundle.json", "--new-bundle-format", "--insecure-ignore-tlog", "--trusted-root", "/w/root.json", "--type", "slsaprovenance1", "/w/manifest.json"])
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(o.status.success(), "cosign: {said}");
    }
    Ok(())
}

/// What a `kiln/build-image@v1` run left: the server, the image it registered, the leg's log.
struct Built {
    _dir: tempfile::TempDir,
    s: common::Server,
    image: Value,
    log: String,
}

/// Builds `.kiln/images/tools/<file>` (`content`, beside a `hello.txt`) with `kiln/build-image@v1`
/// on a local server's own runner, as a job with `packages: write`; fails unless the run succeeds.
fn build_image(file: &str, content: &str) -> anyhow::Result<Built> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln/images/tools"))?;
    std::fs::write(repo.join(".kiln/images/tools").join(file), content)?;
    std::fs::write(repo.join(".kiln/images/tools/hello.txt"), "built in kiln\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/images\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let port = s.port;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/images") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        r#"name: images
on: { push: {} }
jobs:
  build:
    runs-on: [self-hosted, local]
    permissions: { contents: read, packages: write }
    steps:
      - uses: kiln/checkout@v1
      - id: tools
        uses: kiln/build-image@v1
        with: { name: tools, tag: v1 }
      - run: echo "built ${{ steps.tools.outputs.image }}"
"#,
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "images"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/images&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    let (_, steps) = common::http(port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    anyhow::ensure!(run.get("conclusion").and_then(Value::as_str) == Some("success"), "{steps}\n{log}\n{}", s.log());
    let (_, body) = common::http(port, "GET", "/api/ci/images?repo=acme/images", &s.admin, None)?;
    let image = serde_json::from_str::<Value>(&body)?.pointer("/images/0").cloned().unwrap_or_default();
    Ok(Built { _dir: dir, s, image, log })
}

/// Checks what `kiln/build-image@v1` registered against kiln's registry: the image by its name,
/// tags `v1` and `latest` naming the registered digest, the step's output that image by digest,
/// its platforms `platforms`.
fn check_built(b: &Built, platforms: &Value) -> anyhow::Result<()> {
    let (port, image) = (b.s.port, &b.image);
    assert_eq!(image.get("name").and_then(Value::as_str), Some("tools"), "{image}");
    assert_eq!(image.get("reference").and_then(Value::as_str), Some(format!("127.0.0.1:{port}/acme/images/images/tools:v1").as_str()), "{image}");
    assert_eq!(image.get("platforms"), Some(platforms), "{image}");
    let digest = image.get("digest").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(digest.starts_with("sha256:"), "{image}");
    assert!(b.log.contains(&format!("built 127.0.0.1:{port}/acme/images/images/tools@{digest}")), "the step's output is the image by digest: {}", b.log);
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", b.s.admin)));
    let accept = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";
    for tag in ["v1", "latest"] {
        let g = send(port, "GET", &format!("/v2/acme/images/images/tools/manifests/{tag}"), &[("Authorization", basic.as_str()), ("Accept", accept)], b"")?;
        assert_eq!(g.status, 200, "{tag}: {}", String::from_utf8_lossy(&g.body));
        assert_eq!(g.header("docker-content-digest"), digest, "{tag}");
    }
    let (_, body) = common::http(port, "GET", "/api/ci/images?repo=acme/images&name=other", &b.s.admin, None)?;
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("images"), Some(&json!([])), "{body}");
    Ok(())
}

fn on_path(prog: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// A runner image built in kiln (docs/ci.md W13-R11): `kiln/build-image@v1` builds
/// `.kiln/images/<name>/Dockerfile` with `docker buildx`, pushes it to kiln's registry under the
/// repository's `images/<name>` with the job's token (BuildKit asks for it with OAuth2's form),
/// and the server registers it (`ci_images`) with the digest its registry holds and the
/// platforms the image's manifest says.
#[test]
fn w13_r11_build_image() -> anyhow::Result<()> {
    if !std::process::Command::new("docker").args(["buildx", "version"]).output().is_ok_and(|o| o.status.success()) {
        eprintln!("w13_r11_build_image: no docker buildx here");
        return Ok(());
    }
    let b = build_image("Dockerfile", "FROM scratch\nCOPY hello.txt /hello.txt\n")?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    check_built(&b, &json!([format!("linux/{arch}")]))?;
    assert!(b.log.contains("docker buildx build --push"), "{}", b.log);
    Ok(())
}

/// The same for an Agentfile, which shards builds (W13-R06, R11): `shards build`, then
/// `shards push` with kiln's credential helper, never Docker; its microVM image is Linux on the
/// machine's architecture.
#[test]
fn w13_r11_build_agentfile() -> anyhow::Result<()> {
    if !on_path("shards") {
        eprintln!("w13_r11_build_agentfile: no shards here");
        return Ok(());
    }
    let b = build_image("Agentfile", "FROM alpine:3.22\nCOPY hello.txt /hello.txt\nRUN cat /hello.txt\n")?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    check_built(&b, &json!([format!("linux/{arch}")]))?;
    assert!(b.log.contains("shards build") && b.log.contains("shards push"), "{}", b.log);
    assert!(!b.log.contains("docker buildx"), "an Agentfile is never built with Docker: {}", b.log);
    Ok(())
}
