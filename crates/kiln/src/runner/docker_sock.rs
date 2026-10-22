//! A `docker` leg's Docker socket (docs/ci.md W13-R04). A hosted runner's steps share a machine
//! with its Docker daemon, so what they bind-mount is what they see: `docker run -v ~/.cargo:/cargo`
//! (as `cross` does) mounts the runner's own `~/.cargo`. A `docker` leg's machine is a container
//! the daemon doesn't look into, so the leg gets this socket in the daemon's place. It passes every
//! request on, and translates the bind mounts' sources of the containers the leg creates to the
//! host's paths: a path the leg has from the host to that path, the kernel's (`/proc`, `/sys`,
//! `/dev`, `/lib/modules`) as they are, and anything else to the leg container's own root, which
//! the host has mounted.
//!
//! It's for fidelity, not containment: a leg given the host's Docker is trusted (W13-R04), and
//! the daemon still resolves a symlink in the leg's files against the host's root.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

/// A request head larger than this goes on as it came, and the connection's rest with it.
const MAX_HEAD: usize = 64 << 10;
/// A container create's body larger than this goes on untranslated.
const MAX_CREATE: usize = 16 << 20;
/// The kernel's and the daemon's own: a hosted runner's are its machine's, as the host's are here.
const HOST_OWN: &[&str] = &["/proc", "/sys", "/dev", "/lib/modules", "/var/lib/docker"];

/// Where a leg container's paths are on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paths {
    /// Its root, as the host has it mounted: `None` when it couldn't be found.
    root: Option<PathBuf>,
    /// Its mounts with a source on the host, `(inside, host)`, the deepest first.
    mounts: Vec<(PathBuf, PathBuf)>,
}

impl Paths {
    pub fn new(root: Option<PathBuf>, mounts: Vec<(PathBuf, PathBuf)>) -> Paths {
        // `/var/run` is `/run` in the images kiln runs (FHS 3.0): the leg's socket is at both.
        let mut mounts: Vec<(PathBuf, PathBuf)> = mounts.into_iter().map(|(inside, host)| (normalize(&inside.to_string_lossy()), host)).collect();
        let aliases: Vec<(PathBuf, PathBuf)> =
            mounts.iter().filter_map(|(inside, host)| inside.strip_prefix("/var/run").ok().map(|rest| (normalize(&Path::new("/run").join(rest).to_string_lossy()), host.clone()))).collect();
        mounts.extend(aliases);
        mounts.sort_by_key(|(inside, _)| std::cmp::Reverse(inside.components().count()));
        Paths { root, mounts }
    }

    /// Whether the leg container's own files are the host's to mount.
    pub fn has_root(&self) -> bool {
        self.root.is_some()
    }

    /// Where `inside`, a path in the leg container, is on the host: `None` for what isn't a path
    /// (a volume's name), or what can't be found, which goes on as it is.
    pub fn host(&self, inside: &str) -> Option<String> {
        if !inside.starts_with('/') {
            return None;
        }
        let path = normalize(inside);
        if HOST_OWN.iter().any(|own| path.starts_with(own)) {
            return Some(path.to_string_lossy().into_owned());
        }
        if let Some((at, host)) = self.mounts.iter().find(|(at, _)| path.starts_with(at)) {
            return Some(joined(host, path.strip_prefix(at).unwrap_or(Path::new(""))));
        }
        let root = self.root.as_ref()?;
        Some(joined(root, path.strip_prefix("/").unwrap_or(&path)))
    }
}

/// `p` with `.` and `..` resolved as written: never above `/`.
fn normalize(p: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in Path::new(p).components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

fn joined(base: &Path, rest: &Path) -> String {
    if rest.as_os_str().is_empty() { base.to_string_lossy().into_owned() } else { base.join(rest).to_string_lossy().into_owned() }
}

/// A container create's body with its bind mounts' sources on the host: `None` when there's
/// nothing to change (or it isn't JSON), and it goes on as it came.
fn translate_create(body: &[u8], paths: &Paths) -> Option<Vec<u8>> {
    use serde_json::Value;
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let config = v.get_mut("HostConfig")?;
    let mut changed = false;
    // `-v src:dst[:options]`: a source that isn't a path is a volume's name.
    if let Some(binds) = config.get_mut("Binds").and_then(Value::as_array_mut) {
        for bind in binds {
            let Some((src, rest)) = bind.as_str().and_then(|b| b.split_once(':')) else { continue };
            if let Some(host) = paths.host(src).filter(|h| h != src) {
                *bind = Value::String(format!("{host}:{rest}"));
                changed = true;
            }
        }
    }
    // `--mount type=bind,source=…`.
    if let Some(mounts) = config.get_mut("Mounts").and_then(Value::as_array_mut) {
        for mount in mounts.iter_mut().filter_map(Value::as_object_mut) {
            if mount.get("Type").and_then(Value::as_str) != Some("bind") {
                continue;
            }
            let Some(src) = mount.get("Source").and_then(Value::as_str) else { continue };
            if let Some(host) = paths.host(src).filter(|h| h != src) {
                mount.insert("Source".to_owned(), Value::String(host));
                changed = true;
            }
        }
    }
    if changed { serde_json::to_vec(&v).ok() } else { None }
}

/// A chunked body read as it comes (RFC 9112 §7.1): where it ends, and its data.
#[derive(Debug, Default)]
struct Chunked {
    stage: Stage,
    line: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// A chunk's size line.
    #[default]
    Size,
    /// A chunk's data: so many bytes more.
    Data(u64),
    /// The line end after a chunk's data.
    DataEnd,
    /// The trailers, to the empty line that ends the body.
    Trailers,
}

impl Chunked {
    /// Takes what of `input` belongs to the body: how much, and whether the body ended there. Its
    /// data goes to `data` when given. `None` when it isn't chunked as it says.
    fn take(&mut self, input: &[u8], mut data: Option<&mut Vec<u8>>) -> Option<(usize, bool)> {
        let mut at = 0;
        while let Some(rest) = input.get(at..).filter(|r| !r.is_empty()) {
            if let Stage::Data(left) = self.stage {
                let n = usize::try_from(left).unwrap_or(usize::MAX).min(rest.len());
                if let Some(d) = data.as_deref_mut() {
                    d.extend_from_slice(rest.get(..n)?);
                }
                at += n;
                let left = left - n as u64;
                self.stage = if left == 0 { Stage::DataEnd } else { Stage::Data(left) };
                continue;
            }
            let Some(end) = rest.iter().position(|&b| b == b'\n') else {
                self.line.extend_from_slice(rest);
                if self.line.len() > 4096 {
                    return None;
                }
                return Some((input.len(), false));
            };
            self.line.extend_from_slice(rest.get(..end)?);
            at += end + 1;
            let line = std::mem::take(&mut self.line);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            match self.stage {
                Stage::Size => {
                    let size = line.split(|&b| b == b';').next().unwrap_or_default();
                    let size = std::str::from_utf8(size).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok())?;
                    self.stage = if size == 0 { Stage::Trailers } else { Stage::Data(size) };
                }
                Stage::DataEnd if line.is_empty() => self.stage = Stage::Size,
                Stage::DataEnd => return None,
                Stage::Trailers if line.is_empty() => return Some((at, true)),
                Stage::Trailers | Stage::Data(_) => {}
            }
        }
        Some((at, false))
    }
}

/// A request's body, as its head frames it.
#[derive(Debug)]
enum Body {
    Length(u64),
    Chunked(Chunked),
}

impl Body {
    /// Takes what of `input` belongs to the body: as [`Chunked::take`].
    fn take(&mut self, input: &[u8], data: Option<&mut Vec<u8>>) -> Option<(usize, bool)> {
        match self {
            Body::Length(left) => {
                let n = usize::try_from(*left).unwrap_or(usize::MAX).min(input.len());
                if let Some(d) = data {
                    d.extend_from_slice(input.get(..n)?);
                }
                *left -= n as u64;
                Some((n, *left == 0))
            }
            Body::Chunked(c) => c.take(input, data),
        }
    }
}

/// What a request head says.
#[derive(Debug)]
struct Head {
    /// The head as it came.
    raw: Vec<u8>,
    /// Its start line, and its fields but its framing's, to say again with a new length.
    start: String,
    fields: Vec<(String, Vec<u8>)>,
    create: bool,
    /// The connection is taken over once the request is through: an attach's or an exec's
    /// streams, BuildKit's session.
    takeover: bool,
    body: Option<Body>,
}

/// `None` while the head isn't all there; `Some(None)` for what isn't HTTP/1.1 as expected.
fn parse_head(buf: &[u8]) -> Option<Option<Head>> {
    let mut fields = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut fields);
    let n = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return None,
        Err(_) => return Some(None),
    };
    let (Some(method), Some(target), Some(version)) = (req.method, req.path, req.version) else { return Some(None) };
    let path = target.split('?').next().unwrap_or_default();
    let mut body = None;
    let mut takeover = false;
    let mut kept = Vec::new();
    for f in req.headers.iter() {
        let value = String::from_utf8_lossy(f.value).to_ascii_lowercase();
        if f.name.eq_ignore_ascii_case("transfer-encoding") {
            if value.split(',').any(|c| c.trim() == "chunked") {
                body = Some(Body::Chunked(Chunked::default()));
            }
        } else if f.name.eq_ignore_ascii_case("content-length") {
            if body.is_none() {
                let Ok(len) = value.trim().parse::<u64>() else { return Some(None) };
                body = (len > 0).then_some(Body::Length(len));
            }
        } else {
            takeover |= f.name.eq_ignore_ascii_case("upgrade") || (f.name.eq_ignore_ascii_case("connection") && value.contains("upgrade"));
            kept.push((f.name.to_owned(), f.value.to_vec()));
        }
    }
    // The API's paths, with their version or without: `/v1.51/containers/create`.
    let unversioned = match path.strip_prefix("/v") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.find('/').and_then(|i| rest.get(i..)).unwrap_or_default(),
        _ => path,
    };
    let parts: Vec<&str> = unversioned.split('/').collect();
    // An attach or an exec's start is taken over without saying so by older clients.
    takeover |= matches!(parts.as_slice(), ["", "containers", _, "attach"] | ["", "exec", _, "start"] | ["", "session"] | ["", "grpc"]);
    let create = method == "POST" && unversioned == "/containers/create";
    Some(Some(Head { raw: buf.get(..n)?.to_vec(), start: format!("{method} {target} HTTP/1.{version}"), fields: kept, create, takeover, body }))
}

/// What the leg sends the daemon, read as HTTP/1.1 requests until a connection is taken over:
/// containers' creates translated, everything else as it came.
#[derive(Debug, Default)]
struct Requests {
    buf: Vec<u8>,
    stage: Flow,
}

#[derive(Debug, Default)]
enum Flow {
    /// Awaiting a request's head.
    #[default]
    Head,
    /// Passing a body on; the connection is taken over after it when `.1`.
    Pass(Body, bool),
    /// Gathering a create's body (`raw` as it came, `data` its content) to translate it whole.
    Create { head: Head, body: Body, raw: Vec<u8>, data: Vec<u8> },
    /// Everything as it comes.
    Raw,
}

impl Requests {
    /// Takes what the leg sent, `input`, and puts what goes on to the daemon in `out`.
    fn feed(&mut self, input: &[u8], paths: &RwLock<Paths>, out: &mut Vec<u8>) {
        self.buf.extend_from_slice(input);
        loop {
            match std::mem::replace(&mut self.stage, Flow::Raw) {
                Flow::Raw => {
                    out.append(&mut self.buf);
                    return;
                }
                Flow::Head => {
                    if self.buf.is_empty() {
                        self.stage = Flow::Head;
                        return;
                    }
                    let head = match parse_head(&self.buf) {
                        None if self.buf.len() <= MAX_HEAD => {
                            self.stage = Flow::Head;
                            return;
                        }
                        Some(Some(h)) => h,
                        // Not what was expected: the connection's rest goes on as it came.
                        None | Some(None) => continue,
                    };
                    self.buf.drain(..head.raw.len());
                    let next = if head.takeover { Flow::Raw } else { Flow::Head };
                    self.stage = match head.body {
                        Some(_) if head.create => {
                            let mut head = head;
                            match head.body.take() {
                                Some(body) => Flow::Create { head, body, raw: Vec::new(), data: Vec::new() },
                                None => next,
                            }
                        }
                        Some(body) => {
                            out.extend_from_slice(&head.raw);
                            Flow::Pass(body, head.takeover)
                        }
                        None => {
                            out.extend_from_slice(&head.raw);
                            next
                        }
                    };
                }
                Flow::Pass(mut body, takeover) => {
                    let Some((n, done)) = body.take(&self.buf, None) else { continue };
                    out.extend(self.buf.drain(..n));
                    self.stage = match (done, takeover) {
                        (false, _) => {
                            self.stage = Flow::Pass(body, takeover);
                            return;
                        }
                        (true, true) => Flow::Raw,
                        (true, false) => Flow::Head,
                    };
                }
                Flow::Create { head, mut body, mut raw, mut data } => {
                    let Some((n, done)) = body.take(&self.buf, Some(&mut data)) else {
                        // Not chunked as it says: on as it came.
                        out.extend_from_slice(&head.raw);
                        out.append(&mut raw);
                        continue;
                    };
                    raw.extend(self.buf.drain(..n));
                    if !done && raw.len() <= MAX_CREATE {
                        self.stage = Flow::Create { head, body, raw, data };
                        return;
                    }
                    let translated = if done { translate_create(&data, &paths.read()) } else { None };
                    match translated {
                        Some(new) => {
                            out.extend_from_slice(head.start.as_bytes());
                            out.extend_from_slice(b"\r\n");
                            for (name, value) in &head.fields {
                                out.extend_from_slice(name.as_bytes());
                                out.extend_from_slice(b": ");
                                out.extend_from_slice(value);
                                out.extend_from_slice(b"\r\n");
                            }
                            out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", new.len()).as_bytes());
                            out.extend_from_slice(&new);
                        }
                        None => {
                            out.extend_from_slice(&head.raw);
                            out.append(&mut raw);
                        }
                    }
                    self.stage = match (done, head.takeover) {
                        (false, takeover) => Flow::Pass(body, takeover),
                        (true, true) => Flow::Raw,
                        (true, false) => Flow::Head,
                    };
                }
            }
        }
    }
}

/// A leg's socket, served until it's dropped.
pub struct Socket {
    path: PathBuf,
    paths: Arc<RwLock<Paths>>,
    task: crate::rt::JoinHandle<()>,
}

impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Socket {
    /// Listens at `path` for a leg, passing its connections on to the daemon at `daemon`.
    pub async fn listen(path: PathBuf, daemon: PathBuf) -> anyhow::Result<Socket> {
        let paths = Arc::new(RwLock::new(Paths::default()));
        let (bound, listening) = futures::channel::oneshot::channel::<Result<(), String>>();
        let (at, served) = (path.clone(), paths.clone());
        let task = crate::rt::spawn(async move {
            let listener = match hyper_rt::local::LocalListener::bind(&at, 64) {
                Ok(l) => l,
                Err(e) => {
                    let _ = bound.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = bound.send(Ok(()));
            while let Ok(leg) = listener.accept().await {
                if let Err(e) = crate::rt::spawn_local(connection(leg, daemon.clone(), served.clone())) {
                    tracing::warn!("a leg's Docker connection was dropped: {e}");
                }
            }
        });
        match listening.await {
            Ok(Ok(())) => Ok(Socket { path, paths, task }),
            Ok(Err(e)) => anyhow::bail!("listen at {}: {e}", path.display()),
            Err(_) => anyhow::bail!("listen at {}: the runtime took no task", path.display()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the leg container's paths are, once it's started.
    pub fn set_paths(&self, paths: Paths) {
        *self.paths.write() = paths;
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One of the leg's connections: its requests on to the daemon, the daemon's answers back.
async fn connection(leg: hyper_rt::local::LocalStream, daemon: PathBuf, paths: Arc<RwLock<Paths>>) {
    use hyper_rt::tcp::Shutdown;
    let Ok(up) = hyper_rt::local::LocalStream::connect(&daemon).await else { return };
    let requests = async {
        let mut requests = Requests::default();
        let (mut buf, mut out) = (vec![0u8; 64 << 10], Vec::new());
        while let Ok(n @ 1..) = leg.read(&mut buf).await {
            out.clear();
            requests.feed(buf.get(..n).unwrap_or_default(), &paths, &mut out);
            if !out.is_empty() && up.write_all(&out).await.is_err() {
                break;
            }
        }
        let _ = up.shutdown(Shutdown::Write);
    };
    let answers = async {
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n @ 1..) = up.read(&mut buf).await {
            if leg.write_all(buf.get(..n).unwrap_or_default()).await.is_err() {
                break;
            }
        }
        let _ = leg.shutdown(Shutdown::Write);
    };
    futures::future::join(requests, answers).await;
}

/// Where the leg container `id`'s paths are on the host: its mounts from `docker inspect`, and its
/// root: the graph driver's merged directory (`overlay2`, Podman's `overlay`), else, on an engine
/// with containerd's image store, where Docker mounts it.
pub async fn paths_of(cli: &super::docker::Cli, id: &str) -> anyhow::Result<Paths> {
    let none = std::collections::BTreeMap::new();
    let call = std::time::Duration::from_secs(60);
    // The whole of it: an engine on containerd's image store leaves `GraphDriver` out.
    let out = cli.run(&["inspect".to_owned(), id.to_owned()], &none, call).await?;
    let inspected: serde_json::Value = serde_json::from_str(&out)?;
    let it = inspected.get(0).ok_or_else(|| anyhow::anyhow!("docker inspect {id}: nothing"))?;
    let full = it.get("Id").and_then(serde_json::Value::as_str).unwrap_or_default();
    let mounts = it.get("Mounts").and_then(serde_json::Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mounts = mounts.iter().filter_map(|m| Some((PathBuf::from(m.get("Destination")?.as_str()?), PathBuf::from(m.get("Source")?.as_str().filter(|s| !s.is_empty())?)))).collect();
    let mut root = it.pointer("/GraphDriver/Data/MergedDir").and_then(serde_json::Value::as_str).map(PathBuf::from);
    if root.is_none() {
        let info = cli.run(&["info".to_owned(), "--format".to_owned(), "{{.DockerRootDir}}\t{{.Driver}}".to_owned()], &none, call).await.unwrap_or_default();
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        root = snapshotter_root(&info, full, &mountinfo);
    }
    Ok(Paths::new(root, mounts))
}

/// A container's root on an engine with containerd's image store: `<root dir>/rootfs/<driver>/<id>`
/// (`info` is `<root dir>\t<driver>`), as this machine's mounts show it when they're the engine's;
/// else a mount named for the container.
fn snapshotter_root(info: &str, id: &str, mountinfo: &str) -> Option<PathBuf> {
    if id.is_empty() {
        return None;
    }
    // A mount point is the fifth field, its spaces and such written in octal.
    let points: Vec<String> = mountinfo.lines().filter_map(|l| l.split(' ').nth(4)).map(|p| p.replace("\\040", " ")).collect();
    let expected = info.split_once('\t').map(|(dir, driver)| Path::new(dir.trim()).join("rootfs").join(driver.trim()).join(id));
    if let Some(e) = expected.as_ref().filter(|e| points.is_empty() || points.iter().any(|p| Path::new(p) == e.as_path())) {
        return Some(e.clone());
    }
    points.iter().map(PathBuf::from).find(|p| p.file_name().is_some_and(|n| n == id) || (p.ends_with("rootfs") && p.parent().and_then(Path::file_name).is_some_and(|n| n == id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg() -> Paths {
        Paths::new(
            Some(PathBuf::from("/var/lib/docker/rootfs/overlayfs/abc")),
            vec![
                (PathBuf::from("/work/w/1234/ripgrep"), PathBuf::from("/work/w/1234/ripgrep")),
                (PathBuf::from("/work/w/1234"), PathBuf::from("/work/w/1234")),
                (PathBuf::from("/opt/hostedtoolcache"), PathBuf::from("/work/_tool/kiln-runner")),
                (PathBuf::from("/var/run/docker.sock"), PathBuf::from("/work/w/1234.docker.sock")),
            ],
        )
    }

    #[test]
    fn w13_r04_paths_are_the_hosts() {
        let p = leg();
        // The leg's own files: its image's toolchain, its home, its /tmp.
        assert_eq!(p.host("/usr/local/cargo").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/usr/local/cargo"));
        assert_eq!(p.host("/home/runner/.xargo/").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/home/runner/.xargo"));
        assert_eq!(p.host("/").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc"));
        // What it has from the host is the host's, the deepest mount first.
        assert_eq!(p.host("/work/w/1234/ripgrep/target").as_deref(), Some("/work/w/1234/ripgrep/target"));
        assert_eq!(p.host("/work/w/1234/_temp").as_deref(), Some("/work/w/1234/_temp"));
        assert_eq!(p.host("/opt/hostedtoolcache/node").as_deref(), Some("/work/_tool/kiln-runner/node"));
        // Its socket, at either of its names.
        assert_eq!(p.host("/var/run/docker.sock").as_deref(), Some("/work/w/1234.docker.sock"));
        assert_eq!(p.host("/run/docker.sock").as_deref(), Some("/work/w/1234.docker.sock"));
        // The kernel's are the host's, as a hosted runner's are its machine's.
        assert_eq!(p.host("/proc").as_deref(), Some("/proc"));
        assert_eq!(p.host("/lib/modules").as_deref(), Some("/lib/modules"));
        assert_eq!(p.host("/devices").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/devices"));
        // Never above the leg's root, nor out of a mount.
        assert_eq!(p.host("/../../etc").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/etc"));
        assert_eq!(p.host("/work/w/1234/ripgrep/../../../../etc/passwd").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/etc/passwd"));
        assert_eq!(p.host("/work/w/1234/ripgrep/../../x").as_deref(), Some("/var/lib/docker/rootfs/overlayfs/abc/work/w/x"));
        // A volume's name isn't a path.
        assert_eq!(p.host("cargo-cache"), None);
        // Without the leg's root, only its mounts are known.
        let mounts_only = Paths::new(None, vec![(PathBuf::from("/w"), PathBuf::from("/host/w"))]);
        assert_eq!(mounts_only.host("/w/x").as_deref(), Some("/host/w/x"));
        assert_eq!(mounts_only.host("/usr/local/cargo"), None);
    }

    #[test]
    fn w13_r04_creates_are_translated() -> anyhow::Result<()> {
        let p = leg();
        let body = serde_json::json!({
            "Image": "ghcr.io/cross-rs/x86_64-unknown-linux-musl:0.2.5",
            "HostConfig": {
                "Binds": ["/usr/local/cargo:/cargo:Z", "/work/w/1234/ripgrep:/project:Z,ro", "cache:/cache", "/home/runner/.xargo:/xargo:Z"],
                "Mounts": [{"Type": "bind", "Source": "/tmp/data", "Target": "/data"}, {"Type": "volume", "Source": "named", "Target": "/v"}],
                "Memory": 0
            }
        });
        let out = translate_create(&serde_json::to_vec(&body)?, &p).ok_or_else(|| anyhow::anyhow!("nothing translated"))?;
        let out: serde_json::Value = serde_json::from_slice(&out)?;
        let binds: Vec<&str> = out.pointer("/HostConfig/Binds").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(serde_json::Value::as_str).collect()).unwrap_or_default();
        assert_eq!(
            binds,
            ["/var/lib/docker/rootfs/overlayfs/abc/usr/local/cargo:/cargo:Z", "/work/w/1234/ripgrep:/project:Z,ro", "cache:/cache", "/var/lib/docker/rootfs/overlayfs/abc/home/runner/.xargo:/xargo:Z"]
        );
        assert_eq!(out.pointer("/HostConfig/Mounts/0/Source").and_then(serde_json::Value::as_str), Some("/var/lib/docker/rootfs/overlayfs/abc/tmp/data"));
        assert_eq!(out.pointer("/HostConfig/Mounts/1/Source").and_then(serde_json::Value::as_str), Some("named"));
        assert_eq!(out.get("Image"), body.get("Image"));
        // Nothing to change, or not JSON: on as it came.
        let same = serde_json::json!({"HostConfig": {"Binds": ["/work/w/1234/ripgrep:/src"]}});
        assert_eq!(translate_create(&serde_json::to_vec(&same)?, &p), None);
        assert_eq!(translate_create(b"{not json", &p), None);
        Ok(())
    }

    fn create_request(body: &[u8], chunked: bool) -> Vec<u8> {
        let mut r = b"POST /v1.51/containers/create?name=x HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: Docker-Client/29.0.0 (linux)\r\nContent-Type: application/json\r\n".to_vec();
        if chunked {
            r.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            for piece in body.chunks(7) {
                r.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                r.extend_from_slice(piece);
                r.extend_from_slice(b"\r\n");
            }
            r.extend_from_slice(b"0\r\n\r\n");
        } else {
            r.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            r.extend_from_slice(body);
        }
        r
    }

    /// What `Requests` passes on for `input`, fed `step` bytes at a time.
    fn fed(input: &[u8], step: usize, paths: &RwLock<Paths>) -> Vec<u8> {
        let (mut r, mut out) = (Requests::default(), Vec::new());
        for piece in input.chunks(step.max(1)) {
            r.feed(piece, paths, &mut out);
        }
        out
    }

    #[test]
    fn w13_r04_a_connections_requests_go_on() -> anyhow::Result<()> {
        let paths = RwLock::new(leg());
        let body = serde_json::to_vec(&serde_json::json!({"Image": "alpine", "HostConfig": {"Binds": ["/usr/local/cargo:/cargo"]}}))?;
        let translated = translate_create(&body, &paths.read()).ok_or_else(|| anyhow::anyhow!("nothing translated"))?;
        let ping = b"GET /_ping HTTP/1.1\r\nHost: api.moby.localhost\r\n\r\n".to_vec();
        // A build's context, chunked, then the create a compose `up --build` sends on the same connection.
        let build = b"POST /v1.51/build HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n3;ext=1\r\nabc\r\n0\r\nX-Trailer: y\r\n\r\n".to_vec();
        for chunked in [false, true] {
            let mut input = ping.clone();
            input.extend_from_slice(&build);
            input.extend_from_slice(&create_request(&body, chunked));
            input.extend_from_slice(&ping);
            let mut expected = ping.clone();
            expected.extend_from_slice(&build);
            expected.extend_from_slice(b"POST /v1.51/containers/create?name=x HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: Docker-Client/29.0.0 (linux)\r\nContent-Type: application/json\r\n");
            expected.extend_from_slice(format!("Content-Length: {}\r\n\r\n", translated.len()).as_bytes());
            expected.extend_from_slice(&translated);
            expected.extend_from_slice(&ping);
            for step in [1, 2, 3, 7, 64, input.len()] {
                assert_eq!(String::from_utf8_lossy(&fed(&input, step, &paths)), String::from_utf8_lossy(&expected), "chunked {chunked}, {step} bytes at a time");
            }
        }
        Ok(())
    }

    #[test]
    fn w13_r04_a_taken_over_connection_goes_on_as_it_came() -> anyhow::Result<()> {
        let paths = RwLock::new(leg());
        let body = serde_json::to_vec(&serde_json::json!({"HostConfig": {"Binds": ["/usr/local/cargo:/cargo"]}}))?;
        // After an exec's start, what the leg sends is the process's input, whatever it looks like.
        let mut input = b"POST /v1.51/exec/abc/start HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        input.extend_from_slice(&create_request(&body, false));
        for step in [1, 5, input.len()] {
            assert_eq!(fed(&input, step, &paths), input, "{step} bytes at a time");
        }
        // Older clients take an attach over without saying so.
        let mut attach = b"POST /containers/abc/attach?stream=1 HTTP/1.1\r\nHost: x\r\n\r\n".to_vec();
        attach.extend_from_slice(&create_request(&body, false));
        assert_eq!(fed(&attach, 9, &paths), attach);
        // What isn't HTTP goes on as it came.
        let junk = b"\x16\x03\x01 not http at all\r\n\r\n".to_vec();
        assert_eq!(fed(&junk, 4, &paths), junk);
        Ok(())
    }

    /// The host's Docker through a leg's socket: what a container the leg runs mounts is the leg's
    /// own, found where the host has it, both ways; its output streams back as it does from the
    /// host's own socket.
    #[test]
    fn w13_r04_docker_through_the_legs_socket() -> anyhow::Result<()> {
        let Some(bin) = super::super::docker::find() else { return Ok(()) };
        if !Path::new("/var/run/docker.sock").exists() {
            return Ok(());
        }
        let tmp = tempfile::tempdir()?;
        // The leg's root as the host has it: here a directory with the leg's file in it.
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("home/runner/probe"))?;
        std::fs::write(root.join("home/runner/probe/f"), "the leg's own")?;
        let (at, config) = (tmp.path().join("leg.sock"), tmp.path().join("config"));
        let user = super::super::docker::host_user().map(|(u, g)| format!("{u}:{g}")).unwrap_or_else(|| "0:0".to_owned());
        let leg_root = root.clone();
        let out = crate::rt::block_on(async move {
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock")).await?;
            socket.set_paths(Paths::new(Some(leg_root), Vec::new()));
            let cli = super::super::docker::Cli::new(&bin, config);
            let env = std::collections::BTreeMap::from([("DOCKER_HOST".to_owned(), format!("unix://{}", at.display()))]);
            let args: Vec<String> = ["run", "--rm", "--user", &user, "-v", "/home/runner/probe:/probe", "alpine:3.20", "sh", "-c", "cat /probe/f; echo back > /probe/g"].map(str::to_owned).into();
            cli.run(&args, &env, std::time::Duration::from_secs(300)).await
        })?;
        assert_eq!(out, "the leg's own");
        assert_eq!(std::fs::read_to_string(root.join("home/runner/probe/g"))?, "back\n");
        Ok(())
    }

    /// The corpus's `cross` case (ripgrep's and bat's legs), end to end: in a `docker` executor's
    /// container of kiln's runner image, given Docker through the leg's socket, `cross test
    /// --manifest-path … -- --nocapture` runs in cross's own mode, which mounts the image's Rust
    /// toolchain, as on a hosted runner.
    #[test]
    #[ignore = "downloads cross and the musl target's standard library, and needs kiln's runner image built on this machine"]
    fn w13_r04_cross_in_a_docker_leg() -> anyhow::Result<()> {
        use super::super::docker::{Cli, Containers, PathMap, Spec};
        let Some(bin) = super::super::docker::find() else { return Ok(()) };
        let images = std::process::Command::new(&bin).args(["images", "--format", "{{.Repository}}:{{.Tag}}"]).output()?;
        // Built already, as a routing rule names it.
        if !String::from_utf8_lossy(&images.stdout).lines().any(|l| l.starts_with("kiln/runner-ubuntu-24.04:")) {
            return Ok(());
        }
        let image = "kiln/runner-ubuntu-24.04".to_owned();
        let tmp = tempfile::tempdir()?;
        let (ws, temp) = (tmp.path().join("w").join("probe"), tmp.path().join("w").join("_temp"));
        std::fs::create_dir_all(&ws)?;
        std::fs::create_dir_all(&temp)?;
        let at = tmp.path().join("w.docker.sock");
        let leg = format!("test-cross-{}", std::process::id());
        let cli = Cli::new(&bin, tmp.path().join("config")).owned_by(tmp.path());
        let out = crate::rt::block_on(async move {
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock")).await?;
            let map = PathMap::new(vec![(ws.clone(), ws.to_string_lossy().into_owned()), (temp.clone(), temp.to_string_lossy().into_owned())]);
            let spec = Spec { image, user: super::super::docker::host_user(), docker_socket: true, docker_proxy: Some(at.clone()), ..Spec::default() };
            let c = Containers::start(cli, &leg, map, &ws.to_string_lossy(), Some(spec), Vec::new(), None, &|_| {}).await?;
            let ran = async {
                let job = c.job.as_ref().ok_or_else(|| anyhow::anyhow!("no job container"))?;
                socket.set_paths(paths_of(&c.cli, &job.id).await?);
                let script = format!(
                    "set -e; cd {temp}; curl -fsSL https://github.com/cross-rs/cross/releases/download/v0.2.5/cross-x86_64-unknown-linux-musl.tar.gz | tar xz; cd {ws}; \
                     printf '[workspace]\\nmembers = [\"crates/cli\"]\\nresolver = \"2\"\\n' > Cargo.toml; cargo new -q --lib crates/cli; \
                     printf '#[test]\\nfn print_hostname() {{ println!(\"hostname: probe\"); }}\\n' >> crates/cli/src/lib.rs; \
                     {temp}/cross test --manifest-path crates/cli/Cargo.toml --target x86_64-unknown-linux-musl --lib print_hostname -- --nocapture 2>&1",
                    temp = temp.display(),
                    ws = ws.display()
                );
                let mut args = vec!["exec".to_owned(), "-e".to_owned(), "HOME=/home/runner".to_owned()];
                if let Some(u) = &job.user {
                    args.extend(["--user".to_owned(), u.clone()]);
                }
                args.extend([job.id.clone(), "bash".to_owned(), "-c".to_owned(), script]);
                c.cli.run(&args, &std::collections::BTreeMap::new(), std::time::Duration::from_secs(1800)).await
            }
            .await;
            c.stop().await;
            ran
        })?;
        assert!(out.contains("hostname: probe") && out.contains("test result: ok"), "{out}");
        Ok(())
    }

    #[test]
    fn w13_r04_the_leg_containers_root_is_found() {
        let id = "828818ba542c44ed2d07bdeae41c160225b70293b69f9abea33c887ed2d0b24e";
        let mountinfo =
            format!("36 25 0:32 / / rw,relatime - btrfs /dev/nvme0n1p2 rw\n812 36 0:91 / /var/lib/docker/rootfs/overlayfs/{id} rw,relatime - overlay overlay rw,lowerdir=a,upperdir=b,workdir=c\n");
        let expected = PathBuf::from(format!("/var/lib/docker/rootfs/overlayfs/{id}"));
        assert_eq!(snapshotter_root("/var/lib/docker\toverlayfs", id, &mountinfo), Some(expected.clone()));
        // Mounts this machine can't see: as the engine says.
        assert_eq!(snapshotter_root("/var/lib/docker\toverlayfs", id, ""), Some(expected));
        // Elsewhere: the mount named for it.
        let elsewhere = format!("812 36 0:91 / /run/containerd/io.containerd.runtime.v2.task/moby/{id}/rootfs rw - overlay overlay rw\n");
        assert_eq!(snapshotter_root("/var/lib/docker\toverlayfs", id, &elsewhere), Some(PathBuf::from(format!("/run/containerd/io.containerd.runtime.v2.task/moby/{id}/rootfs"))));
        assert_eq!(snapshotter_root("/var/lib/docker\toverlayfs", "", &mountinfo), None);
    }
}
