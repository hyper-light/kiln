//! `kiln/upload@v1` and `kiln/download@v1` (docs/ci.md W07-R10, W08-R08): the job's artifacts,
//! as `actions/upload-artifact@v4` and `actions/download-artifact@v4` keep them (a zip each, through
//! the results service and its blob URLs, so either side reads the other's), run by the runner
//! itself: no Node. The files' root is the deepest directory every path is under, as v4's; hidden
//! files stay out unless asked for; each upload's SHA-256 is checked by the server when it's
//! finalized, and by `kiln/download` when it arrives.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::http::client::Client;

const SERVICE: &str = "twirp/github.actions.results.api.v1.ArtifactService";
/// Uploads up to this are one request; bigger ones go in blocks of [`BLOCK`].
const ONE_PUT: u64 = 64 << 20;
const BLOCK: usize = 8 << 20;

/// Where a leg reaches the results service, and as whom.
pub struct Service<'a> {
    pub http: &'a Client,
    /// `ACTIONS_RESULTS_URL`'s origin (the toolkit drops any path).
    pub base: String,
    pub token: &'a str,
    pub run: &'a str,
    pub job: &'a str,
}

impl Service<'_> {
    /// `ACTIONS_RESULTS_URL` as the service it names.
    pub fn origin_of(results_url: &str) -> String {
        match results_url.split_once("://") {
            Some((scheme, rest)) => format!("{scheme}://{}", rest.split('/').next().unwrap_or(rest)),
            None => results_url.trim_end_matches('/').to_owned(),
        }
    }

    async fn call(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        let url = format!("{}/{SERVICE}/{method}", self.base.trim_end_matches('/'));
        let mut b = body;
        if let Some(o) = b.as_object_mut() {
            o.insert("workflow_run_backend_id".into(), json!(self.run));
            o.insert("workflow_job_run_backend_id".into(), json!(self.job));
        }
        let resp = self.http.post(&url).bearer(self.token).header("content-type", "application/json").body(serde_json::to_vec(&b)?).timeout(Duration::from_secs(120)).send().await?;
        let status = resp.status();
        let v: Value = resp.json(4 << 20).await.unwrap_or_default();
        anyhow::ensure!((200..300).contains(&status), "{method}: {}", v.get("msg").and_then(Value::as_str).unwrap_or("the results service refused it"));
        Ok(v)
    }
}

/// What `kiln/upload@v1` was asked.
pub struct Upload {
    pub name: String,
    /// Paths and patterns, one a line; `!` excludes.
    pub paths: Vec<String>,
    /// `warn`, `error` or `ignore`.
    pub if_none: String,
    pub retention_days: Option<u32>,
    pub level: u32,
    pub overwrite: bool,
    pub hidden: bool,
}

pub struct Uploaded {
    pub id: String,
    pub digest: String,
    pub files: usize,
    pub size: u64,
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// A pattern's literal start: the directory it searches (a file's own parent).
fn search_root(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        if has_glob(&c.as_os_str().to_string_lossy()) {
            return out;
        }
        out.push(c);
    }
    if p.is_file() { p.parent().map(Path::to_path_buf).unwrap_or(out) } else { out }
}

/// The deepest directory both are under.
fn common(a: &Path, b: &Path) -> PathBuf {
    a.components().zip(b.components()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect()
}

/// The files `paths` name from `workspace` (relative ones are the workspace's), and the
/// directory they're stored relative to.
pub fn files(workspace: &Path, paths: &[String], hidden: bool) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for p in paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty() && !p.starts_with('#')) {
        let (neg, p) = match p.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, p),
        };
        let full = if Path::new(p).is_absolute() { PathBuf::from(p) } else { workspace.join(p) };
        // A directory is everything under it.
        let full = if !has_glob(p) && full.is_dir() { full.join("**") } else { full };
        if neg { exclude.push(full) } else { include.push(full) }
    }
    anyhow::ensure!(!include.is_empty(), "no `path` to upload");
    let root = include.iter().map(|p| search_root(p)).reduce(|a, b| common(&a, &b)).unwrap_or_else(|| workspace.to_path_buf());
    let rel = |p: &Path| p.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let patterns: Vec<String> = include.iter().map(|p| rel(p)).chain(exclude.iter().map(|p| format!("!{}", rel(p)))).collect();
    let ws = super::hashfiles::Workspace::new(&root).map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let mut found = ws.matching(&patterns).map_err(|e| anyhow::anyhow!("{}", e.message))?;
    if !hidden {
        found.retain(|f| !f.split('/').any(|s| s.starts_with('.')));
    }
    found.sort();
    found.dedup();
    Ok((root, found))
}

/// The files, zipped under `root` at `level` (0, stored, to 9), into `zip`.
fn zip_files(root: &Path, files: &[String], level: u32, zip: &Path) -> anyhow::Result<()> {
    let mut w = zip::ZipWriter::new(std::fs::File::create(zip)?);
    for f in files {
        let full = root.join(f);
        let meta = std::fs::metadata(&full)?;
        let mut o = zip::write::SimpleFileOptions::default().large_file(meta.len() >= u64::from(u32::MAX));
        o = if level == 0 { o.compression_method(zip::CompressionMethod::Stored) } else { o.compression_method(zip::CompressionMethod::Deflated).compression_level(Some(i64::from(level.min(9)))) };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            o = o.unix_permissions(meta.permissions().mode());
        }
        w.start_file(f.as_str(), o)?;
        std::io::copy(&mut std::fs::File::open(&full)?, &mut w)?;
    }
    w.finish()?;
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hex::encode(h.finalize()))
}

/// `kiln/upload@v1`: the files zipped (in `temp`), the artifact made, its blob uploaded, then
/// finalized with its size and SHA-256. `None` when nothing matched and that's allowed.
pub async fn upload(svc: &Service<'_>, workspace: &Path, temp: &Path, u: &Upload) -> anyhow::Result<Option<Uploaded>> {
    let (ws, paths, hidden) = (workspace.to_path_buf(), u.paths.clone(), u.hidden);
    let (root, found) = crate::rt::blocking(move || files(&ws, &paths, hidden)).await??;
    if found.is_empty() {
        let said = format!("No files were found with the provided path: {}. No artifacts will be uploaded.", u.paths.join(", "));
        return match u.if_none.as_str() {
            "error" => anyhow::bail!("{said}"),
            _ => Ok(None),
        };
    }
    std::fs::create_dir_all(temp)?;
    let zip = temp.join(format!("kiln-upload-{}.zip", crate::util::new_id()));
    let (r, f, z, level) = (root.clone(), found.clone(), zip.clone(), u.level);
    crate::rt::blocking(move || zip_files(&r, &f, level, &z)).await??;
    let z = zip.clone();
    let digest = crate::rt::blocking(move || sha256_file(&z)).await??;
    let size = std::fs::metadata(&zip)?.len();
    let mut create = json!({ "name": u.name, "version": 4 });
    if let (Some(days), Some(o)) = (u.retention_days.filter(|d| *d > 0), create.as_object_mut()) {
        o.insert("expires_at".into(), json!(crate::deploy::repo::rfc3339(crate::util::now_unix() + i64::from(days) * 86_400)));
    }
    let made = match svc.call("CreateArtifact", create.clone()).await {
        Ok(v) => v,
        Err(e) if u.overwrite && format!("{e:#}").contains("already exists") => {
            svc.call("DeleteArtifact", json!({ "name": u.name })).await?;
            svc.call("CreateArtifact", create).await?
        }
        Err(e) => return Err(e),
    };
    let url = made.get("signed_upload_url").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("CreateArtifact gave no upload URL"))?.to_owned();
    let sent = put_blob(svc, &url, &zip, size).await;
    let _ = std::fs::remove_file(&zip);
    sent?;
    let done = svc.call("FinalizeArtifact", json!({ "name": u.name, "size": size.to_string(), "hash": format!("sha256:{digest}") })).await?;
    let id = done.get("artifact_id").and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default();
    Ok(Some(Uploaded { id, digest, files: found.len(), size }))
}

/// The zip to its blob URL: one `PUT` when small, else in blocks and their list.
async fn put_blob(svc: &Service<'_>, url: &str, zip: &Path, size: u64) -> anyhow::Result<()> {
    use base64::Engine as _;
    let put = |u: String, body: Vec<u8>| async move {
        let resp = svc.http.put(&u).header("x-ms-blob-type", "BlockBlob").body(body).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(resp.is_success(), "the upload was refused: HTTP {}", resp.status());
        Ok(())
    };
    if size <= ONE_PUT {
        return put(url.to_owned(), std::fs::read(zip)?).await;
    }
    let mut f = std::fs::File::open(zip)?;
    let mut ids = Vec::new();
    let mut buf = vec![0u8; BLOCK];
    for i in 0u32.. {
        let mut n = 0;
        while n < BLOCK {
            let got = f.read(buf.get_mut(n..).unwrap_or_default())?;
            if got == 0 {
                break;
            }
            n += got;
        }
        if n == 0 {
            break;
        }
        let id = base64::engine::general_purpose::STANDARD.encode(format!("{i:08}"));
        put(format!("{url}&comp=block&blockid={}", crate::ci::cli::enc(&id)), buf.get(..n).unwrap_or_default().to_vec()).await?;
        ids.push(id);
    }
    let list: String = ids.iter().map(|id| format!("<Latest>{id}</Latest>")).collect();
    put(format!("{url}&comp=blocklist"), format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{list}</BlockList>").into_bytes()).await
}

/// What `kiln/download@v1` was asked.
pub struct Download {
    /// One artifact by name, else all of the run's (`pattern` narrowing them).
    pub name: Option<String>,
    pub pattern: Option<String>,
    pub path: PathBuf,
    pub merge: bool,
}

/// `kiln/download@v1`: each artifact into `path` (several: each in its own directory unless
/// merged), checked against its digest. The artifacts' names, and where they went.
pub async fn download(svc: &Service<'_>, temp: &Path, d: &Download) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut ask = json!({});
    if let (Some(n), Some(o)) = (&d.name, ask.as_object_mut()) {
        o.insert("name_filter".into(), json!(n));
    }
    let listed = svc.call("ListArtifacts", ask).await?;
    let mut all: Vec<Value> = listed.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(n) = &d.name {
        all.retain(|a| a.get("name").and_then(Value::as_str) == Some(n.as_str()));
        anyhow::ensure!(!all.is_empty(), "Artifact not found for name: {n}");
    }
    if let Some(p) = &d.pattern {
        all.retain(|a| a.get("name").and_then(Value::as_str).is_some_and(|n| wildcard(p, n)));
    }
    std::fs::create_dir_all(temp)?;
    let mut out = Vec::new();
    let one = d.name.is_some() || all.len() == 1;
    for a in all {
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let signed = svc.call("GetSignedArtifactURL", json!({ "name": name })).await?;
        let url = signed.get("signed_url").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{name}: no download URL"))?.to_owned();
        let file = temp.join(format!("kiln-download-{}.zip", crate::util::new_id()));
        let got = fetch(svc, &url, &file).await;
        let digest = match got {
            Ok(h) => h,
            Err(e) => {
                let _ = std::fs::remove_file(&file);
                return Err(e.context(format!("downloading {name}")));
            }
        };
        if let Some(want) = a.get("digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:"))
            && !want.eq_ignore_ascii_case(&digest)
        {
            let _ = std::fs::remove_file(&file);
            anyhow::bail!("{name} arrived as sha256:{digest}, not the sha256:{want} it was uploaded as");
        }
        let dest = if one || d.merge { d.path.clone() } else { d.path.join(&name) };
        let (f, to) = (file.clone(), dest.clone());
        let unzipped = crate::rt::blocking(move || unzip(&f, &to)).await?;
        let _ = std::fs::remove_file(&file);
        unzipped?;
        out.push((name, dest));
    }
    Ok(out)
}

/// A blob into `file`, its SHA-256 as it comes.
async fn fetch(svc: &Service<'_>, url: &str, file: &Path) -> anyhow::Result<String> {
    let mut resp = svc.http.get(url).timeout(Duration::from_secs(3600)).send().await?;
    anyhow::ensure!(resp.is_success(), "HTTP {}", resp.status());
    let mut f = std::fs::File::create(file)?;
    let mut h = Sha256::new();
    while let Some(chunk) = resp.chunk().await? {
        h.update(&chunk);
        f.write_all(&chunk)?;
    }
    f.flush()?;
    Ok(hex::encode(h.finalize()))
}

/// A zip into `to`, its entries kept inside it.
fn unzip(zip: &Path, to: &Path) -> anyhow::Result<()> {
    let mut f = std::fs::File::open(zip)?;
    f.rewind()?;
    let mut a = zip::ZipArchive::new(std::io::BufReader::new(f))?;
    std::fs::create_dir_all(to)?;
    for i in 0..a.len() {
        let mut e = a.by_index(i)?;
        let Some(rel) = e.enclosed_name() else { anyhow::bail!("`{}` would land outside {}", String::from_utf8_lossy(e.name_raw()), to.display()) };
        let dest = to.join(rel);
        if e.is_dir() {
            std::fs::create_dir_all(&dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = std::fs::File::create(&dest)?;
        std::io::copy(&mut e, &mut w)?;
        #[cfg(unix)]
        if let Some(mode) = e.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode & 0o7777));
        }
    }
    Ok(())
}

/// Whether `name` matches `pattern`, its `*` any characters and `?` one (download's `pattern`).
fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    // Greedy with backtracking to the last `*`.
    let (mut i, mut j, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while j < n.len() {
        match p.get(i) {
            Some('*') => {
                star = Some(i);
                mark = j;
                i += 1;
            }
            Some(c) if *c == '?' || Some(c) == n.get(j) => {
                i += 1;
                j += 1;
            }
            _ => match star {
                Some(s) => {
                    i = s + 1;
                    mark += 1;
                    j = mark;
                }
                None => return false,
            },
        }
    }
    p.get(i..).is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w07_r10_upload_roots_and_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let w = dir.path();
        for (p, t) in [("dist/app.js", "a"), ("dist/lib/x.js", "b"), ("dist/.map", "c"), ("logs/one.log", "d"), ("logs/skip.tmp", "e")] {
            let f = w.join(p);
            std::fs::create_dir_all(f.parent().unwrap_or(w))?;
            std::fs::write(f, t)?;
        }
        // A directory: everything under it, relative to it; hidden files out.
        let (root, f) = files(w, &["dist".into()], false)?;
        assert!(root.ends_with("dist"), "{root:?}");
        assert_eq!(f, vec!["app.js".to_owned(), "lib/x.js".to_owned()]);
        let (_, f) = files(w, &["dist".into()], true)?;
        assert!(f.contains(&".map".to_owned()));
        // Two paths: under their common directory (the workspace); `!` excludes.
        let (_, f) = files(w, &["dist/app.js".into(), "logs/*".into(), "!logs/*.tmp".into()], false)?;
        assert_eq!(f, vec!["dist/app.js".to_owned(), "logs/one.log".to_owned()]);
        // One file: its own directory.
        let (root, f) = files(w, &["logs/one.log".into()], false)?;
        assert!(root.ends_with("logs") && f == vec!["one.log".to_owned()], "{root:?} {f:?}");
        Ok(())
    }

    #[test]
    fn w07_r10_zip_round_trip_stays_inside() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("a"))?;
        std::fs::write(src.join("a/b.txt"), "kiln")?;
        let z = dir.path().join("x.zip");
        zip_files(&src, &["a/b.txt".to_owned()], 6, &z)?;
        unzip(&z, &dir.path().join("out"))?;
        assert_eq!(std::fs::read_to_string(dir.path().join("out/a/b.txt"))?, "kiln");
        // An entry that climbs out is refused.
        let evil = dir.path().join("evil.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&evil)?);
        w.start_file("../escape.txt", zip::write::SimpleFileOptions::default())?;
        w.write_all(b"no")?;
        w.finish()?;
        assert!(unzip(&evil, &dir.path().join("out2")).is_err());
        assert!(!dir.path().join("escape.txt").exists());
        Ok(())
    }

    #[test]
    fn w07_r10_download_patterns() {
        assert!(wildcard("build-*", "build-linux") && !wildcard("build-*", "logs"));
        assert!(wildcard("*-x86_64", "app-x86_64") && wildcard("a?c", "abc") && !wildcard("a?c", "ac"));
        assert!(wildcard("*", "") && wildcard("**", "x") && !wildcard("x", "xy"));
        assert_eq!(Service::origin_of("http://h:1/some/path/"), "http://h:1");
    }
}
