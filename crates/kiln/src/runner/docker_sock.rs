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

use std::collections::{HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::Value;

/// A request head larger than this goes on as it came, and the connection's rest with it.
const MAX_HEAD: usize = 64 << 10;
/// A container create's body larger than this goes on untranslated.
const MAX_CREATE: usize = 16 << 20;
/// An answer's body larger than this goes on as it came.
const MAX_ANSWER: usize = 16 << 20;
/// The kernel's and the daemon's own: a hosted runner's are its machine's, as the host's are here.
const HOST_OWN: &[&str] = &["/proc", "/sys", "/dev", "/lib/modules", "/var/lib/docker"];

/// Where a leg container's paths are on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paths {
    /// Its root, as the host has it mounted: `None` when it couldn't be found.
    root: Option<PathBuf>,
    /// Its mounts with a source on the host, `(inside, host)`, the deepest first.
    mounts: Vec<(PathBuf, PathBuf)>,
    /// kiln's server's address, when it isn't on this host ([`super::docker::server_address`]).
    server: Option<String>,
}

impl Paths {
    pub fn new(root: Option<PathBuf>, mounts: Vec<(PathBuf, PathBuf)>) -> Paths {
        // `/var/run` is `/run` in the images kiln runs (FHS 3.0): the leg's socket is at both.
        let mut mounts: Vec<(PathBuf, PathBuf)> = mounts.into_iter().map(|(inside, host)| (normalize(&inside.to_string_lossy()), host)).collect();
        let aliases: Vec<(PathBuf, PathBuf)> =
            mounts.iter().filter_map(|(inside, host)| inside.strip_prefix("/var/run").ok().map(|rest| (normalize(&Path::new("/run").join(rest).to_string_lossy()), host.clone()))).collect();
        mounts.extend(aliases);
        mounts.sort_by_key(|(inside, _)| std::cmp::Reverse(inside.components().count()));
        Paths { root, mounts, server: None }
    }

    /// These paths, with kiln's server at `address` when it isn't on this host: where the leg's
    /// containers find it ([`super::docker::container_hosts`]).
    pub fn with_server(mut self, address: Option<String>) -> Paths {
        self.server = address;
        self
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
/// Where a static kiln goes in a BuildKit container the leg makes.
const RELAY_PATH: &str = "/.kiln-relay";
/// Where kiln's config for it goes (outside `/etc/buildkit`, which buildx copies a workflow's own
/// config into).
const BUILDKITD_PATH: &str = "/.kiln-buildkitd.toml";

/// What a BuildKit container a leg makes needs to reach kiln by the GHES stand-in's name
/// (`kiln.localhost`, docs/ci.md W08-R08): BuildKit fetches a build's git context with git, whose
/// curl takes every `*.localhost` name for its own loopback, whatever `/etc/hosts` says. So a
/// static kiln in it (the egress relay's, [`super::egress::relay_main`]) takes the connections to
/// `127.0.0.1:<port>` there, and the runner dials in to it through a port published on the
/// host's loopback (nothing in the container connects to the host, whose firewall need not let
/// containers in) and splices each on to the server.
#[derive(Debug, Clone)]
pub struct Loopback {
    /// A static kiln, mounted into the container.
    pub binary: PathBuf,
    /// The port the stand-in names (the server's).
    pub port: u16,
    /// Where the runner reaches the server.
    pub server: (String, u16),
    /// The CLI the runner sets the relay up with.
    pub cli: super::docker::Cli,
    /// BuildKit's config naming kiln's pull-through caches ([`buildkitd_config`]), where the runner
    /// wrote it: `None` when kiln mirrors no registry.
    pub buildkitd: Option<PathBuf>,
}

impl Loopback {
    /// A leg's, the server at `server_url` as the runner reaches it (the stand-in keeps its port,
    /// [`super::runtime::not_ghes`]): `None` for a URL without a host.
    pub fn new(binary: PathBuf, server_url: &str, cli: super::docker::Cli) -> Option<Loopback> {
        let (scheme, rest) = server_url.split_once("://")?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (host, port) = kiln_ci::trust::split_host(authority.rsplit_once('@').map_or(authority, |(_, h)| h));
        let port = port.unwrap_or(if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 });
        (!host.is_empty()).then_some(Loopback { binary, port, server: (host, port), cli, buildkitd: None })
    }
}

/// BuildKit's config for the registries kiln mirrors (`(upstream, prefix)`, docs/ci.md W12-R02):
/// each upstream's mirror is kiln's, under its prefix (`/v2/<prefix>/…`), by the stand-in's name at
/// `port`, which a BuildKit container's loopback relay takes to kiln, over plain HTTP as the
/// stand-in is. BuildKit goes to the upstream itself when the mirror can't answer.
pub fn buildkitd_config(mirrors: &[(String, String)], port: u16) -> anyhow::Result<String> {
    let host = format!("{}:{port}", super::runtime::NOT_GHES_HOST);
    let mut registry = toml::Table::new();
    for (upstream, prefix) in mirrors {
        let mut one = toml::Table::new();
        one.insert("mirrors".into(), toml::Value::Array(vec![toml::Value::String(format!("{host}/{}", prefix.trim_matches('/')))]));
        registry.insert(upstream.clone(), toml::Value::Table(one));
    }
    let mut plain = toml::Table::new();
    plain.insert("http".into(), toml::Value::Boolean(true));
    registry.insert(host, toml::Value::Table(plain));
    let mut top = toml::Table::new();
    top.insert("registry".into(), toml::Value::Table(registry));
    Ok(format!("# kiln's pull-through caches (docs/ci.md W12-R02), through the loopback relay (W08-R08).\n{}", toml::to_string(&top)?))
}

/// Whether `v`, the create of a container `name`d so, is a BuildKit daemon's (a `docker-container`
/// builder's, as `setup-buildx-action` makes them: `buildx_buildkit_…`, of BuildKit's image or the
/// one it was given) on a network of its own, which [`Loopback`] is for.
fn wants_relay(v: &Value, name: &str) -> bool {
    let image = v.get("Image").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    let mode = v.pointer("/HostConfig/NetworkMode").and_then(Value::as_str).unwrap_or_default();
    (image.contains("buildkit") || name.starts_with("buildx_buildkit_")) && mode != "host" && mode != "none" && !mode.starts_with("container:")
}

/// A BuildKit container's create made ready for its relay: the static kiln mounted at
/// [`RELAY_PATH`], the relay's runner port published on the host's loopback, and the stand-in's
/// name its own loopback for what reads `/etc/hosts` too. With kiln's config for BuildKit
/// (`buildkitd`), BuildKit reads it, unless the workflow gave its own (a `--config` of buildx's).
fn with_relay(v: &mut Value, binary: &Path, buildkitd: Option<&Path>) -> bool {
    let port = format!("{}/tcp", super::egress::RUNNER_PORT);
    let configured = v.get("Cmd").and_then(Value::as_array).is_some_and(|args| args.iter().filter_map(Value::as_str).any(|a| a == "--config" || a.starts_with("--config=")));
    let entrypoint = v.get("Entrypoint").is_some_and(|e| e.as_array().is_some_and(|a| !a.is_empty()) || e.as_str().is_some_and(|e| !e.is_empty()));
    let buildkitd = buildkitd.filter(|_| !configured && !entrypoint);
    let Some(o) = v.as_object_mut() else { return false };
    if buildkitd.is_some() {
        let cmd = o.entry("Cmd").or_insert(Value::Null);
        if !cmd.is_array() {
            *cmd = serde_json::json!([]);
        }
        if let Some(args) = cmd.as_array_mut() {
            args.extend([Value::String("--config".into()), Value::String(BUILDKITD_PATH.into())]);
        }
    }
    let exposed = o.entry("ExposedPorts").or_insert(Value::Null);
    if !exposed.is_object() {
        *exposed = serde_json::json!({});
    }
    if let Some(e) = exposed.as_object_mut() {
        e.insert(port.clone(), serde_json::json!({}));
    }
    let Some(hc) = o.entry("HostConfig").or_insert(serde_json::json!({})).as_object_mut() else { return false };
    let binds = hc.entry("Binds").or_insert(Value::Null);
    if !binds.is_array() {
        *binds = serde_json::json!([]);
    }
    if let Some(b) = binds.as_array_mut() {
        b.push(Value::String(format!("{}:{RELAY_PATH}:ro", binary.display())));
        if let Some(config) = buildkitd {
            b.push(Value::String(format!("{}:{BUILDKITD_PATH}:ro", config.display())));
        }
    }
    let published = hc.entry("PortBindings").or_insert(Value::Null);
    if !published.is_object() {
        *published = serde_json::json!({});
    }
    if let Some(m) = published.as_object_mut() {
        m.insert(port, serde_json::json!([{ "HostIp": "127.0.0.1", "HostPort": "" }]));
    }
    let hosts = hc.entry("ExtraHosts").or_insert(Value::Null);
    if !hosts.is_array() {
        *hosts = serde_json::json!([]);
    }
    if let Some(list) = hosts.as_array_mut() {
        let stand_in = super::runtime::NOT_GHES_HOST;
        list.retain(|h| h.as_str().is_none_or(|h| h.split([':', '=']).next() != Some(stand_in)));
        list.push(Value::String(format!("{stand_in}:127.0.0.1")));
    }
    true
}

/// BuildKit container `name`'s loopback relay ([`Loopback`]), once it runs (its maker starts it
/// right after making it): the static kiln run in it, and the runner's connections waiting at its
/// published port, each spliced on to the server when it hands one over, until the leg ends.
async fn loopback_relay(name: String, l: Loopback, stop: crate::rt::Shutdown) {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    let (none, call) = (BTreeMap::new(), Duration::from_secs(60));
    let s = |x: &str| x.to_owned();
    let until = Instant::now() + Duration::from_secs(600);
    loop {
        if l.cli.run(&[s("inspect"), s("--format"), s("{{.State.Running}}"), name.clone()], &none, call).await.is_ok_and(|o| o == "true") {
            break;
        }
        if Instant::now() > until || !stop.sleep(Duration::from_millis(500)).await {
            return;
        }
    }
    let token = crate::util::random_token();
    let env: BTreeMap<String, String> = [(s("KILN_EGRESS_TOKEN"), token.clone()), (s("KILN_RELAY_LISTEN"), format!("127.0.0.1:{}", l.port))].into();
    let exec = [s("exec"), s("-d"), s("-e"), s("KILN_EGRESS_TOKEN"), s("-e"), s("KILN_RELAY_LISTEN"), name.clone(), s(RELAY_PATH), s("egress-relay")];
    if let Err(e) = l.cli.run(&exec, &env, call).await {
        tracing::warn!(container = %name, error = %format!("{e:#}"), "a BuildKit container's loopback relay didn't start");
        return;
    }
    let ports = super::docker::parse_ports(&l.cli.run(&[s("port"), name.clone()], &none, call).await.unwrap_or_default());
    let Some(port) = ports.get(&super::egress::RUNNER_PORT.to_string()).and_then(|p| p.parse::<u16>().ok()) else {
        tracing::warn!(container = %name, "a BuildKit container's loopback relay has no published port");
        return;
    };
    tracing::debug!(container = %name, port, "a BuildKit container's loopback relay");
    let pool = super::egress::Pool::direct(port, token, l.server.clone());
    stop.cancelled().await;
    drop(pool);
}

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

/// What a leg's socket makes carries `kiln.socket.leg.<tag>` (the leg's tag: its names' suffix),
/// the leg's id its value: kiln removes what has it when the leg ends
/// ([`super::docker::Containers::stop`]), and the leg's lists leave out what another leg's socket
/// made. A key of each leg's own: a leg's socket within another's (kiln's own tests, in a kiln
/// leg) puts its own beside the other's, and both find what it made.
pub const SOCKET_LEG: &str = "kiln.socket.leg";
/// And `kiln.socket.runner.<hash>`, the runner's ([`super::docker::owner_label`]'s value): what a
/// runner that stopped short left is swept when it starts ([`super::docker::sweep`]).
pub const SOCKET_RUNNER: &str = "kiln.socket.runner";

/// A leg's tag: `k` and 8 hex digits of its id.
fn tag_of(leg: &str) -> String {
    let digest = crate::util::sha256_hex(leg.as_bytes());
    format!("k{}", digest.get(..8).unwrap_or(&digest))
}

/// The label leg `leg`'s socket puts on what it makes, by its key (`label=<key>` finds them).
pub fn leg_label(leg: &str) -> String {
    format!("{SOCKET_LEG}.{}", tag_of(leg))
}

/// The label the sockets of the legs of runner `owner` (its [`super::docker::owner_label`]) put
/// on what they make, by its key.
pub fn runner_label(owner: &str) -> Option<String> {
    owner.split_once('=').map(|(_, v)| format!("{SOCKET_RUNNER}.{v}"))
}
/// Docker's and Podman's own network modes, which take no aliases.
const OWN_NETWORKS: &[&str] = &["bridge", "host", "none", "default", "podman", "nat", "slirp4netns", "pasta", "private"];

/// The names a leg gave what it made through its socket (containers, networks, volumes). On a
/// daemon the runner's other legs share, each is the leg's own by a suffix of its own, and the
/// leg's later requests by that name are taken to it: two legs at once can both make a
/// `cargo_test_0`, as two jobs on hosted runners can. What it makes carries its labels, the
/// daemon's answers say its names as it gave them, and its lists and events leave out what other
/// legs made. Names it didn't make (kiln's, for its services; what was there before) go on as they
/// came, and so does an id.
#[derive(Debug, Default)]
pub struct Names {
    /// Its label's key ([`leg_label`]); empty for a socket whose names are the daemon's.
    key: String,
    suffix: String,
    labels: Vec<(String, String)>,
    containers: HashSet<String>,
    networks: HashSet<String>,
    volumes: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Container,
    Network,
    Volume,
}

impl Names {
    /// Leg `leg`'s names, on the runner whose label is `owner`.
    pub fn for_leg(leg: &str, owner: Option<&str>) -> Names {
        let key = leg_label(leg);
        let mut labels = vec![(key.clone(), leg.to_owned())];
        labels.extend(owner.and_then(runner_label).map(|r| (r, "1".to_owned())));
        Names { key, suffix: format!("-{}", tag_of(leg)), labels, ..Names::default() }
    }

    fn kind(&mut self, k: Kind) -> &mut HashSet<String> {
        match k {
            Kind::Container => &mut self.containers,
            Kind::Network => &mut self.networks,
            Kind::Volume => &mut self.volumes,
        }
    }

    /// `name`, made now: the leg's own from here on, as the daemon has it.
    fn made(&mut self, k: Kind, name: &str) -> String {
        let bare = name.strip_prefix('/').unwrap_or(name);
        if self.suffix.is_empty() || bare.is_empty() {
            return name.to_owned();
        }
        self.kind(k).insert(bare.to_owned());
        format!("{bare}{}", self.suffix)
    }

    /// `name` as the daemon has it: the leg's own, else as it is (a container's may say `/`).
    fn own(&self, k: Kind, name: &str) -> String {
        let (slash, bare) = name.strip_prefix('/').map_or(("", name), |b| ("/", b));
        let made = match k {
            Kind::Container => &self.containers,
            Kind::Network => &self.networks,
            Kind::Volume => &self.volumes,
        };
        if made.contains(bare) { format!("{slash}{bare}{}", self.suffix) } else { name.to_owned() }
    }

    /// Whether `labels` (an object's, an event's attributes) say another leg's socket made it, and
    /// not through this one.
    fn theirs(&self, labels: Option<&Value>) -> bool {
        let Some(l) = labels.and_then(Value::as_object).filter(|_| !self.key.is_empty()) else { return false };
        let leg = format!("{SOCKET_LEG}.");
        l.keys().any(|k| k.starts_with(&leg)) && !l.contains_key(&self.key)
    }

    /// `v`'s labels with the leg's. Whether it changed.
    fn label(&self, v: &mut Value) -> bool {
        let Some(o) = v.as_object_mut().filter(|_| !self.labels.is_empty()) else { return false };
        let labels = o.entry("Labels").or_insert(Value::Null);
        if !labels.is_object() {
            *labels = Value::Object(serde_json::Map::new());
        }
        let Some(l) = labels.as_object_mut() else { return false };
        for (k, value) in &self.labels {
            l.insert(k.clone(), Value::String(value.clone()));
        }
        true
    }

    /// `body` with the leg's names as it gave them: every suffix of its own that ends a name gone.
    /// `None` when there's none.
    fn strip(&self, body: &[u8]) -> Option<Vec<u8>> {
        let suffix = self.suffix.as_bytes();
        if suffix.is_empty() {
            return None;
        }
        let (mut out, mut rest, mut changed) = (Vec::with_capacity(body.len()), body, false);
        while let Some(i) = rest.windows(suffix.len()).position(|w| w == suffix) {
            let after = rest.get(i + suffix.len()..).unwrap_or_default();
            out.extend_from_slice(rest.get(..i).unwrap_or_default());
            if after.first().is_some_and(|b| b.is_ascii_alphanumeric() || b"_.-".contains(b)) {
                out.extend_from_slice(suffix);
            } else {
                changed = true;
            }
            rest = after;
        }
        out.extend_from_slice(rest);
        changed.then_some(out)
    }

    /// An answer's body as the leg is given it: its names as it gave them, and a list without what
    /// other legs made. `None` when it's as it came.
    fn given(&self, answer: Answer, body: &[u8]) -> Option<Vec<u8>> {
        let listed = (answer == Answer::List).then(|| self.listed(body)).flatten();
        let body = listed.as_deref().unwrap_or(body);
        self.strip(body).or(listed)
    }

    /// A list (`[...]`, a volumes list's `{"Volumes": [...]}`) without what other legs made.
    fn listed(&self, body: &[u8]) -> Option<Vec<u8>> {
        let mut v: Value = serde_json::from_slice(body).ok()?;
        let list = match &mut v {
            Value::Array(a) => a,
            Value::Object(o) => o.get_mut("Volumes")?.as_array_mut()?,
            _ => return None,
        };
        let before = list.len();
        list.retain(|e| !self.theirs(e.get("Labels")));
        if list.len() == before { None } else { serde_json::to_vec(&v).ok() }
    }

    /// One event of an event stream as the leg is given it: `None` for what another leg made.
    fn event(&self, line: &[u8]) -> Option<Vec<u8>> {
        let theirs = serde_json::from_slice::<Value>(line).ok().is_some_and(|e| self.theirs(e.pointer("/Actor/Attributes")));
        (!theirs).then(|| self.strip(line).unwrap_or_else(|| line.to_vec()))
    }
}

/// What a request's body says that names what the leg made, to be taken to the daemon's names.
#[derive(Debug, Clone, PartialEq)]
enum Gather {
    /// A container's create: its bind mounts (the host's paths), what it names, its labels; and
    /// the name it was given, found by on its networks.
    ContainerCreate(Option<String>),
    NetworkCreate,
    VolumeCreate,
    /// A network's connect (`true`) or disconnect: its container.
    NetworkMember(bool),
}

/// What's done with a request's answer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Answer {
    /// On as it comes.
    Plain,
    /// No body, whatever its head says: a `HEAD`'s.
    Bodiless,
    /// An object (an inspect's): the leg's names as it gave them.
    One,
    /// A list: the leg's names, and nothing other legs made.
    List,
    /// An event stream: likewise, event by event.
    Events,
    /// The connection is taken over once it's through: an attach's or an exec's streams.
    Takeover,
}

/// Where a request goes: its target with the leg's names the daemon's (`None`: as it came), what
/// of its body to gather and take likewise, and what's done with its answer.
#[derive(Debug, PartialEq)]
struct Route {
    target: Option<String>,
    gather: Option<Gather>,
    answer: Answer,
}

/// A path's API version and the rest: `/v1.51/containers/create` is `("/v1.51", "/containers/create")`.
fn versioned(path: &str) -> (&str, &str) {
    match path.strip_prefix("/v") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.find('/').and_then(|i| path.split_at_checked(i + 2)).unwrap_or(("", path)),
        _ => ("", path),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |j: usize| bytes.get(j).and_then(|c| char::from(*c).to_digit(16));
        match (b, hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
                i += 3;
            }
            // A form's space, as Go's `ParseQuery` reads it.
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// `query` with its `key`'s value as `f` makes it (given `None` when it has none; giving `None`
/// leaves it). `None` when nothing changed.
fn requery(query: Option<&str>, key: &str, mut f: impl FnMut(Option<&str>) -> Option<String>) -> Option<String> {
    let (mut changed, mut seen) = (false, false);
    let mut pairs: Vec<String> = query
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if k == key => {
                seen = true;
                match f(Some(&percent_decode(v))) {
                    Some(now) => {
                        changed = true;
                        format!("{k}={}", percent_encode(&now))
                    }
                    None => kv.to_owned(),
                }
            }
            _ => kv.to_owned(),
        })
        .collect();
    if !seen && let Some(now) = f(None) {
        changed = true;
        pairs.push(format!("{key}={}", percent_encode(&now)));
    }
    changed.then(|| pairs.join("&"))
}

/// A list's or a prune's `filters` with `label` among them: Docker's filters of one kind are all
/// to be met, so what's listed has that label as well as what was asked.
fn with_label(filters: Option<&str>, label: &str) -> String {
    let mut v = filters.and_then(|f| serde_json::from_str::<Value>(f).ok()).filter(Value::is_object).unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if let Some(o) = v.as_object_mut() {
        match o.entry("label").or_insert(Value::Array(Vec::new())) {
            // An older client's `{"label": {"a=b": true}}`.
            Value::Object(m) => {
                m.insert(label.to_owned(), Value::Bool(true));
            }
            Value::Array(a) => a.push(Value::String(label.to_owned())),
            other => *other = Value::Array(vec![Value::String(label.to_owned())]),
        }
    }
    v.to_string()
}

/// A list's or an event stream's `filters` with the leg's names the daemon's, each of `keys` as
/// its kind is named: exactly, or (`true`) a pattern anchored at its end (`^/db$`, as a script
/// checks a name), which the suffix would otherwise leave matching nothing. `None` when none
/// changed.
fn named_filters(filters: &str, names: &Names, keys: &[(&str, Kind, bool)]) -> Option<String> {
    let mut v: Value = serde_json::from_str(filters).ok()?;
    let o = v.as_object_mut()?;
    let mut changed = false;
    for (key, k, anchored) in keys {
        let named = |s: &str| -> Option<String> {
            if *anchored {
                let core = s.strip_suffix('$')?;
                let bare = core.trim_start_matches('^').trim_start_matches('/');
                (names.own(*k, bare) != bare).then(|| format!("{core}{}$", names.suffix))
            } else {
                Some(names.own(*k, s)).filter(|now| now != s)
            }
        };
        match o.get_mut(*key) {
            Some(Value::Array(a)) => {
                for x in a.iter_mut() {
                    if let Some(now) = x.as_str().and_then(named) {
                        *x = Value::String(now);
                        changed = true;
                    }
                }
            }
            // An older client's `{"name": {"db": true}}`.
            Some(Value::Object(m)) => {
                let old: Vec<String> = m.keys().filter(|x| named(x).is_some()).cloned().collect();
                for x in old {
                    if let (Some(now), Some(value)) = (named(&x), m.remove(&x)) {
                        m.insert(now, value);
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed.then(|| v.to_string())
}

/// The route of a request to `target` with `method`, the leg's names taken to the daemon's.
fn route(method: &str, target: &str, names: &mut Names) -> Route {
    let (path, query) = target.split_once('?').map_or((target, None), |(p, q)| (p, Some(q)));
    let (version, unversioned) = versioned(path);
    let parts: Vec<&str> = unversioned.split('/').collect();
    let leg = !names.suffix.is_empty();
    let (get, post) = (method == "GET", method == "POST");
    let plain = if method == "HEAD" { Answer::Bodiless } else { Answer::Plain };
    let to = |segments: &[&str], q: Option<&str>| format!("{version}{}{}", segments.join("/"), q.map(|q| format!("?{q}")).unwrap_or_default());
    let made = |names: &mut Names, k: Kind, v: Option<&str>| v.filter(|v| !v.is_empty()).map(|v| names.made(k, v)).filter(|now| Some(now.as_str()) != v);
    // A list or an event stream, its filters' names the daemon's.
    let filtered = |names: &Names, keys: &[(&str, Kind, bool)], answer: Answer| Route {
        target: requery(query, "filters", |v| v.and_then(|f| named_filters(f, names, keys))).map(|q| to(&parts, Some(&q))),
        gather: None,
        answer,
    };
    match parts.as_slice() {
        ["", "containers", "create"] if post => {
            let mut name = None;
            let q = requery(query, "name", |v| {
                name = v.filter(|v| !v.is_empty()).map(|v| v.trim_start_matches('/').to_owned());
                made(names, Kind::Container, v)
            });
            Route { target: q.map(|q| to(&parts, Some(&q))), gather: Some(Gather::ContainerCreate(name)), answer: plain }
        }
        ["", "networks", "create"] if post => Route { target: None, gather: Some(Gather::NetworkCreate), answer: plain },
        ["", "volumes", "create"] if post => Route { target: None, gather: Some(Gather::VolumeCreate), answer: if leg { Answer::One } else { plain } },
        // A leg prunes what it made, as on a machine of its own.
        ["", "containers" | "networks" | "volumes", "prune"] if post && leg => {
            let label = names.key.clone();
            let q = requery(query, "filters", |v| Some(with_label(v, &label)));
            Route { target: q.map(|q| to(&parts, Some(&q))), gather: None, answer: Answer::One }
        }
        ["", "containers", "json"] if get && leg => {
            let keys = [("name", Kind::Container, true), ("before", Kind::Container, false), ("since", Kind::Container, false), ("network", Kind::Network, false), ("volume", Kind::Volume, false)];
            filtered(names, &keys, Answer::List)
        }
        ["", "networks"] | ["", "networks", ""] if get && leg => filtered(names, &[("name", Kind::Network, true)], Answer::List),
        ["", "volumes"] | ["", "volumes", ""] if get && leg => filtered(names, &[("name", Kind::Volume, true)], Answer::List),
        ["", "events"] if get && leg => filtered(names, &[("container", Kind::Container, false), ("network", Kind::Network, false), ("volume", Kind::Volume, false)], Answer::Events),
        ["", "commit"] if post => {
            let q = requery(query, "container", |v| v.map(|c| names.own(Kind::Container, c)).filter(|now| Some(now.as_str()) != v));
            Route { target: q.map(|q| to(&parts, Some(&q))), gather: None, answer: plain }
        }
        ["", kind @ ("containers" | "networks" | "volumes"), name, rest @ ..] if !matches!(*name, "json" | "prune" | "create" | "") => {
            let k = match *kind {
                "containers" => Kind::Container,
                "networks" => Kind::Network,
                _ => Kind::Volume,
            };
            let theirs = names.own(k, name);
            // `docker rename`: the new name is the leg's own too.
            let q = if k == Kind::Container && post && rest == ["rename"] { requery(query, "name", |v| made(names, Kind::Container, v)) } else { None };
            let gather = match rest {
                ["connect"] if k == Kind::Network && post => Some(Gather::NetworkMember(true)),
                ["disconnect"] if k == Kind::Network && post => Some(Gather::NetworkMember(false)),
                _ => None,
            };
            let answer = match (k, rest) {
                (Kind::Container, ["json"]) | (Kind::Network | Kind::Volume, []) if get && leg => Answer::One,
                _ => plain,
            };
            let target = (theirs != *name || q.is_some()).then(|| {
                let mut segments = parts.clone();
                if let Some(s) = segments.get_mut(2) {
                    *s = theirs.as_str();
                }
                to(&segments, q.as_deref().or(query))
            });
            Route { target, gather, answer }
        }
        _ => Route { target: None, gather: None, answer: plain },
    }
}

/// A body `gather` names, with the leg's names the daemon's (and a create's bind mounts the
/// host's): `None` when there's nothing to change (or it isn't JSON), and it goes on as it came.
/// A BuildKit container's create given its loopback relay ([`Loopback`]) names it, as the daemon
/// calls it, in `relayed`.
fn rewrite(gather: &Gather, body: &[u8], paths: &Paths, names: &mut Names, loopback: Option<&Loopback>, relayed: &mut Option<String>) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let changed = match gather {
        Gather::ContainerCreate(name) => {
            let mounts = translate_create(&mut v, paths);
            let named = name_references(&mut v, names, name.as_deref());
            // After the mounts' translation: the static kiln's path is the host's.
            let relay = match (loopback, name) {
                (Some(l), Some(n)) if wants_relay(&v, n) && with_relay(&mut v, &l.binary, l.buildkitd.as_deref()) => {
                    *relayed = Some(names.own(Kind::Container, n));
                    true
                }
                _ => false,
            };
            mounts | named | relay | names.label(&mut v)
        }
        Gather::NetworkCreate | Gather::VolumeCreate => {
            let kind = if *gather == Gather::NetworkCreate { Kind::Network } else { Kind::Volume };
            let renamed = match v.get("Name").and_then(Value::as_str).map(str::to_owned) {
                Some(n) if !n.is_empty() => {
                    let now = names.made(kind, &n);
                    now != n && v.as_object_mut().is_some_and(|o| o.insert("Name".to_owned(), Value::String(now)).is_some())
                }
                _ => false,
            };
            renamed | names.label(&mut v)
        }
        Gather::NetworkMember(connect) => {
            let c = v.get("Container").and_then(Value::as_str)?.to_owned();
            let now = names.own(Kind::Container, &c);
            match v.as_object_mut().filter(|_| now != c) {
                Some(o) => {
                    o.insert("Container".to_owned(), Value::String(now));
                    // Found on the network by the name the leg gave it.
                    if *connect {
                        alias(o.entry("EndpointConfig").or_insert(Value::Null), c.trim_start_matches('/'));
                    }
                    true
                }
                None => false,
            }
        }
    };
    if changed { serde_json::to_vec(&v).ok() } else { None }
}

/// `endpoint` (a network's endpoint settings) with `name` among its aliases. Whether it changed.
fn alias(endpoint: &mut Value, name: &str) -> bool {
    if !endpoint.is_object() {
        *endpoint = Value::Object(serde_json::Map::new());
    }
    let Some(aliases) = endpoint.as_object_mut().map(|o| o.entry("Aliases").or_insert(Value::Null)) else { return false };
    if !aliases.is_array() {
        *aliases = Value::Array(Vec::new());
    }
    match aliases.as_array_mut() {
        Some(list) if !list.iter().any(|a| a.as_str() == Some(name)) => {
            list.push(Value::String(name.to_owned()));
            true
        }
        _ => false,
    }
}

/// Whether network mode `m` is a network of the leg's or kiln's making, which takes aliases.
fn user_network(m: &str) -> bool {
    !m.is_empty() && !m.contains(':') && !OWN_NETWORKS.contains(&m)
}

/// A container create's names of what the leg made: its network (`NetworkMode`; another
/// container's, with `PidMode` and `IpcMode`, with `container:`), named volumes (`Binds`,
/// `Mounts`: one it names is its own, made by its first use as on a machine of its own), links
/// and `VolumesFrom`, and the networks it joins, where it's found by `name`, the name it was
/// given. Whether it changed.
fn name_references(v: &mut Value, names: &mut Names, name: Option<&str>) -> bool {
    let mut changed = false;
    let mut set = |slot: &mut Value, now: String| {
        if slot.as_str() != Some(now.as_str()) {
            *slot = Value::String(now);
            changed = true;
        }
    };
    let mut network = None;
    if let Some(config) = v.get_mut("HostConfig") {
        for mode in ["NetworkMode", "PidMode", "IpcMode"] {
            let Some(slot) = config.get_mut(mode) else { continue };
            let Some(m) = slot.as_str().map(str::to_owned) else { continue };
            let now = match m.strip_prefix("container:") {
                Some(c) => format!("container:{}", names.own(Kind::Container, c)),
                None if mode == "NetworkMode" => names.own(Kind::Network, &m),
                None => continue,
            };
            if mode == "NetworkMode" && user_network(&m) {
                network = Some(now.clone());
            }
            set(slot, now);
        }
        for bind in config.get_mut("Binds").and_then(Value::as_array_mut).into_iter().flatten() {
            let Some((src, rest)) = bind.as_str().and_then(|b| b.split_once(':')).map(|(s, r)| (s.to_owned(), r.to_owned())) else { continue };
            if !src.is_empty() && !src.contains('/') {
                set(bind, format!("{}:{rest}", names.made(Kind::Volume, &src)));
            }
        }
        for mount in config.get_mut("Mounts").and_then(Value::as_array_mut).into_iter().flatten() {
            if mount.get("Type").and_then(Value::as_str) == Some("volume")
                && let Some(src) = mount.get_mut("Source")
                && let Some(now) = src.as_str().filter(|s| !s.is_empty()).map(|s| names.made(Kind::Volume, s))
            {
                set(src, now);
            }
        }
        // `--link name[:alias]`: the alias stays the name the leg knows.
        for link in config.get_mut("Links").and_then(Value::as_array_mut).into_iter().flatten() {
            let Some(l) = link.as_str().map(str::to_owned) else { continue };
            let (target, alias) = l.split_once(':').unwrap_or((l.as_str(), l.trim_start_matches('/')));
            let now = names.own(Kind::Container, target);
            if now != target {
                set(link, format!("{now}:{alias}"));
            }
        }
        for from in config.get_mut("VolumesFrom").and_then(Value::as_array_mut).into_iter().flatten() {
            let Some(f) = from.as_str().map(str::to_owned) else { continue };
            let (target, mode) = f.split_once(':').map_or((f.as_str(), None), |(n, m)| (n, Some(m)));
            let now = names.own(Kind::Container, target);
            if now != target {
                set(from, mode.map_or(now.clone(), |m| format!("{now}:{m}")));
            }
        }
    }
    let endpoints = v.pointer_mut("/NetworkingConfig/EndpointsConfig").and_then(Value::as_object_mut);
    let renamed = name.filter(|n| names.own(Kind::Container, n) != *n);
    match (endpoints, renamed) {
        (Some(endpoints), _) => {
            let keys: Vec<String> = endpoints.keys().filter(|k| names.own(Kind::Network, k) != **k).cloned().collect();
            for k in keys {
                if let Some(e) = endpoints.remove(&k) {
                    endpoints.insert(names.own(Kind::Network, &k), e);
                    changed = true;
                }
            }
            if let Some(n) = renamed {
                // Its network's endpoint, the CLI's when it said none.
                if let Some(net) = network.filter(|net| !endpoints.contains_key(net)) {
                    endpoints.insert(net, Value::Null);
                }
                for (_, e) in endpoints.iter_mut().filter(|(k, _)| user_network(k)) {
                    changed |= alias(e, n);
                }
            }
        }
        (None, Some(n)) => {
            if let Some(net) = network
                && let Some(o) = v.as_object_mut()
            {
                let mut e = Value::Null;
                alias(&mut e, n);
                let endpoints = serde_json::Map::from_iter([(net, e)]);
                o.insert("NetworkingConfig".to_owned(), Value::Object(serde_json::Map::from_iter([("EndpointsConfig".to_owned(), Value::Object(endpoints))])));
                changed = true;
            }
        }
        (None, None) => {}
    }
    changed
}

/// A container create's bind mounts, their sources on the host. Whether it changed.
fn translate_create(v: &mut Value, paths: &Paths) -> bool {
    let Some(config) = v.get_mut("HostConfig") else { return false };
    let mut changed = false;
    // The names the leg's own containers have for the host and kiln's server, as its job
    // container has them (BuildKit's fetches a build's git context from kiln through them), but
    // a name the container gives itself. One sharing another's network has that one's hosts.
    let mode = config.get("NetworkMode").and_then(Value::as_str).unwrap_or_default().to_owned();
    if !mode.starts_with("container:")
        && mode != "none"
        && let Some(o) = config.as_object_mut()
    {
        let extra = o.entry("ExtraHosts").or_insert(Value::Null);
        if extra.is_null() {
            *extra = Value::Array(Vec::new());
        }
        if let Some(list) = extra.as_array_mut() {
            for h in super::docker::container_hosts(mode == "host", paths.server.as_deref()) {
                let name = h.split(':').next().unwrap_or_default().to_owned();
                if !list.iter().any(|e| e.as_str().is_some_and(|x| x.split([':', '=']).next() == Some(name.as_str()))) {
                    list.push(Value::String(h));
                    changed = true;
                }
            }
        }
    }
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
    changed
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

/// A message head's fields as they came, each with whether it frames the body (said anew with a
/// new length).
type Fields = Vec<(String, Vec<u8>, bool)>;

/// How a message's head frames its body.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Framing {
    /// Not at all: a request has none, an answer goes to the connection's end.
    None,
    Length(u64),
    Chunked,
}

/// `headers` as [`Fields`], and the body's framing: `None` for a length that isn't one.
fn fields_of(headers: &[httparse::Header]) -> Option<(Fields, Framing)> {
    let (mut fields, mut framing) = (Vec::new(), Framing::None);
    for f in headers {
        let value = String::from_utf8_lossy(f.value).to_ascii_lowercase();
        let frames = if f.name.eq_ignore_ascii_case("transfer-encoding") {
            if value.split(',').any(|c| c.trim() == "chunked") {
                framing = Framing::Chunked;
            }
            true
        } else if f.name.eq_ignore_ascii_case("content-length") {
            if framing == Framing::None {
                framing = Framing::Length(value.trim().parse::<u64>().ok()?);
            }
            true
        } else {
            false
        };
        fields.push((f.name.to_owned(), f.value.to_vec(), frames));
    }
    Some((fields, framing))
}

/// Writes a head: `start`, then `fields`, the framing's as they came or a length of `len`.
fn write_head(start: &str, fields: &Fields, len: Option<usize>, out: &mut Vec<u8>) {
    out.extend_from_slice(start.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (name, value, frames) in fields {
        if *frames && len.is_some() {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    if let Some(n) = len {
        out.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
}

/// What a request head says.
#[derive(Debug)]
struct Head {
    /// The head as it came.
    raw: Vec<u8>,
    method: String,
    target: String,
    version: u8,
    fields: Fields,
    /// The connection is taken over once the request is through: an attach's or an exec's
    /// streams, BuildKit's session.
    takeover: bool,
    body: Option<Body>,
}

impl Head {
    /// The head on to the daemon: to `target` when it's another, its body as `len` bytes when it's
    /// said anew.
    fn write(&self, target: Option<&str>, len: Option<usize>, out: &mut Vec<u8>) {
        if target.is_none() && len.is_none() {
            out.extend_from_slice(&self.raw);
            return;
        }
        let start = format!("{} {} HTTP/1.{}", self.method, target.unwrap_or(&self.target), self.version);
        write_head(&start, &self.fields, len, out);
    }
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
    let Some((fields, framing)) = fields_of(req.headers) else { return Some(None) };
    let body = match framing {
        Framing::Chunked => Some(Body::Chunked(Chunked::default())),
        Framing::Length(len) if len > 0 => Some(Body::Length(len)),
        Framing::Length(_) | Framing::None => None,
    };
    let upgrade = fields
        .iter()
        .any(|(name, value, _)| name.eq_ignore_ascii_case("upgrade") || (name.eq_ignore_ascii_case("connection") && String::from_utf8_lossy(value).to_ascii_lowercase().contains("upgrade")));
    // An attach or an exec's start is taken over without saying so by older clients.
    let parts: Vec<&str> = versioned(target.split('?').next().unwrap_or_default()).1.split('/').collect();
    let takeover = upgrade || matches!(parts.as_slice(), ["", "containers", _, "attach"] | ["", "exec", _, "start"] | ["", "session"] | ["", "grpc"]);
    Some(Some(Head { raw: buf.get(..n)?.to_vec(), method: method.to_owned(), target: target.to_owned(), version, fields, takeover, body }))
}

/// What the leg sends the daemon, read as HTTP/1.1 requests until a connection is taken over: the
/// leg's names taken to the daemon's, containers' creates translated, everything else as it came.
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
    /// Gathering a body (`raw` as it came, `data` its content) to rewrite it whole, the request to
    /// `target` when that's another.
    Gather { head: Box<Head>, target: Option<String>, gather: Gather, body: Body, raw: Vec<u8>, data: Vec<u8> },
    /// Everything as it comes.
    Raw,
}

impl Requests {
    /// Takes what the leg sent, `input`, and puts what goes on to the daemon in `out`, and what's
    /// to be done with each request's answer in `answers`.
    fn feed(&mut self, input: &[u8], leg: &Leg, answers: &mut VecDeque<Answer>, out: &mut Vec<u8>) {
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
                    let mut head = match parse_head(&self.buf) {
                        None if self.buf.len() <= MAX_HEAD => {
                            self.stage = Flow::Head;
                            return;
                        }
                        Some(Some(h)) => h,
                        // Not what was expected: the connection's rest goes on as it came, both ways.
                        None | Some(None) => {
                            answers.push_back(Answer::Takeover);
                            continue;
                        }
                    };
                    self.buf.drain(..head.raw.len());
                    let route = route(&head.method, &head.target, &mut leg.names.lock());
                    answers.push_back(if head.takeover { Answer::Takeover } else { route.answer });
                    let next = if head.takeover { Flow::Raw } else { Flow::Head };
                    self.stage = match (head.body.take(), route.gather) {
                        (Some(body), Some(gather)) => Flow::Gather { head: Box::new(head), target: route.target, gather, body, raw: Vec::new(), data: Vec::new() },
                        (body, _) => {
                            head.write(route.target.as_deref(), None, out);
                            match body {
                                Some(body) => Flow::Pass(body, head.takeover),
                                None => next,
                            }
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
                Flow::Gather { head, target, gather, mut body, mut raw, mut data } => {
                    let Some((n, done)) = body.take(&self.buf, Some(&mut data)) else {
                        // Not chunked as it says: on as it came.
                        head.write(target.as_deref(), None, out);
                        out.append(&mut raw);
                        continue;
                    };
                    raw.extend(self.buf.drain(..n));
                    if !done && raw.len() <= MAX_CREATE {
                        self.stage = Flow::Gather { head, target, gather, body, raw, data };
                        return;
                    }
                    let (loopback, mut relayed) = (leg.loopback.read().clone(), None);
                    let rewritten = if done { rewrite(&gather, &data, &leg.paths.read(), &mut leg.names.lock(), loopback.as_ref(), &mut relayed) } else { None };
                    if let (Some(name), Some(l)) = (relayed, loopback) {
                        crate::rt::spawn(loopback_relay(name, l, leg.stop.clone()));
                    }
                    match rewritten {
                        Some(new) => {
                            head.write(target.as_deref(), Some(new.len()), out);
                            out.extend_from_slice(&new);
                        }
                        None => {
                            head.write(target.as_deref(), None, out);
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

/// What an answer's head says.
#[derive(Debug)]
struct Reply {
    raw: Vec<u8>,
    start: String,
    status: u16,
    fields: Fields,
    framing: Framing,
}

/// As [`parse_head`], for an answer.
fn parse_reply(buf: &[u8]) -> Option<Option<Reply>> {
    let mut fields = [httparse::EMPTY_HEADER; 64];
    let mut res = httparse::Response::new(&mut fields);
    let n = match res.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return None,
        Err(_) => return Some(None),
    };
    let (Some(status), Some(version)) = (res.code, res.version) else { return Some(None) };
    let start = format!("HTTP/1.{version} {status} {}", res.reason.unwrap_or_default());
    let Some((fields, framing)) = fields_of(res.headers) else { return Some(None) };
    Some(Some(Reply { raw: buf.get(..n)?.to_vec(), start, status, fields, framing }))
}

/// What the daemon answers the leg, read as HTTP/1.1 answers in its requests' order until the
/// connection is taken over: what names the leg's own as the leg gave them, the rest as it came.
#[derive(Debug, Default)]
struct Answers {
    buf: Vec<u8>,
    stage: Back,
}

#[derive(Debug, Default)]
enum Back {
    /// Awaiting an answer's head.
    #[default]
    Head,
    Pass(Body),
    /// Gathering a body (as it came, and its content) to give it whole.
    Gather {
        reply: Reply,
        answer: Answer,
        body: Body,
        raw: Vec<u8>,
        data: Vec<u8>,
    },
    /// An event stream, said again in chunks of an event each; `line` the event coming.
    Events {
        body: Chunked,
        line: Vec<u8>,
    },
    Raw,
}

/// `data` as one chunk of a chunked body.
fn chunk(data: &[u8], out: &mut Vec<u8>) {
    if !data.is_empty() {
        out.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
}

impl Answers {
    /// Takes what the daemon answered, `input`, and puts what goes on to the leg in `out`;
    /// `pending` says what's done with each answer.
    fn feed(&mut self, input: &[u8], names: &Mutex<Names>, pending: &mut VecDeque<Answer>, out: &mut Vec<u8>) {
        self.buf.extend_from_slice(input);
        loop {
            match std::mem::replace(&mut self.stage, Back::Raw) {
                Back::Raw => {
                    out.append(&mut self.buf);
                    return;
                }
                Back::Head => {
                    if self.buf.is_empty() {
                        self.stage = Back::Head;
                        return;
                    }
                    let reply = match parse_reply(&self.buf) {
                        None if self.buf.len() <= MAX_HEAD => {
                            self.stage = Back::Head;
                            return;
                        }
                        Some(Some(r)) => r,
                        None | Some(None) => continue,
                    };
                    self.buf.drain(..reply.raw.len());
                    self.stage = Back::Head;
                    // An interim answer (`100 Continue`): the request's own is still to come.
                    if (100..200).contains(&reply.status) && reply.status != 101 {
                        out.extend_from_slice(&reply.raw);
                        continue;
                    }
                    let answer = pending.pop_front().unwrap_or(Answer::Plain);
                    let body = match reply.framing {
                        _ if reply.status == 101 || answer == Answer::Takeover => {
                            out.extend_from_slice(&reply.raw);
                            self.stage = Back::Raw;
                            continue;
                        }
                        _ if answer == Answer::Bodiless || matches!(reply.status, 204 | 304) => None,
                        Framing::Length(0) => None,
                        Framing::Length(n) => Some(Body::Length(n)),
                        Framing::Chunked => Some(Body::Chunked(Chunked::default())),
                        // To the connection's end.
                        Framing::None => {
                            out.extend_from_slice(&reply.raw);
                            self.stage = Back::Raw;
                            continue;
                        }
                    };
                    let Some(body) = body else {
                        out.extend_from_slice(&reply.raw);
                        continue;
                    };
                    let (ok, named) = ((200..300).contains(&reply.status), !names.lock().suffix.is_empty());
                    self.stage = match answer {
                        Answer::One | Answer::List if ok && named => Back::Gather { reply, answer, body, raw: Vec::new(), data: Vec::new() },
                        // A refusal's message names what it's about.
                        _ if reply.status >= 400 && named => Back::Gather { reply, answer: Answer::One, body, raw: Vec::new(), data: Vec::new() },
                        Answer::Events if ok && named && matches!(body, Body::Chunked(_)) => {
                            out.extend_from_slice(&reply.raw);
                            Back::Events { body: Chunked::default(), line: Vec::new() }
                        }
                        _ => {
                            out.extend_from_slice(&reply.raw);
                            Back::Pass(body)
                        }
                    };
                }
                Back::Pass(mut body) => {
                    let Some((n, done)) = body.take(&self.buf, None) else { continue };
                    out.extend(self.buf.drain(..n));
                    self.stage = if done { Back::Head } else { Back::Pass(body) };
                    if !done {
                        return;
                    }
                }
                Back::Gather { reply, answer, mut body, mut raw, mut data } => {
                    let Some((n, done)) = body.take(&self.buf, Some(&mut data)) else {
                        out.extend_from_slice(&reply.raw);
                        out.append(&mut raw);
                        continue;
                    };
                    raw.extend(self.buf.drain(..n));
                    if !done && raw.len() <= MAX_ANSWER {
                        self.stage = Back::Gather { reply, answer, body, raw, data };
                        return;
                    }
                    match done.then(|| names.lock().given(answer, &data)).flatten() {
                        Some(new) => {
                            write_head(&reply.start, &reply.fields, Some(new.len()), out);
                            out.extend_from_slice(&new);
                        }
                        None => {
                            out.extend_from_slice(&reply.raw);
                            out.append(&mut raw);
                        }
                    }
                    self.stage = if done { Back::Head } else { Back::Pass(body) };
                }
                Back::Events { mut body, mut line } => {
                    let mut data = Vec::new();
                    let Some((n, done)) = body.take(&self.buf, Some(&mut data)) else {
                        chunk(&line, out);
                        continue;
                    };
                    self.buf.drain(..n);
                    line.extend_from_slice(&data);
                    {
                        let names = names.lock();
                        while let Some(end) = line.iter().position(|&b| b == b'\n') {
                            let event: Vec<u8> = line.drain(..=end).collect();
                            if let Some(kept) = names.event(&event) {
                                chunk(&kept, out);
                            }
                        }
                    }
                    if !done {
                        self.stage = Back::Events { body, line };
                        return;
                    }
                    chunk(&line, out);
                    out.extend_from_slice(b"0\r\n\r\n");
                    self.stage = Back::Head;
                }
            }
        }
    }

    /// What's held when the daemon's side ends: on as it came.
    fn finish(&mut self, out: &mut Vec<u8>) {
        match std::mem::replace(&mut self.stage, Back::Raw) {
            Back::Gather { reply, mut raw, .. } => {
                out.extend_from_slice(&reply.raw);
                out.append(&mut raw);
            }
            Back::Events { line, .. } => chunk(&line, out),
            _ => {}
        }
        out.append(&mut self.buf);
    }
}

/// What a leg's connections share: where its paths are, its names, what its BuildKit containers'
/// loopback relays need, and the leg's end, which ends them.
#[derive(Default)]
struct Leg {
    paths: RwLock<Paths>,
    names: Mutex<Names>,
    loopback: RwLock<Option<Loopback>>,
    stop: crate::rt::Shutdown,
}

impl std::fmt::Debug for Leg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leg").field("paths", &*self.paths.read()).finish_non_exhaustive()
    }
}

/// A leg's socket, served until it's dropped.
pub struct Socket {
    path: PathBuf,
    leg: Arc<Leg>,
    task: crate::rt::JoinHandle<()>,
}

impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Socket {
    /// Listens at `path` for a leg whose names are `names`, passing its connections on to the
    /// daemon at `daemon`.
    pub async fn listen(path: PathBuf, daemon: PathBuf, names: Names) -> anyhow::Result<Socket> {
        let leg = Arc::new(Leg { paths: RwLock::new(Paths::default()), names: Mutex::new(names), ..Leg::default() });
        let (bound, listening) = futures::channel::oneshot::channel::<Result<(), String>>();
        let (at, served) = (path.clone(), leg.clone());
        let task = crate::rt::spawn(async move {
            let listener = match hyper_rt::local::LocalListener::bind(&at, 64) {
                Ok(l) => l,
                Err(e) => {
                    let _ = bound.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = bound.send(Ok(()));
            while let Ok(stream) = listener.accept().await {
                if let Err(e) = crate::rt::spawn_local(connection(stream, daemon.clone(), served.clone())) {
                    tracing::warn!("a leg's Docker connection was dropped: {e}");
                }
            }
        });
        match listening.await {
            Ok(Ok(())) => Ok(Socket { path, leg, task }),
            Ok(Err(e)) => anyhow::bail!("listen at {}: {e}", path.display()),
            Err(_) => anyhow::bail!("listen at {}: the runtime took no task", path.display()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the leg container's paths are, once it's started.
    pub fn set_paths(&self, paths: Paths) {
        *self.leg.paths.write() = paths;
    }

    /// What the BuildKit containers the leg makes get, to reach kiln by the GHES stand-in's name.
    pub fn set_loopback(&self, loopback: Loopback) {
        *self.leg.loopback.write() = Some(loopback);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
        self.leg.stop.cancel();
        let _ = std::fs::remove_file(&self.path);
        if let Some(config) = self.leg.loopback.read().as_ref().and_then(|l| l.buildkitd.as_ref()) {
            let _ = std::fs::remove_file(config);
        }
    }
}

/// One of the leg's connections: its requests on to the daemon, the daemon's answers back.
async fn connection(stream: hyper_rt::local::LocalStream, daemon: PathBuf, leg: Arc<Leg>) {
    use hyper_rt::tcp::Shutdown;
    let Ok(up) = hyper_rt::local::LocalStream::connect(&daemon).await else { return };
    // What's done with each answer, in the order of the requests.
    let pending = Mutex::new(VecDeque::new());
    let requests = async {
        let mut requests = Requests::default();
        let (mut buf, mut out) = (vec![0u8; 64 << 10], Vec::new());
        while let Ok(n @ 1..) = stream.read(&mut buf).await {
            out.clear();
            requests.feed(buf.get(..n).unwrap_or_default(), &leg, &mut pending.lock(), &mut out);
            if !out.is_empty() && up.write_all(&out).await.is_err() {
                break;
            }
        }
        let _ = up.shutdown(Shutdown::Write);
    };
    let answers = async {
        let mut answers = Answers::default();
        let (mut buf, mut out) = (vec![0u8; 64 << 10], Vec::new());
        let mut open = true;
        while let Ok(n @ 1..) = up.read(&mut buf).await {
            out.clear();
            answers.feed(buf.get(..n).unwrap_or_default(), &leg.names, &mut pending.lock(), &mut out);
            if !out.is_empty() && stream.write_all(&out).await.is_err() {
                open = false;
                break;
            }
        }
        out.clear();
        answers.finish(&mut out);
        if open && !out.is_empty() {
            let _ = stream.write_all(&out).await;
        }
        let _ = stream.shutdown(Shutdown::Write);
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
        let out = translated(&serde_json::to_vec(&body)?, &p).ok_or_else(|| anyhow::anyhow!("nothing translated"))?;
        let out: serde_json::Value = serde_json::from_slice(&out)?;
        let binds: Vec<&str> = out.pointer("/HostConfig/Binds").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(serde_json::Value::as_str).collect()).unwrap_or_default();
        assert_eq!(
            binds,
            ["/var/lib/docker/rootfs/overlayfs/abc/usr/local/cargo:/cargo:Z", "/work/w/1234/ripgrep:/project:Z,ro", "cache:/cache", "/var/lib/docker/rootfs/overlayfs/abc/home/runner/.xargo:/xargo:Z"]
        );
        assert_eq!(out.pointer("/HostConfig/Mounts/0/Source").and_then(serde_json::Value::as_str), Some("/var/lib/docker/rootfs/overlayfs/abc/tmp/data"));
        assert_eq!(out.pointer("/HostConfig/Mounts/1/Source").and_then(serde_json::Value::as_str), Some("named"));
        assert_eq!(out.get("Image"), body.get("Image"));
        // The host's and kiln's server's names, as the leg's job container has them: at the
        // host's gateway, its loopback on its network, the server's own address when it's elsewhere.
        assert_eq!(out.pointer("/HostConfig/ExtraHosts"), Some(&serde_json::json!(["host.docker.internal:host-gateway", "kiln.localhost:host-gateway"])));
        let hosts = |body: serde_json::Value, paths: &Paths| -> anyhow::Result<serde_json::Value> {
            let out = translated(&serde_json::to_vec(&body)?, paths).ok_or_else(|| anyhow::anyhow!("nothing translated: {body}"))?;
            Ok(serde_json::from_slice::<serde_json::Value>(&out)?.pointer("/HostConfig/ExtraHosts").cloned().unwrap_or_default())
        };
        assert_eq!(hosts(serde_json::json!({"HostConfig": {"NetworkMode": "host"}}), &p)?, serde_json::json!(["host.docker.internal:127.0.0.1", "kiln.localhost:127.0.0.1"]));
        let elsewhere = leg().with_server(Some("10.0.0.5".to_owned()));
        assert_eq!(
            hosts(serde_json::json!({"HostConfig": {"NetworkMode": "bridge", "ExtraHosts": null}}), &elsewhere)?,
            serde_json::json!(["host.docker.internal:host-gateway", "kiln.localhost:10.0.0.5"])
        );
        // A name the container gives itself stays its own.
        assert_eq!(
            hosts(serde_json::json!({"HostConfig": {"ExtraHosts": ["host.docker.internal=192.168.1.9"]}}), &p)?,
            serde_json::json!(["host.docker.internal=192.168.1.9", "kiln.localhost:host-gateway"])
        );
        // One on another container's network, or none, takes no hosts of its own.
        let shared = serde_json::json!({"HostConfig": {"NetworkMode": "container:abc"}});
        assert_eq!(translated(&serde_json::to_vec(&shared)?, &p), None);
        // Nothing to change, or not JSON: on as it came.
        let same = serde_json::json!({"HostConfig": {"Binds": ["/work/w/1234/ripgrep:/src"], "ExtraHosts": ["host.docker.internal:host-gateway", "kiln.localhost:host-gateway"]}});
        assert_eq!(translated(&serde_json::to_vec(&same)?, &p), None);
        assert_eq!(translated(b"{not json", &p), None);
        Ok(())
    }

    /// A create's body as a socket whose names are the daemon's passes it on.
    fn translated(body: &[u8], paths: &Paths) -> Option<Vec<u8>> {
        rewrite(&Gather::ContainerCreate(None), body, paths, &mut Names::default(), None, &mut None)
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
    fn fed(input: &[u8], step: usize, leg: &Leg) -> Vec<u8> {
        fed_with(input, step, leg).0
    }

    /// And what's to be done with the answers.
    fn fed_with(input: &[u8], step: usize, leg: &Leg) -> (Vec<u8>, VecDeque<Answer>) {
        let (mut r, mut out, mut answers) = (Requests::default(), Vec::new(), VecDeque::new());
        for piece in input.chunks(step.max(1)) {
            r.feed(piece, leg, &mut answers, &mut out);
        }
        (out, answers)
    }

    fn leg_of(paths: Paths, names: Names) -> Leg {
        Leg { paths: RwLock::new(paths), names: Mutex::new(names), ..Leg::default() }
    }

    #[test]
    fn w13_r04_a_connections_requests_go_on() -> anyhow::Result<()> {
        let paths = leg_of(leg(), Names::default());
        let body = serde_json::to_vec(&serde_json::json!({"Image": "alpine", "HostConfig": {"Binds": ["/usr/local/cargo:/cargo"]}}))?;
        let translated = translated(&body, &leg()).ok_or_else(|| anyhow::anyhow!("nothing translated"))?;
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
        let paths = leg_of(leg(), Names::for_leg("leg-a", None));
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

    #[test]
    fn w13_r04_a_legs_names_are_its_own() -> anyhow::Result<()> {
        let mut names = Names::for_leg("leg-a", Some("kiln.runner-work=abc"));
        let k = names.suffix.clone();
        // What a leg makes is its own on the daemon, and what it says by that name is taken to it.
        let made = route("POST", "/v1.51/containers/create?name=cargo_test_0", &mut names);
        assert_eq!(made.target, Some(format!("/v1.51/containers/create?name=cargo_test_0{k}")));
        assert_eq!(made.gather, Some(Gather::ContainerCreate(Some("cargo_test_0".to_owned()))));
        assert_eq!(route("POST", "/v1.51/containers/cargo_test_0/start", &mut names).target, Some(format!("/v1.51/containers/cargo_test_0{k}/start")));
        assert_eq!(route("DELETE", "/containers/cargo_test_0?force=1", &mut names).target, Some(format!("/containers/cargo_test_0{k}?force=1")));
        let inspect = route("GET", "/v1.51/containers/cargo_test_0/json", &mut names);
        assert_eq!((inspect.target, inspect.answer), (Some(format!("/v1.51/containers/cargo_test_0{k}/json")), Answer::One));
        // An id, or a name it didn't make (kiln's for a service, what was there before): as it came.
        for target in ["/v1.51/containers/4f6c0d2e9b1a/json", "/containers/kiln_leg_db/logs?follow=1", "/v1.51/containers/json?all=1", "/images/cargo_test_0/json"] {
            assert_eq!(route("GET", target, &mut names).target, None, "{target}");
        }
        // A rename's new name is its own too.
        assert_eq!(route("POST", "/containers/cargo_test_0/rename?name=renamed", &mut names).target, Some(format!("/containers/cargo_test_0{k}/rename?name=renamed{k}")));
        assert_eq!(route("POST", "/containers/renamed/stop?t=1", &mut names).target, Some(format!("/containers/renamed{k}/stop?t=1")));
        assert_eq!(route("POST", "/commit?container=renamed&repo=x", &mut names).target, Some(format!("/commit?container=renamed{k}&repo=x")));
        // Lists and events come back without what other legs made.
        assert_eq!(route("GET", "/v1.51/containers/json?all=1", &mut names).answer, Answer::List);
        assert_eq!(route("GET", "/networks", &mut names).answer, Answer::List);
        assert_eq!(route("GET", "/v1.51/volumes?filters=x", &mut names).answer, Answer::List);
        assert_eq!(route("GET", "/events?since=1", &mut names).answer, Answer::Events);
        assert_eq!(route("HEAD", "/_ping", &mut names).answer, Answer::Bodiless);
        // A list's filters by name, anchored as a script checks one, and an event stream's.
        let decoded = |target: Option<String>| percent_decode(&target.unwrap_or_default());
        let listed = route("GET", &format!("/v1.51/containers/json?all=1&filters={}", percent_encode(r#"{"name":["^/renamed$"],"network":["net"],"before":["renamed"]}"#)), &mut names);
        assert_eq!(decoded(listed.target), format!(r#"/v1.51/containers/json?all=1&filters={{"name":["^/renamed{k}$"],"network":["net"],"before":["renamed{k}"]}}"#));
        assert_eq!(route("GET", &format!("/containers/json?filters={}", percent_encode(r#"{"name":["renamed"]}"#)), &mut names).target, None, "a name unanchored matches with the suffix");
        let watched = route("GET", &format!("/events?filters={}", percent_encode(r#"{"container":{"renamed":true},"type":["container"]}"#)), &mut names);
        assert_eq!(decoded(watched.target), format!(r#"/events?filters={{"container":{{"renamed{k}":true}},"type":["container"]}}"#));
        // A prune takes what the leg made, and only that.
        let prune = route("POST", "/v1.51/containers/prune?filters=%7B%22until%22%3A%5B%2224h%22%5D%7D", &mut names).target.unwrap_or_default();
        let filters = percent_decode(prune.split_once("filters=").map(|(_, f)| f).unwrap_or_default());
        let key = names.key.clone();
        assert_eq!(serde_json::from_str::<Value>(&filters)?, serde_json::json!({"until": ["24h"], "label": [key]}));
        let bare = route("POST", "/volumes/prune", &mut names).target.unwrap_or_default();
        assert_eq!(percent_decode(&bare), format!(r#"/volumes/prune?filters={{"label":["{key}"]}}"#));
        assert_eq!(with_label(Some(r#"{"label":{"a=b":true}}"#), "c=d"), r#"{"label":{"a=b":true,"c=d":true}}"#);
        assert_eq!(route("POST", "/networks/net/connect", &mut names).gather, Some(Gather::NetworkMember(true)));
        // A socket whose names are the daemon's changes none.
        let mut daemons = Names::default();
        let r = route("POST", "/containers/create?name=x", &mut daemons);
        assert_eq!((r.target, r.answer), (None, Answer::Plain));
        assert_eq!(route("GET", "/containers/json", &mut daemons).answer, Answer::Plain);
        assert_eq!(route("POST", "/containers/prune", &mut daemons).target, None);
        Ok(())
    }

    fn rewritten(gather: &Gather, body: &Value, names: &mut Names) -> anyhow::Result<Value> {
        let out = rewrite(gather, &serde_json::to_vec(body)?, &leg(), names, None, &mut None).ok_or_else(|| anyhow::anyhow!("nothing rewritten: {body}"))?;
        Ok(serde_json::from_slice(&out)?)
    }

    fn created(name: &str, body: &Value, names: &mut Names) -> anyhow::Result<Value> {
        let gather = route("POST", &format!("/containers/create?name={name}"), names).gather.ok_or_else(|| anyhow::anyhow!("no gather"))?;
        rewritten(&gather, body, names)
    }

    #[test]
    fn w13_r04_a_legs_creates_name_its_own() -> anyhow::Result<()> {
        use serde_json::json;
        let mut names = Names::for_leg("leg-a", Some("kiln.runner-work=abc"));
        let k = names.suffix.clone();
        let ours = json!({ names.key.clone(): "leg-a", "kiln.socket.runner.abc": "1" });
        let net = rewritten(&Gather::NetworkCreate, &json!({"Name": "net", "Driver": "bridge"}), &mut names)?;
        assert_eq!(net, json!({"Name": format!("net{k}"), "Driver": "bridge", "Labels": ours}));
        let volume = rewritten(&Gather::VolumeCreate, &json!({"Name": "data", "Labels": {"x": "y"}}), &mut names)?;
        let mut labelled = ours.clone();
        if let Some(o) = labelled.as_object_mut() {
            o.insert("x".into(), json!("y"));
        }
        assert_eq!(volume, json!({"Name": format!("data{k}"), "Labels": labelled}));
        created("db", &json!({"Image": "postgres"}), &mut names)?;
        let web = created(
            "web",
            &json!({
                "Image": "alpine",
                "Labels": {"app": "x"},
                "HostConfig": {
                    "NetworkMode": "net",
                    "Binds": ["data:/var/lib/postgresql/data", "/usr/local/cargo:/cargo", "cache:/cache:ro"],
                    "Mounts": [{"Type": "volume", "Source": "more", "Target": "/m"}, {"Type": "volume", "Source": "", "Target": "/anonymous"}],
                    "Links": ["db:database", "kiln_leg_svc:svc"],
                    "VolumesFrom": ["db:ro", "other"],
                    "PidMode": "container:db",
                    "IpcMode": "shareable"
                }
            }),
            &mut names,
        )?;
        let config = web.get("HostConfig").cloned().unwrap_or_default();
        assert_eq!(config.get("NetworkMode"), Some(&json!(format!("net{k}"))));
        // A volume it names is its own, made by its first use as on a machine of its own.
        assert_eq!(config.get("Binds"), Some(&json!([format!("data{k}:/var/lib/postgresql/data"), "/var/lib/docker/rootfs/overlayfs/abc/usr/local/cargo:/cargo", format!("cache{k}:/cache:ro")])));
        assert_eq!(config.pointer("/Mounts/0/Source"), Some(&json!(format!("more{k}"))));
        assert_eq!(config.pointer("/Mounts/1/Source"), Some(&json!("")));
        assert_eq!(config.get("Links"), Some(&json!([format!("db{k}:database"), "kiln_leg_svc:svc"])));
        assert_eq!(config.get("VolumesFrom"), Some(&json!([format!("db{k}:ro"), "other"])));
        assert_eq!(config.get("PidMode"), Some(&json!(format!("container:db{k}"))));
        assert_eq!(config.get("IpcMode"), Some(&json!("shareable")));
        // Found on its network by the name it was given, as it would be without the suffix.
        assert_eq!(web.pointer(&format!("/NetworkingConfig/EndpointsConfig/net{k}")), Some(&json!({"Aliases": ["web"]})));
        let mut labelled = ours.clone();
        if let Some(o) = labelled.as_object_mut() {
            o.insert("app".into(), json!("x"));
        }
        assert_eq!(web.get("Labels"), Some(&labelled));
        // A leg's socket within another leg's: what it made keeps the inner leg's label too.
        let inner = rewritten(&Gather::VolumeCreate, &json!({"Name": "deep", "Labels": {"kiln.socket.leg.k0123abcd": "leg-inner"}}), &mut names)?;
        assert_eq!(inner.pointer("/Labels/kiln.socket.leg.k0123abcd"), Some(&json!("leg-inner")));
        // On every network it joins that takes aliases: its own, kiln's.
        let joined = created("web2", &json!({"HostConfig": {"NetworkMode": "net"}, "NetworkingConfig": {"EndpointsConfig": {"net": {"Aliases": ["w"]}, "kiln_leg_net": null}}}), &mut names)?;
        assert_eq!(joined.pointer("/NetworkingConfig/EndpointsConfig"), Some(&json!({format!("net{k}"): {"Aliases": ["w", "web2"]}, "kiln_leg_net": {"Aliases": ["web2"]}})));
        // Docker's own networks take none.
        let bridged = created("web3", &json!({"HostConfig": {"NetworkMode": "bridge"}}), &mut names)?;
        assert_eq!(bridged.get("NetworkingConfig"), None);
        assert_eq!(bridged.pointer("/HostConfig/NetworkMode"), Some(&json!("bridge")));
        // A network's connect and disconnect: the leg's container, found by its name on it.
        assert_eq!(rewritten(&Gather::NetworkMember(true), &json!({"Container": "db"}), &mut names)?, json!({"Container": format!("db{k}"), "EndpointConfig": {"Aliases": ["db"]}}));
        assert_eq!(rewritten(&Gather::NetworkMember(false), &json!({"Container": "db", "Force": false}), &mut names)?, json!({"Container": format!("db{k}"), "Force": false}));
        assert_eq!(rewrite(&Gather::NetworkMember(true), br#"{"Container":"theirs"}"#, &leg(), &mut names, None, &mut None), None);
        // A socket whose names are the daemon's: on as it came, unless a bind mount is the host's elsewhere.
        assert_eq!(
            rewrite(
                &Gather::ContainerCreate(Some("web".to_owned())),
                br#"{"HostConfig":{"NetworkMode":"net","Binds":["data:/d"],"ExtraHosts":["host.docker.internal:host-gateway","kiln.localhost:host-gateway"]}}"#,
                &leg(),
                &mut Names::default(),
                None,
                &mut None
            ),
            None
        );
        Ok(())
    }

    /// A BuildKit container a leg makes (`setup-buildx-action`'s builder) gets its loopback relay:
    /// the static kiln mounted, the relay's port published on the host's loopback, the stand-in's
    /// name its own loopback; and it's named, as the daemon has it, for the relay to start in.
    /// Others, and those on the host's network or another's, get none.
    #[test]
    fn w08_r08_buildkit_containers_get_a_loopback_relay() -> anyhow::Result<()> {
        use serde_json::json;
        let cli = super::super::docker::Cli::new("docker", PathBuf::from("/nowhere"));
        let l = Loopback::new(PathBuf::from("/opt/kiln/kiln-musl"), "https://kiln.example:8443/x", cli.clone()).ok_or_else(|| anyhow::anyhow!("no loopback"))?;
        assert_eq!((l.port, l.server.clone()), (8443, ("kiln.example".to_owned(), 8443)));
        let plain = Loopback::new(PathBuf::new(), "http://kiln:token@127.0.0.1", cli.clone()).ok_or_else(|| anyhow::anyhow!("no loopback"))?;
        assert_eq!((plain.port, plain.server), (80, ("127.0.0.1".to_owned(), 80)));
        assert_eq!(Loopback::new(PathBuf::new(), "http://[::1]:7070/", cli.clone()).map(|l| l.server), Some(("::1".to_owned(), 7070)));
        assert!(Loopback::new(PathBuf::new(), "kiln.example", cli).is_none());
        let made = |name: Option<&str>, body: &Value, names: &mut Names, l: Option<&Loopback>| -> anyhow::Result<(Value, Option<String>)> {
            let target = name.map_or_else(|| "/containers/create".to_owned(), |n| format!("/containers/create?name={n}"));
            let gather = route("POST", &target, names).gather.ok_or_else(|| anyhow::anyhow!("no gather"))?;
            let mut relayed = None;
            let out = rewrite(&gather, &serde_json::to_vec(body)?, &leg(), names, l, &mut relayed).ok_or_else(|| anyhow::anyhow!("nothing rewritten: {body}"))?;
            Ok((serde_json::from_slice(&out)?, relayed))
        };
        let mut names = Names::for_leg("leg-a", None);
        let k = names.suffix.clone();
        // As buildx makes a `docker-container` builder.
        let builder = json!({
            "Image": "moby/buildkit:buildx-stable-1",
            "HostConfig": {"Privileged": true, "Binds": ["/usr/local/cargo:/cargo"], "Mounts": [{"Type": "volume", "Source": "buildx_buildkit_b0_state", "Target": "/var/lib/buildkit"}]}
        });
        let (out, relayed) = made(Some("buildx_buildkit_b0"), &builder, &mut names, Some(&l))?;
        assert_eq!(relayed, Some(format!("buildx_buildkit_b0{k}")));
        assert_eq!(out.pointer("/HostConfig/Binds"), Some(&json!(["/var/lib/docker/rootfs/overlayfs/abc/usr/local/cargo:/cargo", "/opt/kiln/kiln-musl:/.kiln-relay:ro"])));
        assert_eq!(out.get("ExposedPorts"), Some(&json!({"3129/tcp": {}})));
        assert_eq!(out.pointer("/HostConfig/PortBindings"), Some(&json!({"3129/tcp": [{"HostIp": "127.0.0.1", "HostPort": ""}]})));
        assert_eq!(out.pointer("/HostConfig/ExtraHosts"), Some(&json!(["host.docker.internal:host-gateway", "kiln.localhost:127.0.0.1"])));
        // Of another image, by buildx's name; and through a socket whose names are the daemon's.
        let custom = json!({"Image": "registry.example/tools/bk:1", "HostConfig": {"NetworkMode": "default", "PortBindings": {"8080/tcp": [{"HostPort": "8080"}]}}});
        let (out, relayed) = made(Some("buildx_buildkit_c0"), &custom, &mut Names::default(), Some(&l))?;
        assert_eq!(relayed.as_deref(), Some("buildx_buildkit_c0"));
        assert_eq!(out.pointer("/HostConfig/PortBindings/8080~1tcp"), Some(&json!([{"HostPort": "8080"}])));
        assert_eq!(out.pointer("/HostConfig/PortBindings/3129~1tcp/0/HostIp"), Some(&json!("127.0.0.1")));
        // None for the rest.
        let on = |mode: &str| json!({"Image": "moby/buildkit:v0.25.1", "HostConfig": {"NetworkMode": mode}});
        let cases = [
            (Some("buildx_buildkit_h0"), on("host"), Some(&l)),
            (Some("buildx_buildkit_n0"), on("none"), Some(&l)),
            (Some("buildx_buildkit_s0"), on("container:db"), Some(&l)),
            (Some("web"), json!({"Image": "nginx", "HostConfig": {}}), Some(&l)),
            (None, on("bridge"), Some(&l)),
            (Some("buildx_buildkit_b1"), on("bridge"), None),
        ];
        for (name, body, l) in cases {
            let (out, relayed) = made(name, &body, &mut names, l)?;
            assert_eq!(relayed, None, "{name:?} {body}");
            assert!(!out.to_string().contains(RELAY_PATH), "{out}");
        }
        Ok(())
    }

    /// Where kiln mirrors Docker Hub (W12-R02), a builder buildx makes through a leg's socket (its
    /// `docker-container` driver, as `setup-buildx-action` uses) pulls a `FROM` image through kiln,
    /// by the stand-in's name through its loopback relay: the image is only at a mirror on the
    /// runner's loopback, so the build finds it there or not at all. Needs Docker, buildx, BuildKit's
    /// image here already (nothing is pulled from Docker Hub) and a static kiln (`KILN_EGRESS_BINARY`).
    #[test]
    fn w12_r02_a_buildkit_builder_pulls_through_kiln() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let Some(bin) = super::super::docker::find() else { return Ok(()) };
        let Some(binary) = super::super::egress::relay_binary() else { return Ok(()) };
        let have = |args: &[&str]| std::process::Command::new(&bin).args(args).output().is_ok_and(|o| o.status.success());
        if !Path::new("/var/run/docker.sock").exists() || super::super::docker::host_network() || !have(&["buildx", "version"]) || !have(&["image", "inspect", "moby/buildkit:buildx-stable-1"]) {
            return Ok(());
        }
        // An image of no layers, at a mirror on the runner's loopback only.
        let config = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]},"config":{}}"#.to_vec();
        let digest = |b: &[u8]| format!("sha256:{}", crate::util::sha256_hex(b));
        let (config_digest, config_size) = (digest(&config), config.len());
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config_size},
            "layers": []
        }))?;
        let manifest_digest = digest(&manifest);
        let mirror = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = mirror.local_addr()?.port();
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&asked);
        std::thread::spawn(move || {
            for conn in mirror.incoming().flatten() {
                let (config, manifest, config_digest, manifest_digest, seen) = (config.clone(), manifest.clone(), config_digest.clone(), manifest_digest.clone(), Arc::clone(&seen));
                std::thread::spawn(move || {
                    let mut conn = conn;
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && head.len() < 64 << 10 && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                        head.extend_from_slice(&byte);
                    }
                    let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned();
                    let mut words = line.split_whitespace();
                    let (method, target) = (words.next().unwrap_or_default().to_owned(), words.next().unwrap_or_default().to_owned());
                    seen.lock().push(format!("{method} {target}"));
                    // A mirror is asked with the upstream's name in `ns` (`?ns=docker.io`).
                    let at = target.split('?').next().unwrap_or_default().to_owned();
                    let image = "/v2/docker.io/library/kiln-mirror-probe";
                    let (status, kind, body): (&str, &str, Vec<u8>) = if at == format!("{image}/manifests/1") || at == format!("{image}/manifests/{manifest_digest}") {
                        ("200 OK", "application/vnd.oci.image.manifest.v1+json", manifest)
                    } else if at == format!("{image}/blobs/{config_digest}") {
                        ("200 OK", "application/vnd.oci.image.config.v1+json", config)
                    } else if at == "/v2/" {
                        ("200 OK", "application/json", b"{}".to_vec())
                    } else {
                        ("404 Not Found", "application/json", br#"{"errors":[{"code":"NAME_UNKNOWN","message":"not here"}]}"#.to_vec())
                    };
                    let _ = write!(conn, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nDocker-Content-Digest: {}\r\nConnection: close\r\n\r\n", body.len(), digest(&body));
                    if method != "HEAD" {
                        let _ = conn.write_all(&body);
                    }
                });
            }
        });
        let tmp = tempfile::tempdir()?;
        let context = tmp.path().join("context");
        std::fs::create_dir_all(&context)?;
        std::fs::write(context.join("Dockerfile"), "FROM docker.io/library/kiln-mirror-probe:1\nCOPY f /f\n")?;
        std::fs::write(context.join("f"), "built\n")?;
        let buildkitd = tmp.path().join("buildkitd.toml");
        std::fs::write(&buildkitd, buildkitd_config(&[("docker.io".into(), "docker.io".into())], port)?)?;
        let (at, dir) = (tmp.path().join("leg.sock"), tmp.path().to_owned());
        let leg = format!("test-mirror-{}", std::process::id());
        let builder = format!("kiln-mirror-{}", std::process::id());
        crate::rt::block_on(async move {
            let cli = super::super::docker::Cli::new(&bin, dir.join("docker")).owned_by(&dir);
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock"), Names::for_leg(&leg, cli.owner())).await?;
            socket.set_paths(Paths::new(None, Vec::new()));
            let l = Loopback::new(binary, &format!("http://127.0.0.1:{port}"), cli.clone()).ok_or_else(|| anyhow::anyhow!("no loopback"))?;
            socket.set_loopback(Loopback { buildkitd: Some(buildkitd), ..l });
            // buildx's own state stays here, not in the user's.
            let env =
                std::collections::BTreeMap::from([("DOCKER_HOST".to_owned(), format!("unix://{}", at.display())), ("BUILDX_CONFIG".to_owned(), dir.join("buildx").to_string_lossy().into_owned())]);
            let checked: anyhow::Result<()> = async {
                docker_at(&cli, &env, &["buildx", "create", "--name", &builder, "--driver", "docker-container", "--driver-opt", "image=moby/buildkit:buildx-stable-1", "--bootstrap"]).await?;
                let container = format!("buildx_buildkit_{builder}0");
                // Its relay is up once it answers at the stand-in's name (a workflow's next steps
                // give it that time).
                let mut up = false;
                for _ in 0..60 {
                    let probe = format!("http://kiln.localhost:{port}/v2/");
                    if docker_at(&cli, &env, &["exec", &container, "wget", "-qO-", "-T", "5", &probe]).await.is_ok() {
                        up = true;
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                }
                anyhow::ensure!(up, "the builder's relay never answered");
                let context = context.to_string_lossy().into_owned();
                let built = docker_at(&cli, &env, &["buildx", "build", "--builder", &builder, "--progress", "plain", "--output", "type=cacheonly", &context]).await;
                let asked = asked.lock().clone();
                anyhow::ensure!(built.is_ok(), "the build: {built:?}; the mirror was asked {asked:?}");
                anyhow::ensure!(asked.iter().any(|a| a.contains("/v2/docker.io/library/kiln-mirror-probe/manifests/1")), "the mirror was asked {asked:?}");
                Ok(())
            }
            .await;
            let _ = docker_at(&cli, &env, &["buildx", "rm", &builder]).await;
            // What the leg made goes with it, whatever buildx left.
            super::super::docker::Containers::start(cli.clone(), &leg, super::super::docker::PathMap::new(Vec::new()), "/w", None, Vec::new(), None, &|_| {}).await?.stop().await;
            drop(socket);
            checked
        })
    }

    /// Where kiln mirrors registries (W12-R02), a BuildKit container a leg makes reads kiln's
    /// config naming them, by the stand-in's name over plain HTTP; one the workflow configured
    /// itself (buildx's `--config`), or whose entrypoint isn't BuildKit's, keeps its own.
    #[test]
    fn w12_r02_buildkit_containers_pull_through_kiln() -> anyhow::Result<()> {
        use serde_json::json;
        let config = buildkitd_config(&[("docker.io".into(), "docker.io".into()), ("ghcr.io".into(), "/mirrors/ghcr/".into())], 42511)?;
        let read: toml::Table = toml::from_str(&config)?;
        let registry = read.get("registry").and_then(toml::Value::as_table).ok_or_else(|| anyhow::anyhow!("no registry: {config}"))?;
        let mirrors = |upstream: &str| {
            registry.get(upstream).and_then(|r| r.get("mirrors")).and_then(toml::Value::as_array).map(|m| m.iter().filter_map(toml::Value::as_str).map(str::to_owned).collect::<Vec<_>>())
        };
        assert_eq!(mirrors("docker.io"), Some(vec!["kiln.localhost:42511/docker.io".to_owned()]), "{config}");
        assert_eq!(mirrors("ghcr.io"), Some(vec!["kiln.localhost:42511/mirrors/ghcr".to_owned()]), "{config}");
        assert_eq!(registry.get("kiln.localhost:42511").and_then(|r| r.get("http")).and_then(toml::Value::as_bool), Some(true), "{config}");
        let flags = |v: &Value| v.get("Cmd").cloned().unwrap_or_default();
        let binds = |v: &Value| v.pointer("/HostConfig/Binds").and_then(Value::as_array).map(|b| b.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
        let (binary, at) = (PathBuf::from("/opt/kiln/kiln-musl"), PathBuf::from("/work/w/1234.buildkitd.toml"));
        // As setup-buildx-action has buildx make its builder.
        let mut v = json!({"Image": "moby/buildkit:buildx-stable-1", "Cmd": ["--allow-insecure-entitlement=network.host"], "HostConfig": {"Privileged": true}});
        assert!(with_relay(&mut v, &binary, Some(&at)));
        assert_eq!(flags(&v), json!(["--allow-insecure-entitlement=network.host", "--config", BUILDKITD_PATH]));
        assert_eq!(binds(&v), ["/opt/kiln/kiln-musl:/.kiln-relay:ro", "/work/w/1234.buildkitd.toml:/.kiln-buildkitd.toml:ro"]);
        // No flags at all: BuildKit's entrypoint takes kiln's alone.
        let mut bare = json!({"Image": "moby/buildkit:v0.25.1", "Cmd": null});
        assert!(with_relay(&mut bare, &binary, Some(&at)));
        assert_eq!(flags(&bare), json!(["--config", BUILDKITD_PATH]));
        // The workflow's own config, or another entrypoint: kiln's isn't given.
        for (cmd, entrypoint) in [
            (json!(["--debug", "--config", "/etc/buildkit/buildkitd.toml"]), json!(null)),
            (json!(["--config=/etc/buildkit/buildkitd.toml"]), json!(null)),
            (json!(["--debug"]), json!(["/bin/sh", "-c"])),
        ] {
            let mut theirs = json!({"Image": "moby/buildkit:master", "Cmd": cmd.clone(), "Entrypoint": entrypoint});
            assert!(with_relay(&mut theirs, &binary, Some(&at)));
            assert_eq!(flags(&theirs), cmd);
            assert_eq!(binds(&theirs), ["/opt/kiln/kiln-musl:/.kiln-relay:ro"]);
        }
        // Without mirrors, none.
        let mut plain = json!({"Image": "moby/buildkit:buildx-stable-1", "Cmd": ["--debug"]});
        assert!(with_relay(&mut plain, &binary, None));
        assert_eq!(flags(&plain), json!(["--debug"]));
        Ok(())
    }

    /// A BuildKit container a leg makes through its socket reaches kiln's server at the stand-in's
    /// name and at its own loopback, as curl takes `*.localhost`, without connecting to the host: its
    /// loopback relay (with a static kiln, `KILN_EGRESS_BINARY`), started once it runs, and the
    /// runner's connections waiting at its published port.
    #[test]
    fn w08_r08_a_buildkit_container_reaches_kiln_through_its_relay() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let Some(bin) = super::super::docker::find() else { return Ok(()) };
        let Some(binary) = super::super::egress::relay_binary() else { return Ok(()) };
        if !Path::new("/var/run/docker.sock").exists() || super::super::docker::host_network() {
            return Ok(());
        }
        // kiln's server, on the runner's loopback only.
        let server = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = server.local_addr()?.port();
        std::thread::spawn(move || {
            for conn in server.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut conn = conn;
                    let mut got = [0u8; 4096];
                    let n = conn.read(&mut got).unwrap_or_default();
                    let host = String::from_utf8_lossy(got.get(..n).unwrap_or_default()).lines().find_map(|l| l.strip_prefix("Host: ").map(str::to_owned)).unwrap_or_default();
                    let body = format!("kiln, as {host}");
                    let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                });
            }
        });
        let tmp = tempfile::tempdir()?;
        let (at, dir) = (tmp.path().join("leg.sock"), tmp.path().to_owned());
        let leg = format!("test-loopback-{}", std::process::id());
        crate::rt::block_on(async move {
            let cli = super::super::docker::Cli::new(&bin, dir.join("config")).owned_by(&dir);
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock"), Names::for_leg(&leg, cli.owner())).await?;
            socket.set_paths(Paths::new(None, Vec::new()));
            let l = Loopback::new(binary, &format!("http://127.0.0.1:{port}"), cli.clone()).ok_or_else(|| anyhow::anyhow!("no loopback"))?;
            socket.set_loopback(l);
            let env = std::collections::BTreeMap::from([("DOCKER_HOST".to_owned(), format!("unix://{}", at.display()))]);
            let checked: anyhow::Result<()> = async {
                docker_at(&cli, &env, &["run", "-d", "--name", "buildx_buildkit_probe0", "alpine:3", "sleep", "300"]).await?;
                let (mut by_name, mut by_loopback) = (String::new(), String::new());
                for _ in 0..60 {
                    let fetch = |url: String| ["exec", "buildx_buildkit_probe0", "wget", "-qO-", "-T", "5"].map(str::to_owned).into_iter().chain([url]).collect::<Vec<_>>();
                    by_name = cli.run(&fetch(format!("http://kiln.localhost:{port}/")), &env, std::time::Duration::from_secs(30)).await.unwrap_or_default();
                    by_loopback = cli.run(&fetch(format!("http://127.0.0.1:{port}/")), &env, std::time::Duration::from_secs(30)).await.unwrap_or_default();
                    if !by_name.is_empty() && !by_loopback.is_empty() {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                }
                anyhow::ensure!(by_name == format!("kiln, as kiln.localhost:{port}"), "by the stand-in's name: {by_name:?}");
                anyhow::ensure!(by_loopback == format!("kiln, as 127.0.0.1:{port}"), "at its loopback: {by_loopback:?}");
                Ok(())
            }
            .await;
            let _ = docker_at(&cli, &env, &["rm", "-f", "buildx_buildkit_probe0"]).await;
            drop(socket);
            checked
        })
    }

    /// What `Answers` gives the leg for `input`, fed `step` bytes at a time, its requests' answers
    /// `pending`.
    fn answered(input: &[u8], step: usize, names: &Mutex<Names>, pending: &[Answer]) -> Vec<u8> {
        let (mut a, mut out, mut pending) = (Answers::default(), Vec::new(), pending.iter().copied().collect::<VecDeque<_>>());
        for piece in input.chunks(step.max(1)) {
            a.feed(piece, names, &mut pending, &mut out);
        }
        a.finish(&mut out);
        out
    }

    fn json_answer(status: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nApi-Version: 1.51\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn w13_r04_answers_say_the_legs_names() {
        let mut names = Names::for_leg("leg-a", None);
        let (k, mine) = (names.suffix.clone(), names.key.clone());
        names.made(Kind::Container, "db");
        let names = Mutex::new(names);
        let theirs = "-k0123abcd";
        let mut input = String::new();
        let mut expected = String::new();
        // A ping's head: no body, whatever it says.
        let ping = "HTTP/1.1 200 OK\r\nApi-Version: 1.51\r\nContent-Length: 2\r\n\r\n";
        input.push_str(ping);
        expected.push_str(ping);
        // An inspect: the leg's names as it gave them.
        input.push_str(&json_answer("200 OK", &format!(r#"{{"Name":"/db{k}","HostConfig":{{"NetworkMode":"net{k}"}},"NetworkSettings":{{"Networks":{{"net{k}":{{}}}}}},"Id":"4f6c{k}x"}}"#)));
        expected.push_str(&json_answer("200 OK", &format!(r#"{{"Name":"/db","HostConfig":{{"NetworkMode":"net"}},"NetworkSettings":{{"Networks":{{"net":{{}}}}}},"Id":"4f6c{k}x"}}"#)));
        // A list, chunked: without what another leg made.
        let list = format!(
            r#"[{{"Names":["/db{k}"],"Labels":{{"{mine}":"leg-a"}}}},{{"Names":["/db{theirs}"],"Labels":{{"kiln.socket.leg.k0123abcd":"leg-b"}}}},{{"Names":["/nested{k}"],"Labels":{{"kiln.socket.leg.kfedcba98":"x","{mine}":"leg-a"}}}},{{"Names":["/kiln_leg_db"],"Labels":{{}}}}]"#
        );
        input.push_str("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n");
        for piece in list.as_bytes().chunks(11) {
            input.push_str(&format!("{:x}\r\n{}\r\n", piece.len(), String::from_utf8_lossy(piece)));
        }
        input.push_str("0\r\n\r\n");
        let listed = format!(
            r#"[{{"Names":["/db"],"Labels":{{"{mine}":"leg-a"}}}},{{"Names":["/nested"],"Labels":{{"kiln.socket.leg.kfedcba98":"x","{mine}":"leg-a"}}}},{{"Names":["/kiln_leg_db"],"Labels":{{}}}}]"#
        );
        expected.push_str(&format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{listed}", listed.len()));
        // A start's: nothing to say. A refusal's message: the leg's names.
        let started = "HTTP/1.1 204 No Content\r\nApi-Version: 1.51\r\n\r\n";
        input.push_str(started);
        expected.push_str(started);
        input.push_str(&json_answer("404 Not Found", &format!(r#"{{"message":"No such container: db{k}"}}"#)));
        expected.push_str(&json_answer("404 Not Found", r#"{"message":"No such container: db"}"#));
        // An interim answer, then the request's own.
        let interim = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        input.push_str(interim);
        expected.push_str(interim);
        // Events, an event to a line however they're chunked: another leg's left out.
        let [mine, other, network] = [
            format!(r#"{{"Type":"container","Action":"start","Actor":{{"ID":"1","Attributes":{{"{mine}":"leg-a","name":"db{k}"}}}}}}"#),
            format!(r#"{{"Type":"container","Action":"start","Actor":{{"ID":"2","Attributes":{{"kiln.socket.leg.k0123abcd":"leg-b","name":"db{theirs}"}}}}}}"#),
            format!(r#"{{"Type":"network","Action":"connect","Actor":{{"ID":"3","Attributes":{{"name":"net{k}","container":"1"}}}}}}"#),
        ];
        let stream: String = [&mine, &other, &network].iter().map(|e| format!("{e}\n")).collect();
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        input.push_str(head);
        for piece in stream.as_bytes().chunks(13) {
            input.push_str(&format!("{:x}\r\n{}\r\n", piece.len(), String::from_utf8_lossy(piece)));
        }
        input.push_str("0\r\n\r\n");
        expected.push_str(head);
        for e in [mine.replace(&k, ""), network.replace(&k, "")] {
            expected.push_str(&format!("{:x}\r\n{e}\n\r\n", e.len() + 1));
        }
        expected.push_str("0\r\n\r\n");
        let pending = [Answer::Bodiless, Answer::One, Answer::List, Answer::Plain, Answer::Plain, Answer::Plain, Answer::Events];
        for step in [1, 2, 3, 7, 64, input.len()] {
            assert_eq!(String::from_utf8_lossy(&answered(input.as_bytes(), step, &names, &pending)), expected, "{step} bytes at a time");
        }
        // Taken over: the streams as they come, whatever they look like.
        let taken = format!("HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\ndb{k}\r\nHTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(String::from_utf8_lossy(&answered(taken.as_bytes(), 5, &names, &[Answer::Takeover])), taken);
        // A socket whose names are the daemon's: all as it came.
        let daemons = Mutex::new(Names::default());
        assert_eq!(String::from_utf8_lossy(&answered(input.as_bytes(), 9, &daemons, &[Answer::Bodiless; 1])), input);
        // An answer the daemon ended short: what came of it.
        let short = input.as_bytes().get(..ping.len() + 120).unwrap_or_default();
        assert_eq!(answered(short, 4, &names, &pending), short);
    }

    /// The requests' side tells the answers' what each is: in order, pipelined.
    #[test]
    fn w13_r04_each_request_says_what_its_answer_is() {
        let leg = leg_of(leg(), Names::for_leg("leg-a", None));
        let input = b"HEAD /_ping HTTP/1.1\r\nHost: x\r\n\r\nGET /v1.51/containers/json HTTP/1.1\r\nHost: x\r\n\r\nGET /events HTTP/1.1\r\nHost: x\r\n\r\nPOST /containers/abc/attach?stream=1 HTTP/1.1\r\nHost: x\r\n\r\nGET /containers/json HTTP/1.1\r\n\r\n";
        for step in [1, 7, input.len()] {
            let (out, answers) = fed_with(input, step, &leg);
            assert_eq!(out, input.to_vec(), "{step}");
            assert_eq!(answers, [Answer::Bodiless, Answer::List, Answer::Events, Answer::Takeover], "{step}");
        }
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
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock"), Names::default()).await?;
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

    async fn docker_at(cli: &super::super::docker::Cli, env: &std::collections::BTreeMap<String, String>, args: &[&str]) -> anyhow::Result<String> {
        cli.run(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(), env, std::time::Duration::from_secs(300)).await
    }

    /// Two legs on one runner make the same names at once on the daemon they share, each through a
    /// socket of its own: each finds what it made by them (on its network too), sees that and not
    /// what the other made, a compose project of the same name included, and what each made goes
    /// when its leg ends.
    #[test]
    fn w13_r04_two_legs_make_the_same_names() -> anyhow::Result<()> {
        use super::super::docker::{Cli, Containers, PathMap};
        use std::collections::BTreeMap;
        let Some(bin) = super::super::docker::find() else { return Ok(()) };
        if !Path::new("/var/run/docker.sock").exists() || super::super::docker::host_network() {
            return Ok(());
        }
        let tmp = tempfile::tempdir()?;
        let legs = [format!("test-names-a-{}", std::process::id()), format!("test-names-b-{}", std::process::id())];
        let suffixes: Vec<String> = legs.iter().map(|l| Names::for_leg(l, None).suffix).collect();
        let compose = tmp.path().join("compose.yml");
        std::fs::write(&compose, "services:\n  web:\n    image: alpine:3\n    command: sleep 300\n")?;
        let composes = std::process::Command::new(&bin).args(["compose", "version"]).output().is_ok_and(|o| o.status.success());
        let dir = tmp.path().to_owned();
        crate::rt::block_on(async move {
            let cli = Cli::new(&bin, dir.join("config")).owned_by(&dir);
            let (mut sockets, mut envs) = (Vec::new(), Vec::new());
            for (i, leg) in legs.iter().enumerate() {
                let at = dir.join(format!("{i}.sock"));
                sockets.push(Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock"), Names::for_leg(leg, cli.owner())).await?);
                envs.push(BTreeMap::from([("DOCKER_HOST".to_owned(), format!("unix://{}", at.display()))]));
            }
            let checked: anyhow::Result<()> = async {
                for env in &envs {
                    docker_at(&cli, env, &["network", "create", "net"]).await?;
                    docker_at(&cli, env, &["run", "-d", "--name", "db", "--network", "net", "alpine:3", "sleep", "300"]).await?;
                }
                for (i, (env, other)) in envs.iter().zip(suffixes.iter().rev()).enumerate() {
                    // Found by its name on its network.
                    docker_at(&cli, env, &["run", "--rm", "--network", "net", "alpine:3", "sh", "-c", "getent hosts db || ping -c 1 -W 5 db"]).await?;
                    let name = docker_at(&cli, env, &["inspect", "--format", "{{.Name}}", "db"]).await?;
                    anyhow::ensure!(name == "/db", "leg {i} inspects {name}");
                    let listed = docker_at(&cli, env, &["ps", "-a", "--format", "{{.Names}}"]).await?;
                    anyhow::ensure!(listed.lines().any(|l| l == "db") && !listed.contains(&format!("db{other}")), "leg {i} lists {listed}");
                    // A volume of its own, made by its first use.
                    docker_at(&cli, env, &["run", "--rm", "-v", "data:/d", "alpine:3", "sh", "-c", &format!("echo {i} > /d/f")]).await?;
                }
                for (i, env) in envs.iter().enumerate() {
                    let f = docker_at(&cli, env, &["run", "--rm", "-v", "data:/d", "alpine:3", "cat", "/d/f"]).await?;
                    anyhow::ensure!(f == i.to_string(), "leg {i}'s volume has {f}");
                }
                if composes {
                    let file = compose.to_string_lossy().into_owned();
                    let (up, ps, down) =
                        (["compose", "-p", "proj", "-f", &file, "up", "-d"], ["compose", "-p", "proj", "-f", &file, "ps", "--format", "{{.Name}}"], ["compose", "-p", "proj", "-f", &file, "down"]);
                    for env in &envs {
                        docker_at(&cli, env, &up).await?;
                    }
                    let (Some(a), Some(b)) = (envs.first(), envs.get(1)) else { anyhow::bail!("two legs") };
                    // Again: its network and its container, found by the names compose gave them.
                    docker_at(&cli, a, &up).await?;
                    let listed = docker_at(&cli, a, &ps).await?;
                    anyhow::ensure!(listed == "proj-web-1", "{listed}");
                    docker_at(&cli, a, &down).await?;
                    let theirs = docker_at(&cli, b, &ps).await?;
                    anyhow::ensure!(theirs == "proj-web-1", "the other leg's project is its own: {theirs}");
                }
                Ok(())
            }
            .await;
            // The legs end: what each made goes with it.
            for leg in &legs {
                Containers::start(cli.clone(), leg, PathMap::new(Vec::new()), "/w", None, Vec::new(), None, &|_| {}).await?.stop().await;
            }
            drop(sockets);
            let (none, mut left) = (BTreeMap::new(), String::new());
            let kinds: [&[&str]; 3] = [&["ps", "-aq"], &["network", "ls", "-q"], &["volume", "ls", "-q"]];
            for leg in &legs {
                for kind in kinds {
                    let mut args: Vec<String> = kind.iter().map(|a| (*a).to_owned()).collect();
                    args.extend(["--filter".to_owned(), format!("label={}", leg_label(leg))]);
                    left.push_str(&cli.run(&args, &none, std::time::Duration::from_secs(60)).await?);
                }
            }
            checked?;
            anyhow::ensure!(left.is_empty(), "left: {left}");
            Ok(())
        })
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
            let socket = Socket::listen(at.clone(), PathBuf::from("/var/run/docker.sock"), Names::for_leg(&leg, None)).await?;
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
