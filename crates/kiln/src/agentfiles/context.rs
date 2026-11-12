//! A built-in Agentfile's build context, made where a build needs it (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, steps 19 to 28; plan 01a1cf48e8157494ac9a51f7a564f526, step 3):
//! its Agentfile, and for each platform asked its harness's directory, `<harness>-<arch>[-<variant>]`,
//! where its HARNESS line takes it from. Each file the pin names is downloaded and held to the pin's size
//! and SHA-256 as it streams: a HARNESS source fetched over http(s) carries no checksum, so kiln fetches
//! it and hands the build a path (research 01a1cf48e8157494b048e1fb2c437f08, f26). The built-in's own
//! small files, its harness.json, are written beside them. A directory is made under a name of its own
//! and renamed into place only once whole, so a file that fails leaves nothing behind. And its recipe:
//! the SHA-256 of everything a build of it takes, the tag its image is built and found under.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::builtin::Builtin;

/// What the fetcher reads of a pin: its builds. The rest is its pin.sh's record of how its publisher's
/// build was checked, for people; steps 19 to 28's pins differ there (f7).
#[derive(Debug, Deserialize)]
pub struct Pin {
    pub builds: Vec<Build>,
}

/// One platform's build, and its variant (a libc) where the publisher makes more than one.
#[derive(Debug, Deserialize)]
pub struct Build {
    pub platform: String,
    #[serde(default)]
    pub variant: Option<String>,
    pub files: Vec<PinnedFile>,
}

/// A file of a build: where it comes from, its SHA-256 and size, and where it goes in the harness's
/// directory, with its mode. An archive's `unpack` says what of it to take.
#[derive(Debug, Deserialize)]
pub struct PinnedFile {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub path: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub unpack: Option<serde_json::Value>,
}

/// Why a context wasn't made.
#[derive(Debug)]
pub enum Error {
    /// The pin isn't one the fetcher can follow.
    Pin(String),
    /// No build of the pin is for the platform (and variant) asked.
    Platform { asked: String, available: Vec<String> },
    /// The pinned URL couldn't be fetched.
    Fetch { url: String, why: String },
    /// It answered with something other than its file.
    Status { url: String, status: u16 },
    /// Its body wasn't the pinned size: `observed` is as far as it got, at most one chunk past the pin.
    Size { url: String, expected: u64, observed: u64 },
    /// Its body wasn't the pinned file.
    Digest { url: String, expected: String, observed: String },
    /// An archive: unpacking one is step 25's unpacker's, not in this build of kiln yet.
    Unpack { url: String },
    /// The disk.
    Io { path: PathBuf, why: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pin(why) => write!(f, "its pin: {why}"),
            Self::Platform { asked, available } => write!(f, "its pin has no build for {asked}; it has {}", available.join(", ")),
            Self::Fetch { url, why } => write!(f, "{url}: {why}"),
            Self::Status { url, status } => write!(f, "{url} answered {status}"),
            Self::Size { url, expected, observed } => write!(f, "{url}: {observed} bytes, pinned {expected}"),
            Self::Digest { url, expected, observed } => write!(f, "{url}: SHA-256 {observed}, pinned {expected}"),
            Self::Unpack { url } => write!(f, "{url} is an archive to unpack, which this build of kiln can't do yet"),
            Self::Io { path, why } => write!(f, "{}: {why}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Io { path: path.to_owned(), why: e.to_string() }
}

impl Pin {
    pub fn read(text: &str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|e| Error::Pin(e.to_string()))
    }

    /// The build for `platform` and `variant`. Without a variant asked, a platform's only build.
    pub fn build(&self, platform: &str, variant: Option<&str>) -> Result<&Build, Error> {
        let mut of: Vec<&Build> = self.builds.iter().filter(|b| b.platform == platform).collect();
        if let Some(v) = variant {
            of.retain(|b| b.variant.as_deref() == Some(v));
        }
        match of.as_slice() {
            [one] => Ok(*one),
            _ => Err(Error::Platform {
                asked: variant.map_or_else(|| platform.to_owned(), |v| format!("{platform} {v}")),
                available: self.builds.iter().map(|b| b.variant.as_deref().map_or_else(|| b.platform.clone(), |v| format!("{} {v}", b.platform))).collect(),
            }),
        }
    }
}

/// The harness's directory for a build: `<harness>-<arch>[-<variant>]`, `arch` as BuildKit's TARGETARCH
/// names it (`linux/arm64/v8`: `arm64`).
pub fn harness_dir(harness: &str, platform: &str, variant: Option<&str>) -> Result<String, Error> {
    let mut parts = platform.split('/');
    let (Some("linux"), Some(arch)) = (parts.next(), parts.next()) else {
        return Err(Error::Pin(format!("{platform} isn't a Linux platform")));
    };
    if arch.is_empty() || !arch.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::Pin(format!("{platform} names no architecture")));
    }
    Ok(match variant {
        Some(v) => format!("{harness}-{arch}-{v}"),
        None => format!("{harness}-{arch}"),
    })
}

/// A pinned file's path, inside its harness's directory: relative, with no empty, `.` or `..` part.
fn inside(path: &str) -> Result<&Path, Error> {
    if path.starts_with('/') || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(Error::Pin(format!("{path} isn't a path inside its harness's directory")));
    }
    Ok(Path::new(path))
}

/// A pinned file's mode, octal permissions (`0755`).
fn octal(mode: &str) -> Result<u32, Error> {
    let bits = u32::from_str_radix(mode, 8).ok().filter(|m| (3..=4).contains(&mode.len()) && *m <= 0o777);
    bits.ok_or_else(|| Error::Pin(format!("{mode} isn't octal permissions")))
}

/// The built-in's own files for a build of `variant`, by name: `harness.<variant>.json` is its harness.json,
/// in place of the plain one, and another variant's harness file isn't one of them.
pub fn files_for(b: &Builtin, variant: Option<&str>) -> Vec<(&'static str, &'static str)> {
    let own = variant.map(|v| format!("harness.{v}.json"));
    let has_own = own.as_deref().is_some_and(|o| b.files.iter().any(|(n, _)| *n == o));
    let variant_file = |name: &str| name.strip_prefix("harness.").and_then(|r| r.strip_suffix(".json")).is_some_and(|v| !v.is_empty());
    b.files
        .iter()
        .filter_map(|&(name, text)| {
            if own.as_deref() == Some(name) {
                Some(("harness.json", text))
            } else if name == "harness.json" {
                (!has_own).then_some((name, text))
            } else if variant_file(name) {
                None
            } else {
                Some((name, text))
            }
        })
        .collect()
}

/// The base the Agentfile builds on when none is given: its `ARG BASE=` default.
pub fn default_base(agentfile: &str) -> Option<&str> {
    agentfile.lines().map(str::trim).find_map(|l| l.strip_prefix("ARG BASE="))
}

/// The recipe of `b`'s image built on `base` for `variant`: the SHA-256, in hex, of what such a build takes
/// (its Agentfile, the files its harness's directory is given for the variant, its pin, the base and the
/// variant), each field length-prefixed so no two sets of fields make one stream. It's the tag the image
/// is built and found under.
pub fn recipe(b: &Builtin, base: &str, variant: Option<&str>) -> String {
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        h.update(bytes);
    };
    field(b"kiln agentfile recipe 1");
    field(b.text.as_bytes());
    for (name, text) in files_for(b, variant) {
        field(name.as_bytes());
        field(text.as_bytes());
    }
    field(b.pin.as_bytes());
    field(base.as_bytes());
    field(variant.unwrap_or_default().as_bytes());
    hex::encode(h.finalize())
}

/// Writes `b`'s build context into `dir`, made where it's missing: for each of `platforms` (`linux/amd64`
/// ...) its harness's directory, from the pin's build for that platform and `variant`, then its Agentfile.
/// Every build is found before anything is fetched, nothing already there is replaced, and a context
/// that fails anywhere leaves none of itself: a part of one builds nothing. Returns the harness's
/// directories made.
pub async fn materialize(b: &Builtin, platforms: &[String], variant: Option<&str>, dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let pin = Pin::read(b.pin)?;
    let mut chosen = Vec::new();
    for platform in platforms {
        let build = pin.build(platform, variant)?;
        chosen.push((harness_dir(b.harness, platform, build.variant.as_deref())?, build));
    }
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let client = crate::http::client::Client::public().map_err(|e| Error::Fetch { url: String::new(), why: format!("kiln's HTTP client: {e:#}") })?;
    let mut made = Vec::new();
    let result = async {
        for (name, build) in &chosen {
            let at = dir.join(name);
            let staging = dir.join(format!(".{name}.{}", crate::util::new_id()));
            let filled = match fill(&client, b, build, &staging).await {
                Ok(()) => std::fs::rename(&staging, &at).map_err(io(&at)),
                Err(e) => Err(e),
            };
            if filled.is_err() {
                let _ = std::fs::remove_dir_all(&staging);
            }
            filled?;
            made.push(at);
        }
        write_new(&dir.join("Agentfile"), b.text)
    }
    .await;
    if let Err(e) = result {
        for at in &made {
            let _ = std::fs::remove_dir_all(at);
        }
        return Err(e);
    }
    Ok(made)
}

/// One harness's directory, made at `staging`: each pinned file, then the built-in's own files.
async fn fill(client: &crate::http::client::Client, b: &Builtin, build: &Build, staging: &Path) -> Result<(), Error> {
    std::fs::create_dir(staging).map_err(io(staging))?;
    for f in &build.files {
        // An archive's path may be the directory itself (`.`: a tree merged into it), the unpacker's to place.
        if f.unpack.is_some() {
            return Err(Error::Unpack { url: f.url.clone() });
        }
        let rel = inside(&f.path)?;
        let mode = f.mode.as_deref().map(octal).transpose()?;
        let arrived = fetch(client, f, staging).await?;
        let to = staging.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        if to.symlink_metadata().is_ok() {
            return Err(Error::Pin(format!("{} is pinned twice", f.path)));
        }
        std::fs::rename(&arrived.path, &to).map_err(io(&to))?;
        if let Some(m) = mode {
            crate::os::set_mode(&to, m).map_err(io(&to))?;
        }
    }
    for (name, text) in files_for(b, build.variant.as_deref()) {
        write_new(&staging.join(inside(name)?), text)?;
    }
    Ok(())
}

/// `f` downloaded into `dir`, its size and SHA-256 held to the pin's as it streams.
async fn fetch(client: &crate::http::client::Client, f: &PinnedFile, dir: &Path) -> Result<crate::updates::Arrived, Error> {
    let fetch_failed = |e: anyhow::Error| Error::Fetch { url: f.url.clone(), why: format!("{e:#}") };
    let mut resp = crate::updates::get_following(client, &f.url, crate::http::client::DEFAULT_TIMEOUT).await.map_err(fetch_failed)?;
    if !resp.is_success() {
        return Err(Error::Status { url: f.url.clone(), status: resp.status() });
    }
    let disk = |e: anyhow::Error| Error::Io { path: dir.to_owned(), why: format!("{e:#}") };
    let mut arriving = crate::updates::Arriving::new(dir).map_err(disk)?;
    let mut got: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(fetch_failed)? {
        got = got.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if got > f.size {
            return Err(Error::Size { url: f.url.clone(), expected: f.size, observed: got });
        }
        arriving.write(&chunk).map_err(disk)?;
    }
    let arrived = arriving.done().map_err(disk)?;
    if arrived.size != f.size {
        return Err(Error::Size { url: f.url.clone(), expected: f.size, observed: arrived.size });
    }
    if arrived.sha256 != f.sha256 {
        return Err(Error::Digest { url: f.url.clone(), expected: f.sha256.clone(), observed: arrived.sha256.clone() });
    }
    Ok(arrived)
}

/// `text` written at `path`, which mustn't be there.
fn write_new(path: &Path, text: &str) -> Result<(), Error> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(io(path))?;
    file.write_all(text.as_bytes()).map_err(io(path))
}

#[cfg(test)]
mod tests;
