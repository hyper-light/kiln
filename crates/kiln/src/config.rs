//! Configuration: a TOML file plus environment overrides. Every field has a default so kiln starts
//! with no config at all (bare-metal quick start).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub mod ci;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub server: Server,
    pub gitops: GitOps,
    pub repos: Vec<RepoConfig>,
    pub local_agent: LocalAgent,
    pub kubernetes: Kubernetes,
    pub terminal: Terminal,
    pub retention: Retention,
    pub telemetry: Telemetry,
    /// Speech to text for the console's microphone where the browser can't do it itself.
    pub voice: crate::voice::Voice,
    /// Upgrades: where kiln's own binaries for every platform come from.
    pub updates: Updates,
    pub discovery: Discovery,
    /// Deployments (docs/cd.md).
    pub cd: Cd,
    /// CI (docs/ci.md).
    pub ci: ci::Ci,
    /// Conversations (docs/conversations.md): the runners that keep a copy of the server's store.
    pub conversations: Conversations,
    /// What a runner's administrator declares of it (docs/ci.md W13-R02): `[agent.facts]` and
    /// `[agent.probes]`. The server's own runner reads them here; `kiln agent` from its config file.
    pub agent: crate::runner::probe::Declared,
    /// Environment variables injected into every job (e.g. registry mirrors). Values starting with
    /// `file:` are read from that path at job dispatch time and masked in logs.
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub http_addr: String,
    pub quic_addr: String,
    /// Base URL agents and humans use to reach the HTTP API.
    pub public_url: String,
    /// `host:port` agents dial for QUIC.
    pub quic_advertise: String,
    pub data_dir: PathBuf,
    pub admin_token_file: Option<PathBuf>,
    pub metrics_public: bool,
    /// Extra DNS names / IPs for the server certificate.
    pub tls_names: Vec<String>,
    /// Also serve HTTPS on `http_addr` (plain HTTP keeps working there: each connection is
    /// answered in what it starts with). The certificate is `https_cert`/`https_key` when given,
    /// else one from the kiln CA, which devices trust once they install `/ca.pem` (or
    /// `/ca.mobileconfig`). See docs/front-door.md for HTTPS on a runner instead.
    pub https: bool,
    /// A PEM certificate chain (leaf first) for `https`, instead of the kiln CA's.
    pub https_cert: Option<PathBuf>,
    /// Its PEM private key.
    pub https_key: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitOps {
    pub orgs: Vec<String>,
    pub poll_seconds: u32,
    pub discover_seconds: u32,
    /// Only auto-register discovered repos that are not archived / forks.
    pub skip_forks: bool,
    pub webhook_secret_file: Option<PathBuf>,
    /// Optional token for commit statuses and higher API limits (never required).
    pub github_token_file: Option<PathBuf>,
    /// Concurrent git operations (fetches) across all repos.
    pub git_concurrency: usize,
    /// Build pull requests from refs/pull/*/head.
    pub pull_requests: bool,
    /// Synthesize a build + test pipeline (from Cargo.toml, go.mod, pyproject.toml, package.json,
    /// rebar.config) for repositories that have no `.kiln/` directory yet.
    pub auto_pipelines: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct RepoConfig {
    pub name: String,
    pub clone_url: Option<String>,
    pub branches: Vec<String>,
    pub poll_seconds: Option<u32>,
    /// `native`: kiln hosts it (docs/ci.md W14-R01), made empty or from `import`; `upstream`
    /// (the default): kiln follows `clone_url`.
    pub mode: Option<String>,
    /// A native repository's history, copied once from this URL.
    pub import: Option<String>,
    /// A new, empty native repository's trunk (`main`).
    pub default_branch: Option<String>,
    /// A native repository's upstream: its trunk is pushed there after each land.
    pub mirror: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalAgent {
    pub enabled: bool,
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub slots: u32,
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kubernetes {
    pub enabled: bool,
    pub namespace: String,
    pub agent_image: String,
    pub service_account: String,
    pub cache_pvc: Option<String>,
    pub max_pods: u32,
    /// Seconds to wait for a launched pod's agent to connect before retrying.
    pub start_timeout_seconds: u32,
    pub node_selector: BTreeMap<String, String>,
    pub image_pull_policy: String,
    /// Default requests when a job doesn't specify resources.
    pub default_cpu: String,
    pub default_memory: String,
    /// URL / address pods use to reach the server (defaults to public_url / quic_advertise).
    pub server_url: Option<String>,
    pub server_quic: Option<String>,
    /// PriorityClass for job pods (low, so CI never starves the control plane).
    pub priority_class: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terminal {
    pub enabled: bool,
    pub shell: Option<String>,
    pub max_sessions: usize,
    /// Close a terminal session after this long with nobody attached.
    pub idle_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    pub days: u32,
    /// Hard cap on a single job's log size.
    pub max_job_log_mb: u64,
    /// How long the audit log keeps its rows (docs/ci.md W19-R05).
    pub audit_days: u32,
}

/// `[conversations]` (docs/conversations.md, "Mirrors").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conversations {
    /// How many runners keep a copy of the server's conversation store. 0: none.
    pub mirrors: usize,
    /// Runners to pick first, by name (each still has to keep a store of its own).
    pub mirror_on: Vec<String>,
    /// Days the server's store keeps an ended agent's transcript (its events stay). 0: for good.
    /// Runners and mirrors keep them the default 30.
    pub keep_closed_days: u32,
}

impl Conversations {
    /// How long ended agents' transcripts are kept (None: for good).
    pub fn keep_closed(&self) -> Option<std::time::Duration> {
        (self.keep_closed_days > 0).then(|| std::time::Duration::from_secs(u64::from(self.keep_closed_days) * 86_400))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub sample_seconds: u32,
    pub log_format: String,
}

/// Finding other kiln instances (multicast DNS `_kiln._tcp.local.` or the OS's own service
/// discovery, unicast DNS-SD, probes of addresses, Tailscale peers) and letting machines ask to join
/// this server as agents. Joining always needs an admin's approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    /// Master switch for announcing and browsing.
    pub enabled: bool,
    /// Advertise this server on the local network (UDP 5353 must be open to the LAN).
    pub announce: bool,
    /// Keep a table of other kiln instances seen on the network (`GET /api/discovery`).
    pub browse: bool,
    /// Accept join requests (`POST /api/join-requests`) from machines running `kiln join`.
    pub join_requests: bool,
    /// Network interfaces to use (e.g. `["enp5s0"]`). Empty: every multicast-capable LAN
    /// interface except loopback, point-to-point links and container bridges.
    pub interfaces: Vec<String>,
    /// Use the system's service discovery where there is one (Bonjour on macOS, DNS-SD on
    /// Windows 10 1809+) instead of kiln's own multicast DNS. Linux always uses kiln's own.
    pub native: bool,
    /// Unicast DNS-SD domains to browse for `_kiln._tcp.<domain>`, besides the system's search
    /// domains and the browse domains they list (`b._dns-sd._udp.<domain>`).
    pub domains: Vec<String>,
    /// Name servers for unicast DNS-SD (`"192.168.1.1"`, `"[fd00::1]:53"`). Empty: those of
    /// `/etc/resolv.conf`, or of the network adapters on Windows.
    pub dns_servers: Vec<String>,
    /// Who may query this instance's multicast DNS responder by unicast from outside its own
    /// subnets (scans and probes from other networks): `"private"` (RFC 1918, 100.64.0.0/10,
    /// fc00::/7, link-local), `"any"`, or addresses and CIDRs. Empty: the local subnets only.
    pub answer_from: Vec<String>,
    /// Probe the peers `tailscale status` lists, every few minutes.
    pub tailscale: bool,
    /// Addresses or host names (optionally `:<http port>`) probed every minute.
    pub seeds: Vec<String>,
    /// Publish this instance in a unicast DNS zone with DNS UPDATE signed with TSIG.
    pub publish: Option<Publish>,
}

/// `[discovery.publish]`: the zone kiln adds its DNS-SD records to (RFC 2136 + RFC 8945).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Publish {
    /// The primary name server of the zone, `host[:port]`.
    pub server: String,
    /// The zone, e.g. `home.example.com`.
    pub zone: String,
    /// The TSIG key's name.
    pub key_name: String,
    /// A file holding the key's base64 secret (alone, or as `tsig-keygen` writes it).
    pub key_secret_file: PathBuf,
    /// `hmac-sha256` (default), `hmac-sha384` or `hmac-sha512`.
    pub algorithm: String,
}

fn hostname() -> String {
    crate::os::hostname()
}

/// `/var/lib/kiln` as root; otherwise per user: `~/.local/share/kiln` (`$XDG_DATA_HOME`) on Unix,
/// `%LOCALAPPDATA%\kiln` on Windows.
/// Deployments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cd {
    pub enabled: bool,
    /// How often sources are checked for new commits (webhooks check at once).
    pub poll_seconds: u64,
    /// Outside Kubernetes, the kubeconfig context that stands in for Argo CD's in-cluster
    /// destination (`https://kubernetes.default.svc`).
    pub in_cluster_context: String,
    /// The kubeconfig file contexts are read from (the default one when empty).
    pub kubeconfig: String,
    pub helm: String,
    pub kubectl: String,
    /// The kustomize binary; `kubectl kustomize` when empty.
    pub kustomize: String,
    /// Extra `kustomize build` options (Argo CD's `kustomize.buildOptions`).
    pub kustomize_options: Vec<String>,
    pub render_timeout_seconds: u64,
    /// Requests a second kiln sends a cluster, unless the cluster says otherwise.
    pub qps: f64,
    /// Wait between sync waves (Argo CD's `ARGOCD_SYNC_WAVE_DELAY`).
    pub wave_delay_ms: u64,
    /// Whether agents may sync, roll back, take over and delete apps outside any environment.
    /// An environment says so itself (`agents: { deploy: true }`). Off: agents plan and ask.
    pub agents_deploy: bool,
}

impl Default for Cd {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_seconds: 60,
            in_cluster_context: String::new(),
            kubeconfig: String::new(),
            helm: "helm".into(),
            kubectl: "kubectl".into(),
            kustomize: String::new(),
            kustomize_options: Vec::new(),
            render_timeout_seconds: 90,
            qps: 50.0,
            wave_delay_ms: 2_000,
            agents_deploy: false,
        }
    }
}

pub fn default_data_dir() -> PathBuf {
    if crate::os::is_root() {
        return crate::os::system_dirs().0;
    }
    crate::os::data_home().join("kiln")
}

/// `$KILN_CONFIG`; `/etc/kiln/kiln.toml` as root; otherwise per user: `~/.config/kiln/kiln.toml`
/// (`$XDG_CONFIG_HOME`) on Unix, `%APPDATA%\kiln\kiln.toml` on Windows.
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KILN_CONFIG") {
        return PathBuf::from(p);
    }
    if crate::os::is_root() {
        return crate::os::system_dirs().1;
    }
    crate::os::config_home().join("kiln").join("kiln.toml")
}

impl Default for Server {
    fn default() -> Self {
        let host = hostname();
        Self {
            http_addr: "0.0.0.0:7070".into(),
            quic_addr: "0.0.0.0:7071".into(),
            public_url: format!("http://{host}:7070"),
            quic_advertise: format!("{host}:7071"),
            data_dir: default_data_dir(),
            admin_token_file: None,
            metrics_public: true,
            tls_names: Vec::new(),
            https: false,
            https_cert: None,
            https_key: None,
        }
    }
}

impl Default for GitOps {
    fn default() -> Self {
        Self {
            orgs: Vec::new(),
            poll_seconds: 60,
            discover_seconds: 900,
            skip_forks: true,
            webhook_secret_file: None,
            github_token_file: None,
            git_concurrency: 8,
            pull_requests: true,
            auto_pipelines: true,
        }
    }
}

impl Default for LocalAgent {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self {
            enabled: true,
            name: None,
            labels: vec!["baremetal".into(), "local".into()],
            slots: u32::try_from((cpus / 4).max(1)).unwrap_or(4),
            work_dir: None,
        }
    }
}

impl Default for Kubernetes {
    fn default() -> Self {
        Self {
            enabled: false,
            namespace: "kiln-jobs".into(),
            agent_image: "kiln:dev".into(),
            service_account: "kiln-job".into(),
            cache_pvc: None,
            max_pods: 64,
            start_timeout_seconds: 300,
            node_selector: BTreeMap::new(),
            image_pull_policy: "IfNotPresent".into(),
            default_cpu: "1".into(),
            default_memory: "2Gi".into(),
            server_url: None,
            server_quic: None,
            priority_class: None,
        }
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            enabled: true,
            announce: true,
            browse: true,
            join_requests: true,
            interfaces: Vec::new(),
            native: true,
            domains: Vec::new(),
            dns_servers: Vec::new(),
            answer_from: vec!["private".into()],
            tailscale: true,
            seeds: Vec::new(),
            publish: None,
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self { enabled: true, shell: None, max_sessions: 32, idle_minutes: 720 }
    }
}

impl Default for Conversations {
    fn default() -> Self {
        Self { mirrors: 1, mirror_on: Vec::new(), keep_closed_days: 30 }
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self { days: 30, max_job_log_mb: 256, audit_days: 400 }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self { sample_seconds: 5, log_format: "text".into() }
    }
}


impl Config {
    /// Load from `path` if it exists (a missing file is fine), then apply env overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str::<Config>(&s).with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if let Some(v) = std::env::var_os("KILN_DATA_DIR") {
            cfg.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("KILN_HTTP_ADDR") {
            cfg.server.http_addr = v;
        }
        if let Ok(v) = std::env::var("KILN_QUIC_ADDR") {
            cfg.server.quic_addr = v;
        }
        if let Ok(v) = std::env::var("KILN_PUBLIC_URL") {
            cfg.server.public_url = v;
        }
        if let Ok(v) = std::env::var("KILN_QUIC_ADVERTISE") {
            cfg.server.quic_advertise = v;
        }
        Ok(cfg)
    }

    pub fn data(&self, sub: &str) -> PathBuf {
        self.server.data_dir.join(sub)
    }

    pub fn local_agent_name(&self) -> String {
        self.local_agent.name.clone().unwrap_or_else(hostname)
    }

    pub fn hostname() -> String {
        hostname()
    }

    pub fn render_default() -> String {
        toml::to_string_pretty(&Config::default()).unwrap_or_default()
    }
}

/// `[updates]`: the pipeline that builds kiln for every platform (`.kiln/release.yaml`). When its
/// job succeeds, the binaries it leaves (`dist/kiln-<target>`) are what that platform's runners
/// upgrade to: no token, no upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    /// The repository, as kiln names it.
    pub release_repo: String,
    /// Its pipeline.
    pub release_pipeline: String,
    /// The branch "Build every platform" builds.
    pub release_branch: String,
    /// Keep every platform in step with the server: whenever the other platforms' binaries
    /// aren't built from the commit the server runs, run the pipeline on that commit
    /// (`crate::updates::in_step_loop`), so every runner can be upgraded to what the server runs.
    /// Needs a server built knowing its commit (`KILN_COMMIT`).
    pub in_step: bool,
    /// Runners whose update policy is Automatic follow the newest kiln (`crate::fleet`): when the
    /// server or a runner runs a newer commit of the release branch, they are brought to it, by a
    /// binary of it, or by building it itself on a machine with a kiln checkout. `false`: none is,
    /// whatever its policy.
    pub follow: bool,
    /// On a runner's machine: it takes upgrades from its server. `false`: its runner refuses every
    /// one, whatever the server's policy for it (`kiln config set updates.accept false`); the
    /// server shows it so.
    pub accept: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self { release_repo: "hyper-light/kiln".to_owned(), release_pipeline: "release".to_owned(), release_branch: "main".to_owned(), in_step: true, follow: true, accept: true }
    }
}
