//! The web UI, embedded into the binary at build time (`ui/dist`). Hashed assets are served as
//! immutable; every other path falls back to `index.html` for client-side routing. Compressible
//! files are gzipped once, on first request, and cached in memory.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::OnceLock;

use bytes::Bytes;
use http::{StatusCode, header};
use parking_lot::Mutex;

use crate::http::server::{self as srv, Request, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Assets;

const PLACEHOLDER: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>kiln</title>
<style>body{background:#08090a;color:#ededee;font:15px/1.6 system-ui;display:grid;place-items:center;height:100vh;margin:0}
code{color:#cdc4e0}</style></head><body><div><h1>kiln</h1><p>The UI was not built into this binary.
Run <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code> and rebuild.</p></div></body></html>"#;

fn gz_cache() -> &'static Mutex<HashMap<String, Bytes>> {
    static C: OnceLock<Mutex<HashMap<String, Bytes>>> = OnceLock::new();
    C.get_or_init(Default::default)
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

pub fn serve(req: &Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let (file, name) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (Some(f), path),
        None => (Assets::get("index.html"), "index.html"),
    };
    let Some(file) = file else {
        return srv::response(StatusCode::OK, "text/html; charset=utf-8", srv::full(PLACEHOLDER));
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let accepts_gzip = srv::header_str(req, &header::ACCEPT_ENCODING).is_some_and(|v| v.contains("gzip"));
    let gz = (accepts_gzip && compressible(mime.as_ref()) && file.data.len() > 1024)
        .then(|| gzip(name, &file.data))
        .flatten();
    let mut r = match gz {
        Some(b) => srv::with_header(srv::response(StatusCode::OK, mime.as_ref(), srv::full(b)), header::CONTENT_ENCODING, "gzip"),
        None => srv::response(StatusCode::OK, mime.as_ref(), srv::full(Bytes::from(file.data.into_owned()))),
    };
    r = srv::with_header(r, header::CACHE_CONTROL, cache);
    r = srv::with_header(r, header::VARY, "Accept-Encoding");
    srv::with_header(r, header::HeaderName::from_static("x-content-type-options"), "nosniff")
}
