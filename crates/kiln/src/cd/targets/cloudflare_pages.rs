//! `cloudflare-pages`' I/O half (docs/cd-targets.md §3): Cloudflare's Pages API, its direct
//! upload as wrangler does it. The pure half, `kiln_cd::providers::cloudflare_pages`, renders the
//! objects.
//!
//! A release reads the built site and names each file as wrangler does (BLAKE3 of its base64 and
//! extension, 32 hex digits), asks Pages which it lacks (with the project's upload token), uploads
//! those, then deploys the manifest to the production branch and waits until it's deployed. kiln
//! keeps the deployments it made by site, so a rollback to one of them is Pages' rollback.

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::cloudflare_pages::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::cloudflare::{Cf, CfError, Part};
use crate::http::client::Method;

/// The longest a deployment takes.
const DEPLOYED: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its deployments kiln keeps to roll back to.
const KEEP: usize = 20;
/// How many files go up in one request.
const BATCH: usize = 50;

/// BLAKE3's hash, its default mode, 32 bytes out: how Pages names uploads. Written from the
/// reference implementation, as nothing else here needs it.
mod blake3 {
    const IV: [u32; 8] = [0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19];
    const CHUNK_START: u32 = 1;
    const CHUNK_END: u32 = 2;
    const PARENT: u32 = 4;
    const ROOT: u32 = 8;
    const BLOCK: usize = 64;
    const CHUNK: usize = 1024;

    fn g(a: u32, b: u32, c: u32, d: u32, x: u32, y: u32) -> (u32, u32, u32, u32) {
        let a = a.wrapping_add(b).wrapping_add(x);
        let d = (d ^ a).rotate_right(16);
        let c = c.wrapping_add(d);
        let b = (b ^ c).rotate_right(12);
        let a = a.wrapping_add(b).wrapping_add(y);
        let d = (d ^ a).rotate_right(8);
        let c = c.wrapping_add(d);
        let b = (b ^ c).rotate_right(7);
        (a, b, c, d)
    }

    fn round(s: [u32; 16], m: [u32; 16]) -> [u32; 16] {
        let [s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12, s13, s14, s15] = s;
        let [m0, m1, m2, m3, m4, m5, m6, m7, m8, m9, m10, m11, m12, m13, m14, m15] = m;
        // The columns, then the diagonals.
        let (s0, s4, s8, s12) = g(s0, s4, s8, s12, m0, m1);
        let (s1, s5, s9, s13) = g(s1, s5, s9, s13, m2, m3);
        let (s2, s6, s10, s14) = g(s2, s6, s10, s14, m4, m5);
        let (s3, s7, s11, s15) = g(s3, s7, s11, s15, m6, m7);
        let (s0, s5, s10, s15) = g(s0, s5, s10, s15, m8, m9);
        let (s1, s6, s11, s12) = g(s1, s6, s11, s12, m10, m11);
        let (s2, s7, s8, s13) = g(s2, s7, s8, s13, m12, m13);
        let (s3, s4, s9, s14) = g(s3, s4, s9, s14, m14, m15);
        [s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12, s13, s14, s15]
    }

    /// The message words' order for the next round.
    fn permute(m: [u32; 16]) -> [u32; 16] {
        let [m0, m1, m2, m3, m4, m5, m6, m7, m8, m9, m10, m11, m12, m13, m14, m15] = m;
        [m2, m6, m3, m10, m7, m0, m4, m13, m1, m11, m12, m5, m9, m14, m15, m8]
    }

    fn compress(cv: [u32; 8], block: [u32; 16], counter: u64, len: u32, flags: u32) -> [u32; 16] {
        let [c0, c1, c2, c3, c4, c5, c6, c7] = cv;
        let [i0, i1, i2, i3, ..] = IV;
        let mut s = [c0, c1, c2, c3, c4, c5, c6, c7, i0, i1, i2, i3, counter as u32, (counter >> 32) as u32, len, flags];
        let mut m = block;
        for r in 0..7 {
            s = round(s, m);
            if r < 6 {
                m = permute(m);
            }
        }
        let [s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12, s13, s14, s15] = s;
        [s0 ^ s8, s1 ^ s9, s2 ^ s10, s3 ^ s11, s4 ^ s12, s5 ^ s13, s6 ^ s14, s7 ^ s15, s8 ^ c0, s9 ^ c1, s10 ^ c2, s11 ^ c3, s12 ^ c4, s13 ^ c5, s14 ^ c6, s15 ^ c7]
    }

    fn first8(w: [u32; 16]) -> [u32; 8] {
        let [a, b, c, d, e, f, g, h, ..] = w;
        [a, b, c, d, e, f, g, h]
    }

    /// A block's words, little-endian, zero past its end.
    fn words(block: &[u8]) -> [u32; 16] {
        let mut w = [0u32; 16];
        let mut padded = block.to_vec();
        padded.resize(BLOCK, 0);
        for (x, c) in w.iter_mut().zip(padded.chunks_exact(4)) {
            *x = u32::from_le_bytes(c.try_into().unwrap_or([0; 4]));
        }
        w
    }

    /// A node not yet compressed: its chaining value is one compression, the root another.
    struct Output {
        cv: [u32; 8],
        block: [u32; 16],
        counter: u64,
        len: u32,
        flags: u32,
    }

    impl Output {
        fn chaining_value(&self) -> [u32; 8] {
            first8(compress(self.cv, self.block, self.counter, self.len, self.flags))
        }

        fn root(&self) -> [u8; 32] {
            let w = first8(compress(self.cv, self.block, 0, self.len, self.flags | ROOT));
            let mut out = [0u8; 32];
            for (o, x) in out.chunks_exact_mut(4).zip(w) {
                o.copy_from_slice(&x.to_le_bytes());
            }
            out
        }
    }

    fn chunk(input: &[u8], counter: u64) -> Output {
        let blocks: Vec<&[u8]> = if input.is_empty() { vec![input] } else { input.chunks(BLOCK).collect() };
        let last = blocks.len().saturating_sub(1);
        let mut cv = IV;
        for (j, b) in blocks.iter().enumerate() {
            let start = if j == 0 { CHUNK_START } else { 0 };
            if j == last {
                return Output { cv, block: words(b), counter, len: b.len() as u32, flags: start | CHUNK_END };
            }
            cv = first8(compress(cv, words(b), counter, BLOCK as u32, start));
        }
        Output { cv, block: [0; 16], counter, len: 0, flags: CHUNK_END }
    }

    fn parent(left: [u32; 8], right: [u32; 8]) -> Output {
        let mut block = [0u32; 16];
        for (b, x) in block.iter_mut().zip(left.into_iter().chain(right)) {
            *b = x;
        }
        Output { cv: IV, block, counter: 0, len: BLOCK as u32, flags: PARENT }
    }

    pub fn hash(input: &[u8]) -> [u8; 32] {
        let chunks: Vec<&[u8]> = if input.is_empty() { vec![input] } else { input.chunks(CHUNK).collect() };
        let n = chunks.len();
        // Completed subtrees' chaining values, merged as each pair completes.
        let mut stack: Vec<[u32; 8]> = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            let out = chunk(c, i as u64);
            if i + 1 == n {
                let mut node = out;
                while let Some(left) = stack.pop() {
                    node = parent(left, node.chaining_value());
                }
                return node.root();
            }
            let mut cv = out.chaining_value();
            let mut total = i as u64 + 1;
            while total & 1 == 0 {
                if let Some(left) = stack.pop() {
                    cv = parent(left, cv).chaining_value();
                }
                total >>= 1;
            }
            stack.push(cv);
        }
        [0; 32]
    }

    #[cfg(test)]
    mod tests {
        /// BLAKE3's published test vectors: the input is bytes 0, 1, …, 250, 0, 1, … of each length.
        #[test]
        fn hashes_match_the_test_vectors() {
            let input = |n: usize| (0..n).map(|i| (i % 251) as u8).collect::<Vec<u8>>();
            let h = |b: &[u8]| hex::encode(super::hash(b));
            assert_eq!(h(b""), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
            assert_eq!(h(b"abc"), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
            for (n, want) in [
                (1, "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"),
                (1023, "10108970eeda3eb932baac1428c7a2163b0e924c9a9e25b35bba72b28f70bd11"),
                (1024, "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7"),
                (1025, "d00278ae47eb27b34faecf67b4fe263f82d5412916c1ffd97c8cb7fb814b8444"),
                (2048, "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a"),
                (2049, "5f4d72f40d7a5f82b15ca2b2e44b1de3c2ef86c426c95c1af0b6879522563030"),
                (3072, "b98cb0ff3623be03326b373de6b9095218513e64f1ee2edd2525c7ad1e5cffd2"),
                (3073, "7124b49501012f81cc7f11ca069ec9226cecb8a2c850cfe644e327d22d3e1cd3"),
                (4096, "015094013f57a5277b59d8475c0501042c0b642e531b0a1c8f58d2163229e969"),
                (4097, "9b4052b38f1c5fc8b1f9ff7ac7b27cd242487b3d890d15c96a1c25b8aa0fb995"),
            ] {
                assert_eq!(h(&input(n)), want, "{n} bytes");
            }
        }
    }
}

/// A file's name in Pages' asset store, as wrangler gives it: BLAKE3 of its base64 and its
/// extension, its first 32 hex digits.
pub fn asset_hash(path: &str, data: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.').map_or("", |(_, e)| e);
    let full = hex::encode(blake3::hash(format!("{b64}{ext}").as_bytes()));
    full.get(..32).unwrap_or(&full).to_owned()
}

/// A file's content type, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js" | "mjs") => "application/javascript",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("txt") => "text/plain",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn api<'a>(cx: &'a Cx<'_>) -> Result<(Cf<'a>, String)> {
    let (token, config) = cx.token()?;
    let account = config.get("account_id").and_then(Value::as_str).filter(|a| !a.is_empty()).context("the Cloudflare account's integration has no account_id")?;
    Ok((Cf { token }, account.to_owned()))
}

fn project_path(account: &str, name: &str) -> String {
    format!("/accounts/{account}/pages/projects/{}", encode(name, false))
}

async fn project(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let (cf, account) = api(cx)?;
    cx.conn.budget().await;
    match cf.call(Method::Get, &project_path(&account, name), None).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if CfError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(proj) = project(cx, name).await? else { return Ok(None) };
    let rec = cx.applied(GROUP, "Project", "", name);
    let canonical = proj.pointer("/canonical_deployment/id").and_then(Value::as_str);
    let runs = rec.as_ref().is_some_and(|r| canonical.is_some() && r.made.get("deployment").and_then(Value::as_str) == canonical);
    Ok(p::live_project(&proj, rec.as_ref().map(|r| &r.object), runs))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "Project" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the account holds: its Pages projects (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let (cf, account) = api(cx)?;
    cx.conn.budget().await;
    let projects = cf.call(Method::Get, &format!("/accounts/{account}/pages/projects"), None).await?;
    let out: Vec<Value> = projects.as_array().into_iter().flatten().map(|p| json!({ "kind": "Project", "name": p.get("name"), "summary": p.get("subdomain"), "updated": p.pointer("/latest_deployment/created_on") })).collect();
    Ok(json!({ "resources": out }))
}

/// A project as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(proj) = project(cx, name).await? else { bail!("no Pages project {name} here") };
    Ok(json!({ "definition": { "project": name, "site": "${{ release.artifacts.site.digest }}", "branch": proj.get("production_branch") }, "note": "Pages doesn't say where a deployment's files came from: point site at the site CI builds" }))
}

/// Waits for a deployment to be deployed: why, if a stage failed.
async fn deployed(cx: &Cx<'_>, path: &str, what: &str) -> Result<()> {
    let d = until(cx.conn, DEPLOYED, what, || async {
        let (cf, _) = api(cx)?;
        let d = cf.call(Method::Get, path, None).await?;
        let (stage, status) = (d.pointer("/latest_stage/name").and_then(Value::as_str), d.pointer("/latest_stage/status").and_then(Value::as_str));
        Ok((status == Some("failure") || (stage == Some("deploy") && status == Some("success"))).then_some(d))
    })
    .await?;
    if d.pointer("/latest_stage/status").and_then(Value::as_str) == Some("failure") {
        bail!("{what}: the deployment failed at its {} stage", d.pointer("/latest_stage/name").and_then(Value::as_str).unwrap_or("last"));
    }
    Ok(())
}

/// Sends the site's files Pages lacks, as wrangler does: the deployment's manifest.
async fn upload(cx: &Cx<'_>, account: &str, name: &str, artifact: &str) -> Result<Map<String, Value>> {
    let files = super::aws_static::read_site(cx, artifact).await?;
    let mut manifest = Map::new();
    let mut by_hash: std::collections::BTreeMap<String, (String, Vec<u8>)> = std::collections::BTreeMap::new();
    for (path, data) in files {
        let h = asset_hash(&path, &data);
        manifest.insert(format!("/{path}"), json!(h));
        by_hash.insert(h, (path, data));
    }
    let (cf, _) = api(cx)?;
    cx.conn.budget().await;
    let token = cf.call(Method::Get, &format!("{}/upload-token", project_path(account, name)), None).await?;
    let jwt = token.get("jwt").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Pages gave no upload token"))?.to_owned();
    let assets = Cf { token: &jwt };
    let hashes: Vec<&String> = by_hash.keys().collect();
    cx.conn.budget().await;
    let missing = assets.call(Method::Post, "/pages/assets/check-missing", Some(&json!({ "hashes": hashes }))).await?;
    let missing: Vec<String> = missing.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    for batch in missing.chunks(BATCH) {
        let payload: Vec<Value> = batch
            .iter()
            .filter_map(|h| by_hash.get(h).map(|(path, data)| json!({ "key": h, "value": base64::engine::general_purpose::STANDARD.encode(data), "metadata": { "contentType": content_type(path) }, "base64": true })))
            .collect();
        cx.conn.budget().await;
        assets.call(Method::Post, "/pages/assets/upload", Some(&Value::Array(payload))).await?;
    }
    cx.conn.budget().await;
    assets.call(Method::Post, "/pages/assets/upsert-hashes", Some(&json!({ "hashes": hashes }))).await?;
    Ok(manifest)
}

/// Applies a Project: made if it isn't there, then its site deployed to production unless it
/// serves already; a deployment kiln made of it before is rolled back to.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let (cf, account) = api(cx)?;
    let branch = desired.pointer("/spec/productionBranch").and_then(Value::as_str).unwrap_or("main").to_owned();
    let proj = match project(cx, name).await? {
        Some(p) => p,
        None => {
            cx.conn.budget().await;
            cf.call(Method::Post, &format!("/accounts/{account}/pages/projects"), Some(&json!({ "name": name, "production_branch": branch }))).await?
        }
    };
    let artifact = desired.pointer("/spec/site/artifact").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("project {name} has no site"))?.to_owned();
    let prev = cx.applied(GROUP, "Project", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let mut made: Map<String, Value> = prev.get("deployments").and_then(Value::as_object).cloned().unwrap_or_default();
    let canonical = proj.pointer("/canonical_deployment/id").and_then(Value::as_str).map(str::to_owned);
    let what = format!("deploying Pages project {name}");
    let base = project_path(&account, name);
    let deployment = if prev.get("artifact").and_then(Value::as_str) == Some(artifact.as_str()) && canonical.is_some() && canonical.as_deref() == prev.get("deployment").and_then(Value::as_str) {
        canonical.unwrap_or_default()
    } else if let Some(before) = made.get(&artifact).and_then(Value::as_str).map(str::to_owned) {
        // A deployment kiln made of this site before: Pages rolls back to it.
        cx.conn.budget().await;
        cf.call(Method::Post, &format!("{base}/deployments/{before}/rollback"), None).await?;
        before
    } else {
        let manifest = upload(cx, &account, name, &artifact).await?;
        let parts = [
            Part { name: "manifest".into(), filename: None, content_type: "application/json".into(), data: serde_json::to_vec(&manifest)? },
            Part { name: "branch".into(), filename: None, content_type: "text/plain".into(), data: branch.clone().into_bytes() },
        ];
        cx.conn.budget().await;
        let d = cf.upload(Method::Post, &format!("{base}/deployments"), &parts).await?;
        let id = d.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Pages answered no deployment"))?.to_owned();
        cx.record(GROUP, "Project", "", name, Some(applied(desired, json!({ "artifact": artifact, "deployment": id, "deployments": made })))).await?;
        deployed(cx, &format!("{base}/deployments/{id}"), &what).await?;
        id
    };
    made.insert(artifact.clone(), json!(deployment));
    while made.len() > KEEP {
        let Some(k) = made.keys().next().cloned() else { break };
        made.remove(&k);
    }
    cx.record(GROUP, "Project", "", name, Some(applied(desired, json!({ "artifact": artifact, "deployment": deployment, "deployments": made })))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("Pages project {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let (cf, account) = api(cx)?;
    cx.conn.budget().await;
    match cf.call(Method::Delete, &project_path(&account, &key.name), None).await {
        Ok(_) => Ok(()),
        Err(e) if CfError::is_not_found(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{self, FakeCf, MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    fn zip_of(files: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, text) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())?;
            w.write_all(text.as_bytes())?;
        }
        Ok(w.finish()?.into_inner())
    }

    /// A Pages project on a faked Cloudflare, release by release: the first makes the project,
    /// uploads its files by wrangler's hashes and deploys them to production; the same release
    /// again deploys nothing; the next uploads only the file that changed; the release before,
    /// rolled back to, is Pages' rollback, nothing uploaded; one whose deployment fails reads as
    /// degraded, production on the deployment before; then the project goes.
    #[test]
    fn projects_deploy_each_releases_site() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let cf = Arc::new(FakeCf::default());
            let base = fake::start_cf(cf.clone()).await?;
            crate::ci::integrations::override_endpoint("cloudflare", &base);
            let place = json!({ "account": "cf-test" });
            let (target, _) = kiln_cd::providers::target_of("cloudflare-pages", place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: "cf-token".into(), config: json!({ "account_id": "acct1" }) }, records: &records, files: &files };
            let spec = json!({ "project": "shop-site", "site": "${{ release.artifacts.site.digest }}" });
            let release = |version: &str, site: &[(&str, &str)]| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(&zip_of(site)?));
                released_to("cloudflare-pages", &place, spec.clone(), version, &[], &[("site", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no project"))
            };
            let names = [(String::new(), "shop-site".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let calls = || cf.state.lock().pages_calls.clone();
            let v1 = [("index.html", "<h1>shop 1</h1>"), ("app.js", "app()")];
            let v2 = [("index.html", "<h1>shop 2</h1>"), ("app.js", "app()")];

            let p1 = release("1.0.0", &v1)?;
            let up = apply(&cx, &p1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&p1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(calls(), ["CreateProject shop-site", "Upload 2 assets", "Deploy shop-site 2 files"]);
            assert!(cf.state.lock().assets.contains_key(&asset_hash("index.html", b"<h1>shop 1</h1>")), "named as wrangler names them");
            apply(&cx, &release("1.0.0", &v1)?).await?;
            assert_eq!(calls().len(), 3, "the same release again deploys nothing");

            let p2 = release("1.1.0", &v2)?;
            apply(&cx, &p2).await?;
            assert_eq!(calls().get(3..).map(<[String]>::to_vec), Some(vec!["Upload 1 assets".to_owned(), "Deploy shop-site 2 files".to_owned()]), "only the file that changed went up");

            // Rolled back to the release before: Pages' rollback, nothing uploaded.
            let back = apply(&cx, &p1).await?;
            assert!(calls().last().is_some_and(|c| c.starts_with("Rollback shop-site dpl-")), "{:?}", calls());
            assert!(synced(&p1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose deployment fails: it fails, read as degraded, production as it was.
            let failed = apply(&cx, &release("1.2.0", &[("index.html", "broken")])?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("deployment failed"), "{failed}");
            let down = list(&cx, "Project", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-site")));
            delete(&cx, &key_of(&p1)).await?;
            assert!(list(&cx, "Project", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
