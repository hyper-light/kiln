//! The web UI's static files, embedded into the binary at build time (`ui/dist/client`: TanStack
//! Start's client build). Hashed assets are served as immutable. Pages, server functions and server
//! components come from the UI server (`crate::uiserver`); a page that renders only in the browser
//! (every one the build doesn't list as rendered on the server, `_server-pages.json`) gets the app's
//! shell (`_shell.html`) from here, as does every page without the UI server. Compressible files go
//! out brotli-compressed, or gzipped to a client that doesn't take brotli, as the build packed them into the
//! binary (`build.rs`, `crate::ui_pack`): no request waits for a copy, and none goes out uncompressed after a
//! restart (goal 11's audit, row B15). Only a debug build's file changed since it was built is packed here,
//! with brotli, on its first request; an earlier build's goes as its build packed it, or as it is.
//!
//! Every response carries a strong ETag of the bytes it sends (its file's SHA-256 and its coding),
//! and a request whose If-None-Match names it gets a 304 with no body: the shell and the other
//! unhashed files (`no-cache`) cost a revalidation, not a download, on a repeat visit (goal 8 of the
//! browser protocol program, plan 01a1a832ebfd step 3; docs/http.md "The UI's files").
//!
//! Earlier builds' assets are kept on disk (`keep_assets`), each with the brotli and gzip copies its
//! build packed, so a page loaded before a deploy can still load the parts it hasn't yet. An asset no
//! build has is a 404, never `index.html`: a browser handed HTML for a script fails in a way only a
//! reload fixes.
//!
//! What a deploy changed goes as a delta against what the browser has (Compression Dictionary
//! Transport, RFC 9842; goal 8, plan 01a1a832ebfd step 5): every hashed script and stylesheet is
//! offered as a dictionary for the later versions of its name (`Use-As-Dictionary`), and a request
//! naming one kiln has (`Available-Dictionary`, a version of the same name) gets Brotli against it
//! (`dcb`), made once per pair on the compute pool and kept on disk beside the kept assets; until
//! it's made, the plain brotli copy goes, so no request waits. Only kiln's own static, same-origin,
//! secret-free files. Chromium only: other engines don't ask.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{StatusCode, header};

use crate::http::server::{self as srv, Request, Response};
use crate::rcu::Rcu;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/client"]
#[allow_missing = true]
struct Assets;

/// The UI's files the build packed (`build.rs`, by `crate::ui_pack`): `PACKED` with brotli and `PACKED_GZ` with gzip,
/// each a file's name, the SHA-256 of the file it was packed from and its packed bytes, in name order. They're served
/// from the binary's own pages: no server compresses them as it starts or as they're asked for, nor keeps the copies
/// on its heap (goal 11, plan 01a1a87b2e56 step 5; goal 11's audit, row B15).
mod built {
    include!(concat!(env!("OUT_DIR"), "/ui_packed.rs"));
}

/// A table of the build's packed copies, as [`built::PACKED`] is.
type Table = &'static [(&'static str, [u8; 32], &'static [u8])];

/// The build's tables a response is packed from: brotli's, then gzip's.
const BUILT: (Table, Table) = (built::PACKED, built::PACKED_GZ);

/// `name`'s copy in `table` (one like [`built::PACKED`]), if it was packed from the very file whose SHA-256 is
/// `sha`: a debug build reads its files from disk, which may have changed since it was built.
fn built_in(table: Table, name: &str, sha: &[u8; 32]) -> Option<Bytes> {
    let at = table.binary_search_by(|(n, _, _)| (*n).cmp(name)).ok()?;
    table.get(at).filter(|(_, s, _)| s == sha).map(|(_, _, b)| Bytes::from_static(b))
}

const PLACEHOLDER: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>kiln</title>
<style>body{background:#08090a;color:#ededee;font:15px/1.6 system-ui;display:grid;place-items:center;height:100vh;margin:0}
code{color:#cdc4e0}</style></head><body><div><h1>kiln</h1><p>The UI was not built into this binary.
Run <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code> and rebuild.</p></div></body></html>"#;

/// Where earlier builds' assets are kept, and whether this build's were sent to be kept there ([`Keeping`]).
static KEPT: Keeping = Keeping::new();
/// How long an asset is kept after the last build that had it.
const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 3600);

/// The kept assets' folder, once the server names it ([`keep_in`]), and whether this build's assets were sent to be
/// kept there ([`serving`]), once a process.
struct Keeping {
    dir: OnceLock<PathBuf>,
    begun: AtomicBool,
}

impl Keeping {
    const fn new() -> Self {
        Self { dir: OnceLock::new(), begun: AtomicBool::new(false) }
    }

    fn get(&self) -> Option<&PathBuf> {
        self.dir.get()
    }

    /// The folder this build's assets are to be kept in, the first time it's asked once the folder is named; none after.
    fn first(&self) -> Option<PathBuf> {
        let dir = self.dir.get()?;
        (!self.begun.load(Ordering::Relaxed) && !self.begun.swap(true, Ordering::Relaxed)).then(|| dir.clone())
    }
}

/// Earlier builds' assets served from `<data>/ui-assets` from now on (a page loaded before a deploy), as the server
/// starts: no file read or written. This build's are written there only once it serves its UI ([`serving`]).
pub fn keep_in(data_dir: &Path) {
    let _ = KEPT.dir.set(data_dir.join("ui-assets"));
}

/// The UI is being served (a page, its shell, an asset): this build's assets are kept beside earlier builds'
/// ([`keep_assets`]), once a process, on the blocking pool, so the pages it serves load what they haven't yet after a later
/// build replaces it. Before its first page no browser has this build's scripts to ask for them, so a server no one loads
/// the UI from writes none of them: they were written at every start (join-burst: 17 MB, each asset and its brotli and gzip
/// copies, into a fresh instance's page cache as runners joined).
pub fn serving() {
    if let Some(dir) = KEPT.first() {
        drop(crate::rt::spawn(async move {
            let _ = crate::rt::blocking(move || keep_assets(&dir, own_files())).await;
        }));
    }
}

/// This build's files as [`keep_assets`] keeps them: each one's name, bytes and SHA-256.
fn own_files() -> impl Iterator<Item = (Cow<'static, str>, Cow<'static, [u8]>, [u8; 32])> {
    Assets::iter().filter_map(|name| {
        let f = Assets::get(&name)?;
        let sha = f.metadata.sha256_hash();
        Some((name, f.data, sha))
    })
}

/// Keeps `files`' hashed assets (this build's: [`own_files`]) in `dir`, beside earlier builds' (each kept for two weeks
/// after the last build that had it), for `serve`.
fn keep_assets(dir: &Path, files: impl Iterator<Item = (Cow<'static, str>, Cow<'static, [u8]>, [u8; 32])>) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "can't keep the UI's earlier assets");
        return;
    }
    let dir = dir.to_path_buf();
    let now = SystemTime::now();
    for (name, data, sha) in files {
        let Some(file) = name.strip_prefix("assets/") else { continue };
        let path = dir.join(file);
        if !path.exists()
            && let Err(e) = std::fs::write(&path, &data)
        {
            tracing::warn!(error = %e, file, "can't keep a UI asset");
            continue;
        }
        // With the brotli and gzip copies this build packed of it, so an earlier build's goes packed too, to a
        // client that takes gzip alone as to a browser (goal 8, plan 01a1a832ebfd step 3; goal 11's audit, row
        // B15): on disk, nothing on the heap.
        let copies = [(dir.join(format!("{file}.br")), BUILT.0), (dir.join(format!("{file}.gz")), BUILT.1)];
        for (copy, table) in &copies {
            if !copy.exists()
                && let Some(b) = built_in(table, &name, &sha)
                && let Err(e) = std::fs::write(copy, &b)
            {
                tracing::warn!(error = %e, file, copy = %copy.display(), "can't keep a UI asset's packed copy");
            }
        }
        // Its age counts from the last build that had it.
        for p in std::iter::once(&path).chain(copies.iter().map(|(c, _)| c)) {
            let _ = std::fs::File::options().write(true).open(p).and_then(|f| f.set_modified(now));
        }
    }
    let mut index = HashMap::new();
    for d in [dir.clone(), dir.join(DELTAS)] {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| now.duration_since(t).ok()).is_some_and(|age| age > KEEP_FOR);
            if old {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            // The kept scripts and stylesheets, by their SHA-256: the dictionaries a browser may name
            // (goal 8, plan 01a1a832ebfd step 5).
            let name = format!("assets/{}", e.file_name().to_string_lossy());
            if d == dir
                && versioned(&name).is_some()
                && let Ok(data) = std::fs::read(e.path())
            {
                index.insert(crate::ui_pack::sha256(&data), name);
            }
        }
    }
    let _ = std::fs::create_dir_all(dir.join(DELTAS));
    kept_dictionaries().set(index);
}

/// Where the deltas are kept, in the kept assets' directory (a name no asset path can reach).
const DELTAS: &str = ".deltas";

/// The kept scripts and stylesheets by their SHA-256: made whole as the builds are kept
/// ([`keep_assets`]) and swapped in, read on a request without a lock (`crate::rcu`; the API runtime's
/// lockless hot path, scripts/locks/hot.toml).
fn kept_dictionaries() -> &'static Rcu<HashMap<[u8; 32], String>> {
    static D: OnceLock<Rcu<HashMap<[u8; 32], String>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// This build's scripts and stylesheets by their SHA-256.
fn embedded_dictionaries() -> &'static HashMap<[u8; 32], String> {
    static D: OnceLock<HashMap<[u8; 32], String>> = OnceLock::new();
    D.get_or_init(|| Assets::iter().filter(|n| versioned(n).is_some()).filter_map(|n| Some((Assets::get(&n)?.metadata.sha256_hash(), n.into_owned()))).collect())
}

/// What a hashed script or stylesheet is a version of: `assets/<stem>-<8 hash chars>.js|css` as
/// (`<stem>`, `js`|`css`).
fn versioned(name: &str) -> Option<(&str, &str)> {
    let file = name.strip_prefix("assets/")?;
    let (base, ext) = file.rsplit_once('.')?;
    if !matches!(ext, "js" | "css") || base.len() < 10 || !base.is_ascii() {
        return None;
    }
    let (stem, dash_hash) = base.split_at_checked(base.len().checked_sub(9)?)?;
    let hash = dash_hash.strip_prefix('-')?;
    let plain = hash.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) && stem.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    (plain && !stem.is_empty()).then_some((stem, ext))
}

/// The `Use-As-Dictionary` a version of `stem` goes with: it serves for the later versions of its
/// name. A stem is letters, digits, `.`, `-` and `_` (`versioned`), none of them special in a URL
/// pattern or a structured field string.
fn use_as_dictionary(stem: &str, ext: &str) -> String {
    let dest = if ext == "css" { "style" } else { "script" };
    format!("match=\"/assets/{stem}-*.{ext}\", match-dest=(\"{dest}\")")
}

/// The SHA-256 an `Available-Dictionary` names (a structured field byte sequence, `:<base64>:`).
fn available_dictionary(header: Option<&str>) -> Option<[u8; 32]> {
    use base64::Engine as _;
    let b64 = header?.trim().strip_prefix(':')?.strip_suffix(':')?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    bytes.try_into().ok()
}

/// A file's bytes, this build's or an earlier build's kept one.
fn any_build(name: &str) -> Option<Bytes> {
    Assets::get(name).map(|f| embedded(f.data)).or_else(|| kept(name).map(Bytes::from))
}

/// The dictionary a request names for `name`: a version of the same name kiln has (this build's or
/// a kept one), not `name` itself, as its bytes.
fn dictionary_for(name: &str, sha: &[u8; 32]) -> Option<(String, Bytes)> {
    let want = versioned(name)?;
    let dict = embedded_dictionaries().get(sha).cloned().or_else(|| kept_dictionaries().load().get(sha).cloned())?;
    if dict == name || versioned(&dict)? != want {
        return None;
    }
    let data = any_build(&dict)?;
    Some((dict, data))
}

/// `data` as a dictionary-compressed Brotli stream against `dict` (RFC 9842 section 4: the magic,
/// the dictionary's SHA-256, then Brotli with the dictionary as its prefix).
fn dcb(data: &[u8], dict: &[u8]) -> Option<Vec<u8>> {
    // The dictionary and the file both fit Brotli's largest standard window (16 MiB).
    if dict.len() + data.len() > (1 << 24) - 16 {
        return None;
    }
    let params = brotli::enc::BrotliEncoderParams { quality: 11, lgwin: 24, size_hint: data.len(), ..Default::default() };
    let mut out = Vec::with_capacity(36 + data.len() / 8);
    out.extend_from_slice(&DCB_MAGIC);
    out.extend_from_slice(&crate::ui_pack::sha256(dict));
    let mut input = vec![0u8; 1 << 16];
    let mut output = vec![0u8; 1 << 16];
    brotli::enc::BrotliCompressCustomIoCustomDict(
        &mut brotli::IoReaderWrapper(&mut &data[..]),
        &mut brotli::IoWriterWrapper(&mut out),
        &mut input,
        &mut output,
        &params,
        brotli::enc::StandardAlloc::default(),
        &mut |_: &mut _, _: &mut _, _: brotli::InputPair, _: &mut _| (),
        dict,
        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "brotli: the file ended early"),
    )
    .ok()?;
    Some(out)
}

/// A dictionary-compressed response's first bytes (RFC 9842 section 4).
const DCB_MAGIC: [u8; 4] = [0xff, 0x44, 0x43, 0x42];

/// Where the delta of `name` against the dictionary `dict` is kept.
fn delta_path(dir: &Path, name: &str, dict: &[u8; 32]) -> Option<PathBuf> {
    let file = asset_file(name)?;
    Some(dir.join(DELTAS).join(format!("{file}.{}.dcb", hex::encode(dict.get(..16).unwrap_or(dict.as_slice())))))
}

/// The delta of `name` against the dictionary the request names, when kiln has made it; else none
/// now, and it's made on the compute pool (once at a time for a pair) for the requests after.
fn delta(name: &str, asked: Option<&str>, available: Option<&str>) -> Option<Bytes> {
    if !accepts(asked, "dcb") {
        return None;
    }
    let sha = available_dictionary(available)?;
    let dir = KEPT.get()?;
    let path = delta_path(dir, name, &sha)?;
    if let Ok(data) = std::fs::read(&path) {
        return Some(Bytes::from(data));
    }
    let (_, dict) = dictionary_for(name, &sha)?;
    let target = any_build(name)?;
    // The pairs being made, so a pair is made once at a time: a set swapped in whole (`crate::rcu`),
    // claimed by compare-and-swap, so no request waits on another for it. Were two to make one pair
    // anyway, each writes a file of its own and renames it into place: the same bytes win.
    static MAKING: OnceLock<Rcu<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    let making = MAKING.get_or_init(Default::default);
    if making.update(|m| m.insert(path.clone())) {
        drop(crate::rt::spawn(async move {
            let p = path.clone();
            let _ = crate::rt::compute(move || {
                let Some(out) = dcb(&target, &dict) else { return };
                static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let tmp = p.with_extension(format!("making-{}", NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
                if std::fs::write(&tmp, &out).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            })
            .await;
            making.update(|m| m.remove(&path));
        }));
    }
    None
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
    kept_in(KEPT.get()?, path)
}

/// `path`'s file kept in `dir`.
fn kept_in(dir: &Path, path: &str) -> Option<Vec<u8>> {
    std::fs::read(dir.join(asset_file(path)?)).ok()
}

/// The copy of an earlier build's asset its build packed with `coding` (`br` or `gzip`), when one is kept
/// (`keep_assets`).
fn kept_packed(dir: &Path, path: &str, coding: &str) -> Option<Bytes> {
    let suffix = if coding == "br" { "br" } else { "gz" };
    std::fs::read(dir.join(format!("{}.{suffix}", asset_file(path)?))).ok().map(Bytes::from)
}

/// A debug build's files brotli-compressed so far (those changed since it was built, which its tables can't hold),
/// by name: read on every request for one, written once a file (goal 5 of the API runtime program, plan
/// 01a1a59c2aae: a copy swapped in whole, `crate::rcu`).
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

/// A debug build's file `name` that its build didn't pack (changed since), brotli-packed already; else none now, and
/// packed on the compute pool for the next request (once at a time a file): CPU work, which held a place of the
/// blocking pool's, meant for calls that block (goal 3, plan 01a1a59c2a8e step 4). Only this build's files (`ours`)
/// are packed and kept: an earlier build's goes as it is, so what's kept is bounded by the build (goal 11, step 3).
fn packed(name: &str, data: &Bytes, ours: bool) -> Option<Bytes> {
    if let Some(b) = br_cache().load().get(name) {
        return Some(b.clone());
    }
    if !ours {
        return None;
    }
    static MAKING: OnceLock<Rcu<std::collections::HashSet<String>>> = OnceLock::new();
    let making = MAKING.get_or_init(Default::default);
    if making.update(|m| m.insert(name.to_owned())) {
        let (name, data) = (name.to_owned(), data.clone());
        drop(crate::rt::spawn(async move {
            let n = name.clone();
            let _ = crate::rt::compute(move || brotli(&n, &data)).await;
            making.update(|m| m.remove(&name));
        }));
    }
    None
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

/// A static file of the UI, if `req` asks for one: this build's, or an earlier build's asset (a page
/// loaded before a deploy). An asset no build has is a 404, never a page.
pub fn static_file(req: &Request) -> Option<Response> {
    let path = req.uri().path().trim_start_matches('/');
    if path.is_empty() || path.starts_with('_') {
        return None;
    }
    let available = srv::header_str(req, &header::HeaderName::from_static("available-dictionary"));
    if let Some(d) = delta(path, srv::header_str(req, &header::ACCEPT_ENCODING), available) {
        let sha = crate::ui_pack::sha256(&d);
        return Some(send(req, path, d, Some("dcb"), &sha));
    }
    let (data, ours) = match Assets::get(path) {
        Some(f) => {
            let sha = f.metadata.sha256_hash();
            (embedded(f.data), Some(sha))
        }
        None if path.starts_with("assets/") => match kept(path) {
            Some(b) => {
                // Packed as its build packed it, in the coding the request takes: brotli, else gzip.
                let sha = crate::ui_pack::sha256(&b);
                let asked = srv::header_str(req, &header::ACCEPT_ENCODING);
                for coding in ["br", "gzip"] {
                    if accepts(asked, coding)
                        && let Some(packed) = KEPT.get().and_then(|d| kept_packed(d, path, coding))
                    {
                        return Some(send(req, path, packed, Some(coding), &sha));
                    }
                }
                (Bytes::from(b), None)
            }
            None => return Some(srv::response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", srv::full("no such asset in any build kiln keeps\n"))),
        },
        None => return None,
    };
    Some(respond(req, path, data, ours))
}

/// Where pages rendered on the server start (`/shared/plans/`, ...), as the build lists them
/// (`_server-pages.json`, ui/scripts/server-pages.mjs); every other page renders in the browser and
/// gets the shell. A build without the list (an older one, none at all): none known, so every page
/// goes to the UI server as before (goal 8, plan 01a1a832ebfd step 3).
fn server_pages() -> Option<&'static [String]> {
    static PAGES: OnceLock<Option<Vec<String>>> = OnceLock::new();
    PAGES.get_or_init(|| serde_json::from_slice(&Assets::get("_server-pages.json")?.data).ok()).as_deref()
}

/// Whether the page at `path` renders only in the browser, so the shell is the whole of what the
/// server would send for it.
pub fn renders_in_browser(path: &str) -> bool {
    server_pages().is_some_and(|prefixes| in_browser(prefixes, path))
}

fn in_browser(prefixes: &[String], path: &str) -> bool {
    !prefixes.iter().any(|p| path.starts_with(p.as_str()))
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

/// The app's shell (any page, rendered in the browser): for pages that render only in the browser,
/// and for when the UI server isn't there.
pub fn shell(req: &Request) -> Response {
    serving();
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
    respond_from(BUILT, req, name, data, ours)
}

/// [`respond`], packed from `tables` (brotli's and gzip's, the build's: [`BUILT`]). A file the build packed goes as it
/// packed it, in the coding the request takes, from its first request on: a client that takes gzip alone got it as
/// it is until a copy was made on request (goal 11's audit, row B15). Only a debug build's file changed since it was
/// built, which no table holds, is brotli-packed here, on the compute pool, and goes as it is until then.
fn respond_from(tables: (Table, Table), req: &Request, name: &str, data: Bytes, ours: Option<[u8; 32]>) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let asked = srv::header_str(req, &header::ACCEPT_ENCODING);
    let worth = crate::ui_pack::worth(mime.as_ref(), data.len());
    let packed = if worth && accepts(asked, "br") {
        ours.and_then(|sha| built_in(tables.0, name, &sha)).or_else(|| packed(name, &data, ours.is_some())).map(|b| (b, "br"))
    } else if worth && accepts(asked, "gzip") {
        ours.and_then(|sha| built_in(tables.1, name, &sha)).map(|b| (b, "gzip"))
    } else {
        None
    };
    let sha = ours.unwrap_or_else(|| crate::ui_pack::sha256(&data));
    match packed {
        Some((b, coding)) => send(req, name, b, Some(coding), &sha),
        None => send(req, name, data, None, &sha),
    }
}

/// `body`, `name`'s file (whose SHA-256 is `sha`) as `coding` packed it, with its strong ETag; or a 304
/// with no body when the request's If-None-Match names that tag (goal 8, plan 01a1a832ebfd step 3).
fn send(req: &Request, name: &str, body: Bytes, coding: Option<&'static str>, sha: &[u8; 32]) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let tag = etag(sha, coding);
    let mut r = if none_match(srv::header_str(req, &header::IF_NONE_MATCH), &tag) {
        srv::status(StatusCode::NOT_MODIFIED)
    } else {
        let r = srv::response(StatusCode::OK, mime.as_ref(), srv::full(body));
        match coding {
            Some(coding) => srv::with_header(r, header::CONTENT_ENCODING, coding),
            None => r,
        }
    };
    r = srv::with_header(r, header::ETAG, &tag);
    r = srv::with_header(r, header::CACHE_CONTROL, cache);
    r = match versioned(name) {
        Some((stem, ext)) => {
            let r = srv::with_header(r, header::HeaderName::from_static("use-as-dictionary"), &use_as_dictionary(stem, ext));
            srv::with_header(r, header::VARY, "Accept-Encoding, Available-Dictionary")
        }
        None => srv::with_header(r, header::VARY, "Accept-Encoding"),
    };
    srv::with_header(r, header::HeaderName::from_static("x-content-type-options"), "nosniff")
}

/// A strong entity tag for a file whose SHA-256 is `sha`, as `coding` packs it (each coding's bytes have
/// their own: a coding packs a file the same way every time, the build's and the server's alike).
fn etag(sha: &[u8; 32], coding: Option<&str>) -> String {
    let hex = hex::encode(sha.get(..16).unwrap_or(sha.as_slice()));
    match coding {
        Some(c) => format!("\"{hex}.{c}\""),
        None => format!("\"{hex}\""),
    }
}

/// Whether an If-None-Match header names `tag` (or is `*`), by weak comparison (RFC 9110 13.1.2).
fn none_match(header: Option<&str>, tag: &str) -> bool {
    header.is_some_and(|v| {
        v.trim() == "*"
            || v.split(',').any(|t| {
                let t = t.trim();
                t.strip_prefix("W/").unwrap_or(t) == tag
            })
    })
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

    /// Goal 8, plan 01a1a832ebfd step 3: an If-None-Match naming the tag is a 304; each coding has its own tag.
    #[test]
    fn g8_s3_a_matching_etag_is_not_modified() {
        let sha = crate::ui_pack::sha256(b"shell");
        let tag = etag(&sha, None);
        assert!(tag.starts_with('"') && tag.ends_with('"') && tag.len() == 34);
        assert_ne!(etag(&sha, Some("br")), tag);
        assert_ne!(etag(&sha, Some("br")), etag(&sha, Some("gzip")));
        assert!(none_match(Some(&tag), &tag));
        assert!(none_match(Some(&format!("\"x\", W/{tag}")), &tag));
        assert!(none_match(Some("*"), &tag));
        assert!(!none_match(Some("\"x\""), &tag));
        assert!(!none_match(Some(&etag(&sha, Some("br"))), &tag));
        assert!(!none_match(None, &tag));
    }

    /// Goal 8, plan 01a1a832ebfd step 3: an earlier build's asset goes packed as its build packed it, kept beside it,
    /// each coding's copy for that coding (goal 11's audit, row B15: gzip's too).
    #[test]
    fn g8_s3_a_kept_asset_goes_as_its_build_packed_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("old-1.js"), b"old".repeat(1000))?;
        std::fs::write(dir.path().join("old-1.js.br"), b"OLDBR")?;
        std::fs::write(dir.path().join("old-1.js.gz"), b"OLDGZ")?;
        assert_eq!(kept_in(dir.path(), "assets/old-1.js"), Some(b"old".repeat(1000)));
        assert_eq!(kept_packed(dir.path(), "assets/old-1.js", "br").as_deref(), Some(&b"OLDBR"[..]));
        assert_eq!(kept_packed(dir.path(), "assets/old-1.js", "gzip").as_deref(), Some(&b"OLDGZ"[..]));
        assert!(kept_packed(dir.path(), "assets/../old-1.js", "br").is_none());
        assert!(kept_packed(dir.path(), "assets/new-2.js", "gzip").is_none());
        Ok(())
    }

    /// Goal 8, plan 01a1a832ebfd step 3: only the pages the build lists go to the UI server.
    #[test]
    fn g8_s3_pages_the_build_lists_go_to_the_ui_server() {
        let prefixes = vec!["/shared/plans/".to_owned()];
        assert!(in_browser(&prefixes, "/agents/abc"));
        assert!(in_browser(&prefixes, "/"));
        assert!(!in_browser(&prefixes, "/shared/plans/tok"));
    }

    /// Goal 8, plan 01a1a832ebfd step 5: a dcb response decodes with its dictionary to the exact file, and its
    /// header carries the dictionary's SHA-256.
    #[test]
    fn g8_s5_a_delta_decodes_with_its_dictionary_to_the_file() -> anyhow::Result<()> {
        use std::io::Read as _;
        let old: Vec<u8> = (0..4000).flat_map(|i| format!("export const v{i}=\"kiln {i}\";").into_bytes()).collect();
        let mut new = old.clone();
        new.extend_from_slice(b"export const added=1;");
        let d = dcb(&new, &old).ok_or_else(|| anyhow::anyhow!("no delta"))?;
        assert_eq!(d.get(..4), Some(&DCB_MAGIC[..]));
        assert_eq!(d.get(4..36), Some(&crate::ui_pack::sha256(&old)[..]));
        let mut back = Vec::new();
        brotli::Decompressor::new_with_custom_dict(d.get(36..).unwrap_or_default(), 4096, old.clone().into()).read_to_end(&mut back)?;
        assert_eq!(back, new);
        // A delta is a sliver of the file.
        assert!(d.len() < 36 + 100, "delta {} bytes of {}", d.len(), new.len());
        Ok(())
    }

    /// Goal 8, plan 01a1a832ebfd step 5: an unknown or malformed Available-Dictionary gets no delta (plain
    /// brotli).
    #[test]
    fn g8_s5_an_unknown_or_malformed_dictionary_gets_plain_brotli() {
        use base64::Engine as _;
        let sha = crate::ui_pack::sha256(b"x");
        let good = format!(":{}:", base64::engine::general_purpose::STANDARD.encode(sha));
        assert_eq!(available_dictionary(Some(&good)), Some(sha));
        for bad in ["", "::", ":notbase64!:", "abc", ":YWJj:"] {
            assert_eq!(available_dictionary(Some(bad)), None, "{bad}");
        }
        assert!(dictionary_for("assets/index-AbCdEfGh.js", &sha).is_none());
        assert!(delta("assets/index-AbCdEfGh.js", Some("br, dcb"), Some(":YWJj:")).is_none());
        assert!(delta("assets/index-AbCdEfGh.js", Some("br"), Some(&good)).is_none());
    }

    /// Goal 8, plan 01a1a832ebfd step 5: the versions of a name share one dictionary pattern; a packed copy, a
    /// WebAssembly file or an unhashed file is no dictionary.
    #[test]
    fn g8_s5_versions_of_a_name_share_a_dictionary_pattern() {
        assert_eq!(versioned("assets/_app.agents._id-Ba-HR-xj.js"), Some(("_app.agents._id", "js")));
        assert_eq!(versioned("assets/index-CWlGCPL-.css"), Some(("index", "css")));
        assert_eq!(versioned("assets/index-CWlGCPL-.css.br"), None);
        assert_eq!(versioned("assets/rust-AbCdEfGh.wasm"), None);
        assert_eq!(versioned("_shell.html"), None);
        assert_eq!(versioned("assets/-AbCdEfGh.js"), None);
        assert_eq!(use_as_dictionary("_app.agents._id", "js"), r#"match="/assets/_app.agents._id-*.js", match-dest=("script")"#);
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
        assert!(packed("assets/earlier-g11test.js", &data, false).is_none());
        assert!(!br_cache().load().contains_key("assets/earlier-g11test.js"));
        let before = crate::memory::UI_ASSETS.bytes();
        let made = brotli("assets/ours-g11test.js", data.as_ref()).map_or(0, |b| b.len());
        assert!(made > 0);
        assert!(crate::memory::UI_ASSETS.bytes() >= before + made);
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a copy made here (a debug build's file changed since it was built) is kept
    /// only when it's smaller than its file.
    #[test]
    fn g11_s5_a_copy_is_kept_only_smaller() {
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
        assert!(brotli("assets/noise-g11s5.bin", &noise).is_none());
        assert!(!br_cache().load().contains_key("assets/noise-g11s5.bin"));
        let text = "kiln's ui, a line again and again\n".repeat(400);
        let br = brotli("assets/asked-g11s5.js", text.as_bytes()).unwrap_or_default();
        assert!(!br.is_empty() && br.len() < text.len());
        assert!(br_cache().load().contains_key("assets/asked-g11s5.js"));
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
        for (name, sha, packed) in built::PACKED_GZ {
            let Some(f) = Assets::get(name).filter(|f| &f.metadata.sha256_hash() == sha) else { continue };
            assert_eq!(gunzip(packed).as_deref(), Some(f.data.as_ref()), "{name}");
            assert!(packed.len() < f.data.len(), "{name}: kept only when smaller");
        }
    }

    /// `data` inflated from gzip, if it's whole.
    fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
        use std::io::Read as _;
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(data).read_to_end(&mut back).ok()?;
        Some(back)
    }

    /// A GET of `path` that takes `encodings` (none: no Accept-Encoding).
    fn get(path: &str, encodings: Option<&str>) -> anyhow::Result<Request> {
        let mut b = http::Request::builder().method(http::Method::GET).uri(path);
        if let Some(e) = encodings {
            b = b.header(header::ACCEPT_ENCODING, e);
        }
        Ok(b.body(srv::Incoming::empty())?)
    }

    /// A response's coding and its body's bytes.
    fn coding_and_body(r: Response) -> anyhow::Result<(Option<String>, Bytes)> {
        use http_body_util::BodyExt as _;
        let coding = r.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body = crate::rt::block_on(async move { Ok(r.into_body().collect().await?.to_bytes()) })?;
        Ok((coding, body))
    }

    /// Goal 11's audit, row B15 (bp-g8, plan 01a1a832ebfd step 3: compressible UI responses sent uncompressed after a
    /// restart = 0): a client that takes gzip alone (every browser over plain http://, which sends no `br`) gets the
    /// build's gzip copy from its very first request, as a browser gets the brotli one. Before, the first gzip-only
    /// request of each file went as it is, while a copy was made on the compute pool for the requests after it
    /// (`gz_cache`), and so again after every restart. Here nothing came before the request: no copy made, none kept.
    #[test]
    fn g11_b15_a_gzip_only_client_gets_no_compressible_file_uncompressed_from_its_first_request() -> anyhow::Result<()> {
        let name = "assets/first-g11b15.js";
        let data: Vec<u8> = (0..4000).flat_map(|i| format!("export const kiln{i} = \"{i}\";\n").into_bytes()).collect();
        let sha = crate::ui_pack::sha256(&data);
        let br: &'static [u8] = Box::leak(crate::ui_pack::brotli(&data).unwrap_or_default().into_boxed_slice());
        let gz: &'static [u8] = Box::leak(crate::ui_pack::gzip(&data).unwrap_or_default().into_boxed_slice());
        let tables: (Table, Table) = (Box::leak(vec![(name, sha, br)].into_boxed_slice()), Box::leak(vec![(name, sha, gz)].into_boxed_slice()));
        for asked in ["gzip", "gzip, deflate", "deflate, gzip;q=0.8", "br;q=0, gzip"] {
            let (coding, body) = coding_and_body(respond_from(tables, &get(&format!("/{name}"), Some(asked))?, name, Bytes::from(data.clone()), Some(sha)))?;
            assert_eq!(coding.as_deref(), Some("gzip"), "{asked}: the first request is packed");
            assert_eq!(gunzip(&body).as_deref(), Some(data.as_slice()), "{asked}: it inflates to the file");
        }
        // A browser's (`br, gzip`) gets the brotli copy; a client that takes neither, the file as it is.
        let (coding, body) = coding_and_body(respond_from(tables, &get(&format!("/{name}"), Some("gzip, deflate, br"))?, name, Bytes::from(data.clone()), Some(sha)))?;
        assert_eq!((coding.as_deref(), body.as_ref()), (Some("br"), br));
        let (coding, body) = coding_and_body(respond_from(tables, &get(&format!("/{name}"), None)?, name, Bytes::from(data.clone()), Some(sha)))?;
        assert_eq!((coding, body.as_ref()), (None, data.as_slice()));
        // Nothing was made or kept to answer them: the server packs no copy on a request.
        assert!(!br_cache().load().contains_key(name));
        // And the same of this build's own files, each that's worth packing (with the UI built in).
        for name in Assets::iter() {
            let Some(f) = Assets::get(&name) else { continue };
            let sha = f.metadata.sha256_hash();
            if !crate::ui_pack::worth(&crate::ui_pack::mime_of(&name), f.data.len()) {
                continue;
            }
            if built_in(BUILT.1, &name, &sha).is_none() {
                // No gzip copy: a debug build's file changed since it was built (no table holds it), or one gzip
                // doesn't shorten (which goes as it is).
                let changed = built_in(BUILT.0, &name, &sha).is_none();
                assert!(changed || crate::ui_pack::gzip(&f.data).is_none_or(|g| g.len() >= f.data.len()), "{name}: worth packing, and no gzip copy");
                continue;
            }
            let (coding, body) = coding_and_body(respond(&get(&format!("/{name}"), Some("gzip"))?, &name, embedded(f.data.clone()), Some(sha)))?;
            assert_eq!(coding.as_deref(), Some("gzip"), "{name}");
            assert_eq!(gunzip(&body).as_deref(), Some(f.data.as_ref()), "{name}");
        }
        Ok(())
    }

    /// join-burst, item 4: a server's start writes none of the UI's files. It names `<data>/ui-assets` and nothing more
    /// (a path made, no file touched), where it spawned `keep_assets` onto the blocking pool, which made the folder and
    /// wrote every hashed asset and its brotli and gzip copies (17 MB into a fresh instance's page cache, as runners
    /// joined). This build's files are kept once it serves its UI: its folder is handed out the first time asked, never
    /// again, and only once it's named; what's kept then is each asset (nothing else of the build) and the deltas' folder.
    #[test]
    fn join_burst_a_start_writes_no_ui_file_until_the_ui_is_served() -> anyhow::Result<()> {
        let data = tempfile::tempdir()?;
        let before = crate::os::counted::made();
        keep_in(data.path());
        let made = crate::os::counted::made();
        anyhow::ensure!(made.blocks.saturating_sub(before.blocks) <= 2, "the start's naming of the folder allocated {} blocks", made.blocks.saturating_sub(before.blocks));
        anyhow::ensure!(std::fs::read_dir(data.path())?.next().is_none(), "the start wrote into the data folder");
        let keeping = Keeping::new();
        assert_eq!(keeping.first(), None, "no folder named yet: nothing to keep in, and the once not spent");
        let dir = data.path().join("ui-assets");
        let _ = keeping.dir.set(dir.clone());
        assert_eq!(keeping.first(), Some(dir.clone()), "the first UI served keeps the build's files");
        assert_eq!(keeping.first(), None, "once a process");
        let script = b"export const kiln = 1;".repeat(64);
        let files = vec![
            (Cow::Borrowed("assets/app-AbCdEfGh.js"), Cow::Owned(script.clone()), crate::ui_pack::sha256(&script)),
            (Cow::Borrowed("_shell.html"), Cow::Borrowed(&b"<html></html>"[..]), crate::ui_pack::sha256(b"<html></html>")),
        ];
        keep_assets(&dir, files.into_iter());
        assert_eq!(std::fs::read(dir.join("app-AbCdEfGh.js"))?, script, "an asset is kept as it is");
        anyhow::ensure!(!dir.join("_shell.html").exists() && dir.join(DELTAS).is_dir(), "only assets are kept, beside the deltas' folder");
        Ok(())
    }
}
