//! The web UI's static files, embedded into the binary at build time (`ui/dist/client`: TanStack
//! Start's client build). Hashed assets are served as immutable. Pages, server functions and server
//! components come from the UI server (`crate::uiserver`); without it, every page gets the app's
//! shell (`_shell.html`) and the app runs in the browser. Compressible files go out brotli-compressed
//! (or gzipped, to a browser that doesn't take brotli), each compressed once and kept in memory:
//! the embedded ones in the background as the server starts (`warm`), so no request waits. Only this
//! build's files are kept so, which bounds what they take (`crate::memory::UI_ASSETS`; goal 11, plan
//! 01a1a87b2e56 step 3); an earlier build's goes as it is.
//!
//! Earlier builds' assets are kept on disk (`keep_assets`), so a page loaded before a deploy can
//! still load the parts it hasn't yet. An asset no build has is a 404, never `index.html`: a
//! browser handed HTML for a script fails in a way only a reload fixes.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{StatusCode, header};

use crate::http::server::{self as srv, Request, Response};
use crate::rcu::Rcu;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/client"]
#[allow_missing = true]
struct Assets;

/// The UI's files the build packed with brotli (`build.rs`, by `crate::ui_pack`): `PACKED`, each a file's name,
/// the SHA-256 of the file it was packed from and its packed bytes, in name order. They're served from the
/// binary's own pages: no server compresses them as it starts, nor keeps the copies on its heap (goal 11, plan
/// 01a1a87b2e56 step 5).
mod built {
    include!(concat!(env!("OUT_DIR"), "/ui_brotli.rs"));
}

/// `name`'s copy in `table` (one like [`built::PACKED`]), if it was packed from the very file whose SHA-256 is
/// `sha`: a debug build reads its files from disk, which may have changed since it was built.
fn built_in(table: &'static [(&'static str, [u8; 32], &'static [u8])], name: &str, sha: &[u8; 32]) -> Option<Bytes> {
    let at = table.binary_search_by(|(n, _, _)| (*n).cmp(name)).ok()?;
    table.get(at).filter(|(_, s, _)| s == sha).map(|(_, _, b)| Bytes::from_static(b))
}

/// `name`'s copy that this build packed, if it was packed from this very file.
fn built(name: &str, sha: &[u8; 32]) -> Option<Bytes> {
    built_in(built::PACKED, name, sha)
}

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

/// The files gzipped so far, by name: read on every request for one, written once a file (goal 5 of the API
/// runtime program, plan 01a1a59c2aae: a copy swapped in whole, `crate::rcu`).
fn gz_cache() -> &'static Rcu<HashMap<String, Bytes>> {
    static C: OnceLock<Rcu<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The files brotli-compressed so far, as [`gz_cache`].
fn br_cache() -> &'static Rcu<HashMap<String, Bytes>> {
    static C: OnceLock<Rcu<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// An embedded file's bytes: the binary's own where they're in it (a release build's), never copied for a
/// request; read from disk where rust-embed reads them so (a debug build's).
fn embedded(data: Cow<'static, [u8]>) -> Bytes {
    match data {
        Cow::Borrowed(b) => Bytes::from_static(b),
        Cow::Owned(v) => Bytes::from(v),
    }
}

/// `packed` kept for `name` in `cache`, counted as the UI's assets' memory (`crate::memory::UI_ASSETS`).
fn keep(cache: &Rcu<HashMap<String, Bytes>>, name: &str, packed: &Bytes) {
    let before = cache.update(|m| m.insert(name.to_owned(), packed.clone()));
    crate::memory::UI_ASSETS.add(packed.len());
    if let Some(b) = before {
        crate::memory::UI_ASSETS.sub(b.len());
    }
}

/// `data` brotli-compressed at the highest quality, once per file, in the smallest window that holds it
/// (`crate::ui_pack::window_bits`): kiln took 22 bits (4 MiB) for every file before, 34 MB of encoder for each at
/// quality 11, a 20 KB file's 256 KB (goal 11, plan 01a1a87b2e56 step 3). The build packs them the same way.
fn brotli(name: &str, data: &[u8]) -> Option<Bytes> {
    if let Some(b) = br_cache().load().get(name) {
        return Some(b.clone());
    }
    smaller(name, data, crate::ui_pack::brotli(data)?, br_cache())
}

/// `out`, `data` packed, kept for `name` in `cache` if it's smaller than `data` (else `data` goes as it is),
/// in an allocation of its own size: the buffer it was made in grew as it went, and what's kept is kept as
/// long as the server runs (goal 11, plan 01a1a87b2e56 step 5).
fn smaller(name: &str, data: &[u8], mut out: Vec<u8>, cache: &Rcu<HashMap<String, Bytes>>) -> Option<Bytes> {
    if out.len() >= data.len() {
        return None;
    }
    out.shrink_to_fit();
    let out = Bytes::from(out);
    keep(cache, name, &out);
    Some(out)
}

/// `name` packed (brotli or gzip, by its cache), made already; else none now, and made that way on the
/// compute pool for the next request (once at a time for a file and a coding): CPU work, which held a
/// place of the blocking pool's, meant for calls that block (goal 3, plan 01a1a59c2a8e step 4). Only the
/// coding asked for is made: browsers take brotli, so a gzip copy is made only for a client that takes
/// gzip alone (goal 11, plan 01a1a87b2e56 step 5). Only this build's files (`ours`) are packed and kept:
/// an earlier build's goes as it is, so what's kept is bounded by the build (goal 11, step 3).
fn packed(name: &str, data: &Bytes, cache: &'static Rcu<HashMap<String, Bytes>>, ours: bool) -> Option<(Bytes, &'static str)> {
    let coding = if std::ptr::eq(cache, br_cache()) { "br" } else { "gzip" };
    if let Some(b) = cache.load().get(name) {
        return Some((b.clone(), coding));
    }
    if !ours {
        return None;
    }
    static MAKING: OnceLock<Rcu<std::collections::HashSet<(&'static str, String)>>> = OnceLock::new();
    let making = MAKING.get_or_init(Default::default);
    if making.update(|m| m.insert((coding, name.to_owned()))) {
        let (name, data) = (name.to_owned(), data.clone());
        drop(crate::rt::spawn(async move {
            let n = name.clone();
            let _ = crate::rt::compute(move || make(&n, &data, coding)).await;
            let key = (coding, name);
            making.update(|m| m.remove(&key));
        }));
    }
    None
}

/// `data`, `name`'s, packed as `coding` says (`br` or `gzip`), and kept so.
fn make(name: &str, data: &[u8], coding: &str) -> Option<Bytes> {
    if coding == "br" { brotli(name, data) } else { gzip(name, data) }
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

/// Compresses every embedded file that's worth it with brotli, the coding browsers take, so no browser's
/// request has to wait for it: on the compute pool, a file at a time (goal 3, plan 01a1a59c2a8e step 4: CPU
/// work, which held one of the blocking pool's places for as long as every file took, written out with the
/// assets' files). A gzip copy is made only for a client that takes gzip alone (`packed`): kept for every
/// file as well, gzip took 3.8 MB of the 5.4 MB the UI's files held in the lab's realistic profile, whose
/// pages (as a browser's: `br, gzip`) never asked for one (goal 11, plan 01a1a87b2e56 step 5). The bound of
/// what the UI's files keep in memory (`crate::memory::UI_ASSETS`) is the brotli copies made here and, for
/// each, its file's own size: a gzip copy is kept only when it's smaller than its file. A file the build packed
/// (`built`) is served from the binary, so nothing is made here for it, and only a gzip copy counts.
pub async fn warm() {
    let names: Vec<String> = Assets::iter().map(|n| n.into_owned()).collect();
    let made = crate::rt::compute_map(names, |name| {
        let Some(f) = Assets::get(&name) else { return 0 };
        if !crate::ui_pack::worth(&crate::ui_pack::mime_of(&name), f.data.len()) {
            return 0;
        }
        if built(&name, &f.metadata.sha256_hash()).is_some() {
            return f.data.len();
        }
        brotli(&name, &f.data).map_or(0, |b| b.len()).saturating_add(f.data.len())
    })
    .await;
    match made {
        Ok(sizes) => crate::memory::UI_ASSETS.bound_at(sizes.iter().fold(0usize, |a, b| a.saturating_add(*b))),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "the UI's files weren't compressed ahead: each is as it's first asked for"),
    }
    // Every file has been read whole by now (here, and by `keep_assets` before it), and what's served of
    // those worth compressing is their brotli copies: the files' own pages in the binary leave resident
    // memory, and come back from the page cache when one is read again (goal 11, plan 01a1a87b2e56 step 5).
    crate::resident::ui_files(embedded_spans()).await;
    crate::resident::warmed();
}

/// Where each embedded file's bytes are in the binary, as an address and a length: a release build's, whose
/// files are in its read-only data. None for a build that reads them from disk (a debug build's).
fn embedded_spans() -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for name in Assets::iter() {
        match Assets::get(&name).map(|f| f.data) {
            Some(Cow::Borrowed(b)) => spans.push((b.as_ptr().addr(), b.len())),
            // Read from disk: so is every other.
            Some(Cow::Owned(_)) => return Vec::new(),
            None => {}
        }
    }
    spans
}

fn gzip(name: &str, data: &[u8]) -> Option<Bytes> {
    if let Some(b) = gz_cache().load().get(name) {
        return Some(b.clone());
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).ok()?;
    smaller(name, data, enc.finish().ok()?, gz_cache())
}

/// A static file of the UI, if `req` asks for one: this build's, or an earlier build's asset (a page
/// loaded before a deploy). An asset no build has is a 404, never a page.
pub fn static_file(req: &Request) -> Option<Response> {
    let path = req.uri().path().trim_start_matches('/');
    if path.is_empty() || path.starts_with('_') {
        return None;
    }
    let (data, ours) = match Assets::get(path) {
        Some(f) => {
            let sha = f.metadata.sha256_hash();
            (embedded(f.data), Some(sha))
        }
        None if path.starts_with("assets/") => match kept(path) {
            Some(b) => (Bytes::from(b), None),
            None => return Some(srv::response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", srv::full("no such asset in any build kiln keeps\n"))),
        },
        None => return None,
    };
    Some(respond(req, path, data, ours))
}

/// One of the fonts the console reads in, for pages agents show (`crate::harness::visuals`):
/// `geist.woff2` or `geist-mono.woff2` (their Latin cuts, every weight).
pub fn font(name: &str) -> Option<Bytes> {
    let prefix = match name {
        "geist.woff2" => "assets/geist-latin-wght-normal-",
        "geist-mono.woff2" => "assets/geist-mono-latin-wght-normal-",
        _ => return None,
    };
    let file = Assets::iter().find(|n| n.starts_with(prefix) && n.ends_with(".woff2"))?;
    Assets::get(&file).map(|f| embedded(f.data))
}

/// The app's shell (any page, rendered in the browser): for when the UI server isn't there.
pub fn shell(req: &Request) -> Response {
    match Assets::get("_shell.html") {
        Some(f) => {
            let sha = f.metadata.sha256_hash();
            respond(req, "_shell.html", embedded(f.data), Some(sha))
        }
        None => srv::response(StatusCode::OK, "text/html; charset=utf-8", srv::full(PLACEHOLDER)),
    }
}

/// `data`, `name`'s, packed as the request takes it where it's worth it; `ours`: this build's file, by its
/// SHA-256 (none for an earlier build's).
fn respond(req: &Request, name: &str, data: Bytes, ours: Option<[u8; 32]>) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let asked = srv::header_str(req, &header::ACCEPT_ENCODING);
    let worth = crate::ui_pack::worth(mime.as_ref(), data.len());
    // Packed as the build packed it, else as made already: one not made yet goes as it is this once,
    // and is packed on the blocking pool for the requests after it (a large file at brotli's best
    // takes seconds, and this request's thread serves every socket on it, a restart's first pages
    // most of all).
    let packed = if worth && accepts(asked, "br") {
        ours.and_then(|sha| built(name, &sha)).map(|b| (b, "br")).or_else(|| packed(name, &data, br_cache(), ours.is_some()))
    } else if worth && accepts(asked, "gzip") {
        packed(name, &data, gz_cache(), ours.is_some())
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

    /// Goal 11, plan 01a1a87b2e56 step 3: brotli's window is the smallest that holds the file, never past the
    /// 22 bits kiln took for every file, and a file packed in its own window comes back whole.
    #[test]
    fn g11_s3_brotlis_window_holds_the_file_and_no_more() {
        let window_bits = crate::ui_pack::window_bits;
        assert_eq!(window_bits(0), 10);
        assert_eq!(window_bits(1008), 10);
        assert_eq!(window_bits(1009), 11);
        assert_eq!(window_bits(20_000), 15);
        assert_eq!(window_bits(510_000), 19);
        assert_eq!(window_bits((1 << 22) - 16), 22);
        assert_eq!(window_bits(64 << 20), 22);
        let data: String = (0..40_000).map(|i| format!("line {i} of kiln's ui ")).collect();
        assert_eq!(window_bits(data.len()), 20);
        let packed = brotli("test-window", data.as_bytes()).unwrap_or_default();
        let mut back = Vec::new();
        let _ = brotli::BrotliDecompress(&mut packed.as_ref(), &mut back);
        assert_eq!(back, data.as_bytes());
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: only this build's files are kept packed (and counted), so what the
    /// UI's files keep is bounded by the build; an earlier build's goes as it is.
    #[test]
    fn g11_s3_only_this_builds_files_are_kept_packed() {
        let data = Bytes::from("an earlier build's script ".repeat(200));
        assert!(packed("assets/earlier-g11test.js", &data, br_cache(), false).is_none());
        assert!(packed("assets/earlier-g11test.js", &data, gz_cache(), false).is_none());
        assert!(!br_cache().load().contains_key("assets/earlier-g11test.js"));
        assert!(!gz_cache().load().contains_key("assets/earlier-g11test.js"));
        let before = crate::memory::UI_ASSETS.bytes();
        let made = gzip("assets/ours-g11test.js", data.as_ref()).map_or(0, |b| b.len());
        assert!(made > 0);
        assert!(crate::memory::UI_ASSETS.bytes() >= before + made);
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a copy is kept only when it's smaller than its file, and only in the
    /// coding asked for: a brotli copy (what browsers take) doesn't bring a gzip one with it.
    #[test]
    fn g11_s5_a_copy_is_kept_only_smaller_and_only_as_asked() {
        // Bytes no compressor shortens (a xorshift stream): nothing kept, the file goes as it is.
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                u8::try_from(x & 0xff).unwrap_or(0)
            })
            .collect();
        assert!(gzip("assets/noise-g11s5.bin", &noise).is_none());
        assert!(!gz_cache().load().contains_key("assets/noise-g11s5.bin"));
        let text = "kiln's ui, a line again and again\n".repeat(400);
        let br = make("assets/asked-g11s5.js", text.as_bytes(), "br").unwrap_or_default();
        assert!(!br.is_empty() && br.len() < text.len());
        assert!(br_cache().load().contains_key("assets/asked-g11s5.js"));
        assert!(!gz_cache().load().contains_key("assets/asked-g11s5.js"), "a gzip copy only when one is asked for");
        let gz = make("assets/asked-g11s5.js", text.as_bytes(), "gzip").unwrap_or_default();
        assert!(!gz.is_empty() && gz.len() < text.len());
        assert!(gz_cache().load().contains_key("assets/asked-g11s5.js"));
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a copy the build packed goes out only for the very file it was packed
    /// from, found by name in the table's order; and each copy this build packed comes back as its file.
    #[test]
    fn g11_s5_a_copy_the_build_packed_goes_out_only_for_its_own_file() {
        static TABLE: &[(&str, [u8; 32], &[u8])] = &[("assets/a.js", [1; 32], b"A"), ("assets/b.js", [2; 32], b"B"), ("z.html", [3; 32], b"Z")];
        assert_eq!(built_in(TABLE, "assets/b.js", &[2; 32]).as_deref(), Some(b"B".as_slice()));
        assert_eq!(built_in(TABLE, "z.html", &[3; 32]).as_deref(), Some(b"Z".as_slice()));
        assert!(built_in(TABLE, "assets/b.js", &[9; 32]).is_none(), "a file changed since the build has no copy");
        assert!(built_in(TABLE, "assets/c.js", &[2; 32]).is_none());
        for (name, sha, packed) in built::PACKED {
            // A debug build reads its files from disk: one changed since the build isn't its copy's.
            let Some(f) = Assets::get(name).filter(|f| &f.metadata.sha256_hash() == sha) else { continue };
            let mut back = Vec::new();
            let mut from: &[u8] = packed;
            let _ = brotli::BrotliDecompress(&mut from, &mut back);
            assert_eq!(back.as_slice(), f.data.as_ref(), "{name}");
            assert!(packed.len() < f.data.len(), "{name}: kept only when smaller");
        }
    }
}
