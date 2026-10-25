//! Container operations (docs/ci.md W24-R29): what building an image does, as kiln's own
//! operations rather than BuildKit's, each one a W24 unit of work keyed like any other, so image
//! builds share the ledger's one cache, its reports and its trust rules.
//!
//! - Sources: an image ([`Image`]), a git repository ([`Git`]), a file over HTTP ([`Http`]) and a
//!   build context ([`Local`]).
//! - A process run over a tree, with its mounts ([`Exec`]).
//! - File operations: [`CopyFiles`], [`Mkdir`], [`Mkfile`], [`Remove`], and [`Op::Merge`], which
//!   layers trees one over another.
//! - Image config edits ([`ConfigEdit`]), chained from a base image's config.
//! - An OCI image: its config and its chain of layers ([`Export`]).
//!
//! A [`Graph`] holds them as a DAG whose edges are named ([`Input::role`]). A node's id is a digest
//! of its operation's canonical text ([`Op::canonical`]) and its inputs' roles and ids: the same
//! operation over the same inputs is one node however the graph was built, and nothing outside the
//! operation enters it. [`Graph::unit`] makes a node a W24 unit of kind `Action` for
//! [`key::weak`]: its definition is the canonical text, and its upstream units are its inputs by
//! role, each as its output's digest once that's known (R03), and until then as its node id.
//!
//! [`dockerfile`] lowers a Dockerfile to a graph, as BuildKit's own frontend lowers it to LLB, and
//! names what kiln can't build natively yet.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::key::{self, Key, Kind, Unit};

pub mod dockerfile;
mod shell;

#[cfg(test)]
mod tests;

/// Bumped when what a node's id covers changes: every id made before then changes with it.
pub const VERSION: &str = "kiln/ops/v1";

/// A platform as OCI names one: `linux/amd64`, `linux/arm/v7`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub variant: String,
}

impl Platform {
    /// A platform, normalized as containerd normalizes one (`x86_64` is `amd64`, `aarch64` is
    /// `arm64`, a bare `arm` is `arm/v7`).
    pub fn new(os: &str, arch: &str, variant: &str) -> Self {
        let os = os.to_ascii_lowercase();
        let (arch, variant) = normalize_arch(arch, variant);
        Platform { os: if os == "macos" { "darwin".into() } else { os }, arch, variant }
    }

    /// Reads `os/arch[/variant]`. Both the system and the architecture are needed: pure code
    /// can't default either to the machine it happens to run on.
    pub fn parse(s: &str) -> Option<Platform> {
        let mut parts = s.trim().split('/');
        let os = parts.next().filter(|p| !p.is_empty())?;
        let arch = parts.next().filter(|p| !p.is_empty())?;
        let variant = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return None;
        }
        Some(Platform::new(os, arch, variant))
    }
}

fn normalize_arch(arch: &str, variant: &str) -> (String, String) {
    let (arch, variant) = (arch.to_ascii_lowercase(), variant.to_ascii_lowercase());
    match arch.as_str() {
        "i386" => ("386".into(), String::new()),
        "x86_64" | "x86-64" | "amd64" => ("amd64".into(), if variant == "v1" { String::new() } else { variant }),
        "aarch64" | "arm64" => ("arm64".into(), if matches!(variant.as_str(), "8" | "v8" | "v8.0") { String::new() } else { variant }),
        "armhf" => ("arm".into(), "v7".into()),
        "armel" => ("arm".into(), "v6".into()),
        "arm" => {
            let variant = match variant.as_str() {
                "" | "7" => "v7".into(),
                "5" | "6" | "8" => format!("v{variant}"),
                _ => variant,
            };
            ("arm".into(), variant)
        }
        _ => (arch, variant),
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)?;
        if !self.variant.is_empty() {
            write!(f, "/{}", self.variant)?;
        }
        Ok(())
    }
}

/// An image from a registry, for its root filesystem and its config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Normalized, with its tag: `docker.io/library/alpine:3.20`; no tag when only a digest pins it.
    pub reference: String,
    /// `sha256:…`, when it's pinned: in the Dockerfile, or as the caller resolved it.
    pub digest: Option<String>,
    /// Which of its platforms.
    pub platform: Platform,
}

/// A git repository's tree at a ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Git {
    /// The remote, without its fragment or query.
    pub url: String,
    /// A branch, a tag or a commit; empty for the remote's default branch.
    pub reference: String,
    /// The folder of the repository taken, empty for all of it.
    pub subdir: String,
    /// The commit it must be.
    pub checksum: Option<String>,
    /// Whether the `.git` folder stays.
    pub keep_git_dir: bool,
    /// Whether submodules are fetched.
    pub submodules: bool,
}

/// A file fetched over HTTP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http {
    pub url: String,
    /// The digest it must have (`sha256:…`).
    pub checksum: Option<String>,
    /// Its name in the tree it makes.
    pub filename: String,
}

/// A build context the caller sends: its files, as its patterns select them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Local {
    /// `context` for the main one.
    pub name: String,
    /// Paths it's limited to; empty for all of it.
    pub include: Vec<String>,
    /// Patterns it leaves out (its `.dockerignore`).
    pub exclude: Vec<String>,
}

/// What an [`Exec`] runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// Exec form: the process's arguments, as they are.
    Argv(Vec<String>),
    /// Shell form: `script` run by `shell` (the stage's `SHELL`, or the platform's default).
    Shell { shell: Vec<String>, script: String },
}

impl Command {
    /// The process's arguments: a shell form's are its shell's, then its script. Both forms key
    /// by these, so the same process written either way is the same operation.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Command::Argv(a) => a.clone(),
            Command::Shell { shell, script } => shell.iter().cloned().chain([script.clone()]).collect(),
        }
    }
}

/// An [`Exec`]'s network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// The build's own: what the builder gives every process.
    Default,
    /// None at all.
    None,
    /// The host's.
    Host,
}

/// An [`Exec`]'s sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    Sandbox,
    /// Privileged, as `RUN --security=insecure` asks.
    Insecure,
}

/// How a cache mount is shared between processes that use it at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    Shared,
    Private,
    Locked,
}

/// A mount of an [`Exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Where, in the process's filesystem.
    pub target: String,
    pub kind: MountKind,
}

/// What a [`Mount`] is. Those that read a tree take it from the exec's input `mount.<n>`, `n`
/// being the mount's place among the exec's mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// A tree from another operation, at `source` within it; what the process writes there is
    /// discarded.
    Bind { source: String, readonly: bool },
    /// A folder kept between builds by `id`, shared as `sharing` says, seeded from its input at
    /// `source` when it has one; created with `mode`, `uid` and `gid` when they're given.
    Cache { id: String, sharing: Sharing, source: String, readonly: bool, mode: Option<u32>, uid: Option<u32>, gid: Option<u32> },
    /// An empty in-memory folder.
    Tmpfs { size: Option<u64> },
    /// A secret, by id only: a file at the target (none when the target is empty), and the
    /// variable `env` when it's named.
    Secret { id: String, env: Option<String>, required: bool, mode: Option<u32>, uid: Option<u32>, gid: Option<u32> },
    /// An SSH agent's socket, by id; at the builder's own path when the target is empty.
    Ssh { id: String, required: bool, mode: Option<u32>, uid: Option<u32>, gid: Option<u32> },
}

/// A process run over a tree (its input `rootfs`, or an empty one), whose output is that tree as
/// the process left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    pub command: Command,
    /// Its whole environment: its image's, then the Dockerfile's `ENV`s and the `ARG`s in scope.
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    /// Empty for the image's default user.
    pub user: String,
    pub mounts: Vec<Mount>,
    pub network: Network,
    pub security: Security,
    pub platform: Platform,
    /// Proxy settings from the build's arguments (`HTTP_PROXY`, …): the process gets them, its
    /// key doesn't, as BuildKit keeps them out of its cache keys.
    pub proxy: BTreeMap<String, String>,
}

impl Exec {
    /// The secrets it receives, by name: its secret mounts' ids, and `ssh:<id>` for each SSH
    /// agent it's given.
    pub fn secrets(&self) -> BTreeSet<String> {
        self.mounts
            .iter()
            .filter_map(|m| match &m.kind {
                MountKind::Secret { id, .. } => Some(id.clone()),
                MountKind::Ssh { id, .. } => Some(format!("ssh:{id}")),
                _ => None,
            })
            .collect()
    }
}

/// Files copied into a tree (its input `dest`, or an empty one), each source from its own input
/// (`src.<n>`, `n` being the source's place).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyFiles {
    /// Where, absolute; a trailing `/` copies into a folder.
    pub dest: String,
    pub sources: Vec<CopySource>,
    /// `user[:group]`, by name or number.
    pub chown: Option<String>,
    /// Octal (`0644`) or symbolic (`u+x`).
    pub chmod: Option<String>,
    /// Patterns left out, of every source.
    pub exclude: Vec<String>,
}

/// One source of a [`CopyFiles`], with how BuildKit's Dockerfile frontend copies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySource {
    /// The path in its input.
    pub path: String,
    pub follow_symlinks: bool,
    /// A folder's contents, not the folder.
    pub contents_only: bool,
    /// `path` may be a wildcard, which may match nothing.
    pub wildcard: bool,
    /// An archive is unpacked (ADD's local files, `--unpack`).
    pub unpack: bool,
    /// `COPY --parents`: the patterns under `path` whose parent folders are kept.
    pub include: Vec<String>,
    /// Paths that must exist.
    pub required: Vec<String>,
}

/// A folder made in a tree (its input `dest`, or an empty one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mkdir {
    pub path: String,
    pub mode: u32,
    pub parents: bool,
    /// `user[:group]`, when it isn't root's.
    pub owner: Option<String>,
    /// Its time, in seconds (`SOURCE_DATE_EPOCH`).
    pub created: Option<i64>,
}

/// A file written into a tree (its input `dest`, or an empty one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mkfile {
    pub path: String,
    pub mode: u32,
    pub data: String,
    pub owner: Option<String>,
}

/// A path removed from a tree (its input `dest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remove {
    pub path: String,
    pub allow_not_found: bool,
    pub wildcard: bool,
}

/// A process's health check, as an image config holds it: durations in nanoseconds, 0 for the
/// runtime's default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    /// `["NONE"]`, `["CMD", …]` or `["CMD-SHELL", "…"]`.
    pub test: Vec<String>,
    pub interval: u64,
    pub timeout: u64,
    pub start_period: u64,
    pub start_interval: u64,
    pub retries: u32,
}

/// One edit of an image config. Edits chain from a base (input `base`): another edit, an image
/// (whose config, with `PATH` set to the default where it has none, as BuildKit sets it), or
/// nothing, which is BuildKit's empty config (working folder `/`, the default `PATH`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEdit {
    /// Variables set (`ENV`).
    Env(BTreeMap<String, String>),
    Label(BTreeMap<String, String>),
    Workdir(String),
    User(String),
    /// `keep_cmd` is false when it clears the command it inherited: no `CMD` came before it in
    /// its stage.
    Entrypoint { args: Vec<String>, keep_cmd: bool },
    /// The command, with `ArgsEscaped` set as BuildKit sets it.
    Cmd(Vec<String>),
    /// Ports added, as `port/protocol`.
    Expose(BTreeSet<String>),
    /// Volumes added.
    Volume(BTreeSet<String>),
    StopSignal(String),
    Healthcheck(Healthcheck),
    Shell(Vec<String>),
    /// The image's author (`MAINTAINER`).
    Maintainer(String),
    /// A trigger for images built on this one. Not native yet: kiln doesn't run triggers, so
    /// [`dockerfile`] reports `ONBUILD` rather than making this.
    Onbuild(String),
}

/// An OCI image for `platform`: its config is input `config`'s (BuildKit's empty one when there's
/// none), its layers are inputs `layer.<n>`, in order. Each layer is what its operation changed in
/// the tree it worked on (its `rootfs` or `dest`), or all of its output when it had none (a
/// linked copy, an image's own layers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    pub platform: Platform,
    /// `SOURCE_DATE_EPOCH`, in seconds: the time its history and files get.
    pub epoch: Option<i64>,
}

/// One operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Image(Image),
    Git(Git),
    Http(Http),
    Local(Local),
    Exec(Exec),
    Copy(CopyFiles),
    Mkdir(Mkdir),
    Mkfile(Mkfile),
    Remove(Remove),
    /// Its inputs' trees (`input.<n>`), each laid over the ones before it.
    Merge,
    Config(ConfigEdit),
    Export(Export),
}

/// Canonical text: a kind, then each field as ` name=value` in a fixed order, every string
/// quoted and every list, map and nested value bracketed, so no two different operations write
/// the same text.
struct Text(String);

impl Text {
    fn new(kind: &str) -> Self {
        Text(kind.to_owned())
    }

    fn name(&mut self, name: &str) {
        self.0.push(' ');
        self.0.push_str(name);
        self.0.push('=');
    }

    fn s(mut self, name: &str, v: &str) -> Self {
        self.name(name);
        quote(&mut self.0, v);
        self
    }

    fn o(mut self, name: &str, v: Option<&str>) -> Self {
        self.name(name);
        match v {
            None => self.0.push_str("null"),
            Some(v) => quote(&mut self.0, v),
        }
        self
    }

    fn n(mut self, name: &str, v: impl fmt::Display) -> Self {
        self.name(name);
        self.0.push_str(&v.to_string());
        self
    }

    fn on(mut self, name: &str, v: Option<impl fmt::Display>) -> Self {
        self.name(name);
        match v {
            None => self.0.push_str("null"),
            Some(v) => self.0.push_str(&v.to_string()),
        }
        self
    }

    fn l<'a>(mut self, name: &str, v: impl IntoIterator<Item = &'a String>) -> Self {
        self.name(name);
        self.0.push('[');
        for (i, s) in v.into_iter().enumerate() {
            if i > 0 {
                self.0.push(',');
            }
            quote(&mut self.0, s);
        }
        self.0.push(']');
        self
    }

    fn m(mut self, name: &str, v: &BTreeMap<String, String>) -> Self {
        self.name(name);
        self.0.push('{');
        for (i, (k, v)) in v.iter().enumerate() {
            if i > 0 {
                self.0.push(',');
            }
            quote(&mut self.0, k);
            self.0.push(':');
            quote(&mut self.0, v);
        }
        self.0.push('}');
        self
    }

    /// Nested values, each already canonical text.
    fn nested(mut self, name: &str, v: impl IntoIterator<Item = String>) -> Self {
        self.name(name);
        self.0.push('[');
        for (i, s) in v.into_iter().enumerate() {
            if i > 0 {
                self.0.push(',');
            }
            self.0.push('(');
            self.0.push_str(&s);
            self.0.push(')');
        }
        self.0.push(']');
        self
    }
}

/// `s` as a JSON string.
fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn mount_text(m: &Mount) -> String {
    let t = match &m.kind {
        MountKind::Bind { source, readonly } => Text::new("bind").s("source", source).n("readonly", readonly),
        MountKind::Cache { id, sharing, source, readonly, mode, uid, gid } => Text::new("cache")
            .s("id", id)
            .s("sharing", match sharing {
                Sharing::Shared => "shared",
                Sharing::Private => "private",
                Sharing::Locked => "locked",
            })
            .s("source", source)
            .n("readonly", readonly)
            .on("mode", *mode)
            .on("uid", *uid)
            .on("gid", *gid),
        MountKind::Tmpfs { size } => Text::new("tmpfs").on("size", *size),
        MountKind::Secret { id, env, required, mode, uid, gid } => Text::new("secret").s("id", id).o("env", env.as_deref()).n("required", required).on("mode", *mode).on("uid", *uid).on("gid", *gid),
        MountKind::Ssh { id, required, mode, uid, gid } => Text::new("ssh").s("id", id).n("required", required).on("mode", *mode).on("uid", *uid).on("gid", *gid),
    };
    t.s("target", &m.target).0
}

fn source_text(s: &CopySource) -> String {
    Text::new("source")
        .s("path", &s.path)
        .n("follow_symlinks", s.follow_symlinks)
        .n("contents_only", s.contents_only)
        .n("wildcard", s.wildcard)
        .n("unpack", s.unpack)
        .l("include", &s.include)
        .l("required", &s.required)
        .0
}

impl Op {
    /// Its kind's name: `image`, `exec`, `copy`, `config`, ….
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Image(_) => "image",
            Op::Git(_) => "git",
            Op::Http(_) => "http",
            Op::Local(_) => "local",
            Op::Exec(_) => "exec",
            Op::Copy(_) => "copy",
            Op::Mkdir(_) => "mkdir",
            Op::Mkfile(_) => "mkfile",
            Op::Remove(_) => "remove",
            Op::Merge => "merge",
            Op::Config(_) => "config",
            Op::Export(_) => "export",
        }
    }

    /// Its canonical text: every field that decides what it does, in a fixed order. What doesn't
    /// decide it stays out: an [`Exec`]'s proxy settings, and whether its command was written in
    /// shell form (its arguments are what count).
    pub fn canonical(&self) -> String {
        match self {
            Op::Image(i) => Text::new("image").s("reference", &i.reference).o("digest", i.digest.as_deref()).s("platform", &i.platform.to_string()).0,
            Op::Git(g) => Text::new("git")
                .s("url", &g.url)
                .s("ref", &g.reference)
                .s("subdir", &g.subdir)
                .o("checksum", g.checksum.as_deref())
                .n("keep_git_dir", g.keep_git_dir)
                .n("submodules", g.submodules)
                .0,
            Op::Http(h) => Text::new("http").s("url", &h.url).o("checksum", h.checksum.as_deref()).s("filename", &h.filename).0,
            Op::Local(l) => Text::new("local").s("name", &l.name).l("include", &l.include).l("exclude", &l.exclude).0,
            Op::Exec(e) => Text::new("exec")
                .l("args", &e.command.argv())
                .m("env", &e.env)
                .s("cwd", &e.cwd)
                .s("user", &e.user)
                .nested("mounts", e.mounts.iter().map(mount_text))
                .s("network", match e.network {
                    Network::Default => "default",
                    Network::None => "none",
                    Network::Host => "host",
                })
                .s("security", match e.security {
                    Security::Sandbox => "sandbox",
                    Security::Insecure => "insecure",
                })
                .s("platform", &e.platform.to_string())
                .0,
            Op::Copy(c) => Text::new("copy")
                .s("dest", &c.dest)
                .nested("sources", c.sources.iter().map(source_text))
                .o("chown", c.chown.as_deref())
                .o("chmod", c.chmod.as_deref())
                .l("exclude", &c.exclude)
                .0,
            Op::Mkdir(m) => Text::new("mkdir").s("path", &m.path).n("mode", format!("{:o}", m.mode)).n("parents", m.parents).o("owner", m.owner.as_deref()).on("created", m.created).0,
            Op::Mkfile(m) => Text::new("mkfile").s("path", &m.path).n("mode", format!("{:o}", m.mode)).s("data", &m.data).o("owner", m.owner.as_deref()).0,
            Op::Remove(r) => Text::new("remove").s("path", &r.path).n("allow_not_found", r.allow_not_found).n("wildcard", r.wildcard).0,
            Op::Merge => Text::new("merge").0,
            Op::Config(c) => match c {
                ConfigEdit::Env(m) => Text::new("config.env").m("set", m).0,
                ConfigEdit::Label(m) => Text::new("config.label").m("set", m).0,
                ConfigEdit::Workdir(w) => Text::new("config.workdir").s("path", w).0,
                ConfigEdit::User(u) => Text::new("config.user").s("user", u).0,
                ConfigEdit::Entrypoint { args, keep_cmd } => Text::new("config.entrypoint").l("args", args).n("keep_cmd", keep_cmd).0,
                ConfigEdit::Cmd(args) => Text::new("config.cmd").l("args", args).0,
                ConfigEdit::Expose(p) => Text::new("config.expose").l("ports", p).0,
                ConfigEdit::Volume(v) => Text::new("config.volume").l("paths", v).0,
                ConfigEdit::StopSignal(s) => Text::new("config.stopsignal").s("signal", s).0,
                ConfigEdit::Healthcheck(h) => Text::new("config.healthcheck")
                    .l("test", &h.test)
                    .n("interval", h.interval)
                    .n("timeout", h.timeout)
                    .n("start_period", h.start_period)
                    .n("start_interval", h.start_interval)
                    .n("retries", h.retries)
                    .0,
                ConfigEdit::Shell(s) => Text::new("config.shell").l("shell", s).0,
                ConfigEdit::Maintainer(m) => Text::new("config.maintainer").s("author", m).0,
                ConfigEdit::Onbuild(t) => Text::new("config.onbuild").s("trigger", t).0,
            },
            Op::Export(x) => Text::new("export").s("platform", &x.platform.to_string()).on("epoch", x.epoch).0,
        }
    }
}

/// A node's id: a digest of its operation's canonical text and its inputs' roles, ids and paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Its first 12 hex digits, for people.
    pub fn short(&self) -> String {
        self.hex().chars().take(12).collect()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

/// An edge: the node an operation reads, in the role the operation gives it (`rootfs`, `dest`,
/// `src.0`, `mount.1`, `base`, `config`, `layer.2`, `input.0`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Input {
    pub role: String,
    pub node: NodeId,
    /// What it reads of that node's output, when it's less than all of it (a copy's source, a
    /// mount's folder): a ledger can digest just those (W24-R03, R04).
    pub paths: Vec<String>,
}

impl Input {
    pub fn new(role: impl Into<String>, node: NodeId) -> Self {
        Input { role: role.into(), node, paths: Vec::new() }
    }

    /// The same edge, reading only `paths`.
    pub fn reading(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.paths = paths.into_iter().collect();
        self
    }
}

/// A node: an operation and what it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub op: Op,
    pub inputs: Vec<Input>,
    /// The Dockerfile lines it came from; not part of its id.
    pub lines: BTreeSet<u32>,
}

/// Why a node can't be added.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("input `{role}` reads node {node}, which isn't in the graph")]
    Missing { role: String, node: NodeId },
    #[error("two inputs are both `{0}`")]
    Duplicate(String),
}

/// Container operations as a DAG. Nodes can only read nodes already in it, so it never has a
/// cycle; adding an operation that's there already, over the same inputs, gives its node again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    nodes: BTreeMap<NodeId, Node>,
}

/// SHA-256 over length-prefixed strings.
fn put(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_be_bytes());
    h.update(s.as_bytes());
}

impl Graph {
    pub fn new() -> Self {
        Graph::default()
    }

    /// What `op` reading `inputs` would be called: its operation's canonical text and its
    /// inputs' roles, ids and paths, in their order, under [`VERSION`].
    pub fn id_of(op: &Op, inputs: &[Input]) -> NodeId {
        let mut h = Sha256::new();
        put(&mut h, VERSION);
        put(&mut h, &op.canonical());
        h.update((inputs.len() as u64).to_be_bytes());
        for i in inputs {
            put(&mut h, &i.role);
            h.update(i.node.0);
            h.update((i.paths.len() as u64).to_be_bytes());
            for p in &i.paths {
                put(&mut h, p);
            }
        }
        NodeId(h.finalize().into())
    }

    /// Adds `op`, reading `inputs` (in its roles' order, which is part of what it does: a merge's
    /// later inputs lie over its earlier ones), and gives its id.
    pub fn add(&mut self, op: Op, inputs: Vec<Input>) -> Result<NodeId, GraphError> {
        let mut roles = BTreeSet::new();
        for i in &inputs {
            if !roles.insert(i.role.as_str()) {
                return Err(GraphError::Duplicate(i.role.clone()));
            }
            if !self.nodes.contains_key(&i.node) {
                return Err(GraphError::Missing { role: i.role.clone(), node: i.node });
            }
        }
        let id = Graph::id_of(&op, &inputs);
        self.nodes.entry(id).or_insert(Node { op, inputs, lines: BTreeSet::new() });
        Ok(id)
    }

    /// Notes that `line` of a Dockerfile made node `id`.
    pub fn note(&mut self, id: &NodeId, line: u32) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.lines.insert(line);
        }
    }

    pub fn get(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Every node, by id.
    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every node `id` reads, directly or not.
    pub fn ancestors(&self, id: &NodeId) -> BTreeSet<NodeId> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![*id];
        while let Some(n) = todo.pop() {
            for i in self.nodes.get(&n).map(|n| n.inputs.as_slice()).unwrap_or_default() {
                if seen.insert(i.node) {
                    todo.push(i.node);
                }
            }
        }
        seen
    }

    /// Every node that reads `id`, directly or not.
    pub fn dependents(&self, id: &NodeId) -> BTreeSet<NodeId> {
        let mut readers: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for (n, node) in &self.nodes {
            for i in &node.inputs {
                readers.entry(i.node).or_default().push(*n);
            }
        }
        let mut seen = BTreeSet::new();
        let mut todo = vec![*id];
        while let Some(n) = todo.pop() {
            for r in readers.get(&n).map(Vec::as_slice).unwrap_or_default() {
                if seen.insert(*r) {
                    todo.push(*r);
                }
            }
        }
        seen
    }

    /// Every node, each after what it reads; ties go by id, so the order is always the same.
    pub fn topo(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut done = BTreeSet::new();
        for id in self.nodes.keys() {
            let mut stack = vec![(*id, false)];
            while let Some((n, expanded)) = stack.pop() {
                if done.contains(&n) {
                    continue;
                }
                if expanded {
                    done.insert(n);
                    out.push(n);
                    continue;
                }
                stack.push((n, true));
                for i in self.nodes.get(&n).map(|n| n.inputs.as_slice()).unwrap_or_default().iter().rev() {
                    if !done.contains(&i.node) {
                        stack.push((i.node, false));
                    }
                }
            }
        }
        out
    }

    /// Node `id` as a W24 unit of kind `Action`, which [`key::weak`] keys (W24-R01, R03, R29).
    ///
    /// - Its definition is its operation's canonical text, which holds everything that decides
    ///   what it does, a secret by name only.
    /// - Its upstream units are its inputs, by role: each one as `digest` gives the content digest
    ///   of what it reads (the whole output, or its [`Input::paths`]), once that's known (R03:
    ///   outputs, never keys); until then as `node:<id>` of the node read, which changes whenever
    ///   anything that node depends on does.
    /// - Its secrets are its exec's secret and SSH mounts, by name (R05).
    ///
    /// Nothing else enters it: no environment, platform, inputs or trust domain beyond the
    /// operation's own, so the same operation keys alike wherever it's built. The ledger scopes
    /// entries by domain (R21) and may set `trust` before keying.
    pub fn unit(&self, id: &NodeId, digest: impl Fn(&Input) -> Option<String>) -> Option<Unit> {
        let node = self.nodes.get(id)?;
        let upstream = node.inputs.iter().map(|i| (i.role.clone(), digest(i).unwrap_or_else(|| format!("node:{}", i.node.hex())))).collect();
        let secrets = match &node.op {
            Op::Exec(e) => e.secrets(),
            _ => BTreeSet::new(),
        };
        Some(Unit {
            kind: Kind::Action,
            definition: node.op.canonical(),
            secrets,
            env: BTreeMap::new(),
            platform: key::Platform::default(),
            inputs: BTreeMap::new(),
            upstream,
            trust: String::new(),
        })
    }

    /// Node `id`'s weak key ([`Graph::unit`]).
    pub fn key(&self, id: &NodeId, digest: impl Fn(&Input) -> Option<String>) -> Option<Key> {
        self.unit(id, digest).map(|u| key::weak(&u))
    }

    /// Every node's weak key, with what `digest` knows of their inputs.
    pub fn keys(&self, digest: impl Fn(&Input) -> Option<String>) -> BTreeMap<NodeId, Key> {
        self.nodes.keys().filter_map(|id| self.key(id, &digest).map(|k| (*id, k))).collect()
    }
}
