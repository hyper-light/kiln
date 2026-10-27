//! Files a conversation's messages mention (a screenshot, a recording, a PDF, a log), shown in the
//! conversation for whoever views it. A file lives on the machine its session runs on: the server
//! asks that machine's runner for it in pieces ([`crate::proto::ServerMsg::HarnessFileRead`]), or
//! reads it itself for its own sessions, and keeps a copy once the message is done
//! (`super::kept`), so it still shows after the file moves or goes.
//!
//! Only what a message says is read: paths written in what the agent or the person said
//! ([`mentioned`]), never ones found in tools' input or output. Of those, only files a person can
//! look at ([`viewable`]), never what looks like a secret ([`secret`]).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The most read in one piece (a bulk frame carries up to 4 MiB).
pub const PIECE: u32 = 1 << 20;
/// Of a file's start, what's read to tell its type and dimensions.
const HEAD: usize = 64 * 1024;
/// The most paths one message is taken to mention.
const MOST_MENTIONED: usize = 32;

/// A file as its machine has it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "FileMeta"))]
pub struct Meta {
    pub size: u64,
    /// Unix ms.
    pub mtime: i64,
    pub mime: String,
    /// In pixels as it shows (turned as its own data says), when it's an image or a video and its
    /// start says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub height: Option<u32>,
}

/// The answer to a read ([`crate::proto::AgentMsg::HarnessFileData`]'s JSON): the file, or why it
/// can't be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Answer {
    pub fn of(r: &Result<(Meta, Vec<u8>), String>) -> Self {
        match r {
            Ok((meta, _)) => Self { meta: Some(meta.clone()), error: None },
            Err(e) => Self { meta: None, error: Some(e.clone()) },
        }
    }
}

/// Read up to `len` bytes of `path` from `offset` (at most [`PIECE`]), with what the file is.
/// `path` is absolute, or starts at the home folder (`~/`).
/// All of a file, up to `most` bytes, as [`read`] allows it (never a secret, only types a person
/// or a page can use): for the pages agents look at on their own machine (`crate::browser`).
pub fn read_whole(path: &str, most: u64) -> Result<(Meta, Vec<u8>), String> {
    let (meta, _) = read(path, 0, 0)?;
    if meta.size > most {
        return Err(format!("{path} is {} MB; a page's file may be up to {} MB", meta.size >> 20, most >> 20));
    }
    let p = expand(path).ok_or_else(|| format!("{path} isn't a full path"))?;
    let data = std::fs::read(&p).map_err(|e| why(path, &e))?;
    Ok((meta, data))
}

pub fn read(path: &str, offset: u64, len: u32) -> Result<(Meta, Vec<u8>), String> {
    let p = expand(path).ok_or_else(|| format!("{path} isn't a full path"))?;
    if secret(&p) {
        return Err("kiln doesn't show what looks like a secret".to_owned());
    }
    // A link is followed, and the place it leads to must pass too.
    if let Ok(real) = std::fs::canonicalize(&p)
        && secret(&real)
    {
        return Err("kiln doesn't show what looks like a secret".to_owned());
    }
    let md = std::fs::metadata(&p).map_err(|e| why(path, &e))?;
    if !md.is_file() {
        return Err(format!("{path} isn't a file"));
    }
    let mut f = std::fs::File::open(&p).map_err(|e| why(path, &e))?;
    let mut head = Vec::with_capacity(HEAD.min(usize::try_from(md.len()).unwrap_or(HEAD)));
    (&mut f).take(HEAD as u64).read_to_end(&mut head).map_err(|e| why(path, &e))?;
    let mime = sniff(&head, &p);
    if !viewable(&mime) {
        return Err(format!("kiln doesn't show {mime} files"));
    }
    let (width, height) = dims(&head, &mime).unzip();
    let meta = Meta { size: md.len(), mtime: mtime_ms(&md), mime, width, height };
    let want = u64::from(len.min(PIECE)).min(md.len().saturating_sub(offset));
    let mut data = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
    if want > 0 {
        f.seek(SeekFrom::Start(offset)).map_err(|e| why(path, &e))?;
        f.take(want).read_to_end(&mut data).map_err(|e| why(path, &e))?;
    }
    Ok((meta, data))
}

fn why(path: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("{path} isn't there any more"),
        std::io::ErrorKind::PermissionDenied => format!("{path} can't be read by kiln"),
        _ => format!("{path}: {e}"),
    }
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `~/x` at the home folder; anything else must be absolute.
pub fn expand(path: &str) -> Option<PathBuf> {
    let p = if let Some(rest) = path.strip_prefix("~/") { crate::os::home_dir().join(rest) } else { PathBuf::from(path) };
    p.is_absolute().then_some(p)
}

/// `path` as the session sees it: a relative one is in its folder, `cwd`. Dots are resolved here
/// (`a/../b`); links are left for the machine, which follows them when it reads.
pub fn resolve(path: &str, cwd: &str) -> Option<String> {
    let p = if path.starts_with('/') || path.starts_with("~/") {
        PathBuf::from(path)
    } else {
        if cwd.is_empty() {
            return None;
        }
        Path::new(cwd).join(path)
    };
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    let s = out.to_string_lossy().into_owned();
    (s.starts_with('/') || s.starts_with("~/") || s == "~").then_some(s)
}

/// Whether `path` looks like it holds a secret: keys, credentials, tokens, the system's own files.
/// kiln never reads those for a viewer, nor keeps a copy, whatever a message says.
pub fn secret(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if ["/proc/", "/sys/", "/dev/", "/etc/shadow", "/etc/gshadow", "/etc/sudoers"].iter().any(|p| s.starts_with(p)) {
        return true;
    }
    const DIRS: [&str; 10] = [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".password-store", "keyrings", ".gcloud", "gcloud"];
    let all: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    if all.iter().any(|p| DIRS.contains(&p.as_str())) {
        return true;
    }
    let Some(name) = all.last() else { return false };
    name.starts_with(".env")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_dsa")
        || name.contains("credential")
        || name.contains("secret")
        || [".pem", ".key", ".p12", ".pfx", ".kdbx", ".keystore", ".jks", ".asc", ".gpg"].iter().any(|e| name.ends_with(e))
        || [".netrc", ".npmrc", ".pypirc", ".git-credentials", "kiln.toml", "known_hosts", "authorized_keys", ".htpasswd", "shadow"].contains(&name.as_str())
}

/// Whether a person can look at a file of this type in the conversation.
pub fn viewable(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or(mime).trim();
    m.starts_with("image/")
        || m.starts_with("video/")
        || m.starts_with("audio/")
        || m.starts_with("text/")
        || m.starts_with("font/")
        || m.starts_with("model/")
        || matches!(m, "application/pdf" | "application/json" | "application/wasm")
}

/// Text files by their name: what they are, whatever a general table guesses (".ts" is not a
/// video here).
fn text_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if ["dockerfile", "makefile", "justfile", "procfile", "license", "readme"].contains(&lower.as_str()) {
        return Some("text/plain; charset=utf-8");
    }
    let ext = lower.rsplit_once('.')?.1;
    Some(match ext {
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "tsv" => "text/tab-separated-values; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "xml" => "text/xml; charset=utf-8",
        "yaml" | "yml" => "text/yaml; charset=utf-8",
        "txt" | "text" | "log" | "out" | "err" | "diff" | "patch" | "toml" | "ini" | "cfg" | "conf" | "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "go" | "rb" | "java" | "kt"
        | "swift" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "sh" | "bash" | "zsh" | "fish" | "sql" | "css" | "scss" | "lua" | "zig" | "nix" | "proto" | "graphql" | "vue" | "svelte" => {
            "text/plain; charset=utf-8"
        }
        _ => return None,
    })
}

/// A file's type from its first bytes, else its name; text only when it reads as text.
pub fn sniff(head: &[u8], path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    let at = |i: usize, s: &[u8]| head.get(i..i + s.len()) == Some(s);
    let m = if at(0, b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if at(0, b"\xff\xd8\xff") {
        "image/jpeg"
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        "image/gif"
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        "audio/wav"
    } else if at(0, b"RIFF") && at(8, b"AVI ") {
        "video/x-msvideo"
    } else if at(4, b"ftyp") {
        match head.get(8..12) {
            Some(b"avif" | b"avis") => "image/avif",
            Some(b"heic" | b"heix" | b"mif1" | b"msf1") => "image/heic",
            Some(b"qt  ") => "video/quicktime",
            Some(b"M4A " | b"M4B ") => "audio/mp4",
            _ => "video/mp4",
        }
    } else if at(0, b"\x1a\x45\xdf\xa3") {
        if head.windows(4).take(64).any(|w| w == b"webm") { "video/webm" } else { "video/x-matroska" }
    } else if at(0, b"OggS") {
        "audio/ogg"
    } else if at(0, b"fLaC") {
        "audio/flac"
    } else if at(0, b"ID3") || matches!(head.get(..2), Some([0xff, 0xfb | 0xf3 | 0xf2])) {
        "audio/mpeg"
    } else if at(0, b"%PDF-") {
        "application/pdf"
    } else if at(0, b"BM") && ext == "bmp" {
        "image/bmp"
    } else if at(0, b"\0\0\x01\0") && ext == "ico" {
        "image/x-icon"
    } else if (ext == "svg" || at(0, b"<?xml") || at(0, b"<svg")) && find(head, b"<svg").is_some() {
        "image/svg+xml"
    // What pages agents show use (`super::visuals`): fonts, WebAssembly, 3D models.
    } else if at(0, b"wOF2") {
        "font/woff2"
    } else if at(0, b"wOFF") {
        "font/woff"
    } else if at(0, b"OTTO") {
        "font/otf"
    } else if (at(0, b"\0\x01\0\0") || at(0, b"true")) && matches!(ext.as_str(), "ttf" | "ttc") {
        "font/ttf"
    } else if at(0, b"\0asm") {
        "application/wasm"
    } else if at(0, b"glTF") {
        "model/gltf-binary"
    } else if let Some(t) = text_type(&name) {
        if head.contains(&0) {
            return "application/octet-stream".to_owned();
        }
        t
    } else {
        return mime_guess::from_path(path).first_raw().filter(|m| !m.starts_with("text/") || !head.contains(&0)).unwrap_or("application/octet-stream").to_owned();
    };
    m.to_owned()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn be16(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?)))
}

fn be32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn le16(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?)))
}

fn le24(b: &[u8], i: usize) -> Option<u32> {
    let [lo, mid, hi]: [u8; 3] = b.get(i..i + 3)?.try_into().ok()?;
    Some(u32::from(lo) | (u32::from(mid) << 8) | (u32::from(hi) << 16))
}

/// An image's or a video's size as it shows, from the start of its file.
pub fn dims(head: &[u8], mime: &str) -> Option<(u32, u32)> {
    let d = match mime {
        "image/png" => (be32(head, 16)?, be32(head, 20)?),
        "image/gif" => (le16(head, 6)?, le16(head, 8)?),
        "image/webp" => webp_dims(head)?,
        "image/jpeg" => jpeg_dims(head)?,
        "image/bmp" => (i32::from_le_bytes(head.get(18..22)?.try_into().ok()?).unsigned_abs(), i32::from_le_bytes(head.get(22..26)?.try_into().ok()?).unsigned_abs()),
        "image/avif" | "image/heic" => heif_dims(head)?,
        "image/svg+xml" => svg_dims(head)?,
        "video/webm" | "video/x-matroska" => webm_dims(head)?,
        "video/mp4" | "video/quicktime" => mp4_dims(head)?,
        _ => return None,
    };
    (d.0 > 0 && d.1 > 0 && d.0 <= 1 << 16 && d.1 <= 1 << 16).then_some(d)
}

fn webp_dims(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8 " => Some((le16(b, 26)? & 0x3fff, le16(b, 28)? & 0x3fff)),
        b"VP8L" => {
            let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
            Some(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((le24(b, 24)? + 1, le24(b, 27)? + 1)),
        _ => None,
    }
}

/// A JPEG's frame size, turned as its EXIF orientation says (browsers show it turned).
fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    let mut turned = false;
    while i + 4 <= b.len() {
        if *b.get(i)? != 0xff {
            return None;
        }
        let marker = *b.get(i + 1)?;
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        let len = usize::try_from(be16(b, i + 2)?).ok()?;
        if marker == 0xe1 && b.get(i + 4..i + 10) == Some(b"Exif\0\0") {
            turned = exif_turned(b.get(i + 10..i + 2 + len)?).unwrap_or(false);
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let (h, w) = (be16(b, i + 5)?, be16(b, i + 7)?);
            return Some(if turned { (h, w) } else { (w, h) });
        }
        i += 2 + len;
    }
    None
}

/// Whether a JPEG's EXIF orientation turns it a quarter (5–8).
fn exif_turned(tiff: &[u8]) -> Option<bool> {
    let le = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |i: usize| -> Option<u16> {
        let s: [u8; 2] = tiff.get(i..i + 2)?.try_into().ok()?;
        Some(if le { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let s: [u8; 4] = tiff.get(i..i + 4)?.try_into().ok()?;
        Some(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let n = usize::from(u16_at(ifd)?);
    for k in 0..n.min(64) {
        let e = ifd + 2 + k * 12;
        if u16_at(e)? == 0x0112 {
            return Some(matches!(u16_at(e + 8)?, 5..=8));
        }
    }
    Some(false)
}

/// AVIF/HEIC: the image's spatial extent (`ispe`), turned by its rotation (`irot`).
fn heif_dims(b: &[u8]) -> Option<(u32, u32)> {
    let at = find(b, b"ispe")?;
    let (w, h) = (be32(b, at + 8)?, be32(b, at + 12)?);
    let turned = find(b, b"irot").and_then(|r| b.get(r + 4)).is_some_and(|a| a & 1 == 1);
    Some(if turned { (h, w) } else { (w, h) })
}

/// An SVG's size from its root element: `width`/`height` in pixels, else its `viewBox`.
fn svg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let start = find(b, b"<svg")?;
    let end = start + find(b.get(start..)?, b">")?;
    let tag = std::str::from_utf8(b.get(start..end)?).ok()?;
    let attr = |name: &str| -> Option<&str> {
        let mut rest = tag;
        loop {
            let i = rest.find(name)?;
            let before = rest.get(..i)?.chars().last();
            rest = rest.get(i + name.len()..)?;
            if before.is_some_and(char::is_whitespace)
                && let Some(v) = rest.trim_start().strip_prefix('=')
            {
                let v = v.trim_start();
                let q = v.chars().next()?;
                let v = v.get(q.len_utf8()..)?;
                return v.get(..v.find(q)?);
            }
        }
    };
    let px = |v: &str| -> Option<f64> {
        let v = v.trim();
        let n = v.strip_suffix("px").unwrap_or(v);
        n.parse::<f64>().ok().filter(|n| *n > 0.0)
    };
    let (w, h) = match (attr("width").and_then(px), attr("height").and_then(px)) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            let vb: Vec<f64> = attr("viewBox")?.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
            (*vb.get(2)?, *vb.get(3)?)
        }
    };
    let px = |v: f64| (v.is_finite() && v >= 1.0 && v <= f64::from(1u32 << 16)).then(|| v.round() as u32);
    Some((px(w)?, px(h)?))
}

/// An EBML variable-length integer at `i`: its value (the marker bit kept for an id, dropped for a
/// size) and its length.
fn vint(b: &[u8], i: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *b.get(i)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut v = if keep_marker { u64::from(first) } else { u64::from(first) & (0xff >> len) };
    for k in 1..len {
        v = (v << 8) | u64::from(*b.get(i + k)?);
    }
    Some((v, len))
}

/// WebM/Matroska: the first video track's pixel size, from the elements at the file's start.
fn webm_dims(b: &[u8]) -> Option<(u32, u32)> {
    const SEGMENT: u64 = 0x1853_8067;
    const TRACKS: u64 = 0x1654_ae6b;
    const TRACK: u64 = 0xae;
    const VIDEO: u64 = 0xe0;
    const WIDTH: u64 = 0xb0;
    const HEIGHT: u64 = 0xba;
    const CLUSTER: u64 = 0x1f43_b675;
    let (mut w, mut h) = (None, None);
    let mut i = 0;
    while i < b.len() {
        let (id, n) = vint(b, i, true)?;
        let (size, m) = vint(b, i + n, false)?;
        let body = i + n + m;
        let unknown = size == (1u64 << (7 * m)) - 1;
        match id {
            // Containers on the way to the video's size: go into them.
            SEGMENT | TRACKS | TRACK | VIDEO => i = body,
            WIDTH | HEIGHT => {
                let len = usize::try_from(size).ok()?;
                let mut v = 0u32;
                for k in 0..len.min(4) {
                    v = (v << 8) | u32::from(*b.get(body + k)?);
                }
                if id == WIDTH { w = w.or(Some(v)) } else { h = h.or(Some(v)) }
                if let (Some(w), Some(h)) = (w, h) {
                    return Some((w, h));
                }
                i = body + len;
            }
            CLUSTER => return None,
            _ if unknown => return None,
            _ => i = body + usize::try_from(size).ok()?,
        }
    }
    None
}

/// MP4/QuickTime: the first track with a size (`tkhd`), turned as its matrix says, when the movie's
/// index (`moov`) comes first.
fn mp4_dims(b: &[u8]) -> Option<(u32, u32)> {
    fn boxes(b: &[u8], from: usize, to: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
        let mut i = from;
        while i + 8 <= to {
            let size = usize::try_from(be32(b, i)?).ok()?;
            let (body, end) = match size {
                1 => (i + 16, i + usize::try_from(u64::from_be_bytes(b.get(i + 8..i + 16)?.try_into().ok()?)).ok()?),
                0 => (i + 8, to),
                s => (i + 8, i + s),
            };
            if b.get(i + 4..i + 8)? == want {
                return Some((body, end.min(to)));
            }
            if end <= i {
                return None;
            }
            i = end;
        }
        None
    }
    let (moov, moov_end) = boxes(b, 0, b.len(), b"moov")?;
    let mut at = moov;
    while let Some((trak, trak_end)) = boxes(b, at, moov_end, b"trak") {
        if let Some((tkhd, _)) = boxes(b, trak, trak_end, b"tkhd") {
            let v1 = b.get(tkhd) == Some(&1);
            let base = tkhd + if v1 { 88 } else { 76 };
            let matrix = tkhd + if v1 { 52 } else { 40 };
            let (w, h) = (be32(b, base)? >> 16, be32(b, base + 4)? >> 16);
            if w > 0 && h > 0 {
                // A quarter turn puts zeros where the matrix's a and d are.
                let turned = be32(b, matrix)? == 0 && be32(b, matrix + 16)? == 0;
                return Some(if turned { (h, w) } else { (w, h) });
            }
        }
        at = trak_end;
    }
    None
}

/// What a message mentions: full paths (or ones from the home folder) in its links and images,
/// in code, and in its words; and, in links and images, paths in the session's folder. Each once,
/// in order, at most [`MOST_MENTIONED`]. Words and code count only for files of the kinds people
/// show (images, recordings, sounds, documents), so prose about `/api/...` routes isn't taken for
/// files.
pub fn mentioned(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |p: String| {
        if !p.is_empty() && out.len() < MOST_MENTIONED && !out.contains(&p) {
            out.push(p);
        }
    };
    // Links and images: [..](dest) / ![..](dest), dest maybe <in brackets>, maybe with a title.
    let mut rest = text;
    while let Some(i) = rest.find("](") {
        let Some(after) = rest.get(i + 2..) else { break };
        rest = after;
        let dest = if let Some(inner) = rest.strip_prefix('<') {
            inner.split('>').next().unwrap_or("")
        } else {
            rest.split([')', ' ', '\n']).next().unwrap_or("")
        };
        if let Some(p) = link_path(dest) {
            add(p);
        }
    }
    // Code spans that are a path.
    let mut parts = text.split('`');
    parts.next();
    while let (Some(code), _) = (parts.next(), parts.next()) {
        let c = code.trim();
        if !c.contains(char::is_whitespace) && shown_kind(c) && let Some(p) = bare_path(c) {
            add(p);
        }
    }
    // Paths among the words.
    for word in text.split(char::is_whitespace) {
        let w = word.trim_start_matches(['(', '[', '{', '"', '\'', '<', '*', '_', '`']).trim_end_matches([')', ']', '}', '"', '\'', '>', ',', '.', ';', ':', '!', '?', '*', '_', '`']);
        if w.contains("](") || w.contains("://") {
            continue;
        }
        if shown_kind(w) && let Some(p) = bare_path(w) {
            add(p);
        }
    }
    out
}

/// A link's destination as a path: a full one, one from home, a `file://` one, or (resolved later)
/// one in the session's folder. Not a web address.
fn link_path(dest: &str) -> Option<String> {
    let d = dest.trim();
    if d.is_empty() || d.starts_with('#') || d.starts_with("//") {
        return None;
    }
    let d = if let Some(p) = d.strip_prefix("file://") {
        p.strip_prefix("localhost").unwrap_or(p)
    } else if d.contains("://") || d.starts_with("mailto:") || d.starts_with("data:") || d.starts_with("tel:") {
        return None;
    } else {
        d
    };
    let d = d.split(['?', '#']).next().unwrap_or(d);
    let p = percent_decode(d);
    (!p.is_empty() && !p.contains('\0')).then_some(p)
}

fn bare_path(w: &str) -> Option<String> {
    let full = w.starts_with('/') || w.starts_with("~/");
    let deep = w.get(1..).is_some_and(|r| r.contains('/'));
    (full && w.len() > 2 && (deep || w.contains('.'))).then(|| percent_decode(w))
}

/// Whether a name is of a kind people show: images, recordings, sounds, documents, logs.
fn shown_kind(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else { return false };
    matches!(
        ext.to_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "heic" | "svg" | "bmp" | "mp4" | "webm" | "mov" | "mkv" | "mp3" | "wav" | "ogg" | "oga" | "flac" | "m4a" | "aac" | "pdf" | "log" | "md" | "csv"
    )
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(c) = bytes.next() {
        if c == b'%' {
            let mut ahead = bytes.clone();
            if let (Some(h), Some(l)) = (ahead.next().and_then(|h| char::from(h).to_digit(16)), ahead.next().and_then(|l| char::from(l).to_digit(16)))
                && let Ok(v) = u8::try_from(h * 16 + l)
            {
                out.push(v);
                bytes = ahead;
                continue;
            }
        }
        out.push(c);
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_mention_the_files_they_show_and_not_routes() {
        let said = "Here's the phone view: ![phone](/tmp/kiln-header-390.png) and the [log](~/logs/run%201.log).\n\
                    The recording is `/home/ada/rec/flow.webm`, the report at /home/ada/report.pdf.\n\
                    The API answers at `/api/harness/sessions` and lives in /home/ada/kiln/crates.\n\
                    See [the docs](https://example.com/a.png) and [notes](docs/notes.md).";
        assert_eq!(
            mentioned(said),
            vec!["/tmp/kiln-header-390.png", "~/logs/run 1.log", "docs/notes.md", "/home/ada/rec/flow.webm", "/home/ada/report.pdf"],
        );
        assert!(mentioned("![x](file:///tmp/a%20b.png)").contains(&"/tmp/a b.png".to_owned()));
        assert!(mentioned("no paths here, only a/b and 1/2").is_empty());
    }

    #[test]
    fn a_relative_mention_is_in_the_sessions_folder() {
        assert_eq!(resolve("docs/notes.md", "/home/ada/kiln").as_deref(), Some("/home/ada/kiln/docs/notes.md"));
        assert_eq!(resolve("../shots/a.png", "/home/ada/kiln").as_deref(), Some("/home/ada/shots/a.png"));
        assert_eq!(resolve("/tmp/a.png", "/home/ada").as_deref(), Some("/tmp/a.png"));
        assert_eq!(resolve("a.png", ""), None);
    }

    #[test]
    fn secrets_are_never_read() {
        for p in ["/home/ada/.ssh/id_ed25519", "/home/ada/.aws/credentials", "/srv/app/.env.local", "/etc/tls/server.key", "/home/ada/.config/kiln/kiln.toml", "/proc/self/environ", "/home/ada/.kube/config"] {
            assert!(secret(Path::new(p)), "{p} is a secret");
        }
        for p in ["/tmp/kiln-header-390.png", "/home/ada/notes/keys-to-success.md", "/home/ada/report.pdf"] {
            assert!(!secret(Path::new(p)), "{p} is not a secret");
        }
    }

    #[test]
    fn types_come_from_the_bytes_and_text_from_its_name() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x04\x92\0\0\x09\xe4\x08\x06\0\0\0";
        assert_eq!(sniff(png, Path::new("/tmp/shot.bin")), "image/png");
        assert_eq!(dims(png, "image/png"), Some((1170, 2532)));
        assert_eq!(sniff(b"export const a = 1;\n", Path::new("/w/a.ts")), "text/plain; charset=utf-8", ".ts is TypeScript here");
        assert_eq!(sniff(b"# Notes\n", Path::new("/w/notes.md")), "text/markdown; charset=utf-8");
        assert_eq!(sniff(b"\0\x01binary", Path::new("/w/a.log")), "application/octet-stream");
        assert!(!viewable("application/octet-stream") && viewable("video/webm") && viewable("text/plain; charset=utf-8"));
        // What pages use: fonts, WebAssembly, models.
        assert_eq!(sniff(b"wOF2\0\x01\0\0", Path::new("/w/geist.woff2")), "font/woff2");
        assert_eq!(sniff(b"\0asm\x01\0\0\0", Path::new("/w/lib.wasm")), "application/wasm");
        assert_eq!(sniff(b"glTF\x02\0\0\0", Path::new("/w/robot.glb")), "model/gltf-binary");
        assert_eq!(sniff(b"\0\x01\0\0\0\x0f", Path::new("/w/a.ttf")), "font/ttf");
        assert!(viewable("font/woff2") && viewable("application/wasm") && viewable("model/gltf-binary"));
    }

    #[test]
    fn image_sizes_come_from_their_headers() {
        let gif = b"GIF89a\x90\x01\x2c\x01";
        assert_eq!(dims(gif, "image/gif"), Some((400, 300)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[0x7f, 0x02, 0x00, 0xdf, 0x01, 0x00]);
        assert_eq!(dims(&webp, "image/webp"), Some((640, 480)));
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 390 844"><rect/></svg>"#;
        assert_eq!(dims(svg, "image/svg+xml"), Some((390, 844)));
        let svg = br#"<svg width="120px" height="80" xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert_eq!(dims(svg, "image/svg+xml"), Some((120, 80)));
        // A JPEG with EXIF orientation 6 (a quarter turn): shown 3024 wide as 4032 tall.
        let mut jpeg = vec![0xff, 0xd8];
        let tiff = [b"MM\0\x2a\0\0\0\x08".as_slice(), &[0, 1], &[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0], &[0, 0, 0, 0]].concat();
        let app1 = [b"Exif\0\0".as_slice(), &tiff].concat();
        jpeg.extend_from_slice(&[0xff, 0xe1]);
        jpeg.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap_or(0).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xff, 0xc0, 0, 17, 8, 0x0b, 0xd0, 0x0f, 0xc0, 3]);
        jpeg.extend_from_slice(&[0; 9]);
        assert_eq!(dims(&jpeg, "image/jpeg"), Some((3024, 4032)));
    }

    #[test]
    fn a_recordings_size_comes_from_its_start() {
        // EBML header, then a Segment of unknown size holding Tracks > TrackEntry > Video.
        let mut webm = vec![0x1a, 0x45, 0xdf, 0xa3, 0x84, 0x42, 0x82, 0x81, 0x77];
        webm.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        webm.extend_from_slice(&[0x16, 0x54, 0xae, 0x6b, 0x8c, 0xae, 0x8a, 0xe0, 0x88, 0xb0, 0x82, 0x01, 0x86, 0xba, 0x82, 0x03, 0x4c]);
        assert_eq!(dims(&webm, "video/webm"), Some((390, 844)));
    }

    #[test]
    fn a_file_is_read_a_piece_at_a_time_with_what_it_is() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("shot.png");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x01\x86\0\0\x03\x4c\x08\x06\0\0\0".to_vec();
        png.resize(3000, 7);
        std::fs::write(&f, &png)?;
        let path = f.to_string_lossy().into_owned();
        let (meta, first) = read(&path, 0, 1000).map_err(anyhow::Error::msg)?;
        assert_eq!((meta.size, meta.mime.as_str(), meta.width, meta.height), (3000, "image/png", Some(390), Some(844)));
        assert_eq!(Some(first.as_slice()), png.get(..1000));
        let (_, last) = read(&path, 2500, 1000).map_err(anyhow::Error::msg)?;
        assert_eq!(Some(last.as_slice()), png.get(2500..));
        assert!(read(&dir.join("gone.png").to_string_lossy(), 0, 10).is_err());
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2])?;
        assert!(read(&dir.join("blob.bin").to_string_lossy(), 0, 10).is_err_and(|e| e.contains("doesn't show")));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
