//! `kiln install --on kubernetes|docker`: a whole kiln (its server, UI and stores, and a runner of
//! its own) in a cluster or a container, from this binary or an image given (docs/p2p.md,
//! "Installing"); and, for every target, the check an install ends with.
//!
//! - **The image.** `--image` names one; else one is built with this machine's Docker, as kiln's
//!   own image is (`Dockerfile`), for the CPU of the nodes (or Docker) it runs on: from this binary
//!   when it's a Linux kiln for that CPU, on Alpine for a static kiln (the released ones are), else
//!   on this machine's own system, whose C library the binary was linked against; from any other
//!   kiln (a Mac's, Windows') or for another CPU, from the static Linux build a kiln server has
//!   for it (`--server`'s, else the one this machine's runner is enrolled with).
//! - **Into a cluster.** minikube, kind and k3d take the image from this machine's Docker (an image
//!   `--image` names too, when that Docker has it); Docker Desktop's cluster (its kind nodes pull
//!   from its image store through its mirror) and OrbStack's share it; any other cluster pulls it
//!   from a registry, pushed there first: `--registry`, else the one the cluster names
//!   (`local-registry-hosting` in kube-public, as kind, minikube and k3d document theirs).
//! - **Kubernetes.** In `--namespace` (`kiln`): a service account with the role kiln's updates use
//!   (its own Deployment, its pods), the admin token as a secret, a NodePort service, the config, a
//!   volume and one Deployment (three nodes come with docs/p2p.md's P3). It's opened at a node's
//!   address and the service's port, or through a port forward when no node can be reached.
//! - **Docker.** A container (`kiln`, or `--name`), started again by Docker, its data in a volume
//!   (`kiln-data`), on `--port` and the next port for QUIC.
//! - **Done means it works.** An install ends once the kiln answers, its UI's page and script load
//!   and its runner is connected, and says where to open it; else it says what failed and takes
//!   back what it made.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use bytes::Bytes;
use http::Method;
use kiln_cd::obj::{self, GroupKind, ResKey};
use serde_json::{Value, json};

use crate::cd::kube::Cluster;
use crate::install::{InstallArgs, say};

/// How long a kiln has to start, answer, load its UI and connect its runner.
pub const READY_TIME: Duration = Duration::from_secs(300);
/// How long kiln's pod may wait for a node to take it.
const UNPLACED_TIME: Duration = Duration::from_secs(90);
/// The field manager of what the installer applies.
const MANAGER: &str = "kiln-install";
/// The name of what an install makes, and of its runner.
const APP: &str = "kiln";
const SELECTOR: &str = "app.kubernetes.io/name=kiln";
/// What a cluster install makes in its namespace, by group, kind and name: each must be new, so
/// taking an install back takes back only what it made.
const OBJECTS: [(&str, &str, &str); 8] = [
    ("", "ServiceAccount", APP),
    ("rbac.authorization.k8s.io", "Role", APP),
    ("rbac.authorization.k8s.io", "RoleBinding", APP),
    ("", "Secret", "kiln-admin"),
    ("", "Service", APP),
    ("", "ConfigMap", "kiln-config"),
    ("", "PersistentVolumeClaim", "kiln-data"),
    ("apps", "Deployment", APP),
];

// ------------------------------------------------------------------------------ the check

/// Where the check asks a kiln: at its address, or through its cluster's API server (the service's
/// proxy) when none of the cluster's addresses can be reached from here.
enum Via<'a> {
    Url(&'a crate::http::client::Client, String),
    Proxy(&'a Cluster, String),
}

impl Via<'_> {
    async fn get(&self, path: &str) -> anyhow::Result<(u16, Bytes)> {
        match self {
            Via::Url(http, url) => {
                let r = http.get(&format!("{url}{path}")).timeout(Duration::from_secs(10)).send().await?;
                let status = r.status();
                Ok((status, r.bytes(8 << 20).await?))
            }
            // As a browser asks: kiln serves its page to no one who takes JSON alone.
            Via::Proxy(cluster, base) => cluster.request_as(Method::GET, &format!("{base}{path}"), None, None, "*/*").await,
        }
    }
}

/// Whether a kiln is whole: it answers, its UI's page and the script the page starts with load,
/// and a runner is connected to it (`/healthz/server`, open as `/healthz` is): how many are.
async fn check(via: &Via<'_>) -> anyhow::Result<u64> {
    let (status, _) = via.get("/healthz").await.context("it doesn't answer")?;
    anyhow::ensure!(status == 200, "it answers /healthz with {status}");
    let (status, page) = via.get("/").await.context("its UI doesn't answer")?;
    anyhow::ensure!(status == 200, "its UI's page answers with {status}");
    let page = String::from_utf8_lossy(&page);
    // The path from `/assets/` on: an API server's proxy rewrites a page's links to go through it.
    let script = page
        .split("src=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .filter(|s| s.ends_with(".js"))
        .find_map(|s| s.find("/assets/").and_then(|at| s.get(at..)))
        .context("its UI's page names no script: this kiln was built without its UI")?;
    let (status, _) = via.get(script).await.context("its UI's script doesn't load")?;
    anyhow::ensure!(status == 200, "its UI's script {script} answers with {status}");
    let (status, health) = via.get("/healthz/server").await?;
    anyhow::ensure!(status == 200, "/healthz/server answers with {status}");
    let health: Value = serde_json::from_slice(&health).context("reading /healthz/server")?;
    let runners = health.pointer("/runners/connected").and_then(Value::as_u64).unwrap_or(0);
    anyhow::ensure!(runners > 0, "it answers and its UI loads, but its runner isn't connected");
    Ok(runners)
}

/// Waits until [`check`] passes, for at most `within`: else what the kiln lacked when time ran out.
async fn verify(via: &Via<'_>, within: Duration) -> anyhow::Result<u64> {
    let start = Instant::now();
    loop {
        let e = match check(via).await {
            Ok(runners) => return Ok(runners),
            Err(e) => e,
        };
        if start.elapsed() >= within {
            return Err(e.context(format!("not ready after {} s", within.as_secs())));
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

/// The check at `url`, for at most `within`: how many runners are connected.
pub async fn verify_url(url: &str, within: Duration) -> anyhow::Result<u64> {
    let http = client()?;
    verify(&Via::Url(&http, url.trim_end_matches('/').to_owned()), within).await
}

fn client() -> anyhow::Result<crate::http::client::Client> {
    crate::http::client::Client::new(crate::http::client::Roots::WebPki, None)
}

/// Says an installed kiln is ready, where to open it, its login link, and `later`: the command
/// that makes a new link.
pub fn opened(url: &str, token: &str, runners: u64, later: &str) {
    let more = if runners > 1 { format!(" ({runners} runners)") } else { String::new() };
    say(format!("ready       it answers, its UI loads and its runner is connected{more}"));
    say("");
    say(format!("UI          {url}"));
    say(format!("login link  {} (valid 5 min)", crate::api::otp_url(token, url)));
    say(format!("            a new one: {later}"));
}

/// Keeps a kiln installed elsewhere (a container, a cluster) where `kiln login` and `kiln token`
/// look for one (`access::cluster_credentials_path`), unless that names another kiln already:
/// whether it's kept there.
fn remember(url: &str, token: &str) -> bool {
    let path = crate::access::cluster_credentials_path();
    if let Ok(text) = std::fs::read_to_string(&path)
        && !text.lines().any(|l| l.trim().strip_prefix("KILN_URL=").is_some_and(|u| u.trim_matches('"') == url))
    {
        return false;
    }
    let body = format!("# The kiln `kiln install` set up last; `kiln login` and `kiln token` reach it with these.\nKILN_URL={url}\nKILN_ADMIN_TOKEN={token}\n");
    crate::util::write_secret(&path, body.as_bytes(), 0o600).is_ok()
}

/// The command that makes a new login link for the kiln at `url`: `kiln login` once it's
/// remembered, else one given `token_from`, a shell command printing its admin token.
fn later(url: &str, token: &str, token_from: &str) -> String {
    if remember(url, token) { "kiln login".to_owned() } else { format!("kiln login --url {url} --token \"$({token_from})\"") }
}

// ------------------------------------------------------------------------------ the image

/// Whether the ELF file at `path` names a program interpreter: a binary linked dynamically, which
/// runs only where its C library is (a static one runs on any Linux, Alpine's included).
fn dynamic(path: &Path) -> anyhow::Result<bool> {
    let mut b = Vec::new();
    std::fs::File::open(path)?.take(1 << 16).read_to_end(&mut b)?;
    let at = |o: usize, n: usize| b.get(o..o + n).context("not an ELF file this installer reads");
    anyhow::ensure!(at(0, 4)? == b"\x7fELF" && at(4, 1)? == [2], "not a 64-bit ELF file");
    let little = at(5, 1)? == [1];
    let u = |o: usize, n: usize| -> anyhow::Result<usize> {
        let s = at(o, n)?;
        Ok(s.iter().enumerate().fold(0usize, |acc, (i, x)| acc | (usize::from(*x) << (8 * if little { i } else { n - 1 - i }))))
    };
    let (phoff, phentsize, phnum) = (u(32, 8)?, u(54, 2)?, u(56, 2)?);
    const PT_INTERP: usize = 3;
    for i in 0..phnum {
        if u(phoff + i * phentsize, 4)? == PT_INTERP {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The image's first lines: a system with what a runner's jobs use (git, bash, ssh) and a `kiln`
/// user (uid 10001). Alpine for a static kiln; else this machine's own system (`os_release`, its
/// /etc/os-release), whose C library a dynamic one was linked against.
fn base(dynamic: bool, os_release: &str) -> anyhow::Result<String> {
    if !dynamic {
        return Ok("FROM alpine:3.24\nRUN set -eux; apk add --no-cache git bash openssh-client ca-certificates tzdata curl less ncurses-terminfo-base; \\\n    \
                   addgroup -S -g 10001 kiln; adduser -S -D -H -u 10001 -G kiln -h /var/lib/kiln -s /bin/bash kiln\n"
            .to_owned());
    }
    let field = |k: &str| os_release.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(|v| v.trim().trim_matches('"').to_owned()).unwrap_or_default();
    let (id, version, like) = (field("ID"), field("VERSION_ID"), field("ID_LIKE"));
    const APT: &str = "apt-get update; apt-get install -y --no-install-recommends git bash openssh-client ca-certificates tzdata curl less; rm -rf /var/lib/apt/lists/*";
    let (from, packages) = match id.as_str() {
        "debian" if !version.is_empty() => (format!("debian:{version}-slim"), APT),
        "ubuntu" if !version.is_empty() => (format!("ubuntu:{version}"), APT),
        "fedora" if !version.is_empty() => {
            (format!("fedora:{version}"), "dnf install -y --setopt=install_weak_deps=False git-core bash openssh-clients ca-certificates tzdata curl less shadow-utils; dnf clean all")
        }
        _ if id == "arch" || like.split_whitespace().any(|l| l == "arch") => {
            ("archlinux:latest".to_owned(), "pacman -Syu --noconfirm --needed git bash openssh ca-certificates tzdata curl less; pacman -Scc --noconfirm")
        }
        _ => bail!("this kiln is linked dynamically, and there's no image of this machine's system ({id}) to run it on: give --image, or install a released kiln (they're static)"),
    };
    Ok(format!("FROM {from}\nRUN set -eux; {packages}; \\\n    groupadd -g 10001 kiln; useradd -u 10001 -g kiln -d /var/lib/kiln -M -s /bin/bash kiln\n"))
}

/// The image's build file: `base`, then this binary run as kiln's own image runs it.
fn dockerfile(base: &str) -> String {
    format!(
        "{base}RUN mkdir -p /etc/kiln /var/lib/kiln && chown 10001:10001 /var/lib/kiln\n\
         COPY kiln /usr/local/bin/kiln\nRUN ln -s /usr/local/bin/kiln /kiln\nCOPY kiln.toml /etc/kiln/kiln.toml\n\
         ENV KILN_CONFIG=/etc/kiln/kiln.toml KILN_DATA_DIR=/var/lib/kiln HOME=/var/lib/kiln\nUSER 10001:10001\nWORKDIR /var/lib/kiln\n\
         EXPOSE 7070/tcp 7071/udp\nENTRYPOINT [\"/kiln\", \"supervise\", \"--\"]\nCMD [\"server\"]\n"
    )
}

/// The config an installed kiln starts with: listening on every address, its data in
/// /var/lib/kiln, its own runner on and named `kiln` (a pod's or container's host name changes),
/// and `server`'s lines (its address, its admin token's file).
fn config(server: &str) -> String {
    format!(
        "# kiln, as `kiln install` set it up. Every key is optional; see docs/configuration.md.\n\n\
         [server]\nhttp_addr = \"0.0.0.0:7070\"\nquic_addr = \"0.0.0.0:7071\"\ndata_dir = \"/var/lib/kiln\"\n{server}\n\
         [local_agent]\nenabled = true\nname = \"{APP}\"\n\n[kubernetes]\nenabled = false\n"
    )
}

/// The tag of an image built from this binary: `kiln:` and its build id.
fn own_tag() -> Option<String> {
    let id = crate::updates::own_build();
    (id != "unknown").then(|| format!("{APP}:{id}"))
}

/// Docker's and Kubernetes' word for a CPU Rust names (`x86_64` is `amd64`, `aarch64` `arm64`).
fn docker_arch(cpu: &str) -> &str {
    match cpu {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Whether a kubeconfig names a cluster here: its current context (where `kiln install` goes by
/// default).
pub fn has_cluster() -> bool {
    context_name(None).is_ok()
}

/// An image of kiln for Linux on `arch` CPUs (`amd64`, `arm64`), built with this machine's Docker
/// unless it's built already: from this binary when it's a Linux kiln for them, else from the
/// static build a kiln server has ([`fetch_linux_kiln`]). Its tag, and whether it was built now.
async fn kiln_image(a: &InstallArgs, arch: &str) -> anyhow::Result<(String, bool)> {
    if cfg!(target_os = "linux") && arch == docker_arch(std::env::consts::ARCH) {
        let tag = own_tag().context("this binary's build can't be read to tag an image of it: give one with --image")?;
        let built = build_image(&tag, &std::env::current_exe()?, None)?;
        return Ok((tag, built));
    }
    let dir = std::env::temp_dir().join(format!("kiln-linux-{}", crate::util::new_id()));
    std::fs::create_dir_all(&dir)?;
    let made = async {
        let exe = fetch_linux_kiln(a, arch, &dir).await?;
        let (hash, _) = crate::updates::hash_file(&exe)?;
        let tag = format!("{APP}:{}", crate::updates::build_id(&hash));
        let built = build_image(&tag, &exe, Some(&format!("linux/{arch}")))?;
        anyhow::Ok((tag, built))
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    made
}

/// The static Linux kiln for `arch` CPUs (`amd64`, `arm64`), fetched into `dir` from a kiln server
/// (its `/dl/kiln/<target>`, where every machine's kiln comes from): `--server`'s, else the one this
/// machine's runner is enrolled with. Its path.
async fn fetch_linux_kiln(a: &InstallArgs, arch: &str, dir: &Path) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write as _;
    let cpu = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => bail!("kiln has no Linux build for {other} CPUs: give --image"),
    };
    let target = format!("{cpu}-unknown-linux-musl");
    let server =
        a.server.clone().filter(|s| !s.trim().is_empty()).or_else(|| crate::upgrade::find_existing(&a.service).and_then(|e| crate::upgrade::arg_value(&e.args, "--server"))).with_context(|| {
            format!(
                "its image needs a Linux kiln for {arch} CPUs, which this one isn't, and no kiln server is known here to fetch one from: give --server (a kiln that has its {target} build), or --image"
            )
        })?;
    let mut url = format!("{}/dl/kiln/{target}", server.trim_end_matches('/'));
    say(format!("binary      kiln for {target}, from {url}"));
    let http = client()?;
    for _ in 0..5 {
        let mut resp = http.get(&url).timeout(Duration::from_secs(900)).send().await.with_context(|| format!("fetching {url}"))?;
        if (300..400).contains(&resp.status()) {
            let to = resp.header("location").with_context(|| format!("{url} answered {} going nowhere", resp.status()))?;
            url = if to.starts_with('/') { format!("{}{to}", origin_of(&url)) } else { to.to_owned() };
            continue;
        }
        anyhow::ensure!(resp.is_success(), "{url} answered {}: give --image, or --server (a kiln that has its {target} build)", resp.status());
        let path = dir.join("kiln");
        let mut file = std::fs::File::create(&path)?;
        let mut got = 0usize;
        while let Some(chunk) = resp.chunk().await? {
            got += chunk.len();
            anyhow::ensure!(got <= crate::updates::MAX_BINARY, "{url} sent more than a kiln binary's {} MiB", crate::updates::MAX_BINARY >> 20);
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        drop(file);
        anyhow::ensure!(!dynamic(&path).with_context(|| format!("{url} sent something else than kiln's {target} build"))?, "{url} sent a kiln linked dynamically, not the static {target} build");
        crate::os::make_executable(&path)?;
        return Ok(path);
    }
    bail!("fetching kiln's {target} build: too many redirects")
}

/// `scheme://host[:port]` of `url`.
fn origin_of(url: &str) -> &str {
    let after = url.find("://").map_or(0, |i| i + 3);
    url.get(after..).and_then(|rest| rest.find('/')).map_or(url, |slash| url.get(..after + slash).unwrap_or(url))
}

/// Builds the image `tag` from the Linux kiln `exe` with this machine's Docker (for `platform`,
/// `linux/<arch>`, when that's not Docker's own), unless it's built already: whether it built it.
fn build_image(tag: &str, exe: &Path, platform: Option<&str>) -> anyhow::Result<bool> {
    if !docker(&["image", "ls", "-q", tag])?.is_empty() {
        say(format!("image       {tag} (built from this kiln before)"));
        return Ok(false);
    }
    // A dynamic kiln is this machine's own (the fetched ones are static): its system is the base.
    let base = base(dynamic(exe).context("reading the kiln to build the image from")?, &std::fs::read_to_string("/etc/os-release").unwrap_or_default())?;
    let dir = std::env::temp_dir().join(format!("kiln-image-{}", crate::util::new_id()));
    std::fs::create_dir_all(&dir)?;
    let built = (|| {
        std::fs::copy(exe, dir.join("kiln"))?;
        std::fs::write(dir.join("Dockerfile"), dockerfile(&base))?;
        std::fs::write(dir.join("kiln.toml"), config(""))?;
        say(format!("image       building {tag} ({}{})", base.lines().next().unwrap_or("").trim_start_matches("FROM "), platform.map(|p| format!(", {p}")).unwrap_or_default()));
        let context = dir.to_string_lossy();
        let mut args = vec!["build", "-q", "--label", "kiln.installed=1", "-t", tag];
        if let Some(p) = platform {
            args.extend(["--platform", p]);
        }
        args.push(context.as_ref());
        docker(&args)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    built.map(|_| true)
}

/// Runs docker: what it printed; else what it said.
fn docker(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("docker").args(args).stdin(Stdio::null()).output().context("running docker (is it installed?)")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

// ------------------------------------------------------------------------------ Docker

/// A whole kiln in a container on this machine's Docker.
pub async fn docker_install(a: &InstallArgs) -> anyhow::Result<()> {
    docker(&["version", "--format", "{{.Server.Version}}"]).context("Docker isn't running here, or this user can't use it")?;
    let name = a.name.as_deref().unwrap_or(APP);
    let volume = format!("{name}-data");
    if !docker(&["ps", "-aq", "--filter", &format!("name=^{name}$")])?.is_empty() {
        // Another tool's container may have the name (minikube names its node after its profile).
        if docker(&["inspect", "--format", "{{index .Config.Labels \"kiln.installed\"}}", name]).is_ok_and(|l| l == "1") {
            bail!("kiln is installed in Docker here already (container {name}): `docker rm -f {name}` and `docker volume rm {volume}` remove it, or name another one with --name");
        }
        bail!("a container named {name} is here already, and kiln didn't install it: name kiln's another with --name");
    }
    let (image, built) = match &a.image {
        Some(image) => (image.clone(), false),
        None => kiln_image(a, &docker(&["version", "--format", "{{.Server.Arch}}"])?).await?,
    };
    let made_volume = docker(&["volume", "ls", "-q", "--filter", &format!("name=^{volume}$")])?.is_empty();
    let (http, quic) = (a.port, a.port.checked_add(1).context("--port can't be the last port: QUIC takes the next one")?);
    let url = format!("http://localhost:{http}");
    let undo = || {
        let _ = docker(&["rm", "-f", name]);
        if made_volume {
            let _ = docker(&["volume", "rm", &volume]);
        }
        if built {
            let _ = docker(&["image", "rm", &image]);
        }
    };
    let (ports, quic_ports, data, public) = (format!("{http}:7070"), format!("{quic}:7071/udp"), format!("{volume}:/var/lib/kiln"), format!("KILN_PUBLIC_URL={url}"));
    let run = ["run", "-d", "--name", name, "--hostname", name, "--restart", "unless-stopped", "-p", &ports, "-p", &quic_ports, "-v", &data, "-e", &public];
    if let Err(e) = docker(&[&run[..], &["--label", "kiln.installed=1", &image]].concat()) {
        undo();
        return Err(e.context("starting its container (nothing was left)"));
    }
    say(format!("container   {name} ({image}), its data in volume {volume}{}", if made_volume { "" } else { " (kept from before)" }));
    // Its admin token, as its server made it in its data.
    let mut token = String::new();
    let start = Instant::now();
    while token.is_empty() && start.elapsed() < Duration::from_secs(60) {
        token = docker(&["exec", name, "cat", "/var/lib/kiln/admin.token"]).unwrap_or_default();
        if token.is_empty() {
            crate::rt::sleep(Duration::from_secs(1)).await;
        }
    }
    let ready = match token.is_empty() {
        true => Err(anyhow::anyhow!("its admin token never appeared in its data")),
        false => verify_url(&url, READY_TIME).await,
    };
    match ready {
        Ok(runners) => {
            opened(&url, &token, runners, &later(&url, &token, &format!("docker exec {name} cat /var/lib/kiln/admin.token")));
            say(format!("container   docker logs {name}"));
            Ok(())
        }
        Err(e) => {
            let logs = docker(&["logs", "--tail", "30", name]).unwrap_or_default();
            undo();
            let what = match (made_volume, built) {
                (true, true) => "container, volume and image were",
                (true, false) => "container and volume were",
                (false, true) => "container and image were",
                (false, false) => "container was",
            };
            bail!("{e:#}\n\nits last log lines:\n{logs}\n\nNothing was left: its {what} removed.")
        }
    }
}

// ------------------------------------------------------------------------------ Kubernetes

/// The context `--context` names, else the kubeconfig's current one.
fn context_name(context: Option<&str>) -> anyhow::Result<String> {
    if let Some(c) = context.filter(|c| !c.is_empty()) {
        return Ok(c.to_owned());
    }
    let path = std::env::var("KUBECONFIG").ok().and_then(|p| p.split(':').next().filter(|p| !p.is_empty()).map(std::path::PathBuf::from)).unwrap_or_else(|| crate::os::home_dir().join(".kube/config"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no kubeconfig at {} to install into Kubernetes (the default): give --on docker or --on machine to install elsewhere", path.display()))?;
    let doc: Value = serde_yaml::from_str(&text).context("reading the kubeconfig")?;
    doc.get("current-context").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned).context("the kubeconfig has no current context: give one with --context")
}

/// How the cluster of context `ctx` (`nodes`, its nodes) takes an image from this machine's
/// Docker: a minikube profile's, kind's or k3d's loader; or nothing (an empty command) for Docker
/// Desktop's and OrbStack's, which share that Docker's images: Docker Desktop's kind nodes (its
/// cluster, one node or more, since 4.65) pull them through its registry mirror; else why it can't.
fn loader(ctx: &str, nodes: &[Value]) -> Result<Vec<String>, String> {
    let words = |w: &[&str]| Ok(w.iter().map(|w| (*w).to_owned()).collect());
    if ["docker-desktop", "orbstack"].contains(&ctx) {
        return Ok(Vec::new());
    }
    if let Some(p) = nodes.iter().find_map(|n| n.pointer("/metadata/labels/minikube.k8s.io~1name")?.as_str()) {
        return words(&["minikube", "-p", p, "image", "load"]);
    }
    // kind's nodes say their cluster: `kind://docker/<cluster>/<node>`.
    if let Some(cluster) = nodes.iter().find_map(|n| n.pointer("/spec/providerID")?.as_str()?.strip_prefix("kind://")?.split('/').nth(1)) {
        return words(&["kind", "load", "docker-image", "--name", cluster]);
    }
    if let Some(name) = ctx.strip_prefix("k3d-") {
        return words(&["k3d", "image", "import", "-c", name]);
    }
    Err(format!("the cluster of context {ctx} takes no image from this machine"))
}

/// Runs `load` (a [`loader`]'s command) for `image`.
fn load_image(load: &[String], image: &str) -> anyhow::Result<()> {
    let Some((bin, args)) = load.split_first() else { return Ok(()) };
    let out = Command::new(bin).args(args).arg(image).stdin(Stdio::null()).output().with_context(|| format!("running {bin} (is it installed?)"))?;
    anyhow::ensure!(out.status.success(), "{bin} couldn't load {image} into the cluster: {}", String::from_utf8_lossy(&out.stderr).trim());
    say(format!("image       {image}, loaded into the cluster ({bin})"));
    Ok(())
}

/// The hosts a NodePort service may be reached at from here, best first: this machine for a
/// cluster that publishes node ports on it (Docker Desktop, OrbStack, Rancher Desktop), then each
/// node's external address, then its internal one.
fn addresses(ctx: &str, nodes: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    if ["docker-desktop", "orbstack", "rancher-desktop"].contains(&ctx) {
        out.push("localhost".to_owned());
    }
    for kind in ["ExternalIP", "InternalIP"] {
        for a in nodes.iter().filter_map(|n| n.pointer("/status/addresses")?.as_array()).flatten() {
            if a.get("type").and_then(Value::as_str) == Some(kind)
                && let Some(ip) = a.get("address").and_then(Value::as_str)
                && !out.iter().any(|o| o == ip)
            {
                out.push(ip.to_owned());
            }
        }
    }
    out
}

/// The first port from `from` nothing listens on here, for a port forward (another kiln may have
/// `from`). Nothing answers on it, and it can be had: on macOS a bind can share a port another
/// server holds on every address.
fn free_port(from: u16) -> u16 {
    let free = |p: u16| {
        let at = std::net::SocketAddr::from(([127, 0, 0, 1], p));
        std::net::TcpStream::connect_timeout(&at, Duration::from_millis(200)).is_err() && std::net::TcpListener::bind(at).is_ok()
    };
    (from..from.saturating_add(100)).find(|p| free(*p)).unwrap_or(from)
}

/// `http://host:port`, an IPv6 address in brackets.
fn url_of(host: &str, port: u64) -> String {
    if host.contains(':') { format!("http://[{host}]:{port}") } else { format!("http://{host}:{port}") }
}

/// The CPU of the cluster's nodes, as Kubernetes says it (`amd64`, `arm64`): the image is built for
/// it.
fn nodes_arch(nodes: &[Value]) -> anyhow::Result<String> {
    let mut archs: Vec<&str> = nodes.iter().filter_map(|n| n.pointer("/status/nodeInfo/architecture")?.as_str()).collect();
    archs.sort_unstable();
    archs.dedup();
    match archs.as_slice() {
        [one] => Ok((*one).to_owned()),
        [] => bail!("the cluster's nodes don't say their CPU: give --image"),
        many => bail!("the cluster's nodes have more than one kind of CPU ({}): give --image of an image built for each", many.join(", ")),
    }
}

/// The registry a cluster names for images pushed from this machine (`localRegistryHosting.v1` of
/// kube-public's `local-registry-hosting`, KEP-1755's word, in YAML): where to push (`host`), and
/// the name the cluster's nodes pull by (`hostFromContainerRuntime`, else `host`).
fn hosting(text: &str) -> Option<(String, String)> {
    let v: Value = serde_yaml::from_str(text).ok()?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|h| !h.is_empty());
    let host = field("host")?;
    Some((host.to_owned(), field("hostFromContainerRuntime").unwrap_or(host).to_owned()))
}

/// The registry the cluster names for images pushed from this machine ([`hosting`]): where to
/// push, and the name its nodes pull by.
async fn named_registry(cluster: &Cluster) -> Option<(String, String)> {
    let cm = cluster.get(&ResKey::new("", "ConfigMap", "kube-public", "local-registry-hosting")).await.ok().flatten()?;
    hosting(cm.pointer("/data/localRegistryHosting.v1")?.as_str()?)
}

/// The image the cluster runs, and its pull policy: `--image` (loaded into a cluster that takes
/// images from this machine, when this machine's Docker has it); else one built for the nodes'
/// CPU ([`kiln_image`]): loaded into the cluster (never pulled), shared with it (pulled when its
/// node hasn't it, as Docker Desktop's kind nodes pull through its mirror), or pushed to a registry
/// it pulls from: `--registry`, else the one it names ([`named_registry`]) when it takes no image
/// from this machine.
async fn cluster_image(a: &InstallArgs, cluster: &Cluster, ctx: &str, nodes: &[Value]) -> anyhow::Result<(String, &'static str)> {
    let load = loader(ctx, nodes);
    if let Some(image) = &a.image {
        if let Ok(load) = &load
            && !load.is_empty()
            && docker(&["image", "inspect", "--format", "{{.Id}}", image]).is_ok()
        {
            load_image(load, image)?;
        }
        return Ok((image.clone(), "IfNotPresent"));
    }
    let registry = match (&load, &a.registry) {
        (_, Some(r)) => Some((r.trim_end_matches('/').to_owned(), r.trim_end_matches('/').to_owned())),
        (Ok(_), None) => None,
        (Err(why), None) => {
            let Some((push, pull)) = named_registry(cluster).await else {
                bail!("{why}, and it names no registry (kube-public's local-registry-hosting): give --registry to push the image to, or --image of one it can pull");
            };
            say(format!("registry    {push}{}", if pull == push { String::new() } else { format!(" (the cluster pulls from {pull})") }));
            Some((push, pull))
        }
    };
    let (tag, _) = kiln_image(a, &nodes_arch(nodes)?).await?;
    if let Some((push, pull)) = registry {
        let pushed = format!("{push}/{tag}");
        docker(&["tag", &tag, &pushed])?;
        docker(&["push", "-q", &pushed])?;
        say(format!("image       pushed to {pushed}"));
        return Ok((format!("{pull}/{tag}"), "IfNotPresent"));
    }
    let load = load.unwrap_or_default();
    if load.is_empty() {
        say(format!("image       {tag}, shared with the cluster"));
        return Ok((tag, "IfNotPresent"));
    }
    load_image(&load, &tag)?;
    Ok((tag, "Never"))
}

/// A whole kiln in the cluster of the kubeconfig's current context (or `--context`).
pub async fn kubernetes_install(a: &InstallArgs) -> anyhow::Result<()> {
    let ctx = context_name(a.context.as_deref())?;
    let cluster = Cluster::new(&ctx, crate::cd::kubeconfig_endpoint("", &ctx)?)?;
    cluster.discover().await.with_context(|| format!("the cluster of context {ctx} can't be reached"))?;
    let ns = a.namespace.as_str();
    say(format!("cluster     context {ctx}, namespace {ns}"));
    for (group, kind, name) in OBJECTS {
        if cluster.get(&ResKey::new(group, kind, ns, name)).await?.is_some() {
            bail!("namespace {ns} has a {kind} {name} already (a kiln installed before?): install into another one with --namespace");
        }
    }
    let nodes = cluster.list_in(&GroupKind::new("", "Node"), None, None, false).await.context("listing the cluster's nodes")?;
    let hosts = addresses(&ctx, &nodes);
    anyhow::ensure!(!hosts.is_empty(), "the cluster's nodes have no address");
    let (image, pull) = cluster_image(a, &cluster, &ctx, &nodes).await?;
    let token = crate::util::random_token();
    let mut made = Vec::new();
    let Err(e) = install_in(&cluster, &ctx, ns, (&image, pull), &token, &hosts, &mut made).await else { return Ok(()) };
    let logs = pod_logs(&cluster, ns).await;
    let gone = undo(&cluster, ns, &made).await;
    let mut logs = logs.map(|l| format!("\n\nkiln's last log lines:\n{l}")).unwrap_or_default();
    if format!("{e:#}").contains("ImagePullBackOff") && image.contains('/') {
        logs.push_str(&format!(
            "\n\nThe cluster's nodes couldn't pull {image}: they reach a registry this machine pushes to as its own setup says (kind's: https://kind.sigs.k8s.io/docs/user/local-registry/)."
        ));
    }
    let left = if made.is_empty() {
        "Nothing was made.".to_owned()
    } else if gone {
        format!("Nothing was left: what the install made in {ns} was removed.")
    } else {
        format!("What the install made in {ns} is being removed; namespace {ns} was still going when the install stopped waiting.")
    };
    bail!("{e:#}{logs}\n\n{left}")
}

/// Makes kiln's objects in `ns` (each one made recorded in `made`), waits for its pod, finds where
/// it can be reached from here, and checks it there.
async fn install_in(cluster: &Cluster, ctx: &str, ns: &str, image: (&str, &str), token: &str, hosts: &[String], made: &mut Vec<ResKey>) -> anyhow::Result<()> {
    let first = hosts.first().context("the cluster's nodes have no address")?;
    let port = install_objects(cluster, ns, image, token, first, made).await?;
    wait_ready(cluster, ns).await?;
    let http = client()?;
    let mut reached = None;
    for host in hosts {
        let url = url_of(host, port);
        if http.get(&format!("{url}/healthz")).timeout(Duration::from_secs(3)).send().await.is_ok_and(|r| r.is_success()) {
            reached = Some(url);
            break;
        }
    }
    let token_from = format!("kubectl --context {ctx} -n {ns} get secret kiln-admin -o jsonpath='{{.data.token}}' | base64 -d");
    match reached {
        Some(url) => {
            let runners = verify(&Via::Url(&http, url.clone()), READY_TIME).await?;
            opened(&url, token, runners, &later(&url, token, &token_from));
        }
        None => {
            let runners = verify(&Via::Proxy(cluster, proxy_base(ns)), READY_TIME).await?;
            let local = free_port(7070);
            say("address     no node of the cluster can be reached from here: it was checked through the cluster's API server");
            say(format!("            open it from here with: kubectl --context {ctx} -n {ns} port-forward svc/{APP} {local}:7070"));
            let url = format!("http://localhost:{local}");
            opened(&url, token, runners, &later(&url, token, &token_from));
        }
    }
    say(format!("in cluster  kubectl --context {ctx} -n {ns} get pods"));
    Ok(())
}

/// Where kiln's service in `ns` is reached through the API server (its proxy).
fn proxy_base(ns: &str) -> String {
    format!("/api/v1/namespaces/{ns}/services/{APP}:http/proxy")
}

/// Makes `o`, a new object, and records it in `made`: the object as the cluster stored it.
async fn make(cluster: &Cluster, made: &mut Vec<ResKey>, o: Value) -> anyhow::Result<Value> {
    let key = ResKey::new(obj::group(&o), obj::kind(&o), obj::namespace(&o), obj::name(&o));
    let stored = cluster.apply(&o, MANAGER, false).await.with_context(|| format!("making {} {}", key.kind, key.name))?;
    made.push(key);
    Ok(stored)
}

/// Makes the objects of a kiln in namespace `ns` (the namespace too, if it's new), each one made
/// recorded in `made`: the service's port for HTTP on the nodes.
async fn install_objects(cluster: &Cluster, ns: &str, (image, pull): (&str, &str), token: &str, host: &str, made: &mut Vec<ResKey>) -> anyhow::Result<u64> {
    let labels = json!({ "app.kubernetes.io/name": APP, "app.kubernetes.io/managed-by": MANAGER });
    let meta = |name: &str| json!({ "name": name, "namespace": ns, "labels": labels });
    if cluster.get(&ResKey::new("", "Namespace", "", ns)).await?.is_none() {
        make(cluster, made, json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns, "labels": labels } })).await?;
    }
    make(cluster, made, json!({ "apiVersion": "v1", "kind": "ServiceAccount", "metadata": meta(APP) })).await?;
    // What kiln's updates use (System → Updates): its own Deployment, to restart it onto a new
    // image, and its pods, which the guard of its deploy reads.
    let rules = json!([
        { "apiGroups": ["apps"], "resources": ["deployments"], "resourceNames": [APP], "verbs": ["get", "patch"] },
        { "apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list"] },
    ]);
    make(cluster, made, json!({ "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": meta(APP), "rules": rules })).await?;
    let binding = json!({ "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": meta(APP),
        "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": APP },
        "subjects": [{ "kind": "ServiceAccount", "name": APP, "namespace": ns }] });
    make(cluster, made, binding).await?;
    make(cluster, made, json!({ "apiVersion": "v1", "kind": "Secret", "metadata": meta("kiln-admin"), "type": "Opaque", "stringData": { "token": token } })).await?;
    let ports = json!([
        { "name": "http", "port": 7070, "targetPort": 7070, "protocol": "TCP" },
        { "name": "quic", "port": 7071, "targetPort": 7071, "protocol": "UDP" },
    ]);
    let service =
        make(cluster, made, json!({ "apiVersion": "v1", "kind": "Service", "metadata": meta(APP), "spec": { "type": "NodePort", "selector": { "app.kubernetes.io/name": APP }, "ports": ports } }))
            .await?;
    let node_port = |name: &str| service.pointer("/spec/ports")?.as_array()?.iter().find(|p| p.get("name").and_then(Value::as_str) == Some(name))?.get("nodePort")?.as_u64();
    let (http, quic) = (node_port("http").context("its service has no node port for HTTP")?, node_port("quic").context("its service has no node port for QUIC")?);
    let quic_host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let server = format!("public_url = \"{}\"\nquic_advertise = \"{quic_host}:{quic}\"\nadmin_token_file = \"/etc/kiln-admin/token\"\n", url_of(host, http));
    make(cluster, made, json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": meta("kiln-config"), "data": { "kiln.toml": config(&server) } })).await?;
    let claim = json!({ "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "10Gi" } } });
    make(cluster, made, json!({ "apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": meta("kiln-data"), "spec": claim })).await?;
    let field = |name: &str, path: &str| json!({ "name": name, "valueFrom": { "fieldRef": { "fieldPath": path } } });
    let container = json!({
        "name": APP, "image": image, "imagePullPolicy": pull, "args": ["server"],
        "env": [{ "name": "KILN_CONFIG", "value": "/etc/kiln/kiln.toml" }, field("POD_NAME", "metadata.name"), field("POD_NAMESPACE", "metadata.namespace"), field("NODE_NAME", "spec.nodeName")],
        "ports": [{ "name": "http", "containerPort": 7070, "protocol": "TCP" }, { "name": "quic", "containerPort": 7071, "protocol": "UDP" }],
        "securityContext": { "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } },
        // Kubernetes waits 1s for a probe by default: a server slowed by its disk, not dead, was
        // killed for it (docs/resilience.md, O3). The chart waits as long.
        "readinessProbe": { "httpGet": { "path": "/readyz", "port": "http" }, "periodSeconds": 5, "timeoutSeconds": 3, "failureThreshold": 3 },
        "livenessProbe": { "httpGet": { "path": "/healthz", "port": "http" }, "initialDelaySeconds": 30, "periodSeconds": 10, "timeoutSeconds": 5, "failureThreshold": 6 },
        "volumeMounts": [
            { "name": "data", "mountPath": "/var/lib/kiln" },
            { "name": "config", "mountPath": "/etc/kiln/kiln.toml", "subPath": "kiln.toml", "readOnly": true },
            { "name": "admin", "mountPath": "/etc/kiln-admin", "readOnly": true },
        ],
    });
    let pod = json!({
        "serviceAccountName": APP,
        "securityContext": { "runAsNonRoot": true, "runAsUser": 10001, "runAsGroup": 10001, "fsGroup": 10001, "fsGroupChangePolicy": "OnRootMismatch", "seccompProfile": { "type": "RuntimeDefault" } },
        "containers": [container],
        "volumes": [
            { "name": "data", "persistentVolumeClaim": { "claimName": "kiln-data" } },
            { "name": "config", "configMap": { "name": "kiln-config" } },
            { "name": "admin", "secret": { "secretName": "kiln-admin", "defaultMode": 0o440 } },
        ],
    });
    let spec =
        json!({ "replicas": 1, "strategy": { "type": "Recreate" }, "selector": { "matchLabels": { "app.kubernetes.io/name": APP } }, "template": { "metadata": { "labels": labels }, "spec": pod } });
    make(cluster, made, json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": meta(APP), "spec": spec })).await?;
    say(format!("objects     its account and role, admin token, service, config, volume and deployment in {ns}"));
    Ok(http)
}

/// Waits for kiln's pod in `ns` to be ready. A pod that can't start (its image can't be had, it
/// crashes, its config is wrong) ends the wait at once, saying why; one no node takes, after a while.
async fn wait_ready(cluster: &Cluster, ns: &str) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut unplaced: Option<Instant> = None;
    let mut last = "no pod yet".to_owned();
    while start.elapsed() < READY_TIME {
        let pods = cluster.list_in(&GroupKind::new("", "Pod"), Some(ns), Some(SELECTOR), false).await?;
        for p in &pods {
            if ready(p) {
                say(format!("pod         {} is ready", obj::name(p)));
                return Ok(());
            }
            if let Some(why) = cant_start(p) {
                bail!("kiln's pod {} can't start: {why}", obj::name(p));
            }
            match unplaced_why(p) {
                Some(why) => {
                    if unplaced.get_or_insert_with(Instant::now).elapsed() > UNPLACED_TIME {
                        bail!("no node takes kiln's pod {}: {why}", obj::name(p));
                    }
                    last = why;
                }
                None => {
                    unplaced = None;
                    last = standing(p);
                }
            }
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
    bail!("kiln's pod wasn't ready after {} s: {last}", READY_TIME.as_secs())
}

fn ready(p: &Value) -> bool {
    let cs = p.pointer("/status/conditions").and_then(Value::as_array);
    cs.is_some_and(|cs| cs.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("Ready") && c.get("status").and_then(Value::as_str) == Some("True")))
}

/// Why a pod's container can't start, when Kubernetes gives a reason that won't pass by itself.
fn cant_start(p: &Value) -> Option<String> {
    const STUCK: [&str; 7] = ["ImagePullBackOff", "ErrImageNeverPull", "InvalidImageName", "CrashLoopBackOff", "CreateContainerConfigError", "CreateContainerError", "RunContainerError"];
    p.pointer("/status/containerStatuses")?.as_array()?.iter().find_map(|c| {
        let waiting = c.pointer("/state/waiting")?;
        let reason = waiting.get("reason")?.as_str()?;
        STUCK.contains(&reason).then(|| match waiting.get("message").and_then(Value::as_str) {
            Some(m) if !m.is_empty() => format!("{reason}: {m}"),
            _ => reason.to_owned(),
        })
    })
}

/// Why no node takes a pod, while none does.
fn unplaced_why(p: &Value) -> Option<String> {
    p.pointer("/status/conditions")?.as_array()?.iter().find_map(|c| {
        let unplaced = c.get("type")?.as_str()? == "PodScheduled" && c.get("status")?.as_str()? == "False";
        unplaced.then(|| c.get("message").and_then(Value::as_str).unwrap_or("unschedulable").to_owned())
    })
}

/// Where a pod stands, in a few words.
fn standing(p: &Value) -> String {
    let phase = p.pointer("/status/phase").and_then(Value::as_str).unwrap_or("Pending");
    match p.pointer("/status/containerStatuses/0/state/waiting/reason").and_then(Value::as_str) {
        Some(why) => format!("{phase}: {why}"),
        None if phase == "Running" => "running, not ready yet".to_owned(),
        None => phase.to_owned(),
    }
}

/// The last lines kiln's pod in `ns` logged, if it has a pod.
async fn pod_logs(cluster: &Cluster, ns: &str) -> Option<String> {
    let pods = cluster.list_in(&GroupKind::new("", "Pod"), Some(ns), Some(SELECTOR), false).await.ok()?;
    let pod = obj::name(pods.first()?);
    Some(match cluster.pod_log(ns, pod, "tailLines=30").await {
        Ok(b) if !b.is_empty() => String::from_utf8_lossy(&b).trim_end().to_owned(),
        Ok(_) => "(it logged nothing)".to_owned(),
        Err(e) => format!("(its log can't be read: {e:#})"),
    })
}

/// Takes back what an install made, newest first: a namespace it made goes whole, and is waited
/// for, so another install can make it again at once. Whether all of it is gone.
async fn undo(cluster: &Cluster, ns: &str, made: &[ResKey]) -> bool {
    let namespace = ResKey::new("", "Namespace", "", ns);
    if !made.contains(&namespace) {
        for key in made.iter().rev() {
            let _ = cluster.delete(key, "Background").await;
        }
        return true;
    }
    let _ = cluster.delete(&namespace, "Background").await;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) {
        if cluster.get(&namespace).await.is_ok_and(|n| n.is_none()) {
            return true;
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_takes_an_image_built_here_the_way_its_kind_does() {
        let node = |labels: Value, provider: &str| json!({ "metadata": { "labels": labels }, "spec": { "providerID": provider } });
        let cmd = |ctx: &str, nodes: &[Value]| loader(ctx, nodes).map(|c| c.join(" "));
        assert_eq!(cmd("lab", &[node(json!({ "minikube.k8s.io/name": "kiln-install-check" }), "")]).as_deref(), Ok("minikube -p kiln-install-check image load"));
        assert_eq!(cmd("anything", &[node(json!({}), "kind://docker/dev/dev-control-plane")]).as_deref(), Ok("kind load docker-image --name dev"));
        assert_eq!(cmd("k3d-ci", &[node(json!({}), "k3s://k3d-ci-server-0")]).as_deref(), Ok("k3d image import -c ci"));
        assert_eq!(cmd("docker-desktop", &[node(json!({}), "")]).as_deref(), Ok(""), "Docker Desktop's single node runs on its Docker");
        // Its kind nodes (Docker Desktop's default since 4.65, one node or more) pull its images
        // through its mirror: an image of this machine's Docker is theirs.
        assert_eq!(cmd("docker-desktop", &[node(json!({}), "kind://docker/desktop/desktop-control-plane")]).as_deref(), Ok(""));
        assert!(cmd("gke_prod_us-east1_main", &[node(json!({}), "gce://prod/us-east1-b/n1")]).is_err(), "a cluster elsewhere needs a registry");
    }

    #[test]
    fn a_service_is_opened_at_this_machine_then_external_then_internal_addresses() {
        let node = |addrs: &[(&str, &str)]| json!({ "status": { "addresses": addrs.iter().map(|(t, a)| json!({ "type": t, "address": a })).collect::<Vec<_>>() } });
        let nodes = [node(&[("InternalIP", "10.0.0.2"), ("Hostname", "n1")]), node(&[("InternalIP", "10.0.0.3"), ("ExternalIP", "34.1.2.3")])];
        assert_eq!(addresses("gke", &nodes), ["34.1.2.3", "10.0.0.2", "10.0.0.3"]);
        assert_eq!(addresses("docker-desktop", &nodes[..1]), ["localhost", "10.0.0.2"]);
        assert_eq!(url_of("fd00::2", 30070), "http://[fd00::2]:30070");
    }

    #[test]
    fn a_port_forward_takes_a_port_nothing_listens_on() -> anyhow::Result<()> {
        let taken = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = taken.local_addr()?.port();
        assert_ne!(free_port(port), port);
        Ok(())
    }

    #[test]
    fn this_binary_is_read_as_static_or_dynamic() -> anyhow::Result<()> {
        // The test binary is linked as the build is: whichever it is, it reads.
        dynamic(&std::env::current_exe()?)?;
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("x"), b"not an elf file")?;
        assert!(dynamic(&dir.path().join("x")).is_err());
        Ok(())
    }

    #[test]
    fn an_image_runs_a_dynamic_kiln_on_the_system_it_was_built_on() -> anyhow::Result<()> {
        assert!(base(false, "")?.starts_with("FROM alpine:"), "a static kiln runs on Alpine, as kiln's image does");
        let on = |os: &str| base(true, os).map(|b| b.lines().next().unwrap_or("").to_owned());
        assert_eq!(on("ID=fedora\nVERSION_ID=43\n")?, "FROM fedora:43");
        assert_eq!(on("ID=debian\nVERSION_ID=\"13\"\n")?, "FROM debian:13-slim");
        assert_eq!(on("ID=ubuntu\nVERSION_ID=\"24.04\"\n")?, "FROM ubuntu:24.04");
        assert_eq!(on("NAME=\"CachyOS Linux\"\nID=cachyos\nID_LIKE=arch\n")?, "FROM archlinux:latest");
        assert!(on("ID=gentoo\n").is_err_and(|e| e.to_string().contains("--image")));
        let file = dockerfile(&base(false, "")?);
        assert!(file.contains("USER 10001:10001") && file.contains("ENTRYPOINT [\"/kiln\", \"supervise\", \"--\"]") && file.contains("COPY kiln.toml /etc/kiln/kiln.toml"));
        Ok(())
    }

    #[test]
    fn an_installed_kiln_has_its_own_runner_and_the_address_it_was_given() -> anyhow::Result<()> {
        let c: crate::config::Config = toml::from_str(&config("public_url = \"http://10.0.0.2:30070\"\nadmin_token_file = \"/etc/kiln-admin/token\"\n"))?;
        assert_eq!((c.server.http_addr.as_str(), c.server.public_url.as_str()), ("0.0.0.0:7070", "http://10.0.0.2:30070"));
        assert_eq!((c.local_agent.enabled, c.local_agent.name.as_deref(), c.kubernetes.enabled), (true, Some("kiln"), false));
        assert_eq!(c.server.admin_token_file.as_deref(), Some(Path::new("/etc/kiln-admin/token")));
        Ok(())
    }

    /// A kiln stand-in on a port of its own: `/healthz`, its page (naming its script, when `ui`),
    /// the script, and `/healthz/server` with `runners` connected; at those paths, and through an
    /// API server's proxy to kiln's service in namespace `kiln`, which rewrites the page's links to
    /// go through it, as Kubernetes' does. Its page, as kiln's, isn't for a request that takes JSON
    /// alone (406).
    fn stand_in(ui: bool, runners: u64) -> anyhow::Result<String> {
        use std::io::Write as _;
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", l.local_addr()?);
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let page = if ui {
                    r#"<!doctype html><html><script type="module" async="" src="/assets/index-x1.js"></script></html>"#
                } else {
                    "<!doctype html><html><p>The UI was not built into this binary.</p></html>"
                };
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let proxied = path.strip_prefix(proxy_base("kiln").as_str());
                let page = match proxied {
                    Some(_) => page.replace("src=\"/", &format!("src=\"https://10.0.0.1:6443{}/", proxy_base("kiln"))),
                    None => page.to_owned(),
                };
                let accept = req.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("accept:").map(|v| v.trim().to_owned()));
                let html = accept.is_none_or(|a| a.contains("text/html") || a.contains("*/*"));
                let (status, body) = match proxied.unwrap_or(path) {
                    "/healthz" => ("200 OK", "ok".to_owned()),
                    "/" if !html => ("406 Not Acceptable", String::new()),
                    "/" => ("200 OK", page),
                    "/assets/index-x1.js" => ("200 OK", "export {}".to_owned()),
                    "/healthz/server" => ("200 OK", format!(r#"{{"runners":{{"connected":{runners},"before":0,"back":1.0}}}}"#)),
                    _ => ("404 Not Found", String::new()),
                };
                let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        });
        Ok(url)
    }

    #[test]
    fn a_kiln_is_ready_once_it_answers_its_ui_loads_and_its_runner_is_connected() -> anyhow::Result<()> {
        let (whole, no_ui, alone) = (stand_in(true, 1)?, stand_in(false, 1)?, stand_in(true, 0)?);
        crate::rt::block_on(async move {
            let http = client()?;
            assert_eq!(check(&Via::Url(&http, whole)).await?, 1);
            let Err(e) = check(&Via::Url(&http, no_ui)).await else { bail!("a kiln without its UI was taken as whole") };
            assert!(format!("{e:#}").contains("built without its UI"), "{e:#}");
            let Err(e) = verify(&Via::Url(&http, alone), Duration::from_millis(10)).await else { bail!("a kiln without its runner was taken as whole") };
            assert!(format!("{e:#}").contains("not ready after 0 s") && format!("{e:#}").contains("runner isn't connected"), "{e:#}");
            Ok(())
        })
    }

    /// A 64-bit ELF file's first bytes: with a program interpreter (a dynamic binary), or none.
    fn elf(interp: bool) -> Vec<u8> {
        // Its header (64-bit, little-endian; one program header, 56 bytes, at 64), then that header,
        // the program interpreter's (3) or a loadable segment's (1).
        let mut b = b"\x7fELF\x02\x01".to_vec();
        b.resize(32, 0);
        b.extend_from_slice(&64u64.to_le_bytes());
        b.resize(54, 0);
        b.extend_from_slice(&56u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.resize(64, 0);
        b.extend_from_slice(&(if interp { 3u32 } else { 1u32 }).to_le_bytes());
        b.resize(64 + 56, 0);
        b
    }

    /// A kiln server stand-in for `/dl/kiln/<target>`: the aarch64 build sent on from where it
    /// was asked for (a redirect, as a server's for a build it hasn't), the x86_64 one linked
    /// dynamically, any other target a page that isn't a binary.
    fn builds() -> anyhow::Result<String> {
        use std::io::Write as _;
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", l.local_addr()?);
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let (head, body): (String, Vec<u8>) = match req.split_whitespace().nth(1).unwrap_or("/") {
                    "/dl/kiln/aarch64-unknown-linux-musl" => ("302 Found\r\nlocation: /releases/kiln-aarch64".into(), Vec::new()),
                    "/releases/kiln-aarch64" => ("200 OK".into(), elf(false)),
                    "/dl/kiln/x86_64-unknown-linux-musl" => ("200 OK".into(), elf(true)),
                    _ => ("200 OK".into(), b"<!doctype html><p>no such page</p>".to_vec()),
                };
                let _ = write!(s, "HTTP/1.1 {head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        Ok(url)
    }

    #[test]
    fn a_linux_kiln_for_the_clusters_cpu_comes_from_a_kiln_server() -> anyhow::Result<()> {
        #[derive(clap::Parser)]
        struct Install {
            #[command(flatten)]
            a: InstallArgs,
        }
        use clap::Parser as _;
        let server = builds()?;
        let a = Install::try_parse_from(["install", "--server", server.as_str()])?.a;
        let dir = tempfile::tempdir()?;
        crate::rt::block_on(async move {
            let got = fetch_linux_kiln(&a, "arm64", dir.path()).await?;
            assert_eq!(std::fs::read(&got)?, elf(false), "the static build, where the server sent it on to");
            let Err(e) = fetch_linux_kiln(&a, "amd64", dir.path()).await else { bail!("a dynamic kiln was taken for an image of its own") };
            assert!(format!("{e:#}").contains("linked dynamically"), "{e:#}");
            let Err(e) = fetch_linux_kiln(&a, "riscv64", dir.path()).await else { bail!("a CPU kiln has no build for was fetched") };
            assert!(format!("{e:#}").contains("--image"), "{e:#}");
            Ok(())
        })?;
        assert_eq!(origin_of("http://10.0.0.2:7070/dl/kiln/x"), "http://10.0.0.2:7070");
        assert_eq!(origin_of("https://kiln.example"), "https://kiln.example");
        Ok(())
    }

    #[test]
    fn a_registry_is_the_one_the_cluster_names() {
        let named = "host: \"localhost:5001\"\nhostFromContainerRuntime: \"kind-registry:5000\"\nhelp: \"https://kind.sigs.k8s.io/docs/user/local-registry/\"\n";
        assert_eq!(hosting(named), Some(("localhost:5001".to_owned(), "kind-registry:5000".to_owned())));
        assert_eq!(
            hosting("host: \"localhost:5002\"\nhostFromClusterNetwork: \"r:5000\"\n"),
            Some(("localhost:5002".to_owned(), "localhost:5002".to_owned())),
            "the nodes pull by `host` when it names no other"
        );
        assert_eq!(hosting("help: x\n"), None);
    }

    #[test]
    fn an_image_is_built_for_the_cpu_of_the_clusters_nodes() {
        let node = |arch: &str| json!({ "status": { "nodeInfo": { "architecture": arch, "operatingSystem": "linux" } } });
        assert_eq!(nodes_arch(&[node("arm64"), node("arm64")]).ok().as_deref(), Some("arm64"));
        assert!(nodes_arch(&[node("arm64"), node("amd64")]).is_err_and(|e| e.to_string().contains("--image")));
        assert!(nodes_arch(&[json!({})]).is_err());
        assert_eq!((docker_arch("x86_64"), docker_arch("aarch64")), ("amd64", "arm64"));
    }

    #[test]
    fn a_kiln_no_node_can_be_reached_at_is_checked_through_the_api_servers_proxy() -> anyhow::Result<()> {
        let server = stand_in(true, 1)?;
        crate::rt::block_on(async move {
            let endpoint = kiln_cd::kube::Endpoint { server, ca: None, insecure: false, tls_server_name: None, auth: kiln_cd::kube::Auth::None, namespace: None };
            let cluster = Cluster::new("stand-in", endpoint)?;
            assert_eq!(check(&Via::Proxy(&cluster, proxy_base("kiln"))).await?, 1);
            Ok(())
        })
    }
}
