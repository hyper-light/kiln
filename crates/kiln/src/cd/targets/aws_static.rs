//! `aws-static`'s I/O half (docs/cd-targets.md §3): S3's and CloudFront's REST APIs. The pure
//! half, `kiln_cd::providers::aws_static`, renders the site and the switch and reads their answers.
//!
//! A site's apply reads the built site (a zip or a .tar.gz CI made) and puts each file under the
//! release's own prefix: copied over in S3 from the release before where that holds the same file
//! (by its hash), uploaded otherwise; the manifest last, which says the prefix is whole. A release
//! whose prefix is whole already (a rollback) uploads nothing. Then `current.json` names it, and
//! the releases beyond `keep` go. A distribution's apply switches its origin's path to the
//! release's prefix (its config sent back as CloudFront gave it, that one element changed, under
//! its ETag) and invalidates what it caches; it's deployed once CloudFront says so.

use std::collections::BTreeMap;
use std::io::Read as _;

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::aws_static::{self as p, CLOUDFRONT, CURRENT, MANIFEST, S3};
use serde_json::{Map, Value, json};

use super::{Cx, applied};
use crate::ci::integrations::aws::{self, Aws, Body, Call, encode, endpoint, xml_all, xml_text};
use crate::http::client::Method;

/// The most a built site may hold, read whole.
const SITE_MAX: usize = 512 << 20;
/// CloudFront's API version in its paths.
const CF: &str = "/2020-05-31";

/// One S3 request: its status, an ETag and the body.
async fn s3(cx: &Cx<'_>, method: Method, path: &str, query: &[(String, String)], headers: Vec<(String, String)>, body: Vec<u8>) -> Result<(u16, String, Vec<u8>)> {
    let aws = cx.aws()?;
    let base = endpoint("s3", &aws.region);
    cx.conn.budget().await;
    let resp = aws.send(Call { service: "s3", base: &base, method, path, query, headers, body: Body::Bytes(body) }).await?;
    let (code, etag) = (resp.status(), resp.header("etag").unwrap_or_default().to_owned());
    let bytes = if method == Method::Head { Vec::new() } else { resp.bytes(64 << 20).await?.to_vec() };
    Ok((code, etag, bytes))
}

fn object_path(bucket: &str, key: &str) -> String {
    format!("/{bucket}/{}", encode(key, true))
}

fn failed(what: &str, code: u16, body: &[u8]) -> anyhow::Error {
    let text = String::from_utf8_lossy(body);
    anyhow::anyhow!("{what}: HTTP {code} {}", xml_text(&text, "Message").unwrap_or_else(|| text.chars().take(200).collect()))
}

async fn get_json(cx: &Cx<'_>, bucket: &str, key: &str) -> Result<Option<Value>> {
    let (code, _, body) = s3(cx, Method::Get, &object_path(bucket, key), &[], Vec::new(), Vec::new()).await?;
    match code {
        200 => Ok(serde_json::from_slice(&body).ok()),
        404 | 403 => Ok(None),
        c => Err(failed(&format!("reading s3://{bucket}/{key}"), c, &body)),
    }
}

async fn put(cx: &Cx<'_>, bucket: &str, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<()> {
    let (code, _, body) = s3(cx, Method::Put, &object_path(bucket, key), &[], vec![("content-type".into(), content_type.into())], bytes).await?;
    if !(200..300).contains(&code) {
        return Err(failed(&format!("uploading s3://{bucket}/{key}"), code, &body));
    }
    Ok(())
}

async fn copy(cx: &Cx<'_>, bucket: &str, from: &str, to: &str) -> Result<()> {
    let source = format!("/{bucket}/{}", encode(from, true));
    let (code, _, body) = s3(cx, Method::Put, &object_path(bucket, to), &[], vec![("x-amz-copy-source".into(), source)], Vec::new()).await?;
    if !(200..300).contains(&code) {
        return Err(failed(&format!("copying s3://{bucket}/{from} to {to}"), code, &body));
    }
    Ok(())
}

/// Every key under `prefix`.
async fn keys_under(cx: &Cx<'_>, bucket: &str, prefix: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = vec![("list-type".to_owned(), "2".to_owned()), ("prefix".to_owned(), prefix.to_owned())];
        if let Some(t) = &token {
            q.push(("continuation-token".to_owned(), t.clone()));
        }
        let (code, _, body) = s3(cx, Method::Get, &format!("/{bucket}"), &q, Vec::new(), Vec::new()).await?;
        if code != 200 {
            return Err(failed(&format!("listing s3://{bucket}/{prefix}"), code, &body));
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        out.extend(xml_all(&text, "Contents").into_iter().filter_map(|c| xml_text(c, "Key")));
        token = (xml_text(&text, "IsTruncated").as_deref() == Some("true")).then(|| xml_text(&text, "NextContinuationToken")).flatten();
        if token.is_none() {
            return Ok(out);
        }
    }
}

async fn delete_under(cx: &Cx<'_>, bucket: &str, prefix: &str) -> Result<usize> {
    let keys = keys_under(cx, bucket, prefix).await?;
    for k in &keys {
        let (code, _, body) = s3(cx, Method::Delete, &object_path(bucket, k), &[], Vec::new(), Vec::new()).await?;
        if !(200..300).contains(&code) && code != 404 {
            return Err(failed(&format!("deleting s3://{bucket}/{k}"), code, &body));
        }
    }
    Ok(keys.len())
}

/// A built site's files, by their paths in it: a zip or a .tar.gz.
pub fn site_files(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let clean = |name: &str| -> Result<Option<String>> {
        let path = name.trim_start_matches("./").trim_start_matches('/');
        if path.is_empty() || path.ends_with('/') {
            return Ok(None);
        }
        if path.split('/').any(|p| p == ".." || p.is_empty()) {
            bail!("the site holds `{name}`, which leaves it");
        }
        Ok(Some(path.to_owned()))
    };
    let mut out = BTreeMap::new();
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| anyhow::anyhow!("the site isn't a zip that reads: {e}"))?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| anyhow::anyhow!("the site's zip: {e}"))?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().map_err(|e| anyhow::anyhow!("the site's zip: {e}"))?.into_owned();
            let Some(path) = clean(&name)? else { continue };
            let mut data = Vec::new();
            f.read_to_end(&mut data)?;
            out.insert(path, data);
        }
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in tar.entries()? {
            let mut e = entry?;
            if !e.header().entry_type().is_file() {
                continue;
            }
            let name = e.path()?.to_string_lossy().into_owned();
            let Some(path) = clean(&name)? else { continue };
            let mut data = Vec::new();
            e.read_to_end(&mut data)?;
            out.insert(path, data);
        }
    } else {
        bail!("the site is neither a zip nor a .tar.gz");
    }
    if out.is_empty() {
        bail!("the site holds no files");
    }
    Ok(out)
}

/// The built site CI made, read whole.
pub async fn read_site(cx: &Cx<'_>, site: &str) -> Result<BTreeMap<String, Vec<u8>>> {
    let (rx, len) = cx.files.open(site.trim_start_matches("sha256:")).await?;
    if usize::try_from(len).unwrap_or(usize::MAX) > SITE_MAX {
        bail!("the site is {len} bytes, past the {} MiB kiln reads", SITE_MAX >> 20);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    while let Ok(chunk) = rx.recv().await {
        bytes.extend_from_slice(&chunk);
    }
    site_files(&bytes)
}

fn spec_of(o: &Value) -> (String, String, String, u64) {
    let g = |k: &str| obj::str_at(o, &["spec", k]).unwrap_or_default().to_owned();
    (g("bucket"), g("prefix"), g("site"), o.pointer("/spec/keep").and_then(Value::as_u64).unwrap_or(p::KEEP))
}

async fn site(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(a) = cx.applied(S3, "Site", "", name) else { return Ok(None) };
    let (bucket, prefix, _, keep) = spec_of(&a.object);
    let Some(current) = get_json(cx, &bucket, &format!("{prefix}/{CURRENT}")).await? else { return Ok(None) };
    Ok(p::live_site(name, &bucket, &prefix, &current, keep))
}

/// CloudFront, which is signed for us-east-1 wherever its target is.
fn cloudfront(cx: &Cx<'_>) -> Result<Aws> {
    Ok(Aws { creds: cx.aws()?.creds.clone(), region: "us-east-1".into() })
}

async fn cf(cx: &Cx<'_>, method: Method, path: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Result<(u16, String, String)> {
    let aws = cloudfront(cx)?;
    let base = endpoint("cloudfront", "us-east-1");
    cx.conn.budget().await;
    let resp = aws.send(Call { service: "cloudfront", base: &base, method, path, query: &[], headers, body: Body::Bytes(body) }).await?;
    let (code, etag) = (resp.status(), resp.header("etag").unwrap_or_default().to_owned());
    Ok((code, etag, String::from_utf8_lossy(&resp.bytes(4 << 20).await?).into_owned()))
}

async fn distribution(cx: &Cx<'_>, id: &str) -> Result<Option<Value>> {
    let (code, _, body) = cf(cx, Method::Get, &format!("{CF}/distribution/{id}"), Vec::new(), Vec::new()).await?;
    match code {
        200 => {}
        404 => return Ok(None),
        c => return Err(failed(&format!("reading distribution {id}"), c, body.as_bytes())),
    }
    let d = aws::xml_json(&body).get("Distribution").cloned().unwrap_or(Value::Null);
    let wanted = cx.applied(CLOUDFRONT, "Distribution", "", id).map(|a| a.object);
    let bucket = wanted.as_ref().and_then(|w| obj::str_at(w, &["spec", "bucket"]).map(str::to_owned)).unwrap_or_default();
    let origin = wanted.as_ref().and_then(|w| obj::str_at(w, &["spec", "origin"]).map(str::to_owned));
    Ok(p::live_distribution(&d, &bucket, origin.as_deref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        let o = match kind {
            "Site" => site(cx, name).await?,
            "Distribution" => distribution(cx, name).await?,
            _ => None,
        };
        out.extend(o);
    }
    Ok(out)
}

pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    match obj::kind(desired) {
        "Site" => apply_site(cx, desired).await,
        "Distribution" => apply_distribution(cx, desired).await,
        other => bail!("aws-static doesn't apply {other}"),
    }
}

async fn apply_site(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let (bucket, prefix, site_ref, keep) = spec_of(desired);
    let release = p::release_prefix(&prefix, &site_ref);
    let record = cx.applied(S3, "Site", "", name);
    let mut history: Vec<String> = record.as_ref().and_then(|r| r.made.get("history").and_then(Value::as_array).cloned()).unwrap_or_default().into_iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let (mut uploaded, mut copied) = (0usize, 0usize);
    let manifest_key = format!("{release}/{MANIFEST}");
    let whole = get_json(cx, &bucket, &manifest_key).await?;
    let files: Map<String, Value> = match whole.and_then(|m| m.get("files").and_then(Value::as_object).cloned()) {
        // Whole already (a rollback, or the same release again): nothing to put there.
        Some(files) => files,
        None => {
            // What the release before holds, by path and hash: copied over rather than uploaded.
            let before = match history.first() {
                Some(prev) => get_json(cx, &bucket, &format!("{prev}/{MANIFEST}")).await?.and_then(|m| m.get("files").and_then(Value::as_object).cloned()).map(|f| (prev.clone(), f)),
                None => None,
            };
            let mut files = Map::new();
            for (path, data) in read_site(cx, &site_ref).await? {
                let sha = crate::util::sha256_hex(&data);
                let key = format!("{release}/{path}");
                match &before {
                    Some((prev, f)) if f.get(&path).and_then(Value::as_str) == Some(sha.as_str()) => {
                        copy(cx, &bucket, &format!("{prev}/{path}"), &key).await?;
                        copied += 1;
                    }
                    _ => {
                        put(cx, &bucket, &key, data, p::content_type(&path)).await?;
                        uploaded += 1;
                    }
                }
                files.insert(path, Value::String(sha));
            }
            // Last: it says the prefix is whole.
            put(cx, &bucket, &manifest_key, serde_json::to_vec(&json!({ "site": site_ref, "files": files }))?, "application/json").await?;
            files
        }
    };
    let current = json!({ "site": site_ref, "path": format!("/{release}"), "files": files.len() });
    put(cx, &bucket, &format!("{prefix}/{CURRENT}"), serde_json::to_vec(&current)?, "application/json").await?;
    history.retain(|h| *h != release);
    history.insert(0, release.clone());
    // Releases beyond what the site keeps go.
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    for gone in history.split_off(keep.min(history.len())) {
        delete_under(cx, &bucket, &format!("{gone}/")).await?;
    }
    cx.record(S3, "Site", "", name, Some(applied(desired, json!({ "prefix": release, "uploaded": uploaded, "copied": copied, "history": history })))).await?;
    let mut live = site(cx, name).await?.ok_or_else(|| anyhow::anyhow!("site {name} reads as nothing"))?;
    if let Some(st) = live.get_mut("status").and_then(Value::as_object_mut) {
        st.insert("uploaded".into(), json!(uploaded));
        st.insert("copied".into(), json!(copied));
    }
    Ok(live)
}

async fn apply_distribution(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let id = obj::name(desired);
    let path = obj::str_at(desired, &["spec", "originPath"]).unwrap_or_default().to_owned();
    let bucket = obj::str_at(desired, &["spec", "bucket"]).unwrap_or_default().to_owned();
    let origin = obj::str_at(desired, &["spec", "origin"]).map(str::to_owned);
    let (code, etag, config) = cf(cx, Method::Get, &format!("{CF}/distribution/{id}/config"), Vec::new(), Vec::new()).await?;
    if code != 200 {
        return Err(failed(&format!("reading distribution {id}'s config"), code, config.as_bytes()));
    }
    let switched = p::with_origin_path(&config, &bucket, origin.as_deref(), &path).map_err(anyhow::Error::msg)?;
    let mut invalidation = Value::Null;
    if switched != config {
        let (code, _, body) = cf(cx, Method::Put, &format!("{CF}/distribution/{id}/config"), vec![("if-match".into(), etag), ("content-type".into(), "text/xml".into())], switched.into_bytes()).await?;
        if !(200..300).contains(&code) {
            return Err(failed(&format!("switching distribution {id} to {path}"), code, body.as_bytes()));
        }
        let reference = format!("kiln-{}-{}", path.rsplit('/').next().unwrap_or_default(), crate::util::now_ms());
        let batch = format!("<InvalidationBatch xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\"><Paths><Quantity>1</Quantity><Items><Path>/*</Path></Items></Paths><CallerReference>{reference}</CallerReference></InvalidationBatch>");
        let (code, _, body) = cf(cx, Method::Post, &format!("{CF}/distribution/{id}/invalidation"), vec![("content-type".into(), "text/xml".into())], batch.into_bytes()).await?;
        if !(200..300).contains(&code) {
            return Err(failed(&format!("invalidating distribution {id}"), code, body.as_bytes()));
        }
        invalidation = json!(xml_text(&body, "Id"));
    }
    cx.record(CLOUDFRONT, "Distribution", "", id, Some(applied(desired, json!({ "invalidation": invalidation })))).await?;
    distribution(cx, id).await?.ok_or_else(|| anyhow::anyhow!("distribution {id} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match &*key.kind {
        "Site" => {
            let Some(a) = cx.applied(S3, "Site", "", &key.name) else { return Ok(()) };
            let (bucket, prefix, _, _) = spec_of(&a.object);
            delete_under(cx, &bucket, &format!("{prefix}/")).await.map(|_| ())
        }
        // Never kiln's to delete: it was there before the app, and stays after it.
        "Distribution" => Ok(()),
        other => bail!("aws-static doesn't delete {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use kiln_cd::model::Health;
    use kiln_cd::providers::Target;

    use super::*;
    use crate::cd::targets::fake::{self, Fake, MemFiles, MemRecords};
    use crate::cd::targets::fake_query::{self as fq, Distribution, FakeQuery, released};
    use crate::cd::targets::{Conn, Creds, key_of};
    use crate::ci::integrations::aws::Credentials;

    fn zip_of(files: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, text) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())?;
            w.write_all(text.as_bytes())?;
        }
        Ok(w.finish()?.into_inner())
    }

    fn tgz_of(files: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        for (name, text) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, text.as_bytes())?;
        }
        Ok(tar.into_inner()?.finish()?)
    }

    #[test]
    fn sites_read_from_zips_and_tarballs_and_never_leave() -> anyhow::Result<()> {
        let z = site_files(&zip_of(&[("index.html", "<h1>hi</h1>"), ("assets/app.js", "x")])?)?;
        assert_eq!(z.keys().collect::<Vec<_>>(), ["assets/app.js", "index.html"]);
        let t = site_files(&tgz_of(&[("./index.html", "<h1>hi</h1>")])?)?;
        assert_eq!(t.get("index.html").map(Vec::as_slice), Some(&b"<h1>hi</h1>"[..]));
        assert!(site_files(&zip_of(&[("../etc/passwd", "x")])?).is_err_and(|e| e.to_string().contains("leaves it")));
        assert!(site_files(b"plain text").is_err());
        Ok(())
    }

    /// A site through releases on a faked S3 and CloudFront: the first uploads its files under its
    /// own prefix and switches the distribution's origin path there, invalidating it; the same
    /// again changes nothing; the next (a tarball) uploads only what changed and copies the rest
    /// over in S3; the release before, rolled back to, uploads nothing and switches back; releases
    /// beyond `keep` go; the distribution is deployed once CloudFront says so; and the site goes,
    /// its distribution left as it is.
    #[test]
    fn sites_switch_atomically_and_roll_back_without_uploading() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let (s3_fake, q) = (Arc::new(Fake::default()), Arc::new(FakeQuery::default()));
            let (base, q_base) = (fake::start(s3_fake.clone()).await?, fq::start(q.clone()).await?);
            aws::override_endpoint("s3", &base);
            aws::override_endpoint("cloudfront", &q_base);
            s3_fake.state.lock().buckets.insert("shop-www".into(), BTreeMap::new());
            q.state.lock().distributions.insert("E2TEST".into(), Distribution { origins: vec![("api".into(), "api.example.com".into(), "/v1".into()), ("www".into(), "shop-www.s3.us-east-1.amazonaws.com".into(), String::new())], status: "Deployed".into(), ..Distribution::default() });
            let target = Target::parse("aws-static/aws-test/us-east-1").ok_or_else(|| anyhow::anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: None, expires_at: None };
            let cx = Cx { conn: &conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &records, files: &files };
            let spec = json!({ "bucket": "shop-www", "site": "${{ release.artifacts.site.digest }}", "distribution": "E2TEST", "keep": 2 });
            let cxr = &cx;
            let release = |version: &str, archive: Vec<u8>| {
                let digest = format!("sha256:{}", files.add(&archive));
                let made = released("aws-static", spec.clone(), version, &[], &[("site", &digest)]);
                async move {
                    let objs = made?;
                    let mut out = Vec::new();
                    for o in &objs {
                        out.push(apply(cxr, o).await?);
                    }
                    Ok::<_, anyhow::Error>((objs, out))
                }
            };
            let calls = |what: &str| s3_fake.state.lock().calls.iter().filter(|c| c.starts_with(what)).count();
            let origin = || q.state.lock().distributions.get("E2TEST").and_then(|d| d.origins.iter().find(|o| o.0 == "www").map(|o| o.2.clone())).unwrap_or_default();
            let invalidations = || q.state.lock().distributions.get("E2TEST").map_or(0, |d| d.invalidations.len());

            // The first: three files and the manifest uploaded under its prefix; the switch.
            let v1 = zip_of(&[("index.html", "<h1>v1</h1>"), ("assets/app.js", "app"), ("logo.svg", "<svg/>")])?;
            let (objs1, live1) = release("1.0.0", v1.clone()).await?;
            let prefix1 = obj::str_at(objs1.get(1).unwrap_or(&Value::Null), &["spec", "originPath"]).unwrap_or_default().to_owned();
            assert!(prefix1.starts_with("/sites/shop-staging/"), "{prefix1}");
            assert_eq!(calls("PutObject"), 5, "three files, the manifest, current.json");
            assert_eq!(live1.first().and_then(|s| s.pointer("/status/uploaded")), Some(&json!(3)));
            assert_eq!(origin(), prefix1, "the origin path switched to the release");
            assert_eq!(invalidations(), 1);
            assert_eq!(q.state.lock().distributions.get("E2TEST").and_then(|d| d.origins.first().map(|o| o.2.clone())).as_deref(), Some("/v1"), "the other origin as it was");
            assert_eq!(live1.get(1).and_then(kiln_cd::providers::health).map(|h| h.health), Some(Health::Progressing), "CloudFront deploying the change");
            let deployed = list(&cx, "Distribution", &[(String::new(), "E2TEST".into())]).await?.into_iter().next().unwrap_or_default();
            assert_eq!(kiln_cd::providers::health(&deployed).map(|h| h.health), Some(Health::Healthy), "{deployed}");
            let s3_key = |k: &str| s3_fake.state.lock().buckets.get("shop-www").and_then(|b| b.get(k).cloned());
            assert_eq!(s3_key(&format!("{}/index.html", prefix1.trim_start_matches('/'))).as_deref(), Some(&b"<h1>v1</h1>"[..]));

            // The same again: nothing uploaded but the pointer, no switch, no invalidation.
            release("1.0.0", v1.clone()).await?;
            assert_eq!((calls("PutObject"), invalidations()), (6, 1));

            // The next, a tarball: one changed file uploaded, the other two copied over in S3.
            let v2 = tgz_of(&[("index.html", "<h1>v2</h1>"), ("assets/app.js", "app"), ("logo.svg", "<svg/>")])?;
            let (objs2, live2) = release("1.1.0", v2).await?;
            let prefix2 = obj::str_at(objs2.get(1).unwrap_or(&Value::Null), &["spec", "originPath"]).unwrap_or_default().to_owned();
            assert_eq!((live2.first().and_then(|s| s.pointer("/status/uploaded")), live2.first().and_then(|s| s.pointer("/status/copied"))), (Some(&json!(1)), Some(&json!(2))));
            assert_eq!(calls("CopyObject"), 2);
            assert_eq!((origin(), invalidations()), (prefix2.clone(), 2));

            // Rolled back to the release before: its prefix is whole; nothing uploaded; switched back.
            let puts = calls("PutObject");
            let (_, back) = release("1.0.0", v1).await?;
            assert_eq!(calls("PutObject"), puts + 1, "only the pointer");
            assert_eq!(back.first().and_then(|s| s.pointer("/status/uploaded")), Some(&json!(0)));
            assert_eq!((origin(), invalidations()), (prefix1.clone(), 3));

            // A third: past `keep: 2`, the oldest release's files go.
            release("1.2.0", zip_of(&[("index.html", "<h1>v3</h1>")])?).await?;
            let kept: Vec<String> = s3_fake.state.lock().buckets.get("shop-www").map(|b| b.keys().cloned().collect()).unwrap_or_default();
            assert!(!kept.iter().any(|k| k.starts_with(prefix2.trim_start_matches('/'))), "release 1.1.0 is past keep: {kept:?}");
            assert!(kept.iter().any(|k| k.starts_with(prefix1.trim_start_matches('/'))), "1.0.0, rolled back to, is kept: {kept:?}");

            // The site goes; the distribution stays as it is.
            delete(&cx, &key_of(objs1.first().unwrap_or(&Value::Null))).await?;
            delete(&cx, &key_of(objs1.get(1).unwrap_or(&Value::Null))).await?;
            assert!(s3_fake.state.lock().buckets.get("shop-www").is_some_and(BTreeMap::is_empty), "every release's files gone");
            assert!(q.state.lock().distributions.contains_key("E2TEST"));
            Ok(())
        })
    }
}
