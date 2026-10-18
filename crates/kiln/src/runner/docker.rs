//! Containers for legs (docs/ci.md W13-R03, W13-R08) and Docker actions (W07-R06), the way
//! GitHub's self-hosted runner does it, through the host's `docker` (or `podman`) CLI.
//!
//! - **Network.** Each leg gets its own network.
//! - **Services.** They start on that network, aliased by their ids, with their ports published
//!   to the host. They are awaited until their health checks pass.
//! - **The job container.** `container:` makes one, idle (`tail -f /dev/null`), with the runner's
//!   directories mounted at GitHub's paths: the workspace at `/__w/<repo>/<repo>`, temp at
//!   `/__w/_temp`, actions at `/__w/_actions`, the tool cache at `/__w/_tool`, kiln's Node at
//!   `/__e`, and `/github/home` and `/github/workflow`. Steps run in it through `docker exec`,
//!   with paths translated. Values reach it by name (`-e KEY`, the value in the CLI's
//!   environment), so secrets never show on a command line.
//! - **Docker actions.** Built from their Dockerfile (tagged by the action's commit, kept) or
//!   pulled (`docker://`), then run with GitHub's mounts and environment.
//! - **Teardown.** After the post steps, everything labelled with the leg is removed.
//! - **Credentials.** They go in a per-leg `DOCKER_CONFIG`, never in a command line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine as _;

/// How long pulls and builds may take.
const SLOW: Duration = Duration::from_secs(1800);
const QUICK: Duration = Duration::from_secs(60);
/// How long services may take to become healthy.
const HEALTHY_WITHIN: Duration = Duration::from_secs(600);

/// The container CLI on this machine (its full path, so a step's `PATH` can't hide it): `docker`,
/// else `podman`, when its daemon answers.
pub fn find() -> Option<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for bin in ["docker", "podman"] {
        let Some(full) = std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file()) else { continue };
        let ok = std::process::Command::new(&full).args(["version", "--format", "{{.Server.Version}}"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
        if ok {
            return Some(full.to_string_lossy().into_owned());
        }
    }
    None
}

/// Whether this machine can run containers, probed once (for `CAP_DOCKER`, W13-R01).
pub fn available() -> bool {
    static PROBED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROBED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(find().is_some());
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false)
    })
}

/// A container to start, its keys evaluated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spec {
    pub image: String,
    pub credentials: Option<(String, String)>,
    pub env: Vec<(String, String)>,
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    pub options: Option<String>,
    /// The host's Docker socket inside: GitHub mounts it in job containers. A `docker`
    /// executor's container doesn't get it (W13-R04: Docker access is opt-in per pool).
    pub docker_socket: bool,
    /// For a `docker` executor's container: the host user (uid, gid) its steps run as, through
    /// the image's `runner` user.
    pub user: Option<(u32, u32)>,
}

/// The group that owns the host's Docker socket.
#[cfg(unix)]
fn socket_group() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/var/run/docker.sock").ok().map(|m| m.gid())
}

#[cfg(not(unix))]
fn socket_group() -> Option<u32> {
    None
}

/// The user this runner runs as: (uid, gid).
#[cfg(unix)]
pub fn host_user() -> Option<(u32, u32)> {
    Some((nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw()))
}

#[cfg(not(unix))]
pub fn host_user() -> Option<(u32, u32)> {
    None
}

/// kiln's runner images (W13-R04), built on the runner the first time a leg needs one: what
/// GitHub's hosted Ubuntu runners have that most workflows count on. Besides the build and
/// network basics, that's the toolchains workflows call without a `setup-*` action: CMake,
/// Ninja, the autotools and Clang; a JDK with Maven, Ant and sbt (`JAVA_HOME` set); PHP with
/// Composer; Ruby and Bundler; Leiningen; pipx; Rust (rustup, with rustfmt and clippy); Node.js
/// 22; the Docker CLI with buildx and compose (for legs given the host's Docker, W13-R04). Each
/// comes from Ubuntu's archive or the project's own release server. apt assumes yes, as GitHub's
/// images set it, so `sudo apt-get install x` works without `-y`. `-slim` (`kiln/runner-ubuntu-24.04-slim`) is
/// the basics alone, for rules that want a small image.
///
/// With `user`, steps run as `runner`, a user with the host's uid and gid and passwordless
/// sudo, as on GitHub's hosted runners. What they write in the mounted work directory then stays
/// the host user's, and tools that refuse root, or another user's files, work; Cargo's and npm's
/// global directories are theirs, so `cargo install` and `npm install -g` work as on GitHub.
pub fn runner_dockerfile(image: &str, user: Option<(u32, u32)>) -> Option<String> {
    let name = image.strip_prefix("kiln/runner-")?;
    let (name, slim) = name.strip_suffix("-slim").map_or((name, false), |n| (n, true));
    // `ImageOS` as GitHub's hosted runners set it: setup-ruby, setup-beam and others pick their
    // prebuilt tools by it.
    let (base, os) = match name {
        "ubuntu-24.04" => ("ubuntu:24.04", "ubuntu24"),
        "ubuntu-22.04" => ("ubuntu:22.04", "ubuntu22"),
        _ => return None,
    };
    let mut d = format!(
        "FROM {base}\nENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 LC_ALL=C.UTF-8 ImageOS={os}\n\
         RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl wget git git-lfs jq unzip zip xz-utils zstd bzip2 gnupg sudo \\\n    \
         build-essential pkg-config python3 python3-venv python3-pip openssh-client rsync file lsb-release tzdata locales iproute2 netcat-openbsd \\\n    \
         && rm -rf /var/lib/apt/lists/*\n"
    );
    if !slim {
        d.push_str(
            "ENV JAVA_HOME=/usr/lib/jvm/default-java RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
             RUN printf 'APT::Get::Assume-Yes \"true\";\\nAcquire::Retries \"5\";\\n' > /etc/apt/apt.conf.d/90kiln-runner \\\n    \
             && apt-get update && apt-get install -y --no-install-recommends cmake ninja-build autoconf automake libtool gettext clang lld \\\n    \
             default-jdk-headless maven ant php-cli php-xml php-mbstring php-curl php-zip php-intl php-sqlite3 composer ruby-full ruby-bundler leiningen pipx \\\n    \
             && rm -rf /var/lib/apt/lists/*\n\
             RUN curl -fsSL https://github.com/sbt/sbt/releases/download/v1.10.7/sbt-1.10.7.tgz | tar -xz -C /usr/local && ln -s /usr/local/sbt/bin/sbt /usr/local/bin/sbt\n\
             RUN case \"$(dpkg --print-architecture)\" in amd64) d=x86_64 b=amd64 ;; arm64) d=aarch64 b=arm64 ;; esac \\\n    \
             && curl -fsSL \"https://download.docker.com/linux/static/stable/$d/docker-27.5.1.tgz\" | tar -xz -C /usr/local/bin --strip-components=1 docker/docker \\\n    \
             && mkdir -p /usr/local/lib/docker/cli-plugins \\\n    \
             && curl -fsSL -o /usr/local/lib/docker/cli-plugins/docker-buildx \"https://github.com/docker/buildx/releases/download/v0.20.1/buildx-v0.20.1.linux-$b\" \\\n    \
             && curl -fsSL -o /usr/local/lib/docker/cli-plugins/docker-compose \"https://github.com/docker/compose/releases/download/v2.32.4/docker-compose-linux-$d\" \\\n    \
             && chmod +x /usr/local/lib/docker/cli-plugins/*\n\
             RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt -c clippy --no-modify-path && chmod -R a+w \"$RUSTUP_HOME\" \"$CARGO_HOME\"\n\
             RUN case \"$(dpkg --print-architecture)\" in amd64) a=x64 ;; arm64) a=arm64 ;; *) a=\"$(dpkg --print-architecture)\" ;; esac \\\n    \
             && f=$(curl -fsSL https://nodejs.org/dist/latest-v22.x/SHASUMS256.txt | awk -v want=\"linux-$a.tar.xz\" 'substr($2, length($2) - length(want) + 1) == want { print $2 }') \\\n    \
             && curl -fsSL \"https://nodejs.org/dist/latest-v22.x/$f\" | tar -xJ -C /usr/local --strip-components=1 --exclude CHANGELOG.md --exclude LICENSE --exclude README.md\n",
        );
    }
    if let Some((uid, gid)) = user.filter(|(uid, _)| *uid != 0) {
        // The base image's own user may hold the uid (Ubuntu's `ubuntu` is 1000).
        d.push_str(&format!(
            "RUN u=$(getent passwd {uid} | cut -d: -f1); if [ -n \"$u\" ]; then userdel -r \"$u\" 2>/dev/null || userdel \"$u\"; fi \\\n    \
             && (getent group {gid} >/dev/null || groupadd -g {gid} runner) \\\n    \
             && useradd -m -u {uid} -g {gid} -s /bin/bash runner \\\n    \
             && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner && chmod 0440 /etc/sudoers.d/runner\n"
        ));
        if !slim {
            // And the directories setup actions install into without sudo, as GitHub's images
            // let them (setup-dotnet's /usr/share/dotnet).
            d.push_str(&format!("RUN mkdir -p /usr/share/dotnet && chown -R {uid}:{gid} /usr/local/cargo /usr/local/rustup /usr/local/lib/node_modules /usr/local/bin /usr/share/dotnet\n"));
        }
    }
    Some(d)
}

/// One runner image build at a time on this machine.
static IMAGE_BUILD: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// The CLI, with the leg's own credential store.
#[derive(Debug, Clone)]
pub struct Cli {
    pub bin: String,
    config: PathBuf,
}

impl Cli {
    pub fn new(bin: &str, config: PathBuf) -> Cli {
        Cli { bin: bin.to_owned(), config }
    }

    fn command(&self, args: &[String], env: &BTreeMap<String, String>) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(args).envs(env).env("DOCKER_CONFIG", &self.config);
        c
    }

    /// The program, arguments and environment that open an interactive shell in `container`
    /// (W17-R01): bash where it has one, else sh, with `env` passed by name.
    pub fn shell_argv(&self, container: &str, user: Option<&str>, workdir: &str, env: &BTreeMap<String, String>) -> (Vec<String>, Vec<(String, String)>) {
        let mut a: Vec<String> = vec![self.bin.clone(), "exec".into(), "-it".into(), "--workdir".into(), workdir.to_owned()];
        if let Some(u) = user {
            a.extend(["--user".to_owned(), u.to_owned()]);
        }
        for k in env.keys() {
            a.extend(["-e".to_owned(), k.clone()]);
        }
        a.extend([container.to_owned(), "sh".into(), "-c".into(), "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi".into()]);
        let mut vars: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        vars.push(("DOCKER_CONFIG".into(), self.config.to_string_lossy().into_owned()));
        (a, vars)
    }

    /// Runs the CLI: its standard output, or an error with its standard error.
    pub async fn run(&self, args: &[String], env: &BTreeMap<String, String>, timeout: Duration) -> anyhow::Result<String> {
        let out = crate::proc::output(self.command(args, env), timeout).await?;
        if !out.status.success() {
            anyhow::bail!("{} {}: {}", self.bin, args.first().map(String::as_str).unwrap_or_default(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The command for a step's process in a container (`docker exec`), its values by name.
    pub fn exec_command(&self, container: &str, user: Option<&str>, workdir: &str, env: &BTreeMap<String, String>, prog: &str, args: &[String]) -> Command {
        let mut a: Vec<String> = vec!["exec".into(), "-i".into(), "--workdir".into(), workdir.to_owned()];
        if let Some(u) = user {
            a.extend(["--user".to_owned(), u.to_owned()]);
        }
        for k in env.keys() {
            a.push("-e".into());
            a.push(k.clone());
        }
        a.push(container.to_owned());
        a.push(prog.to_owned());
        a.extend(args.iter().cloned());
        self.command(&a, env)
    }

    /// Records credentials for `image`'s registry in the leg's credential store.
    pub fn login(&self, image: &str, user: &str, password: &str) -> anyhow::Result<()> {
        let reg = registry(image);
        let key = if reg == "docker.io" { "https://index.docker.io/v1/".to_owned() } else { reg };
        let path = self.config.join("config.json");
        let mut v: serde_json::Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({ "auths": {} }));
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        if let Some(auths) = v.get_mut("auths").and_then(serde_json::Value::as_object_mut) {
            auths.insert(key, serde_json::json!({ "auth": auth }));
        }
        crate::util::write_secret(&path, v.to_string().as_bytes(), 0o600)?;
        Ok(())
    }
}

/// The registry an image is pulled from.
pub fn registry(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => first.to_owned(),
        _ => "docker.io".to_owned(),
    }
}

/// `options:` split as a shell would (quotes kept together).
pub fn split_options(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// Host paths and where they are in the job container, longest first, and the server's
/// loopback addresses as a container reaches them.
#[derive(Debug, Clone, Default)]
pub struct PathMap(Vec<(PathBuf, String)>);

/// The host, as containers reach it (`--add-host host.docker.internal:host-gateway`).
pub const HOST_ALIAS: &str = "host.docker.internal";

/// `KILN_CONTAINER_NETWORK=host` on a runner: a leg's containers (its job container, services
/// and Docker actions) share the host's network instead of a network of their own, for machines
/// whose firewall keeps containers from reaching the host (and so the server on it). Services
/// are then reached by their names at 127.0.0.1, and legs running at once can't both have a
/// service on the same port.
pub fn host_network() -> bool {
    HOST_NETWORK.load(std::sync::atomic::Ordering::Relaxed) || std::env::var("KILN_CONTAINER_NETWORK").is_ok_and(|v| v.trim().eq_ignore_ascii_case("host"))
}

static HOST_NETWORK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Containers on the host's network from now on, as `KILN_CONTAINER_NETWORK=host` says: for
/// `kiln workflow run`, whose server listens on loopback (docs/ci.md W17-R06).
pub fn use_host_network() {
    HOST_NETWORK.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Where `host.docker.internal` points: the host's gateway, or the host itself on its network.
fn host_alias() -> String {
    format!("{HOST_ALIAS}:{}", if host_network() { "127.0.0.1" } else { "host-gateway" })
}

impl PathMap {
    pub fn new(mut pairs: Vec<(PathBuf, String)>) -> PathMap {
        pairs.sort_by_key(|(h, _)| std::cmp::Reverse(h.as_os_str().len()));
        PathMap(pairs)
    }

    /// `s` (a path, or text holding paths) with the host's paths swapped for the container's. A
    /// URL of the host's loopback (`http://127.0.0.1:…`, `http://localhost:…`) becomes the host's
    /// alias, since a container's loopback is its own.
    pub fn translate(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (host, inside) in &self.0 {
            let h = host.to_string_lossy();
            if !h.is_empty() && out.contains(h.as_ref()) {
                out = out.replace(h.as_ref(), inside);
            }
        }
        for lo in ["://127.0.0.1", "://localhost", "://[::1]"] {
            if out.contains(lo) {
                out = out.replace(lo, &format!("://{HOST_ALIAS}"));
            }
        }
        out
    }

    /// A container's path as the host has it, when it is under a mount.
    pub fn to_host(&self, path: &str) -> Option<PathBuf> {
        self.0
            .iter()
            .filter_map(|(host, inside)| {
                let rest = path.strip_prefix(inside.as_str())?;
                (rest.is_empty() || rest.starts_with('/')).then(|| (inside.len(), host.join(rest.trim_start_matches('/'))))
            })
            .max_by_key(|(n, _)| *n)
            .map(|(_, p)| p)
    }

    /// The mounts (`-v host:inside`).
    pub fn mounts(&self) -> Vec<String> {
        self.0.iter().map(|(h, i)| format!("{}:{i}", h.to_string_lossy())).collect()
    }
}

/// A service, started.
#[derive(Debug, Clone)]
pub struct Service {
    pub key: String,
    pub id: String,
    /// Container port → host port.
    pub ports: BTreeMap<String, String>,
}

/// The job container, started.
#[derive(Debug, Clone)]
pub struct JobContainer {
    pub id: String,
    /// `uid:gid` its steps run as, when not the image's own user.
    pub user: Option<String>,
    /// `HOME` for its steps.
    pub home: String,
    /// The image's `PATH`, to which `GITHUB_PATH` entries are prepended.
    pub path: String,
    pub has_bash: bool,
    pub map: PathMap,
}

/// A leg's containers.
#[derive(Debug, Clone)]
pub struct Containers {
    pub cli: Cli,
    pub network: String,
    pub label: String,
    pub job: Option<JobContainer>,
    pub services: Vec<Service>,
}

fn s(x: &str) -> String {
    x.to_owned()
}

impl Containers {
    /// Starts the leg's network, its services (awaited until healthy) and its job container.
    /// `say` reports progress for the leg's "Initialize containers" log.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(cli: Cli, leg: &str, map: PathMap, workdir: &str, job: Option<Spec>, services: Vec<(String, Spec)>, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<Containers> {
        // The whole leg id (ids made in the same instant share their start) and a suffix of this
        // start's own, so a retry never meets what an earlier attempt left.
        let start = crate::util::new_id();
        let short = format!("{leg}_{}", start.get(start.len().saturating_sub(6)..).unwrap_or(&start));
        let label = format!("kiln.leg={leg}");
        let shared = host_network();
        let network = if shared { s("host") } else { format!("kiln_{short}") };
        let none = BTreeMap::new();
        if !shared {
            cli.run(&[s("network"), s("create"), s("--label"), label.clone(), network.clone()], &none, QUICK).await?;
        }
        let mut c = Containers { cli, network, label, job: None, services: Vec::new() };
        for spec in services.iter().map(|(_, x)| x).chain(job.iter()) {
            if let Some((u, p)) = &spec.credentials {
                c.cli.login(&spec.image, u, p)?;
            }
        }
        // Services first, so the job container can reach them by name.
        for (key, spec) in &services {
            if spec.image.is_empty() {
                continue;
            }
            say(format!("Starting service {key} ({})", spec.image));
            let image = c.prepare(&spec.image, None, say).await?;
            let mut args = vec![s("create"), s("--name"), format!("kiln_{short}_{key}"), s("--label"), c.label.clone(), s("--network"), c.network.clone()];
            if !shared {
                args.extend([s("--network-alias"), key.clone()]);
                for p in &spec.ports {
                    args.extend([s("-p"), p.clone()]);
                }
            }
            let env: BTreeMap<String, String> = spec.env.iter().cloned().collect();
            for k in env.keys() {
                args.extend([s("-e"), k.clone()]);
            }
            for v in &spec.volumes {
                args.extend([s("-v"), v.clone()]);
            }
            args.extend(spec.options.as_deref().map(split_options).unwrap_or_default());
            args.push(image);
            let id = c.cli.run(&args, &env, QUICK).await?;
            c.cli.run(&[s("start"), id.clone()], &none, QUICK).await?;
            c.services.push(Service { key: key.clone(), id, ports: BTreeMap::new() });
        }
        if let Some(spec) = &job {
            say(format!("Starting job container ({})", spec.image));
            // A rootless engine's root is the host user already.
            let user = match spec.user {
                Some(u) if u.0 != 0 && !c.rootless().await => Some(u),
                _ => None,
            };
            let image = c.prepare(&spec.image, user, say).await?;
            let mut args = vec![s("create"), s("--name"), format!("kiln_{short}_job"), s("--label"), c.label.clone(), s("--workdir"), workdir.to_owned(), s("--network"), c.network.clone(), s("--add-host"), host_alias()];
            if shared {
                // Services by name, as on a network of the leg's own.
                for svc in &c.services {
                    args.extend([s("--add-host"), format!("{}:127.0.0.1", svc.key)]);
                }
            }
            if spec.docker_socket && Path::new("/var/run/docker.sock").exists() {
                args.extend([s("-v"), s("/var/run/docker.sock:/var/run/docker.sock")]);
                // The runner user reaches it through the socket's group, as the host's user does.
                if user.is_some()
                    && let Some(gid) = socket_group()
                {
                    args.extend([s("--group-add"), gid.to_string()]);
                }
            }
            for m in map.mounts() {
                args.extend([s("-v"), m]);
            }
            if !shared {
                for p in &spec.ports {
                    args.extend([s("-p"), p.clone()]);
                }
            }
            let env: BTreeMap<String, String> = spec.env.iter().cloned().collect();
            for k in env.keys() {
                args.extend([s("-e"), k.clone()]);
            }
            for v in &spec.volumes {
                args.extend([s("-v"), v.clone()]);
            }
            args.extend(spec.options.as_deref().map(split_options).unwrap_or_default());
            args.extend([s("--entrypoint"), s("tail"), image.clone(), s("-f"), s("/dev/null")]);
            let id = c.cli.run(&args, &env, QUICK).await?;
            c.cli.run(&[s("start"), id.clone()], &none, QUICK).await?;
            let path = c.image_path(&image).await.unwrap_or_else(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            let has_bash = c.cli.run(&[s("exec"), id.clone(), s("sh"), s("-c"), s("command -v bash")], &none, QUICK).await.is_ok_and(|o| !o.is_empty());
            let (user, home) = match user {
                Some((uid, gid)) => (Some(format!("{uid}:{gid}")), "/home/runner".to_owned()),
                None => (None, "/github/home".to_owned()),
            };
            c.job = Some(JobContainer { id, user, home, path, has_bash, map });
        }
        // Health checks, then the ports the host got.
        let ids: Vec<(String, String)> = c.services.iter().map(|x| (x.key.clone(), x.id.clone())).chain(c.job.iter().map(|j| ("job container".to_owned(), j.id.clone()))).collect();
        for (key, id) in ids {
            c.healthy(&key, &id, say).await?;
        }
        for i in 0..c.services.len() {
            let id = c.services.get(i).map(|x| x.id.clone()).unwrap_or_default();
            // On the host's network a service listens on its own ports.
            let ports = if shared {
                let key = c.services.get(i).map(|x| x.key.clone()).unwrap_or_default();
                let spec = services.iter().find(|(k, _)| *k == key).map(|(_, x)| x.ports.clone()).unwrap_or_default();
                spec.iter().filter_map(|p| p.rsplit(':').next()).map(|p| p.split('/').next().unwrap_or(p)).filter(|p| !p.is_empty()).map(|p| (p.to_owned(), p.to_owned())).collect()
            } else {
                c.ports(&id).await
            };
            if let Some(x) = c.services.get_mut(i) {
                x.ports = ports;
            }
        }
        Ok(c)
    }

    /// Whether the engine runs rootless.
    async fn rootless(&self) -> bool {
        let none = BTreeMap::new();
        if self.cli.bin.ends_with("podman") {
            return self.cli.run(&[s("info"), s("--format"), s("{{.Host.Security.Rootless}}")], &none, QUICK).await.is_ok_and(|o| o == "true");
        }
        self.cli.run(&[s("info"), s("--format"), s("{{json .SecurityOptions}}")], &none, QUICK).await.is_ok_and(|o| o.contains("rootless"))
    }

    /// The image to run `image` as: kiln's runner images are built here, tagged by their
    /// Dockerfile's digest (so per version and user, and a changed one is built again); others
    /// are pulled.
    async fn prepare(&self, image: &str, user: Option<(u32, u32)>, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<String> {
        if let Some(dockerfile) = runner_dockerfile(image, user) {
            let _g = IMAGE_BUILD.lock().await;
            let digest = crate::util::sha256_hex(dockerfile.as_bytes());
            let tag = format!("{image}:{}", digest.get(..16).unwrap_or(&digest));
            if self.cli.run(&[s("image"), s("inspect"), s("--format"), s("{{.Id}}"), tag.clone()], &BTreeMap::new(), QUICK).await.is_ok() {
                return Ok(tag);
            }
            let owner = user.map_or_else(|| "0".to_owned(), |(uid, _)| uid.to_string());
            let dockerfile = format!("{dockerfile}LABEL kiln.runner-image={image} kiln.runner-user={owner}\n");
            say(format!("Building kiln's runner image {tag} (once per machine and version)"));
            let dir = self.cli.config.join(format!("image-{}", crate::util::new_id()));
            let d = dir.clone();
            crate::rt::blocking(move || -> std::io::Result<()> {
                std::fs::create_dir_all(&d)?;
                std::fs::write(d.join("Dockerfile"), dockerfile)
            })
            .await??;
            let r = self.cli.run(&[s("build"), s("--quiet"), s("-t"), tag.clone(), dir.to_string_lossy().to_string()], &BTreeMap::new(), SLOW).await;
            let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            if r.is_ok() {
                self.forget_older(image, &owner, &tag).await;
            }
            return r.map(|_| tag);
        }
        say(format!("{} pull {image}", self.cli.bin));
        self.cli.run(&[s("pull"), s("--quiet"), image.to_owned()], &BTreeMap::new(), SLOW).await?;
        Ok(image.to_owned())
    }

    /// Removes this user's earlier builds of runner image `image`, all but `keep`; one a
    /// container still uses stays until a later build.
    async fn forget_older(&self, image: &str, owner: &str, keep: &str) {
        let none = BTreeMap::new();
        let filters = [s("--filter"), format!("label=kiln.runner-image={image}"), s("--filter"), format!("label=kiln.runner-user={owner}")];
        let mut args = vec![s("image"), s("ls")];
        args.extend(filters);
        args.extend([s("--format"), s("{{.Repository}}:{{.Tag}}")]);
        let Ok(out) = self.cli.run(&args, &none, QUICK).await else { return };
        let old: Vec<String> = out.lines().map(str::trim).filter(|t| !t.is_empty() && *t != keep && !t.ends_with(":<none>")).map(str::to_owned).collect();
        for t in old {
            if self.cli.run(&[s("image"), s("rm"), t.clone()], &none, QUICK).await.is_ok() {
                tracing::info!(image = %t, "removed an older build of a runner image");
            }
        }
    }

    async fn image_path(&self, image: &str) -> Option<String> {
        let out = self.cli.run(&[s("image"), s("inspect"), s("--format"), s("{{json .Config.Env}}"), image.to_owned()], &BTreeMap::new(), QUICK).await.ok()?;
        let env: Vec<String> = serde_json::from_str(&out).ok()?;
        env.iter().find_map(|e| e.strip_prefix("PATH=")).map(str::to_owned)
    }

    /// Waits for a container's health check to pass, when it has one.
    async fn healthy(&self, key: &str, id: &str, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<()> {
        let fmt = s("{{if .State.Health}}{{.State.Health.Status}}{{end}}");
        let start = std::time::Instant::now();
        let mut wait = Duration::from_secs(1);
        loop {
            let status = self.cli.run(&[s("inspect"), s("--format"), fmt.clone(), id.to_owned()], &BTreeMap::new(), QUICK).await.unwrap_or_default();
            match status.as_str() {
                "" => return Ok(()),
                "healthy" => {
                    say(format!("{key} is healthy"));
                    return Ok(());
                }
                "unhealthy" => anyhow::bail!("{key} is unhealthy"),
                _ => {}
            }
            if start.elapsed() > HEALTHY_WITHIN {
                anyhow::bail!("{key} wasn't healthy within {} minutes", HEALTHY_WITHIN.as_secs() / 60);
            }
            say(format!("{key} is {status}; waiting {} s", wait.as_secs()));
            crate::rt::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(16));
        }
    }

    /// `docker port <id>`: container port to host port.
    async fn ports(&self, id: &str) -> BTreeMap<String, String> {
        let out = self.cli.run(&[s("port"), id.to_owned()], &BTreeMap::new(), QUICK).await.unwrap_or_default();
        parse_ports(&out)
    }

    /// `job.container` and `job.services` (GitHub's contexts).
    pub fn contexts(&self) -> (serde_json::Value, serde_json::Value) {
        let job = self.job.as_ref().map_or(serde_json::Value::Null, |j| serde_json::json!({ "id": j.id, "network": self.network }));
        let services: serde_json::Map<String, serde_json::Value> = self.services.iter().map(|x| (x.key.clone(), serde_json::json!({ "id": x.id, "network": self.network, "ports": x.ports }))).collect();
        (job, serde_json::Value::Object(services))
    }

    /// Removes everything the leg started.
    pub async fn stop(&self) {
        let none = BTreeMap::new();
        let ids = self.cli.run(&[s("ps"), s("-aq"), s("--filter"), format!("label={}", self.label)], &none, QUICK).await.unwrap_or_default();
        let ids: Vec<String> = ids.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect();
        if !ids.is_empty() {
            let mut args = vec![s("rm"), s("--force")];
            args.extend(ids);
            if let Err(e) = self.cli.run(&args, &none, QUICK).await {
                tracing::warn!(error = %format!("{e:#}"), "the leg's containers could not be removed");
            }
        }
        if self.network != "host"
            && let Err(e) = self.cli.run(&[s("network"), s("rm"), self.network.clone()], &none, QUICK).await
        {
            tracing::warn!(error = %format!("{e:#}"), "the leg's network could not be removed");
        }
    }
}

/// Lines of `docker port`: `5432/tcp -> 0.0.0.0:49153` → `5432` → `49153` (the first host port).
pub fn parse_ports(out: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in out.lines() {
        let Some((inside, host)) = line.split_once("->") else { continue };
        let port = inside.trim().split('/').next().unwrap_or_default().to_owned();
        let host_port = host.trim().rsplit(':').next().unwrap_or_default().to_owned();
        if !port.is_empty() && !host_port.is_empty() {
            m.entry(port).or_insert(host_port);
        }
    }
    m
}

/// A Docker action's image (W07-R06): built from the action's Dockerfile, tagged by its commit
/// and kept; or pulled for `docker://…`.
pub async fn action_image(cli: &Cli, dir: &Path, image: &str, tag: &str, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<String> {
    if let Some(i) = image.strip_prefix("docker://") {
        say(format!("{} pull {i}", cli.bin));
        cli.run(&[s("pull"), s("--quiet"), i.to_owned()], &BTreeMap::new(), SLOW).await?;
        return Ok(i.to_owned());
    }
    let none = BTreeMap::new();
    if cli.run(&[s("image"), s("inspect"), tag.to_owned()], &none, QUICK).await.is_ok() {
        return Ok(tag.to_owned());
    }
    let dockerfile = dir.join(image);
    let context = dockerfile.parent().map_or_else(|| dir.to_path_buf(), Path::to_path_buf);
    say(format!("{} build -t {tag} -f {} {}", cli.bin, dockerfile.display(), context.display()));
    cli.run(&[s("build"), s("--quiet"), s("-t"), tag.to_owned(), s("-f"), dockerfile.to_string_lossy().to_string(), context.to_string_lossy().to_string()], &none, SLOW).await?;
    Ok(tag.to_owned())
}

/// `docker run` for a Docker action's step, with GitHub's mounts: its arguments (the values
/// come by name from `env`).
#[allow(clippy::too_many_arguments)]
pub fn action_run_args(name: &str, label: &str, network: Option<&str>, mounts: &[String], env: &BTreeMap<String, String>, entrypoint: Option<&str>, image: &str, args: &[String]) -> Vec<String> {
    let mut a = vec![s("run"), s("--rm"), s("--name"), name.to_owned(), s("--label"), label.to_owned(), s("--workdir"), s("/github/workspace"), s("--add-host"), host_alias()];
    if let Some(n) = network {
        a.extend([s("--network"), n.to_owned()]);
    } else if host_network() {
        a.extend([s("--network"), s("host")]);
    }
    if Path::new("/var/run/docker.sock").exists() {
        a.extend([s("-v"), s("/var/run/docker.sock:/var/run/docker.sock")]);
    }
    for m in mounts {
        a.extend([s("-v"), m.clone()]);
    }
    for k in env.keys() {
        a.extend([s("-e"), k.clone()]);
    }
    if let Some(e) = entrypoint.filter(|e| !e.is_empty()) {
        a.extend([s("--entrypoint"), e.to_owned()]);
    }
    a.push(image.to_owned());
    a.extend(args.iter().cloned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_images_have_a_dockerfile() {
        let d = runner_dockerfile("kiln/runner-ubuntu-24.04-slim", None).unwrap_or_default();
        assert!(d.starts_with("FROM ubuntu:24.04\n"), "{d}");
        assert!(d.contains("apt-get install -y --no-install-recommends ca-certificates"), "{d}");
        assert!(d.contains(" \\\n    build-essential"), "line continuations: {d}");
        assert!(d.trim_end().ends_with("rm -rf /var/lib/apt/lists/*"), "{d}");
        assert!(d.contains(" ImageOS=ubuntu24\n"), "{d}");
        // The full image: GitHub's toolchains that workflows call without a setup action.
        let full = runner_dockerfile("kiln/runner-ubuntu-24.04", None).unwrap_or_default();
        assert!(full.starts_with(&d), "the full image starts as the slim one");
        for tool in ["cmake", "clang", "maven", "ant", "sbt", "composer", "ruby-full", "ruby-bundler", "leiningen", "sh.rustup.rs", "nodejs.org/dist/latest-v22.x", "JAVA_HOME=", "Assume-Yes", "docker-buildx", "docker-compose"] {
            assert!(full.contains(tool) && !d.contains(tool), "{tool}");
        }
        assert!(full.lines().filter(|l| l.ends_with('\\')).all(|l| l.ends_with(" \\")), "continuations: {full}");
        assert!(runner_dockerfile("kiln/runner-ubuntu-22.04", None).unwrap_or_default().contains(" ImageOS=ubuntu22\n"));
        assert!(runner_dockerfile("ubuntu:24.04", None).is_none());
        assert!(runner_dockerfile("kiln/runner-centos", None).is_none());
        // The host's user, with sudo; root stays root.
        let u = runner_dockerfile("kiln/runner-ubuntu-24.04", Some((1000, 1000))).unwrap_or_default();
        assert!(u.contains("useradd -m -u 1000 -g 1000 -s /bin/bash runner") && u.contains("NOPASSWD:ALL"), "{u}");
        assert!(u.trim_end().ends_with("chown -R 1000:1000 /usr/local/cargo /usr/local/rustup /usr/local/lib/node_modules /usr/local/bin /usr/share/dotnet"), "{u}");
        assert_eq!(runner_dockerfile("kiln/runner-ubuntu-24.04", Some((0, 0))), runner_dockerfile("kiln/runner-ubuntu-24.04", None));
    }

    #[test]
    fn container_paths_map_back_to_the_host() {
        let m = PathMap::new(vec![(PathBuf::from("/w/leg/repo"), "/__w/repo/repo".into()), (PathBuf::from("/w/leg/_temp"), "/__w/_temp".into()), (PathBuf::from("/w/_tool"), "/opt/hostedtoolcache".into())]);
        assert_eq!(m.to_host("/__w/repo/repo/src/a.ts"), Some(PathBuf::from("/w/leg/repo/src/a.ts")));
        assert_eq!(m.to_host("/__w/_temp"), Some(PathBuf::from("/w/leg/_temp")));
        assert_eq!(m.to_host("/opt/hostedtoolcache/node/x"), Some(PathBuf::from("/w/_tool/node/x")));
        assert_eq!(m.to_host("/__w/repo/repository"), None, "a prefix of a name is not its directory");
        assert_eq!(m.to_host("/etc/passwd"), None);
    }

    #[test]
    fn options_split_like_a_shell() {
        assert_eq!(split_options("--health-cmd \"pg_isready -U postgres\" --health-interval 10s"), vec!["--health-cmd", "pg_isready -U postgres", "--health-interval", "10s"]);
        assert_eq!(split_options("--cpus 1 -e 'A=b c'"), vec!["--cpus", "1", "-e", "A=b c"]);
        assert_eq!(split_options("--x ''"), vec!["--x", ""]);
        assert!(split_options("  ").is_empty());
    }

    #[test]
    fn ports_paths_and_registries() {
        let m = parse_ports("5432/tcp -> 0.0.0.0:49153\n5432/tcp -> [::]:49153\n6379/tcp -> 0.0.0.0:32768\n");
        assert_eq!(m.get("5432").map(String::as_str), Some("49153"));
        assert_eq!(m.get("6379").map(String::as_str), Some("32768"));
        let pm = PathMap::new(vec![(PathBuf::from("/w/abc/_temp"), "/__w/_temp".into()), (PathBuf::from("/w/abc/app"), "/__w/app/app".into()), (PathBuf::from("/w/abc/_temp/_github_home"), "/github/home".into())]);
        assert_eq!(pm.translate("/w/abc/_temp/_github_home/.npmrc"), "/github/home/.npmrc");
        assert_eq!(pm.translate("/w/abc/_temp/x.sh"), "/__w/_temp/x.sh");
        assert_eq!(pm.translate("cd /w/abc/app/src"), "cd /__w/app/app/src");
        assert_eq!(pm.translate("http://127.0.0.1:7070/api/v3"), "http://host.docker.internal:7070/api/v3");
        assert_eq!(pm.translate("http://localhost:7070/"), "http://host.docker.internal:7070/");
        assert_eq!(registry("postgres:16"), "docker.io");
        assert_eq!(registry("ghcr.io/acme/app:1"), "ghcr.io");
        assert_eq!(registry("localhost:5000/x"), "localhost:5000");
        assert_eq!(registry("acme/app"), "docker.io");
    }
}
