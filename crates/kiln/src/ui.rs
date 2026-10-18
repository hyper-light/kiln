//! The web UI's static files, embedded into the binary at build time (`ui/dist/client`: TanStack
//! Start's client build). Hashed assets are served as immutable. Pages, server functions and server
//! components come from the UI server (`crate::uiserver`); without it, every page gets the app's
//! shell (`_shell.html`) and the app runs in the browser. Compressible files go out brotli-compressed
//! (or gzipped, to a browser that doesn't take brotli), each compressed once and kept in memory:
//! the embedded ones in the background as the server starts (`keep_assets`), so no request waits.
//!
//! Earlier builds' assets are kept on disk (`keep_assets`), so a page loaded before a deploy can
//! still load the parts it hasn't yet. An asset no build has is a 404, never `index.html`: a
//! browser handed HTML for a script fails in a way only a reload fixes.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{StatusCode, header};
use parking_lot::Mutex;

use crate::http::server::{self as srv, Request, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/client"]
#[allow_missing = true]
struct Assets;

const PLACEHOLDER: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>kiln</title>
<style>body{background:#08090a;color:#ededee;font:15px/1.6 system-ui;display:grid;place-items:center;height:100vh;margin:0}
code{color:#cdc4e0}</style></head><body><div><h1>kiln</h1><p>The UI was not built into this binary.
Run <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code> and rebuild.</p></div></body></html>"#;

/// Where earlier builds' assets are kept.
static KEPT: OnceLock<PathBuf> = OnceLock::new();
/// How long an asset is kept after the last build that had it.
const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 3600);

/// Keeps this build's hashed assets in `<data>/ui-assets`, beside earlier builds' (each kept for
/// two weeks after the last build that had it), for `serve`.
pub fn keep_assets(data_dir: &Path) {
    let dir = data_dir.join("ui-assets");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "can't keep the UI's earlier assets");
        return;
    }
    let now = SystemTime::now();
    for name in Assets::iter() {
        let Some(file) = name.strip_prefix("assets/") else { continue };
        let path = dir.join(file);
        if !path.exists()
            && let Some(f) = Assets::get(&name)
            && let Err(e) = std::fs::write(&path, &f.data)
        {
            tracing::warn!(error = %e, file, "can't keep a UI asset");
            continue;
        }
        // Its age counts from the last build that had it.
        let _ = std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(now));
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| now.duration_since(t).ok()).is_some_and(|age| age > KEEP_FOR);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let _ = KEPT.set(dir);
    warm();
}

/// The file an asset path names (`assets/<name>`): a plain file name, nothing that leaves the
/// directory.
fn asset_file(path: &str) -> Option<&str> {
    let name = path.strip_prefix("assets/")?;
    let plain = !name.is_empty() && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    plain.then_some(name)
}

/// An earlier build's asset, when one is kept.
fn kept(path: &str) -> Option<Vec<u8>> {
    std::fs::read(KEPT.get()?.join(asset_file(path)?)).ok()
}

fn gz_cache() -> &'static Mutex<HashMap<String, Bytes>> {
    static C: OnceLock<Mutex<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn br_cache() -> &'static Mutex<HashMap<String, Bytes>> {
    static C: OnceLock<Mutex<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// `data` brotli-compressed at the highest quality, once per file.
fn brotli(name: &str, data: &[u8]) -> Option<Bytes> {
    if let Some(b) = br_cache().lock().get(name) {
        return Some(b.clone());
    }
    let mut out = Vec::with_capacity(data.len() / 3);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
        w.write_all(data).ok()?;
        w.flush().ok()?;
    }
    let out = Bytes::from(out);
    br_cache().lock().insert(name.to_owned(), out.clone());
    Some(out)
}

/// Whether an Accept-Encoding header takes `coding` (and doesn't refuse it with q=0).
fn accepts(header: Option<&str>, coding: &str) -> bool {
    header.is_some_and(|v| {
        v.split(',').any(|part| {
            let mut it = part.split(';').map(str::trim);
            it.next().is_some_and(|c| c.eq_ignore_ascii_case(coding)) && !it.any(|p| p.replace(' ', "").eq_ignore_ascii_case("q=0"))
        })
    })
}

/// Compresses every embedded file that's worth it, both ways, so no request has to wait for it.
fn warm() {
    for name in Assets::iter() {
        let Some(f) = Assets::get(&name) else { continue };
        let mime = mime_guess::from_path(name.as_ref()).first_or_octet_stream();
        if compressible(mime.as_ref()) && f.data.len() > 1024 {
            let _ = brotli(&name, &f.data);
            let _ = gzip(&name, &f.data);
        }
    }
}

fn compressible(mime: &str) -> bool {
    mime.starts_with("text/") || mime.contains("javascript") || mime.contains("json") || mime.contains("svg")
}

fn gzip(name: &str, data: &[u8]) -> Option<Bytes> {
    if let Some(b) = gz_cache().lock().get(name) {
        return Some(b.clone());
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).ok()?;
    let out = Bytes::from(enc.finish().ok()?);
    gz_cache().lock().insert(name.to_owned(), out.clone());
    Some(out)
}

/// A static file of the UI, if `req` asks for one: this build's, or an earlier build's asset (a page
/// loaded before a deploy). An asset no build has is a 404, never a page.
pub fn static_file(req: &Request) -> Option<Response> {
    let path = req.uri().path().trim_start_matches('/');
    if path.is_empty() || path.starts_with('_') {
        return None;
    }
    let data = match Assets::get(path) {
        Some(f) => Bytes::from(f.data.into_owned()),
        None if path.starts_with("assets/") => match kept(path) {
            Some(b) => Bytes::from(b),
            None => return Some(srv::response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", srv::full("no such asset in any build kiln keeps\n"))),
        },
        None => return None,
    };
    Some(respond(req, path, data))
}

/// The app's shell (any page, rendered in the browser): for when the UI server isn't there.
pub fn shell(req: &Request) -> Response {
    match Assets::get("_shell.html") {
        Some(f) => respond(req, "_shell.html", Bytes::from(f.data.into_owned())),
        None => srv::response(StatusCode::OK, "text/html; charset=utf-8", srv::full(PLACEHOLDER)),
    }
}

fn respond(req: &Request, name: &str, data: Bytes) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let asked = srv::header_str(req, &header::ACCEPT_ENCODING);
    let worth = compressible(mime.as_ref()) && data.len() > 1024;
    let packed = if worth && accepts(asked, "br") {
        brotli(name, &data).map(|b| (b, "br"))
    } else if worth && accepts(asked, "gzip") {
        gzip(name, &data).map(|b| (b, "gzip"))
    } else {
        None
    };
    let mut r = match packed {
        Some((b, coding)) => srv::with_header(srv::response(StatusCode::OK, mime.as_ref(), srv::full(b)), header::CONTENT_ENCODING, coding),
        None => srv::response(StatusCode::OK, mime.as_ref(), srv::full(data)),
    };
    r = srv::with_header(r, header::CACHE_CONTROL, cache);
    r = srv::with_header(r, header::VARY, "Accept-Encoding");
    srv::with_header(r, header::HeaderName::from_static("x-content-type-options"), "nosniff")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brotli_and_gzip_are_taken_as_the_browser_says() {
        assert!(accepts(Some("gzip, deflate, br, zstd"), "br"));
        assert!(accepts(Some("br;q=1.0, gzip;q=0.8"), "br"));
        assert!(!accepts(Some("gzip, deflate"), "br"));
        assert!(!accepts(Some("br;q=0, gzip"), "br"));
        assert!(!accepts(Some("br; q=0"), "br"));
        assert!(accepts(Some("gzip"), "gzip"));
        assert!(!accepts(None, "gzip"));
        // What comes back decompresses to what went in.
        let data = "kiln ".repeat(1000);
        let packed = brotli("test-brotli", data.as_bytes()).unwrap_or_default();
        let mut back = Vec::new();
        let _ = brotli::BrotliDecompress(&mut packed.as_ref(), &mut back);
        assert_eq!(back, data.as_bytes());
        assert!(packed.len() < data.len() / 10);
    }

    #[test]
    fn only_plain_asset_names_are_looked_up() {
        assert_eq!(asset_file("assets/agent-TYOm7Co7.js"), Some("agent-TYOm7Co7.js"));
        assert_eq!(asset_file("assets/index-a_b.css"), Some("index-a_b.css"));
        for bad in ["assets/", "assets/../kiln.db", "assets/a/b.js", "assets/.hidden", "assets/a%2Fb.js", "index.html", "assets/a b.js"] {
            assert_eq!(asset_file(bad), None, "{bad}");
        }
    }
}
