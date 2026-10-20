//! The store's remote tier (docs/ci.md W08-R02). With `[ci.store.s3]`, each chunk is kept in an
//! S3-compatible bucket too, at `<prefix>chunks/<2 hex>/<2 hex>/<sha256>` as the data directory
//! lays them out, and the data directory's copies are a cache of them:
//!
//! - an object is recorded only once each of its chunks is in the bucket ([`push`]), so what the
//!   store says it holds outlives the server's disk;
//! - a read that needs a chunk the cache no longer holds brings it back first, checked against
//!   its digest ([`fetch`]);
//! - [`trim`] keeps the cache under `cache_gb`, letting go of the least recently used chunks the
//!   bucket holds; [`backfill`] sends the chunks kept before the bucket was set up; collection
//!   deletes from the bucket what no object uses ([`forget`]).
//!
//! `cas_chunks.remote_at` says when a chunk was last sent to the bucket, or found there by an
//! upload that uses it. Collection deletes only chunks found there before its cutoff, and holds
//! [`DELETING`] alone while it does: an upload looks for its chunks in the bucket under it too, so
//! it never counts on one that's being deleted.
//!
//! Requests are signed with Signature Version 4 (`integrations::aws`) and addressed by path, so
//! any S3-compatible store serves: AWS S3, MinIO, Cloudflare R2, Ceph.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use parking_lot::Mutex;
use rusqlite::params;

use super::cas;
use crate::ci::integrations::aws::{self, Aws, Body, Call, Credentials};
use crate::config::ci::{S3Credentials, S3Store};
use crate::http::client::{Client, Method, Response, Roots};
use crate::hub::Hub;

/// Requests to the bucket in flight at once.
const AT_ONCE: usize = 8;
/// The most chunks [`backfill`] sends in one round of upkeep.
const BACKFILL_ROUND: i64 = 4096;

/// Held by uploads looking for their chunks in the bucket (together) and by collection's
/// deletions (alone): a chunk an upload found there isn't deleted from under it.
static DELETING: async_lock::RwLock<()> = async_lock::RwLock::new(());

/// Whether the store keeps its chunks in a bucket too.
pub fn on(hub: &Hub) -> bool {
    hub.cfg.ci.store.s3.is_some()
}

/// The bucket, signed in.
#[derive(Clone)]
pub struct Bucket {
    aws: Aws,
    http: Arc<Client>,
    base: String,
    name: String,
    prefix: String,
}

/// The bucket `[ci.store.s3]` names, with its credentials now; `None` without one.
pub async fn bucket(hub: &Hub) -> Result<Option<Bucket>> {
    let Some(s) = &hub.cfg.ci.store.s3 else { return Ok(None) };
    let creds = credentials(hub, &s.credentials, "[ci.store.s3]").await?;
    Ok(Some(Bucket { aws: Aws { creds, region: s.region.clone() }, http: http(s)?, base: s.endpoint.trim_end_matches('/').to_owned(), name: s.bucket.clone(), prefix: s.prefix.clone() }))
}

/// The store's HTTP client: the public roots, or the store's own CA's. One a process.
fn http(s: &S3Store) -> Result<Arc<Client>> {
    static KEPT: OnceLock<Mutex<HashMap<Option<PathBuf>, Arc<Client>>>> = OnceLock::new();
    let kept = KEPT.get_or_init(Default::default);
    if let Some(c) = kept.lock().get(&s.ca_file) {
        return Ok(Arc::clone(c));
    }
    let roots = match &s.ca_file {
        Some(p) => Roots::Pem(std::fs::read(p).with_context(|| format!("[ci.store.s3] ca_file {}", p.display()))?),
        None => Roots::WebPki,
    };
    let c = Arc::new(Client::new(roots, None)?);
    kept.lock().insert(s.ca_file.clone(), Arc::clone(&c));
    Ok(c)
}

/// S3 credentials as `c` says (`what` names the setting, for errors): an `aws` integration's,
/// minted for kiln itself as the server's own workload (W10-R11); a key pair; or the server's
/// environment.
pub async fn credentials(hub: &Hub, c: &S3Credentials, what: &str) -> Result<Credentials> {
    if let Some(name) = &c.integration {
        let i = crate::ci::integrations::get(hub, name).await?.with_context(|| format!("{what}: no integration `{name}`"))?;
        if i.kind != "aws" {
            bail!("{what}: `{name}` is a {} integration; S3 is signed in to through an aws one", i.kind);
        }
        let w = kiln_ci::oidc::Workload { repository: "kiln", environment: None, git_ref: None, sha: None, actor: "kiln", source: "service", app: None, deployment: None, trigger: Some("store") };
        return aws::credentials(hub, &i, &w).await;
    }
    if let Some(id) = &c.access_key_id {
        let secret = match (&c.secret_access_key_file, &c.secret_access_key_env) {
            (Some(p), _) => std::fs::read_to_string(p).with_context(|| format!("{what}: reading {}", p.display()))?.trim().to_owned(),
            (None, Some(v)) => std::env::var(v).with_context(|| format!("{what}: {v} isn't set"))?,
            (None, None) => bail!("{what}: access_key_id needs secret_access_key_file or secret_access_key_env"),
        };
        return Ok(Credentials { access_key_id: id.clone(), secret_access_key: secret, session_token: None, expires_at: None });
    }
    from_env().with_context(|| format!("{what}: no credentials: an integration, access_key_id, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY in the server's environment"))
}

/// Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
pub fn from_env() -> Result<Credentials> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
        (Some(id), Some(secret)) => Ok(Credentials { access_key_id: id, secret_access_key: secret, session_token: var("AWS_SESSION_TOKEN"), expires_at: None }),
        _ => bail!("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY aren't set"),
    }
}

impl Bucket {
    /// A chunk's key.
    fn key(&self, digest: &str) -> String {
        format!("{}chunks/{}/{}/{digest}", self.prefix, digest.get(0..2).unwrap_or("00"), digest.get(2..4).unwrap_or("00"))
    }

    async fn call(&self, method: Method, digest: &str, body: Body) -> Result<Response> {
        let path = format!("/{}/{}", self.name, aws::encode(&self.key(digest), true));
        self.aws.send_with(&self.http, Call { service: "s3", base: &self.base, method, path: &path, query: &[], headers: Vec::new(), body }).await
    }

    /// Puts a chunk. Its SHA-256 is its digest, which the store checks the bytes against.
    pub async fn put(&self, digest: &str, data: Bytes) -> Result<()> {
        let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let (tx, rx) = async_channel::bounded(1);
        tx.try_send(data).map_err(|_| anyhow::anyhow!("putting chunk {digest}: the body couldn't be queued"))?;
        drop(tx);
        let resp = self.call(Method::Put, digest, Body::Stream { rx, len, sha256: digest.to_owned() }).await?;
        if !resp.is_success() {
            bail!("putting chunk {digest} in {}: {}", self.name, refusal(resp).await);
        }
        Ok(())
    }

    /// A chunk, checked against its digest; `None` when the bucket hasn't it.
    pub async fn get(&self, digest: &str) -> Result<Option<Bytes>> {
        let resp = self.call(Method::Get, digest, Vec::new().into()).await?;
        if resp.status() == 404 {
            return Ok(None);
        }
        if !resp.is_success() {
            bail!("getting chunk {digest} from {}: {}", self.name, refusal(resp).await);
        }
        let data = resp.bytes(kiln_ci::store::cdc::MAX).await?;
        if crate::util::sha256_hex(&data) != digest {
            bail!("chunk {digest} from {} doesn't hash to its name", self.name);
        }
        Ok(Some(data))
    }

    /// Deletes a chunk; one already gone is no error.
    pub async fn delete(&self, digest: &str) -> Result<()> {
        let resp = self.call(Method::Delete, digest, Vec::new().into()).await?;
        if !resp.is_success() && resp.status() != 404 {
            bail!("deleting chunk {digest} from {}: {}", self.name, refusal(resp).await);
        }
        Ok(())
    }
}

/// What the store said as it refused: its code and message, and the status.
async fn refusal(resp: Response) -> String {
    let status = resp.status();
    let text = resp.bytes(64 << 10).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    match (aws::xml_text(&text, "Code"), aws::xml_text(&text, "Message")) {
        (Some(c), Some(m)) => format!("{c}: {m} (HTTP {status})"),
        (Some(c), None) => format!("{c} (HTTP {status})"),
        _ => format!("HTTP {status}"),
    }
}

/// Before an object of `chunks` is recorded: each chunk the bucket holds is marked as found
/// there now (so a collection under way leaves it), and each it doesn't goes there from the
/// cache. Without a bucket, nothing.
pub async fn push(hub: &Hub, chunks: &[(String, u64)]) -> Result<()> {
    if !on(hub) {
        return Ok(());
    }
    let mut seen = HashSet::new();
    let list: Vec<(String, u64)> = chunks.iter().filter(|(d, _)| seen.insert(d.clone())).cloned().collect();
    let digests: Vec<String> = list.iter().map(|(d, _)| d.clone()).collect();
    let now = crate::util::now_unix();
    // What's sent from here on is new to collection's cutoff, so only the looking needs the lock.
    let shared = DELETING.read().await;
    let held: HashSet<String> = hub
        .store
        .call(move |c| {
            let mut found = c.prepare("UPDATE cas_chunks SET remote_at = ?2 WHERE digest = ?1 AND remote_at IS NOT NULL")?;
            let mut held = HashSet::new();
            for d in digests {
                if found.execute(params![d, now])? > 0 {
                    held.insert(d);
                }
            }
            Ok(held)
        })
        .await?;
    drop(shared);
    let todo: Vec<(String, u64)> = list.into_iter().filter(|(d, _)| !held.contains(d)).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let b = bucket(hub).await?.context("no bucket")?;
    send(hub, &b, &todo).await?;
    Ok(())
}

/// Sends `todo` from the cache, [`AT_ONCE`] at a time, each batch marked as sent once it all is.
async fn send(hub: &Hub, b: &Bucket, todo: &[(String, u64)]) -> Result<usize> {
    let root = cas::root(hub);
    for batch in todo.chunks(AT_ONCE) {
        // Batches of `join_all`: hyper-rt's sockets want the task's own waker (cd::live).
        let puts = batch.iter().map(|(d, _)| {
            let path = cas::chunk_path(&root, d);
            async move {
                let data = crate::rt::blocking(move || std::fs::read(&path)).await?.with_context(|| format!("chunk {d} isn't in the data directory"))?;
                b.put(d, Bytes::from(data)).await
            }
        });
        for r in futures::future::join_all(puts).await {
            r?;
        }
        mark(hub, batch).await?;
    }
    Ok(todo.len())
}

/// Marks `chunks` as in the bucket now.
async fn mark(hub: &Hub, chunks: &[(String, u64)]) -> Result<()> {
    let list = chunks.to_vec();
    let now = crate::util::now_unix();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for (d, n) in &list {
                tx.execute(
                    "INSERT INTO cas_chunks (digest, size, remote_at) VALUES (?1, ?2, ?3) ON CONFLICT (digest) DO UPDATE SET remote_at = excluded.remote_at",
                    params![d, i64::try_from(*n).unwrap_or(i64::MAX), now],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Brings a chunk the cache doesn't hold back from the bucket into the data directory under
/// `root`: whether the bucket had it.
pub async fn fetch(b: &Bucket, root: &Path, digest: &str) -> Result<bool> {
    let Some(data) = b.get(digest).await? else { return Ok(false) };
    let (r, d) = (root.to_path_buf(), digest.to_owned());
    crate::rt::blocking(move || cas::keep_chunk(&r, &d, &data)).await??;
    Ok(true)
}

/// Which of `digests` the bucket holds.
pub async fn in_bucket(hub: &Hub, digests: Vec<String>) -> Result<HashSet<String>> {
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT 1 FROM cas_chunks WHERE digest = ?1 AND remote_at IS NOT NULL")?;
            let mut held = HashSet::new();
            for d in digests {
                if q.exists([&d])? {
                    held.insert(d);
                }
            }
            Ok(held)
        })
        .await
}

/// Keeps the cache under `cache_gb`: chunks the bucket holds leave the data directory, least
/// recently used first, none used within the last hour (an upload or a read may want it).
/// Returns how many went, and their bytes.
pub async fn trim(hub: &Hub) -> Result<(usize, u64)> {
    let Some(s) = &hub.cfg.ci.store.s3 else { return Ok((0, 0)) };
    let cap = s.cache_gb.saturating_mul(1 << 30);
    let root = cas::root(hub);
    let mut files = crate::rt::blocking(move || cas::chunk_files(&root)).await?;
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    if total <= cap {
        return Ok((0, 0));
    }
    files.sort_by_key(|f| f.used);
    let held = in_bucket(hub, files.iter().map(|f| f.digest.clone()).collect()).await?;
    let mut out = Vec::new();
    for f in files {
        if total <= cap {
            break;
        }
        if held.contains(&f.digest) && f.used.elapsed().is_ok_and(|age| age > cas::GRACE) {
            total = total.saturating_sub(f.size);
            out.push(f);
        }
    }
    crate::rt::blocking(move || {
        let gone: Vec<&cas::ChunkFile> = out.iter().filter(|f| std::fs::remove_file(&f.path).is_ok()).collect();
        (gone.len(), gone.iter().map(|f| f.size).sum())
    })
    .await
}

/// Sends chunks the data directory kept before the bucket was set up (or whose upload failed),
/// at most [`BACKFILL_ROUND`] a round. Returns how many.
pub async fn backfill(hub: &Hub) -> Result<usize> {
    if !on(hub) {
        return Ok(0);
    }
    // Collection deletes only chunks the bucket holds: these aren't among them.
    let rows: Vec<(String, u64)> = hub
        .store
        .call(|c| {
            let mut q = c.prepare("SELECT digest, size FROM cas_chunks WHERE remote_at IS NULL LIMIT ?1")?;
            let rows = q.query_map([BACKFILL_ROUND], |r| Ok((r.get::<_, String>(0)?, u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0))))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    // What the data directory still has: a chunk it lost can't be sent.
    let root = cas::root(hub);
    let here: Vec<(String, u64)> = crate::rt::blocking(move || rows.into_iter().filter(|(d, _)| cas::chunk_path(&root, d).is_file()).collect()).await?;
    let Some(b) = bucket(hub).await? else { return Ok(0) };
    send(hub, &b, &here).await
}

/// Collection's part in the bucket: chunks no object uses (`used` is every chunk one does),
/// found there last before `cutoff`, are deleted from it. Each one's row goes first, so an
/// upload that wants it later sends it again; a delete the store refuses puts the row back for
/// the next round. Returns how many went.
pub async fn forget(hub: &Hub, used: &HashSet<String>, cutoff: i64) -> Result<usize> {
    let Some(b) = bucket(hub).await? else { return Ok(0) };
    let _alone = DELETING.write().await;
    let stale: Vec<(String, i64, i64)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest, size, remote_at FROM cas_chunks WHERE remote_at IS NOT NULL AND remote_at < ?1")?;
            let rows = q.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let doomed: Vec<(String, i64, i64)> = stale.into_iter().filter(|(d, _, _)| !used.contains(d)).collect();
    if doomed.is_empty() {
        return Ok(0);
    }
    let rows = doomed.clone();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for (d, _, _) in &rows {
                tx.execute("DELETE FROM cas_chunks WHERE digest = ?1", [d])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    let (mut gone, mut kept) = (0usize, Vec::new());
    for batch in doomed.chunks(AT_ONCE) {
        let deletes = batch.iter().map(|(d, _, _)| b.delete(d));
        for (row, r) in batch.iter().zip(futures::future::join_all(deletes).await) {
            match r {
                Ok(()) => gone += 1,
                Err(e) => {
                    tracing::warn!(chunk = %row.0, error = %format!("{e:#}"), "a chunk no object uses stays in the bucket until the next collection");
                    kept.push(row.clone());
                }
            }
        }
    }
    if !kept.is_empty() {
        hub.store
            .call(move |c| {
                let tx = c.transaction()?;
                for (d, n, at) in &kept {
                    tx.execute("INSERT OR IGNORE INTO cas_chunks (digest, size, remote_at) VALUES (?1, ?2, ?3)", params![d, n, at])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
    }
    Ok(gone)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, SystemTime};

    use http::StatusCode;

    use super::*;
    use crate::ci::integrations::aws::Signable;
    use crate::http::server::{self as srv, Request};

    /// A fake S3: objects by path, and each request's signature checked as S3 checks it (the
    /// signed headers as they came, the body against its hash), with the one key pair it knows
    /// (`AKIDTEST`, `s3cr3t`, in `us-east-1`).
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) objects: Mutex<HashMap<String, Vec<u8>>>,
        pub(crate) puts: Mutex<usize>,
        pub(crate) gets: Mutex<usize>,
        pub(crate) refused: Mutex<Vec<String>>,
    }

    impl srv::Handler for Fake {
        async fn handle(self: Arc<Self>, req: Request) -> srv::Response {
            let (method, path) = (req.method().to_string(), req.uri().path().to_owned());
            let headers: Vec<(String, String)> = req.headers().iter().map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())).collect();
            let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let auth = header("authorization");
            let body = srv::body_bytes(req.into_body(), 16 << 20).await.unwrap_or_default();
            let names = auth.split("SignedHeaders=").nth(1).and_then(|r| r.split(',').next()).unwrap_or("");
            let signed: Vec<(String, String)> = names.split(';').map(|k| (k.to_owned(), header(k))).collect();
            let (date, payload) = (header("x-amz-date"), header("x-amz-content-sha256"));
            let creds = Credentials { access_key_id: "AKIDTEST".into(), secret_access_key: "s3cr3t".into(), session_token: None, expires_at: None };
            let want = aws::authorization(&creds, "us-east-1", "s3", &date, &Signable { method: &method, path: &path, query: &[], headers: &signed, payload_sha256: &payload }, false);
            if auth != want || crate::util::sha256_hex(&body) != payload {
                self.refused.lock().push(format!("{method} {path}"));
                return srv::response(
                    StatusCode::FORBIDDEN,
                    "application/xml",
                    srv::full(Bytes::from_static(
                        b"<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided.</Message></Error>",
                    )),
                );
            }
            match method.as_str() {
                "PUT" => {
                    self.objects.lock().insert(path, body.to_vec());
                    *self.puts.lock() += 1;
                    srv::status(StatusCode::OK)
                }
                "GET" => {
                    *self.gets.lock() += 1;
                    match self.objects.lock().get(&path) {
                        Some(b) => srv::response(StatusCode::OK, "application/octet-stream", srv::full(Bytes::from(b.clone()))),
                        None => srv::response(StatusCode::NOT_FOUND, "application/xml", srv::full(Bytes::from_static(b"<Error><Code>NoSuchKey</Code></Error>"))),
                    }
                }
                "DELETE" => {
                    self.objects.lock().remove(&path);
                    srv::status(StatusCode::NO_CONTENT)
                }
                _ => srv::status(StatusCode::METHOD_NOT_ALLOWED),
            }
        }
    }

    pub(crate) fn fake_s3() -> Result<(String, Arc<Fake>)> {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        let fake = Arc::new(Fake::default());
        let f = Arc::clone(&fake);
        crate::rt::spawn(async move {
            while let Ok(s) = tcp.accept().await {
                let (h, r) = (Arc::clone(&f), s.peer_addr().ok());
                crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
            }
        });
        Ok((format!("http://{addr}"), fake))
    }

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// Two hours ago, for what collection and trimming leave alone for an hour: the data
    /// directory's chunk files, and the store's objects and chunks.
    async fn age(hub: &Hub) -> Result<()> {
        let root = cas::root(hub);
        crate::rt::blocking(move || {
            for f in cas::chunk_files(&root) {
                if let Ok(file) = std::fs::File::options().write(true).open(&f.path) {
                    let _ = file.set_modified(SystemTime::now() - Duration::from_secs(7200));
                }
            }
        })
        .await?;
        hub.store
            .call(|c| {
                c.execute_batch(
                    "UPDATE cas_chunks SET remote_at = remote_at - 7200 WHERE remote_at IS NOT NULL; UPDATE cas_objects SET created_at = created_at - 7200, last_used_at = last_used_at - 7200;",
                )?;
                Ok(())
            })
            .await
    }

    async fn whole(hub: &Hub, obj: &cas::Object, start: u64, end: u64) -> Vec<u8> {
        let rx = cas::read(hub, obj, start, end).await;
        let mut out = Vec::new();
        while let Ok(b) = rx.recv().await {
            out.extend_from_slice(&b);
        }
        out
    }

    fn key(d: &str) -> String {
        format!("/kiln-store/kiln/cas/chunks/{}/{}/{d}", d.get(0..2).unwrap_or_default(), d.get(2..4).unwrap_or_default())
    }

    /// W08-R02, against the fake: an object's chunks go to the bucket as it's recorded, each
    /// request signed as S3 checks it; recorded again, nothing more is sent. With the cache
    /// trimmed to nothing, a read (whole, or a range) brings back what it needs. Chunks kept
    /// before the bucket was set up go there in upkeep. Collection deletes from the bucket the
    /// chunks of an object nothing refers to, and leaves those a cache entry's object uses. A
    /// wrong secret is refused, and nothing is recorded.
    #[test]
    fn w08_r02_chunks_in_a_bucket() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (base, fake) = fake_s3()?;
            let dir = tempfile::tempdir()?;
            let secret = dir.path().join("secret");
            std::fs::write(&secret, "s3cr3t\n")?;
            // As the server's configuration says it.
            let cfg: crate::config::Config = toml::from_str(&format!(
                "[ci.store.s3]\nendpoint = \"{base}\"\nbucket = \"kiln-store\"\ncache_gb = 0\ncredentials = {{ access_key_id = \"AKIDTEST\", secret_access_key_file = \"{}\" }}\n",
                secret.display()
            ))?;
            let s3 = cfg.ci.store.s3.clone().context("[ci.store.s3]")?;
            assert_eq!((s3.region.as_str(), s3.prefix.as_str()), ("us-east-1", "kiln/cas/"), "its defaults");
            let hub = crate::hub::tests::scratch_hub_with(move |c| c.ci.store.s3 = Some(s3)).await?;

            let data = noise(24 << 20, 9);
            let one = dir.path().join("one");
            std::fs::write(&one, &data)?;
            let obj = cas::put_file(&hub, &one).await?;
            let keys = || fake.objects.lock().keys().cloned().collect::<HashSet<String>>();
            let want: HashSet<String> = obj.chunks.iter().map(|(d, _)| key(d)).collect();
            assert!(obj.chunks.len() > 3, "{} chunks", obj.chunks.len());
            assert_eq!(keys(), want, "each chunk in the bucket, under its digest");
            assert!(fake.refused.lock().is_empty(), "every request signed as S3 checks it: {:?}", fake.refused.lock());
            let puts = *fake.puts.lock();
            cas::put_file(&hub, &one).await?;
            assert_eq!(*fake.puts.lock(), puts, "recorded again: nothing sent");

            // The cache trimmed to nothing: the chunks leave the data directory, and reads bring
            // back what they need.
            age(&hub).await?;
            let (gone, bytes) = trim(&hub).await?;
            assert_eq!((gone, bytes), (want.len(), obj.size), "every chunk the bucket holds");
            assert!(cas::chunk_files(&cas::root(&hub)).is_empty());
            assert!(whole(&hub, &obj, 0, obj.size).await == data, "the whole object, from the bucket");
            assert_eq!(cas::chunk_files(&cas::root(&hub)).len(), want.len(), "and back in the cache");
            age(&hub).await?;
            trim(&hub).await?;
            let (lo, hi) = (5u64 << 20, 9u64 << 20);
            assert!(whole(&hub, &obj, lo, hi).await == data.get(5 << 20..9 << 20).unwrap_or_default(), "a range");
            assert!(cas::chunk_files(&cas::root(&hub)).len() < want.len(), "only the chunks the range needs came back");
            let (first, _) = obj.chunks.first().cloned().unwrap_or_default();
            age(&hub).await?;
            trim(&hub).await?;
            assert!(cas::chunk_here(&hub, &first).await.is_some(), "a chunk asked for by the chunk protocol");

            // Kept before the bucket: chunked and recorded with no bucket, then sent in upkeep.
            let two = dir.path().join("two");
            std::fs::write(&two, noise(6 << 20, 21))?;
            let (old, _) = cas::put_file_blocking(&cas::root(&hub), &two)?;
            let o = old.clone();
            hub.store
                .call(move |c| {
                    for (d, n) in &o.chunks {
                        c.execute("INSERT OR IGNORE INTO cas_chunks (digest, size) VALUES (?1, ?2)", params![d, i64::try_from(*n).unwrap_or(0)])?;
                    }
                    c.execute(
                        "INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                        params![o.digest, i64::try_from(o.size).unwrap_or(0), serde_json::to_string(&o.chunks)?, crate::util::now_unix()],
                    )?;
                    Ok(())
                })
                .await?;
            assert_eq!(backfill(&hub).await?, old.chunks.len(), "sent in upkeep");
            assert!(old.chunks.iter().all(|(d, _)| keys().contains(&key(d))));
            assert_eq!(backfill(&hub).await?, 0, "once");

            // Collection: the first object kept by a cache entry, the second referred to by
            // nothing. Only the second's chunks leave the bucket.
            let d = obj.digest.clone();
            hub.store
                .call(move |c| {
                    c.execute("INSERT INTO ci_caches (repo_id, scope, key, version, object, size, created_at) VALUES ('r', 'refs/heads/main', 'k', 'v', ?1, 1, 0)", [&d])?;
                    Ok(())
                })
                .await?;
            age(&hub).await?;
            let collected = cas::collect(&hub).await?;
            assert_eq!((collected.objects, collected.remote), (1, old.chunks.len()), "{collected:?}");
            assert_eq!(keys(), want, "only the kept object's chunks stay");
            assert!(fake.refused.lock().is_empty(), "{:?}", fake.refused.lock());

            // A wrong secret: refused, and the object isn't recorded.
            let wrong = dir.path().join("wrong");
            std::fs::write(&wrong, "not-it")?;
            let mut bad = hub.cfg.ci.store.s3.clone().context("s3")?;
            bad.credentials.secret_access_key_file = Some(wrong);
            let other = crate::hub::tests::scratch_hub_with(move |c| c.ci.store.s3 = Some(bad)).await?;
            let three = dir.path().join("three");
            std::fs::write(&three, noise(1 << 20, 33))?;
            let e = cas::put_file(&other, &three).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains("SignatureDoesNotMatch"), "{e}");
            let recorded: i64 = other.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM cas_objects", [], |r| r.get(0))?)).await?;
            assert_eq!(recorded, 0);
            Ok(())
        })
    }

    /// W08-R02 against a real S3-compatible store (MinIO, R2, S3), by hand:
    ///
    /// ```text
    /// KILN_S3_ENDPOINT=http://127.0.0.1:9000 KILN_S3_BUCKET=kiln-test KILN_S3_KEY_ID=… KILN_S3_SECRET=… \
    ///   cargo test -p kiln w08_r02_a_real_store -- --ignored
    /// ```
    ///
    /// The bucket is made when it isn't there. An object goes there, the cache is trimmed, a read
    /// brings it back, and collection deletes it from the bucket.
    #[test]
    #[ignore = "needs a real S3-compatible store"]
    fn w08_r02_a_real_store() -> anyhow::Result<()> {
        let var = |k: &str| std::env::var(k).with_context(|| format!("{k} isn't set"));
        let (endpoint, bucket_name, id, secret) = (var("KILN_S3_ENDPOINT")?, var("KILN_S3_BUCKET")?, var("KILN_S3_KEY_ID")?, var("KILN_S3_SECRET")?);
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("secret");
            std::fs::write(&file, &secret)?;
            let s3 = S3Store {
                endpoint,
                bucket: bucket_name,
                region: std::env::var("KILN_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
                prefix: format!("kiln-test/{}/", crate::util::new_id()),
                credentials: crate::config::ci::S3Credentials { access_key_id: Some(id), secret_access_key_file: Some(file), ..Default::default() },
                cache_gb: 0,
                ca_file: None,
            };
            let hub = crate::hub::tests::scratch_hub_with(move |c| c.ci.store.s3 = Some(s3)).await?;
            let data = noise(20 << 20, 5);
            let one = dir.path().join("one");
            std::fs::write(&one, &data)?;
            let b = bucket(&hub).await?.context("bucket")?;
            let path = format!("/{}", b.name);
            let made = b.aws.send_with(&b.http, Call { service: "s3", base: &b.base, method: Method::Put, path: &path, query: &[], headers: Vec::new(), body: Vec::new().into() }).await?;
            anyhow::ensure!(made.is_success() || made.status() == 409, "making the bucket: {}", refusal(made).await);
            let obj = cas::put_file(&hub, &one).await?;
            for (d, _) in &obj.chunks {
                anyhow::ensure!(b.get(d).await?.is_some(), "chunk {d} isn't in the bucket");
            }
            age(&hub).await?;
            let (gone, _) = trim(&hub).await?;
            anyhow::ensure!(gone > 0 && cas::chunk_files(&cas::root(&hub)).is_empty(), "trimmed {gone}");
            anyhow::ensure!(whole(&hub, &obj, 0, obj.size).await == data, "read back from the bucket");
            age(&hub).await?;
            let collected = cas::collect(&hub).await?;
            anyhow::ensure!(collected.remote > 0, "{collected:?}");
            for (d, _) in &obj.chunks {
                anyhow::ensure!(b.get(d).await?.is_none(), "chunk {d} is still in the bucket");
            }
            Ok(())
        })
    }
}
