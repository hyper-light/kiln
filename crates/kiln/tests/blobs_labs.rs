//! CP-T10.1, AC-T10.1.3: the sync, CI and upgrade labs, each on the local store and again on an S3
//! one (docs/blobs.md, "The labs"). Each lab is a real kiln server (and its own runner where it
//! needs one), its bytes kept where `[blobs]` says; the lab is the same either way.
//!
//! - **CI:** a CI run's leg hands the lab its own token; through it the lab saves a cache in two
//!   parts, restores it whole and in a range, uploads an artifact in blocks, finalizes it and
//!   downloads it, as GitHub's toolkit does; then the run ends green. Both are in the store, at
//!   `cas/<sha256>`.
//! - **Sync:** two copies of a synced folder on the server's runner: a file written in one comes
//!   out in the other, and again when it changes; its contents are in the store, at `sync/<sha256>`.
//! - **Upgrade:** a runner binary uploaded is listed, served whole by its platform and its build,
//!   is there still after the server starts again, and goes when it's removed. On the local store,
//!   one an older kiln left in `<data>/binaries` is served from where it lies.
//!
//! The S3 runs need an S3: `KILN_TEST_S3="<endpoint> <bucket> <access key id> <secret file>"` (the
//! lab's MinIO on fedora, docs/blobs.md), and run with `--ignored`.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Options, start};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

// ------------------------------------------------------------------------------------- helpers

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
}

fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<Got> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
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

/// `url` (an absolute URL on the server) as a path and query.
fn path_of(url: &str) -> String {
    url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..).unwrap_or("/").to_owned())).unwrap_or_else(|| url.to_owned())
}

fn sha256(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// `n` bytes of noise, the same for the same seed.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    kiln_seams::blobs::noise(n, seed)
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=kiln", "-c", "user.email=kiln@example.invalid", "-c", "commit.gpgsign=false"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        if f()? {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: not in {within:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Where a lab keeps its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Store {
    Local,
    S3,
}

/// The S3 the lab's S3 runs use, from `KILN_TEST_S3`: endpoint, bucket, access key id, secret file.
fn s3_spec() -> anyhow::Result<(String, String, String, String)> {
    let spec = std::env::var("KILN_TEST_S3").map_err(|_| anyhow::anyhow!("KILN_TEST_S3 isn't set (docs/blobs.md, \"The labs\")"))?;
    let w: Vec<&str> = spec.split_whitespace().collect();
    match w.as_slice() {
        [e, b, a, s] => Ok(((*e).to_owned(), (*b).to_owned(), (*a).to_owned(), (*s).to_owned())),
        _ => anyhow::bail!("KILN_TEST_S3: <endpoint> <bucket> <access key id> <secret file>"),
    }
}

/// A lab's `[blobs]`, and the S3 prefix its bytes go under (none: the local store).
fn blobs_config(store: Store, lab: &str) -> anyhow::Result<(String, Option<String>)> {
    match store {
        Store::Local => Ok((String::new(), None)),
        Store::S3 => {
            let prefix = format!("labs/{}/{lab}", hex::encode(rand::random::<[u8; 6]>()));
            Ok((s3_section("s3", &prefix)?, Some(prefix)))
        }
    }
}

/// `[blobs]` with the lab's S3 under `prefix`, the store kiln uses `store` (`local` or `s3`).
fn s3_section(store: &str, prefix: &str) -> anyhow::Result<String> {
    let (endpoint, bucket, access, secret) = s3_spec()?;
    Ok(format!(
        "[blobs]\nstore = \"{store}\"\n\n[blobs.s3]\nendpoint = \"{endpoint}\"\nbucket = \"{bucket}\"\nprefix = \"{prefix}\"\naccess_key_id = \"{access}\"\nsecret_access_key_file = \"{secret}\"\ncreate_bucket = true\n"
    ))
}

/// The lab's S3 store, as the server keeps it, to look into.
fn s3_store(prefix: &str) -> anyhow::Result<kiln::blobs::s3::S3Blobs> {
    let (endpoint, bucket, access, secret) = s3_spec()?;
    let cfg = kiln::blobs::s3::Config { endpoint, bucket, prefix: prefix.to_owned(), access_key_id: access, secret_access_key_file: Some(PathBuf::from(secret)), ..kiln::blobs::s3::Config::default() };
    kiln::blobs::s3::S3Blobs::new(&cfg, kiln::rt::clock(), kiln::blobs::s3::Limits::default())
}

/// Whether the store a lab's server keeps its bytes in holds `key`, with `sha256` as its etag.
fn held(store: Store, data: &Path, prefix: Option<&str>, key: &str, sha256: &str) -> anyhow::Result<bool> {
    use kiln_seams::blobs::Blobs as _;
    match (store, prefix) {
        (Store::S3, Some(p)) => {
            let s3 = s3_store(p)?;
            let key = key.to_owned();
            let etag = kiln::rt::block_on(async move { Ok(s3.head(&key).await.ok().map(|m| m.etag)) })?;
            Ok(etag.as_deref() == Some(sha256))
        }
        _ => {
            let db = rusqlite::Connection::open_with_flags(data.join("kiln.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let digest: Option<String> = db.query_row("SELECT digest FROM blob_keys WHERE key = ?1", [key], |r| r.get(0)).ok();
            Ok(digest.as_deref() == Some(sha256))
        }
    }
}

// ------------------------------------------------------------------------------------------- CI

/// The CI lab: GitHub's cache and artifacts through a real leg's token, every byte in the store.
fn ci_lab(store: Store) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let lab = dir.path().join("lab");
    std::fs::create_dir_all(&lab)?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "ci lab\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let (blobs, prefix) = blobs_config(store, "ci")?;
    let extra = format!("{blobs}\n[[repos]]\nname = \"acme/lab\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra, terminal: false })?;
    let port = s.port;
    wait_for("the repository polled", Duration::from_secs(60), || {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("acme/lab") && !body.contains("\"last_polled_at\":null"))
    })?;
    // The leg's step hands the lab its runtime and waits until the lab is done with it.
    let (runtime, done) = (lab.join("runtime"), lab.join("done"));
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        format!(
            "name: lab\non: {{ push: {{}} }}\njobs:\n  lab:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: |\n          printf '%s\\n%s\\n%s\\n' \"$ACTIONS_RUNTIME_TOKEN\" \"$ACTIONS_CACHE_URL\" \"$ACTIONS_RESULTS_URL\" > {rt}.part && mv {rt}.part {rt}\n          for i in $(seq 1 1200); do [ -f {done} ] && exit 0; sleep 0.25; done\n          exit 1\n",
            rt = runtime.display(),
            done = done.display()
        ),
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "lab"])?;
    wait_for("the leg's runtime", Duration::from_secs(180), || Ok(runtime.exists()))?;
    let text = std::fs::read_to_string(&runtime)?;
    let mut lines = text.lines();
    let (token, cache_url, results_url) = (lines.next().unwrap_or_default().to_owned(), lines.next().unwrap_or_default().to_owned(), lines.next().unwrap_or_default().to_owned());
    anyhow::ensure!(!token.is_empty() && !cache_url.is_empty() && !results_url.is_empty(), "the leg's runtime: {text:?}");
    let bearer = format!("Bearer {token}");
    let auth = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];

    // A cache, saved in two parts and restored whole and in a range.
    let archive = noise((40 << 20) + 321, 1);
    let cache = format!("{}_apis/artifactcache", path_of(&cache_url));
    let reserve = json!({ "key": "lab-cache", "version": "v1", "cacheSize": archive.len() }).to_string();
    let r = send(port, "POST", &format!("{cache}/caches"), &auth, reserve.as_bytes())?;
    anyhow::ensure!(r.status == 200, "reserve: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let id = r.json().get("cacheId").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("no cacheId"))?;
    for (start, end) in [(0usize, 32 << 20), (32 << 20, archive.len())] {
        let range = format!("bytes {start}-{}/*", end - 1);
        let part = archive.get(start..end).unwrap_or_default();
        let r = send(port, "PATCH", &format!("{cache}/caches/{id}"), &[("Authorization", bearer.as_str()), ("Content-Type", "application/octet-stream"), ("Content-Range", &range)], part)?;
        anyhow::ensure!(r.status == 200, "part {range}: {} {}", r.status, String::from_utf8_lossy(&r.body));
    }
    let r = send(port, "POST", &format!("{cache}/caches/{id}"), &auth, json!({ "size": archive.len() }).to_string().as_bytes())?;
    anyhow::ensure!(r.status == 200 || r.status == 204, "commit: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let r = send(port, "GET", &format!("{cache}/cache?keys=lab-cache&version=v1"), &auth, b"")?;
    anyhow::ensure!(r.status == 200, "restore: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let location = r.json().get("archiveLocation").and_then(Value::as_str).map(path_of).ok_or_else(|| anyhow::anyhow!("no archiveLocation"))?;
    let whole = send(port, "GET", &location, &[], b"")?;
    anyhow::ensure!(whole.status == 200 && whole.body == archive, "the cache restored whole: {} ({} bytes)", whole.status, whole.body.len());
    let part = send(port, "GET", &location, &[("Range", "bytes=33554430-33554440")], b"")?;
    anyhow::ensure!(part.status == 206 && Some(part.body.as_slice()) == archive.get(33_554_430..=33_554_440), "a range across the parts: {}", part.status);

    // An artifact, uploaded in blocks, finalized, listed and downloaded.
    let claims: Value = token.split('.').nth(1).and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let (run, job) = (claims.get("run").and_then(Value::as_str).unwrap_or_default(), claims.get("sub").and_then(Value::as_str).unwrap_or_default());
    // Twirp answers at the root of ACTIONS_RESULTS_URL's origin (the toolkit drops any path).
    anyhow::ensure!(!results_url.is_empty(), "no ACTIONS_RESULTS_URL");
    let twirp = "/twirp/github.actions.results.api.v1.ArtifactService";
    let ids = json!({ "workflow_run_backend_id": run, "workflow_job_run_backend_id": job });
    let mut create = ids.clone();
    if let Some(o) = create.as_object_mut() {
        o.insert("name".into(), json!("lab-artifact"));
        o.insert("version".into(), json!(4));
    }
    let r = send(port, "POST", &format!("{twirp}/CreateArtifact"), &auth, create.to_string().as_bytes())?;
    anyhow::ensure!(r.status == 200, "create: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let upload = r.json().get("signed_upload_url").and_then(Value::as_str).map(path_of).ok_or_else(|| anyhow::anyhow!("no upload URL"))?;
    let zip = noise((9 << 20) + 77, 2);
    let mut blocks = Vec::new();
    for (i, piece) in zip.chunks(4 << 20).enumerate() {
        let id = base64::engine::general_purpose::STANDARD.encode(format!("block-{i:04}"));
        let r = send(port, "PUT", &format!("{upload}&comp=block&blockid={}", id.replace('=', "%3D")), &[], piece)?;
        anyhow::ensure!(r.status == 201, "block {i}: {} {}", r.status, String::from_utf8_lossy(&r.body));
        blocks.push(id);
    }
    let list: String = blocks.iter().map(|b| format!("<Latest>{b}</Latest>")).collect();
    let r = send(port, "PUT", &format!("{upload}&comp=blocklist"), &[("Content-Type", "application/xml")], format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{list}</BlockList>").as_bytes())?;
    anyhow::ensure!(r.status == 201, "block list: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let mut fin = ids.clone();
    if let Some(o) = fin.as_object_mut() {
        o.insert("name".into(), json!("lab-artifact"));
        o.insert("size".into(), json!(zip.len().to_string()));
        o.insert("hash".into(), json!(format!("sha256:{}", sha256(&zip))));
    }
    let r = send(port, "POST", &format!("{twirp}/FinalizeArtifact"), &auth, fin.to_string().as_bytes())?;
    anyhow::ensure!(r.status == 200, "finalize: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let mut get = ids.clone();
    if let Some(o) = get.as_object_mut() {
        o.insert("name".into(), json!("lab-artifact"));
    }
    let r = send(port, "POST", &format!("{twirp}/GetSignedArtifactURL"), &auth, get.to_string().as_bytes())?;
    let download = r.json().get("signed_url").and_then(Value::as_str).map(path_of).ok_or_else(|| anyhow::anyhow!("no download URL: {}", String::from_utf8_lossy(&r.body)))?;
    let got = send(port, "GET", &download, &[], b"")?;
    anyhow::ensure!(got.status == 200 && got.body == zip, "the artifact downloaded: {} ({} bytes)", got.status, got.body.len());

    // Both are in the store, at cas/<sha256>, and the run ends green.
    for (what, bytes) in [("the cache", &archive), ("the artifact", &zip)] {
        let digest = sha256(bytes);
        anyhow::ensure!(held(store, &dir.path().join("data"), prefix.as_deref(), &format!("cas/{digest}"), &digest)?, "{what} isn't in the {store:?} store");
    }
    std::fs::write(&done, b"done")?;
    let mut conclusion = String::new();
    wait_for("the run's end", Duration::from_secs(120), || {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/lab&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned();
        conclusion = run.and_then(|r| r.get("conclusion").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        Ok(!conclusion.is_empty())
    })?;
    anyhow::ensure!(conclusion == "success", "the run: {conclusion}\n{}", s.log());
    Ok(())
}

/// CP-T10.1, AC-T10.1.3: the CI lab on the local store.
#[test]
fn ci_lab_on_the_local_store() -> anyhow::Result<()> {
    ci_lab(Store::Local)
}

/// CP-T10.1, AC-T10.1.3: the CI lab on an S3 store.
#[test]
#[ignore = "needs an S3: KILN_TEST_S3 (docs/blobs.md, \"The labs\")"]
fn ci_lab_on_an_s3_store() -> anyhow::Result<()> {
    ci_lab(Store::S3)
}

// ----------------------------------------------------------------------------------------- sync

/// The sync lab: two copies of a folder on the server's runner, a file's contents through the store.
fn sync_lab(store: Store) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (a, b) = (dir.path().join("copy-a"), dir.path().join("copy-b"));
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    let (blobs, prefix) = blobs_config(store, "sync")?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: &blobs, terminal: false })?;
    let port = s.port;
    let mut machine = String::new();
    wait_for("the server's runner", Duration::from_secs(60), || {
        let (_, body) = common::http(port, "GET", "/api/agents", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        machine = list.iter().find(|x| x.get("connected").and_then(Value::as_bool) == Some(true)).and_then(|x| x.get("name").and_then(Value::as_str)).unwrap_or_default().to_owned();
        Ok(!machine.is_empty())
    })?;
    let ask = json!({ "name": "lab", "copies": [{ "machine": machine, "path": a.to_string_lossy() }, { "machine": machine, "path": b.to_string_lossy() }] });
    let (st, body) = common::http(port, "POST", "/api/sync", &s.admin, Some(&ask))?;
    anyhow::ensure!((200..300).contains(&st), "the synced folder: {st} {body}");
    for (round, content) in [b"from copy a\n".to_vec(), noise(300_000, 3)].into_iter().enumerate() {
        std::fs::write(a.join("lab.bin"), &content)?;
        wait_for(&format!("round {round}: the file in copy b"), Duration::from_secs(120), || Ok(std::fs::read(b.join("lab.bin")).is_ok_and(|got| got == content)))?;
        let digest = sha256(&content);
        let head = send(port, "HEAD", &format!("/api/sync/blobs/{digest}"), &[("Authorization", &format!("Bearer {}", s.admin))], b"")?;
        anyhow::ensure!(head.status == 200, "round {round}: the server holds its contents: {}", head.status);
        anyhow::ensure!(held(store, &dir.path().join("data"), prefix.as_deref(), &format!("sync/{digest}"), &digest)?, "round {round}: the contents aren't in the {store:?} store");
        let got = send(port, "GET", &format!("/api/sync/blobs/{digest}"), &[("Authorization", &format!("Bearer {}", s.admin))], b"")?;
        anyhow::ensure!(got.status == 200 && got.body == content, "round {round}: the contents served: {}", got.status);
    }
    // Contents that don't hash to their name are refused, and nothing is kept.
    let wrong = sha256(b"not these bytes");
    let put = send(port, "PUT", &format!("/api/sync/blobs/{wrong}"), &[("Authorization", &format!("Bearer {}", s.admin))], b"other bytes")?;
    anyhow::ensure!(put.status == 400, "contents under another hash: {}", put.status);
    anyhow::ensure!(!held(store, &dir.path().join("data"), prefix.as_deref(), &format!("sync/{wrong}"), &wrong)?, "kept under the wrong hash");
    Ok(())
}

/// CP-T10.1, AC-T10.1.3: the sync lab on the local store.
#[test]
fn sync_lab_on_the_local_store() -> anyhow::Result<()> {
    sync_lab(Store::Local)
}

/// CP-T10.1, AC-T10.1.3: the sync lab on an S3 store.
#[test]
#[ignore = "needs an S3: KILN_TEST_S3 (docs/blobs.md, \"The labs\")"]
fn sync_lab_on_an_s3_store() -> anyhow::Result<()> {
    sync_lab(Store::S3)
}

// -------------------------------------------------------------------------------------- upgrade

/// A platform other than the server's own, and a binary for it.
fn other_platform() -> (&'static str, Vec<u8>) {
    let target = if cfg!(target_os = "macos") { "x86_64-pc-windows-msvc" } else { "aarch64-apple-darwin" };
    let mut bytes = if target.contains("windows") { b"MZ".to_vec() } else { vec![0xcf, 0xfa, 0xed, 0xfe] };
    bytes.extend(noise(3 << 20, 4));
    (target, bytes)
}

fn listed_build(port: u16, admin: &str, target: &str) -> anyhow::Result<Option<String>> {
    let (_, body) = common::http(port, "GET", "/api/updates", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let found = v.pointer("/binaries").and_then(Value::as_array).into_iter().flatten().find(|b| b.get("target").and_then(Value::as_str) == Some(target) && b.get("origin").and_then(Value::as_str) != Some("server"));
    Ok(found.and_then(|b| b.get("build").and_then(Value::as_str).map(str::to_owned)))
}

/// The upgrade lab: a runner binary kept, served and let go through the store.
fn upgrade_lab(store: Store) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (blobs, prefix) = blobs_config(store, "upgrade")?;
    let (target, binary) = other_platform();
    let data = dir.path().join("data");
    // An older kiln's binary, where it kept them: the local store serves it where it lies.
    let (old_target, mut old) = ("x86_64-unknown-linux-musl", b"\x7fELF".to_vec());
    old.extend(noise(2 << 20, 5));
    if store == Store::Local && kiln::api::TARGET != old_target {
        let bin = data.join("binaries");
        std::fs::create_dir_all(&bin)?;
        std::fs::write(bin.join(format!("kiln-{old_target}")), &old)?;
        let meta = json!({ "target": old_target, "sha256": sha256(&old), "size": old.len(), "origin": "upload", "at": 1, "build": sha256(&old).get(..12).unwrap_or_default() });
        std::fs::write(bin.join(format!("kiln-{old_target}.json")), meta.to_string())?;
    }
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: &blobs, terminal: false })?;
    let auth = format!("Bearer {}", s.admin);
    if store == Store::Local && kiln::api::TARGET != old_target {
        let got = send(s.port, "GET", &format!("/dl/kiln/{old_target}"), &[], b"")?;
        anyhow::ensure!(got.status == 200 && got.body == old, "the older kiln's binary, served: {}", got.status);
        anyhow::ensure!(data.join("binaries").join(format!("kiln-{old_target}")).exists(), "where it lay");
    }
    let r = send(s.port, "POST", &format!("/api/updates/binaries?target={target}"), &[("Authorization", &auth), ("Content-Type", "application/octet-stream")], &binary)?;
    anyhow::ensure!(r.status == 200, "the upload: {} {}", r.status, String::from_utf8_lossy(&r.body));
    let build = r.json().get("build").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(build == sha256(&binary).get(..12).unwrap_or_default(), "its build: {build}");
    anyhow::ensure!(listed_build(s.port, &s.admin, target)?.as_deref() == Some(build.as_str()), "listed");
    for path in [format!("/dl/kiln/{target}"), format!("/dl/kiln/{target}/{build}")] {
        let got = send(s.port, "GET", &path, &[], b"")?;
        anyhow::ensure!(got.status == 200 && got.body == binary, "{path}: {} ({} bytes)", got.status, got.body.len());
    }
    let digest = sha256(&binary);
    anyhow::ensure!(held(store, &data, prefix.as_deref(), &format!("binaries/kiln-{target}"), &digest)?, "the binary isn't in the {store:?} store");
    // The server again: the binary is still kept, read from the store.
    drop(s);
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: &blobs, terminal: false })?;
    let auth = format!("Bearer {}", s.admin);
    // Read from the store as the server starts (an S3 store's in the background).
    wait_for("the binary listed after a restart", Duration::from_secs(30), || Ok(listed_build(s.port, &s.admin, target)?.as_deref() == Some(build.as_str())))?;
    let got = send(s.port, "GET", &format!("/dl/kiln/{target}/{build}"), &[], b"")?;
    anyhow::ensure!(got.status == 200 && got.body == binary, "served after a restart: {}", got.status);
    // Removed, it's gone from the store; its platform's download is GitHub's release again.
    let r = send(s.port, "DELETE", &format!("/api/updates/binaries/{target}"), &[("Authorization", &auth)], b"")?;
    anyhow::ensure!(r.status == 204, "the delete: {}", r.status);
    anyhow::ensure!(!held(store, &data, prefix.as_deref(), &format!("binaries/kiln-{target}"), &digest)?, "still in the store after it was removed");
    let got = send(s.port, "GET", &format!("/dl/kiln/{target}/{build}"), &[], b"")?;
    anyhow::ensure!(got.status == 404, "its build after the delete: {}", got.status);
    Ok(())
}

/// CP-T10.1, AC-T10.1.3: the upgrade lab on the local store.
#[test]
fn upgrade_lab_on_the_local_store() -> anyhow::Result<()> {
    upgrade_lab(Store::Local)
}

/// CP-T10.1, AC-T10.1.3: the upgrade lab on an S3 store.
#[test]
#[ignore = "needs an S3: KILN_TEST_S3 (docs/blobs.md, \"The labs\")"]
fn upgrade_lab_on_an_s3_store() -> anyhow::Result<()> {
    upgrade_lab(Store::S3)
}

// ------------------------------------------------------------------------------- presigned URLs

/// The local store's presigned URLs, as `/api/blobs` serves them (the URL its only credential):
/// a get and a put of the declared length work; a tampered, expired-by-design, wrong-method,
/// wrong-key or wrong-length one is refused with one answer, the same as for no such object.
#[test]
fn presigned_urls_grant_exactly_what_they_say_and_nothing_else() -> anyhow::Result<()> {
    use kiln_seams::blobs::{Access, Blobs as _};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "", terminal: false })?;
    // The server's own key: what its presigned URLs are signed with (`<data>/blobs.key`).
    let data = dir.path().join("data");
    let key: [u8; 32] = std::fs::read(data.join("blobs.key"))?.try_into().map_err(|_| anyhow::anyhow!("blobs.key isn't 32 bytes"))?;
    let base = format!("http://127.0.0.1:{}/api/blobs", s.port);
    let store = kiln::store::Store::open(&data.join("kiln.db"))?;
    let signer = std::sync::Arc::new(kiln::blobs::local::LocalBlobs::new(store, data.join("cas"), kiln::rt::clock(), kiln::blobs::local::Limits::default(), kiln::blobs::local::Presign { base, key }));
    let presign_for = |k: &str, a: Access, lasts: Duration| {
        let (signer, k) = (std::sync::Arc::clone(&signer), k.to_owned());
        kiln::rt::block_on(async move { Ok(signer.presign(&k, a, lasts).await?) })
    };
    let presign = |k: &str, a: Access| presign_for(k, a, Duration::from_secs(60));
    let body = noise(70_000, 6);
    let put_url = presign("lab/shared", Access::Put { length: body.len() as u64 })?;
    let r = send(s.port, "PUT", &path_of(&put_url), &[], &body)?;
    anyhow::ensure!(r.status == 200, "a put of the declared length: {}", r.status);
    let get_url = presign("lab/shared", Access::Get)?;
    let r = send(s.port, "GET", &path_of(&get_url), &[], b"")?;
    anyhow::ensure!(r.status == 200 && r.body == body, "the get: {}", r.status);
    let r = send(s.port, "GET", &path_of(&get_url), &[("Range", "bytes=100-199")], b"")?;
    anyhow::ensure!(r.status == 206 && Some(r.body.as_slice()) == body.get(100..200), "a range: {}", r.status);
    anyhow::ensure!(r.header("content-range") == format!("bytes 100-199/{}", body.len()), "its Content-Range: {}", r.header("content-range"));
    // Each refused alike.
    let refused = send(s.port, "GET", &path_of(&presign("lab/missing", Access::Get)?), &[], b"")?;
    let short_lived = presign_for("lab/shared", Access::Get, Duration::from_secs(1))?;
    std::thread::sleep(Duration::from_millis(2100));
    let expired = send(s.port, "GET", &path_of(&short_lived), &[], b"")?;
    let mut tampered = path_of(&get_url);
    let last = tampered.pop();
    tampered.push(if last == Some('0') { '1' } else { '0' });
    let mut cases: Vec<(&str, Got)> = vec![
        ("tampered", send(s.port, "GET", &tampered, &[], b"")?),
        ("a get's URL put", send(s.port, "PUT", &path_of(&get_url), &[], b"x")?),
        ("a put's URL got", send(s.port, "GET", &path_of(&put_url), &[], b"")?),
        ("another key", send(s.port, "GET", &path_of(&get_url).replace("lab/shared", "lab/sharee"), &[], b"")?),
        ("a longer body", send(s.port, "PUT", &path_of(&presign("lab/long", Access::Put { length: 5 })?), &[], b"123456")?),
        ("a shorter body", send(s.port, "PUT", &path_of(&presign("lab/short", Access::Put { length: 5 })?), &[], b"1234")?),
        ("an expiry changed", send(s.port, "GET", &path_of(&get_url).replace("expires=", "expires=1"), &[], b"")?),
        ("expired", expired),
        ("a key with ..", send(s.port, "GET", &path_of(&get_url).replace("lab/shared", "lab/../shared"), &[], b"")?),
    ];
    cases.push(("no such object", refused));
    let first = cases.first().map(|(_, g)| (g.status, g.body.clone())).unwrap_or_default();
    for (what, got) in &cases {
        anyhow::ensure!(got.status == 403 && (got.status, got.body.clone()) == first, "{what}: {} {}", got.status, String::from_utf8_lossy(&got.body));
    }
    // What the refused puts sent isn't kept.
    let blobs = kiln::store::Store::open(&data.join("kiln.db"))?;
    let kept: i64 = kiln::rt::block_on(async move { blobs.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM blob_keys WHERE key IN ('lab/long', 'lab/short')", [], |r| r.get(0))?)).await })?;
    anyhow::ensure!(kept == 0, "refused puts kept {kept} objects");
    Ok(())
}

// ------------------------------------------------------------------------------ moving to S3

/// CP-T10.1, AC-T10.1.1: a server that kept its bytes on its local store moves them to an S3 one:
/// stopped, `kiln blobs copy`, `[blobs] store = "s3"`, started: what it kept is there, served from
/// S3 (a runner binary, sync's contents, a CI object), and a second copy has nothing to do.
#[test]
#[ignore = "needs an S3: KILN_TEST_S3 (docs/blobs.md, \"The labs\")"]
fn a_server_moves_its_bytes_to_s3() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let prefix = format!("labs/{}/move", hex::encode(rand::random::<[u8; 6]>()));
    let (target, binary) = other_platform();
    let contents = noise(5 << 20, 7);
    let digest = sha256(&contents);
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: &s3_section("local", &prefix)?, terminal: false })?;
    let auth = format!("Bearer {}", s.admin);
    let r = send(s.port, "POST", &format!("/api/updates/binaries?target={target}"), &[("Authorization", &auth)], &binary)?;
    anyhow::ensure!(r.status == 200, "the upload: {}", r.status);
    let build = r.json().get("build").and_then(Value::as_str).unwrap_or_default().to_owned();
    let r = send(s.port, "PUT", &format!("/api/sync/blobs/{digest}"), &[("Authorization", &auth)], &contents)?;
    anyhow::ensure!(r.status == 201, "sync's contents: {}", r.status);
    drop(s);
    let copy = || -> anyhow::Result<String> {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_kiln")).args(["--config", &dir.path().join("kiln.toml").to_string_lossy(), "blobs", "copy"]).env("HOME", dir.path().join("home")).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        anyhow::ensure!(out.status.success(), "kiln blobs copy: {text}");
        Ok(text)
    };
    let first = copy()?;
    anyhow::ensure!(first.contains(&format!("sync/{digest}")) && first.contains(&format!("binaries/kiln-{target}")), "{first}");
    let again = copy()?;
    anyhow::ensure!(again.contains("copied 0 objects"), "a second copy: {again}");
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: &s3_section("s3", &prefix)?, terminal: false })?;
    let auth = format!("Bearer {}", s.admin);
    wait_for("the binary, listed from S3", Duration::from_secs(30), || Ok(listed_build(s.port, &s.admin, target)?.as_deref() == Some(build.as_str())))?;
    let got = send(s.port, "GET", &format!("/dl/kiln/{target}/{build}"), &[], b"")?;
    anyhow::ensure!(got.status == 200 && got.body == binary, "the binary, served from S3: {}", got.status);
    wait_for("sync's contents listed from S3", Duration::from_secs(30), || Ok(send(s.port, "HEAD", &format!("/api/sync/blobs/{digest}"), &[("Authorization", &auth)], b"")?.status == 200))?;
    let got = send(s.port, "GET", &format!("/api/sync/blobs/{digest}"), &[("Authorization", &auth)], b"")?;
    anyhow::ensure!(got.status == 200 && got.body == contents, "sync's contents, served from S3: {}", got.status);
    anyhow::ensure!(held(Store::S3, &dir.path().join("data"), Some(&prefix), &format!("sync/{digest}"), &digest)?, "in the S3 store");
    Ok(())
}
