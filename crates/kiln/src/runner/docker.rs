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
use std::time::{Duration, Instant};

use base64::Engine as _;

/// How long pulls and builds may take.
const SLOW: Duration = Duration::from_secs(1800);
/// How long any other call to the engine may take. Long: a busy machine's daemon can take
/// minutes to create or start a container, and GitHub's runner waits as long as it takes; this
/// only catches a daemon that's stuck.
const CALL: Duration = Duration::from_secs(600);
/// How long a runner's sweep of what it left running may take, at its start.
const SWEEP: Duration = Duration::from_secs(60);
/// How long services may take to become healthy.
const HEALTHY_WITHIN: Duration = Duration::from_secs(600);
/// Says `musl` in a container whose C library is musl (Alpine's): its dynamic loader is.
const MUSL_PROBE: &str = "for f in /lib/ld-musl-*; do [ -e \"$f\" ] && echo musl && break; done; true";

/// The container CLI on this machine (its full path, so a step's `PATH` can't hide it): `docker`,
/// else `podman`, when its daemon answers. In a `k8s` pool's pod, the one its Docker sidecar's
/// copied in (W13-R07).
pub fn find() -> Option<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pod = std::env::var_os("KILN_POD").and(std::env::var_os("KILN_POD_DOCKER")).map(PathBuf::from);
    for bin in ["docker", "podman"] {
        let found = match &pod {
            Some(p) => (bin == "docker").then(|| p.clone()),
            None => std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file()),
        };
        let Some(full) = found else { continue };
        let ok = std::process::Command::new(&full).args(["version", "--format", "{{.Server.Version}}"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
        if ok {
            return Some(full.to_string_lossy().into_owned());
        }
    }
    None
}

/// How long a probe of the container engine is waited for: a runner's report of what it can do
/// waits on it as it connects. A daemon too busy to answer by then isn't taken for absent.
const PROBE_WAIT: Duration = Duration::from_secs(10);
/// A probe under way this long is taken for hung (`docker version` against a daemon that never
/// answers), and the next ask starts another.
const PROBE_HUNG: Duration = Duration::from_secs(600);

/// A probe's answer, once it has one, and since when one is under way.
struct Probe {
    state: std::sync::Mutex<(Option<bool>, Option<Instant>)>,
    done: std::sync::Condvar,
}

impl Probe {
    const fn new() -> Probe {
        Probe { state: std::sync::Mutex::new((None, None)), done: std::sync::Condvar::new() }
    }

    /// `probe`'s answer, kept once there is one. Until then, one probe at a time runs, and it is
    /// waited for until [`PROBE_WAIT`] after it began: one not answered by then (a daemon too
    /// busy) is `false` for now, and asked again next time (a runner's next connection), so a
    /// runner that started on a busy machine doesn't go without Docker for good. Blocks: call it
    /// off the runtime.
    fn answer(&'static self, probe: fn() -> bool) -> bool {
        self.answer_within(PROBE_WAIT, probe)
    }

    fn answer_within(&'static self, wait: Duration, probe: fn() -> bool) -> bool {
        let Ok(mut g) = self.state.lock() else { return false };
        if let Some(v) = g.0 {
            return v;
        }
        let under_way = g.1;
        let since = match under_way {
            Some(t) if t.elapsed() < PROBE_HUNG => t,
            _ => {
                let now = Instant::now();
                g.1 = Some(now);
                std::thread::spawn(move || {
                    let v = probe();
                    if let Ok(mut g) = self.state.lock() {
                        *g = (Some(v), None);
                    }
                    self.done.notify_all();
                });
                now
            }
        };
        let wait = wait.saturating_sub(since.elapsed());
        self.done.wait_timeout_while(g, wait, |g| g.0.is_none()).ok().and_then(|(g, _)| g.0).unwrap_or(false)
    }
}

/// Whether this machine can run containers (for `CAP_DOCKER`, W13-R01).
pub fn available() -> bool {
    static PROBED: Probe = Probe::new();
    PROBED.answer(|| find().is_some())
}

/// Whether this machine's engine runs rootless (for `CAP_DOCKER_ROOTLESS`: untrusted legs use
/// Docker only so, docs/ci.md W18-R07).
pub fn rootless_engine() -> bool {
    static PROBED: Probe = Probe::new();
    PROBED.answer(|| {
        find().is_some_and(|bin| {
            let format = if bin.ends_with("podman") { "{{.Host.Security.Rootless}}" } else { "{{json .SecurityOptions}}" };
            std::process::Command::new(&bin).args(["info", "--format", format]).stderr(std::process::Stdio::null()).output().is_ok_and(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                o.status.success() && (out.trim() == "true" || out.contains("rootless"))
            })
        })
    })
}

/// Who to run `image` as when it says it's GitHub's runner. GitHub's hosted runners run as uid
/// 1001, and an image whose `USER` is 1001 means that user, the owner of the workspace it's
/// given (kubesec's action; images made for Actions), though GitHub asks Docker actions not to set
/// one. Here the workspace is the runner's own user's, so such an image runs as that user: root
/// on a rootless engine, which is the host's user. `None` for any other image.
pub async fn as_runner(cli: &Cli, image: &str) -> Option<String> {
    let user = cli.run(&[s("image"), s("inspect"), s("--format"), s("{{.Config.User}}"), image.to_owned()], &BTreeMap::new(), CALL).await.ok()?;
    if user != "1001" && !user.starts_with("1001:") {
        return None;
    }
    if crate::rt::blocking(rootless_engine).await.unwrap_or(false) {
        return Some(s("0:0"));
    }
    host_user().filter(|(uid, _)| *uid != 1001).map(|(uid, gid)| format!("{uid}:{gid}"))
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
    /// For a `docker` executor's container given Docker: the leg's own socket in the host's
    /// place, which finds what its steps bind-mount ([`super::docker_sock`]).
    pub docker_proxy: Option<PathBuf>,
    /// No seccomp filter, as on a hosted runner's machine: for a `docker` executor's container
    /// given the host's Docker, which can start any container it likes anyway.
    pub seccomp_unconfined: bool,
    /// For a `docker` executor's container: the host user (uid, gid) its steps run as, through
    /// the image's `runner` user.
    pub user: Option<(u32, u32)>,
    /// A test build's full disk (docs/ci.md W22-AC04): the job container's workspace, a volume of
    /// this many bytes in place of the host's directory.
    pub workspace_size: Option<u64>,
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
/// network basics and the headers native builds look for (OpenSSL's, which `openssl-sys` wants,
/// zlib's, curl's, SQLite's, YAML's, libffi's, libxml2's), and the command-line tools of GitHub's
/// image that scripts call (`python` for Python 3, fakeroot for Debian packages, ShellCheck, 7-Zip,
/// GNU parallel, Xvfb, Mercurial and the like), that's the toolchains workflows call without a
/// `setup-*` action: CMake,
/// Ninja, the autotools and Clang; a JDK with Maven, Ant and sbt (`JAVA_HOME` set); PHP with
/// Composer; Ruby and Bundler; Leiningen; pipx; Rust (rustup, with rustfmt and clippy); Node.js
/// 22; the Docker CLI with buildx and compose (for legs given the host's Docker, W13-R04); conda at
/// `$CONDA` (`/usr/share/miniconda`, where GitHub's Miniconda is), from conda-forge's Miniforge,
/// whose channel needs no Anaconda license. Each comes from Ubuntu's archive or the project's own
/// release server. apt assumes yes, as GitHub's
/// images set it, so `sudo apt-get install x` works without `-y`. `-slim` (`kiln/runner-ubuntu-24.04-slim`) is
/// the basics alone, for rules that want a small image.
///
/// With `user`, steps run as `runner`, a user with the host's uid and gid and passwordless
/// sudo, as on GitHub's hosted runners. What they write in the mounted work directory then stays
/// the host user's, and tools that refuse root, or another user's files, work; Cargo's and npm's
/// global directories are theirs, so `cargo install` and `npm install -g` work as on GitHub.
pub fn runner_dockerfile(image: &str, user: Option<(u32, u32)>) -> Option<String> {
    let name = image.strip_prefix("kiln/runner-")?;
    // `-android`: the full image and Android's SDK, opt-in (see below).
    let (name, android) = name.strip_suffix("-android").map_or((name, false), |n| (n, true));
    let (name, slim) = name.strip_suffix("-slim").map_or((name, false), |n| (n, true));
    if android && slim {
        return None;
    }
    // `ImageOS` as GitHub's hosted runners set it: setup-ruby, setup-beam and others pick their
    // prebuilt tools by it.
    // And the release's ICU, which PowerShell needs.
    let (base, os, icu) = match name {
        "ubuntu-26.04" => ("ubuntu:26.04", "ubuntu26", "libicu78"),
        "ubuntu-24.04" => ("ubuntu:24.04", "ubuntu24", "libicu74"),
        "ubuntu-22.04" => ("ubuntu:22.04", "ubuntu22", "libicu70"),
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
             libssl-dev zlib1g-dev libcurl4-openssl-dev libsqlite3-dev libyaml-dev libffi-dev libxml2-dev \\\n    \
             fakeroot python-is-python3 bison flex patchelf shellcheck sqlite3 tree lz4 pigz p7zip-full parallel brotli zsync aria2 sshpass \\\n    \
             dnsutils iputils-ping net-tools telnet ftp acl swig texinfo rpm upx-ucl mediainfo mercurial xvfb \\\n    \
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
             && curl -fsSL \"https://nodejs.org/dist/latest-v22.x/$f\" | tar -xJ -C /usr/local --strip-components=1 --exclude CHANGELOG.md --exclude LICENSE --exclude README.md\n\
             ENV CONDA=/usr/share/miniconda\n\
             RUN u=\"https://github.com/conda-forge/miniforge/releases/download/26.7.2-0/Miniforge3-26.7.2-0-Linux-$(uname -m).sh\" \\\n    \
             && curl -fsSL -o /tmp/miniforge.sh \"$u\" && echo \"$(curl -fsSL \"$u.sha256\" | cut -d' ' -f1)  /tmp/miniforge.sh\" | sha256sum -c - \\\n    \
             && bash /tmp/miniforge.sh -b -p \"$CONDA\" && rm /tmp/miniforge.sh && ln -s \"$CONDA/bin/conda\" /usr/bin/conda\n",
        );
        // PowerShell (`pwsh`) and the GitHub CLI (`gh`), each checked against its release's sums
        // (PowerShell's in UTF-16).
        d.push_str(&format!(
            "RUN apt-get update && apt-get install -y --no-install-recommends {icu} && rm -rf /var/lib/apt/lists/* \\\n    \
             && case \"$(dpkg --print-architecture)\" in amd64) a=x64 g=amd64 ;; arm64) a=arm64 g=arm64 ;; esac \\\n    \
             && f=\"powershell-7.6.7-linux-$a.tar.gz\" u=https://github.com/PowerShell/PowerShell/releases/download/v7.6.7 \\\n    \
             && curl -fsSL -o \"/tmp/$f\" \"$u/$f\" && echo \"$(curl -fsSL \"$u/hashes.sha256\" | iconv -f UTF-16 -t UTF-8 | tr -d '\\r' | grep \" \\*$f$\" | cut -d' ' -f1)  /tmp/$f\" | sha256sum -c - \\\n    \
             && mkdir -p /opt/microsoft/powershell/7 && tar -xzf \"/tmp/$f\" -C /opt/microsoft/powershell/7 && rm \"/tmp/$f\" \\\n    \
             && chmod +x /opt/microsoft/powershell/7/pwsh && ln -s /opt/microsoft/powershell/7/pwsh /usr/bin/pwsh \\\n    \
             && f=\"gh_2.102.0_linux_$g.tar.gz\" u=https://github.com/cli/cli/releases/download/v2.102.0 \\\n    \
             && curl -fsSL -o \"/tmp/$f\" \"$u/$f\" && echo \"$(curl -fsSL \"$u/gh_2.102.0_checksums.txt\" | grep \"  $f$\" | cut -d' ' -f1)  /tmp/$f\" | sha256sum -c - \\\n    \
             && tar -xzf \"/tmp/$f\" -C /tmp && install -m 0755 \"/tmp/gh_2.102.0_linux_$g/bin/gh\" /usr/local/bin/gh && rm -rf \"/tmp/$f\" \"/tmp/gh_2.102.0_linux_$g\"\n"
        ));
        // Go, as GitHub's images have it on `PATH` (prometheus's `promci-setup` asks `go env` for its
        // caches' paths without setting Go up): the newest release, checked against the sums
        // go.dev lists. Its own `PATH` here, so the layers before stay as they were.
        d.push_str(
            "RUN case \"$(dpkg --print-architecture)\" in amd64) a=amd64 ;; arm64) a=arm64 ;; esac \\\n    \
             && curl -fsSL -o /tmp/go.json 'https://go.dev/dl/?mode=json' && v=$(jq -r '.[0].version' /tmp/go.json) && f=\"$v.linux-$a.tar.gz\" \\\n    \
             && s=$(jq -r --arg f \"$f\" '.[0].files[] | select(.filename == $f) | .sha256' /tmp/go.json) \\\n    \
             && curl -fsSL -o \"/tmp/$f\" \"https://go.dev/dl/$f\" && echo \"$s  /tmp/$f\" | sha256sum -c - \\\n    \
             && tar -xzf \"/tmp/$f\" -C /usr/local && rm -f \"/tmp/$f\" /tmp/go.json\n\
             ENV PATH=/usr/local/go/bin:$PATH\n",
        );
        if os != "ubuntu22" {
            // pip installs as it does on GitHub's Ubuntu 24.04 and 26.04 machines, whose
            // `/etc/pip.conf` says this (Ubuntu's Python is otherwise "externally managed", PEP 668).
            d.push_str("RUN printf '[global]\\nbreak-system-packages = true\\n' > /etc/pip.conf\n");
        }
        // Git as GitHub's images have it, the git-core PPA's (2.55 when added) rather than the
        // release's (Ubuntu 24.04's is 2.43): tools go by its version. cargo retries a failed
        // `git fetch` itself only below 2.54, which retries on its own (cargo's ssh and https tests
        // count the retries). Last, so a change here builds on the layers before it.
        d.push_str(
            "RUN apt-get update && apt-get install -y --no-install-recommends software-properties-common gpg-agent \\\n    \
             && add-apt-repository -y ppa:git-core/ppa && apt-get update && apt-get install -y --no-install-recommends git \\\n    \
             && rm -rf /var/lib/apt/lists/*\n",
        );
        // `snap` as far as a container goes (snapd needs a machine of its own): `snap install
        // NAME` installs Ubuntu's package of each name instead, which is what install-action's
        // Valgrind asks it for (tokio's stress tests).
        d.push_str(
            "RUN printf '%s\\n' '#!/bin/sh' \\\n    \
             '# kiln: snapd does not run in a container; snap install NAME installs the apt package NAME.' \\\n    \
             '[ \"$1\" = install ] || { echo \"snap: snapd does not run in a container; only snap install NAME works here, from apt\" >&2; exit 1; }' \\\n    \
             'shift; p=' \\\n    \
             'for a; do case \"$a\" in -*) ;; *) p=\"$p $a\" ;; esac; done' \\\n    \
             'apt-get update -qq && exec apt-get install -y --no-install-recommends $p' > /usr/local/bin/snap \\\n    \
             && chmod 0755 /usr/local/bin/snap\n",
        );
        // Podman, Buildah and Skopeo, as GitHub's images have them. A leg whose steps run them
        // goes where their containers can run, a microVM or a host, not a container executor's
        // container (kiln_ci::sched::routing::runs_containers).
        d.push_str("RUN apt-get update && apt-get install -y --no-install-recommends podman buildah skopeo fuse-overlayfs uidmap slirp4netns && rm -rf /var/lib/apt/lists/*\n");
    }
    if android {
        // Android's SDK where GitHub's images have it, in an image only a routing rule that names
        // it chooses: building it accepts the Android Software Development Kit License Agreement
        // (developer.android.com/studio/terms), which `sdkmanager --licenses` asks for, so the
        // administrator who names it accepts it, and the image says so. The command-line tools
        // are checked against the SHA-1 Google's repository lists (repository2-3.xml), and
        // `sdkmanager` checks NDK 27 and the platform tools against it as it installs them.
        d.push_str(
            "ENV ANDROID_HOME=/usr/local/lib/android/sdk ANDROID_SDK_ROOT=/usr/local/lib/android/sdk\n\
             LABEL kiln.android-sdk-license=accepted\n\
             RUN f=commandlinetools-linux-16111833_latest.zip \\\n    \
             && curl -fsSL -o \"/tmp/$f\" \"https://dl.google.com/android/repository/$f\" && echo \"e025545c62a8e64c7559119566a569fb1dec5f60  /tmp/$f\" | sha1sum -c - \\\n    \
             && mkdir -p \"$ANDROID_HOME/cmdline-tools\" && unzip -q \"/tmp/$f\" -d /tmp/android && mv /tmp/android/cmdline-tools \"$ANDROID_HOME/cmdline-tools/latest\" && rm -rf \"/tmp/$f\" /tmp/android \\\n    \
             && yes | \"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager\" --licenses > /dev/null \\\n    \
             && \"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager\" --install 'ndk;27.3.13750724' platform-tools > /dev/null\n\
             ENV ANDROID_NDK=/usr/local/lib/android/sdk/ndk/27.3.13750724 ANDROID_NDK_HOME=/usr/local/lib/android/sdk/ndk/27.3.13750724 \\\n    \
             ANDROID_NDK_ROOT=/usr/local/lib/android/sdk/ndk/27.3.13750724 ANDROID_NDK_LATEST_HOME=/usr/local/lib/android/sdk/ndk/27.3.13750724 \\\n    \
             PATH=$PATH:/usr/local/lib/android/sdk/cmdline-tools/latest/bin:/usr/local/lib/android/sdk/platform-tools\n",
        );
    }
    if let Some((uid, gid)) = user.filter(|(uid, _)| *uid != 0) {
        // The base image's own user may hold the uid (Ubuntu's `ubuntu` is 1000).
        d.push_str(&format!(
            "RUN u=$(getent passwd {uid} | cut -d: -f1); if [ -n \"$u\" ]; then userdel -r \"$u\" 2>/dev/null || userdel \"$u\"; fi \\\n    \
             && (getent group {gid} >/dev/null || groupadd -g {gid} runner) \\\n    \
             && useradd -m -u {uid} -g {gid} -s /bin/bash runner && install -d -o {uid} -g {gid} /home/runner/work \\\n    \
             && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner && chmod 0440 /etc/sudoers.d/runner\n\
             ENV PATH=/home/runner/.local/bin:$PATH\n"
        ));
        if !slim {
            // And the directories setup actions install into without sudo, as GitHub's images
            // let them (setup-dotnet's /usr/share/dotnet; conda's base environment).
            d.push_str(&format!("RUN mkdir -p /usr/share/dotnet && chown -R {uid}:{gid} /usr/local/cargo /usr/local/rustup /usr/local/lib/node_modules /usr/local/bin /usr/share/dotnet /usr/share/miniconda\n"));
            if android {
                // And the SDK, so `sdkmanager --install` works without sudo.
                d.push_str(&format!("RUN chown -R {uid}:{gid} /usr/local/lib/android\n"));
            }
        }
    }
    Some(d)
}

/// One of kiln's runner images (`kiln/runner-*`), built with `cli` when it isn't here yet: tagged
/// by its Dockerfile's digest (so per version and user, and a changed one is built again), and a
/// new build removes the user's older ones. `None` for any other image.
pub async fn runner_image(cli: &Cli, image: &str, user: Option<(u32, u32)>, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<Option<String>> {
    let Some(dockerfile) = runner_dockerfile(image, user) else { return Ok(None) };
    let _g = IMAGE_BUILD.lock().await;
    let digest = crate::util::sha256_hex(dockerfile.as_bytes());
    let tag = format!("{image}:{}", digest.get(..16).unwrap_or(&digest));
    if cli.run(&[s("image"), s("inspect"), s("--format"), s("{{.Id}}"), tag.clone()], &BTreeMap::new(), CALL).await.is_ok() {
        return Ok(Some(tag));
    }
    let owner = user.map_or_else(|| "0".to_owned(), |(uid, _)| uid.to_string());
    let dockerfile = format!("{dockerfile}LABEL kiln.runner-image={image} kiln.runner-user={owner}\n");
    say(format!("Building kiln's runner image {tag} (once per machine and version)"));
    let dir = cli.config.join(format!("image-{}", crate::util::new_id()));
    let d = dir.clone();
    crate::rt::blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("Dockerfile"), dockerfile)
    })
    .await??;
    let r = cli.run(&[s("build"), s("--quiet"), s("-t"), tag.clone(), dir.to_string_lossy().to_string()], &BTreeMap::new(), SLOW).await;
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
    if r.is_ok() {
        forget_older(cli, image, &owner, &tag).await;
    }
    r.map(|_| Some(tag))
}

/// Removes this user's earlier builds of runner image `image`, all but `keep`; one a container
/// still uses stays until a later build.
async fn forget_older(cli: &Cli, image: &str, owner: &str, keep: &str) {
    let none = BTreeMap::new();
    let filters = [s("--filter"), format!("label=kiln.runner-image={image}"), s("--filter"), format!("label=kiln.runner-user={owner}")];
    let mut args = vec![s("image"), s("ls")];
    args.extend(filters);
    args.extend([s("--format"), s("{{.Repository}}:{{.Tag}}")]);
    let Ok(out) = cli.run(&args, &none, CALL).await else { return };
    let old: Vec<String> = out.lines().map(str::trim).filter(|t| !t.is_empty() && *t != keep && !t.ends_with(":<none>")).map(str::to_owned).collect();
    for t in old {
        if cli.run(&[s("image"), s("rm"), t.clone()], &none, CALL).await.is_ok() {
            tracing::info!(image = %t, "removed an older build of a runner image");
        }
    }
}

/// One runner image build at a time on this machine.
static IMAGE_BUILD: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// The label on everything a runner starts: its legs' networks and containers, and its pools'
/// executors. It comes from the runner's work directory, so another runner on the same engine
/// has its own, and what a runner finds with its own label when it starts, it left behind when it
/// last stopped ([`sweep`]).
pub fn owner_label(work: &Path) -> String {
    let p = std::path::absolute(work).unwrap_or_else(|_| work.to_path_buf());
    let h = crate::util::sha256_hex(p.to_string_lossy().as_bytes());
    format!("kiln.runner-work={}", h.get(..16).unwrap_or(&h))
}

/// Removes what this runner (`work`, its work directory) left running when it last stopped, a
/// crash or a kill among the ways: its legs' containers and networks and its pools' executors,
/// found by [`owner_label`]. A leg never outlives its runner (an upgrade waits for none to run),
/// and this runs before it takes one. Left in place, a leg's network holds one of the engine's
/// few address pools until no leg can start.
pub async fn sweep(work: &Path) {
    let Ok(Some(bin)) = crate::rt::blocking(find).await else { return };
    let cli = Cli::new(&bin, work.join("_docker_config")).owned_by(work);
    let Some(owner) = cli.owner.clone() else { return };
    // And what its legs made through their own sockets.
    let mut labels = vec![owner.clone()];
    labels.extend(super::docker_sock::runner_label(&owner));
    let [containers, networks, volumes] = remove_labelled(&cli, &labels, SWEEP).await;
    if containers + networks + volumes > 0 {
        tracing::info!(containers, networks, volumes, "removed what this runner left running when it last stopped");
    }
}

/// Removes what has one of `labels` (`key=value`): the containers (their anonymous volumes with
/// them), then the networks and volumes they may have used. How many of each it found.
async fn remove_labelled(cli: &Cli, labels: &[String], wait: Duration) -> [usize; 3] {
    let none = BTreeMap::new();
    let mut found = [0; 3];
    for (count, kind) in found.iter_mut().zip([None, Some("network"), Some("volume")]) {
        let mut ids = Vec::new();
        for label in labels {
            let mut ls = kind.map_or_else(|| vec![s("ps"), s("-aq")], |k| vec![s(k), s("ls"), s("-q")]);
            ls.extend([s("--filter"), format!("label={label}")]);
            ids.extend(cli.run(&ls, &none, wait).await.unwrap_or_default().lines().map(str::to_owned).filter(|l| !l.is_empty()));
        }
        ids.sort();
        ids.dedup();
        *count = ids.len();
        if ids.is_empty() {
            continue;
        }
        let mut rm = match kind {
            None => vec![s("rm"), s("--force"), s("--volumes")],
            Some("volume") => vec![s("volume"), s("rm"), s("--force")],
            Some(k) => vec![s(k), s("rm")],
        };
        rm.extend(ids);
        if let Err(e) = cli.run(&rm, &none, wait).await {
            tracing::warn!(error = %format!("{e:#}"), kind = kind.unwrap_or("container"), "what was left could not all be removed");
        }
    }
    found
}

/// The CLI, with the leg's own credential store.
#[derive(Debug, Clone)]
pub struct Cli {
    pub bin: String,
    config: PathBuf,
    /// kiln's registry (its host) and the registries it mirrors, `(upstream, prefix)`: images from
    /// those are pulled through kiln first (W12-R02).
    through: Option<(String, Vec<(String, String)>)>,
    /// The runner's [`owner_label`], on everything it starts.
    owner: Option<String>,
}

impl Cli {
    pub fn new(bin: &str, config: PathBuf) -> Cli {
        Cli { bin: bin.to_owned(), config, through: None, owner: None }
    }

    /// What it starts carries the label of the runner working in `work` ([`owner_label`]).
    #[must_use]
    pub fn owned_by(mut self, work: &Path) -> Cli {
        self.owner = Some(owner_label(work));
        self
    }

    /// The runner's label, when it has one.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// `--label <owner>`, for what it starts.
    pub fn owner_args(&self) -> Vec<String> {
        self.owner.iter().flat_map(|o| [s("--label"), o.clone()]).collect()
    }

    /// Pulls go through kiln's mirrors at `host` for the registries it mirrors.
    #[must_use]
    pub fn through(mut self, host: &str, mirrors: Vec<(String, String)>) -> Cli {
        if !mirrors.is_empty() {
            self.through = Some((host.to_owned(), mirrors));
        }
        self
    }

    /// The reference to pull `image` by through kiln, if kiln mirrors its registry.
    pub fn through_ref(&self, image: &str) -> Option<String> {
        let (host, mirrors) = self.through.as_ref()?;
        let r = crate::ci::integrations::oci::parse_reference(image).ok()?;
        let (_, prefix) = mirrors.iter().find(|(up, _)| *up == r.registry)?;
        let mut out = format!("{host}/{prefix}/{}", r.repository);
        if let Some(t) = &r.tag {
            out.push(':');
            out.push_str(t);
        }
        if let Some(d) = &r.digest {
            out.push('@');
            out.push_str(d);
        }
        Some(out)
    }

    /// Pulls `image`: through kiln's mirror when kiln mirrors its registry (tagged as `image`
    /// after), else, or when that fails, from its registry.
    pub async fn pull(&self, image: &str, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<()> {
        let none = BTreeMap::new();
        if let Some(via) = self.through_ref(image) {
            say(format!("{} pull {via} (kiln's mirror of {image})", self.bin));
            match self.run(&[s("pull"), s("--quiet"), via.clone()], &none, SLOW).await {
                Ok(_) => {
                    self.run(&[s("tag"), via, image.to_owned()], &none, CALL).await?;
                    return Ok(());
                }
                Err(e) => say(format!("kiln's mirror didn't have it ({e:#}); pulling from its registry")),
            }
        }
        say(format!("{} pull {image}", self.bin));
        self.run(&[s("pull"), s("--quiet"), image.to_owned()], &none, SLOW).await.map(|_| ())
    }

    /// `image` saved and unpacked into `dir` as it streams (Docker's archive: `manifest.json`, the
    /// config and the layers, plain tar), never whole on disk. Blocking.
    pub fn save_into(&self, image: &str, dir: &Path) -> anyhow::Result<()> {
        let mut child = self.command(&[s("save"), image.to_owned()], &BTreeMap::new()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
        let out = child.stdout.take().ok_or_else(|| anyhow::anyhow!("{} save: no output", self.bin))?;
        let unpacked = tar::Archive::new(out).unpack(dir);
        let done = child.wait_with_output()?;
        anyhow::ensure!(done.status.success(), "{} save {image}: {}", self.bin, String::from_utf8_lossy(&done.stderr).trim());
        unpacked.map_err(|e| anyhow::anyhow!("{} save {image}: unpacking it: {e}", self.bin))
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

    /// Sends `signal` (`INT`, `TERM`, `KILL`) to every process in `container` whose environment
    /// carries `marker` (`KILN_STEP=…`, a step's: what it started inherits it), as `user` (the
    /// step's, so it may): `docker exec` passes no signal on, so a cancel would otherwise reach only
    /// the CLI on the host. Found by their environments, not by a shell wrapped around the step, as
    /// a shell (dash) drops the names it can't take, GitHub's `INPUT_PERSIST-CREDENTIALS` among them.
    pub async fn signal_in(&self, container: &str, user: Option<&str>, marker: &str, signal: &str) {
        let mut a = vec![s("exec")];
        if let Some(u) = user {
            a.extend([s("--user"), u.to_owned()]);
        }
        let kill = format!(r#"for p in /proc/[0-9]*; do if tr '\000' '\n' < "$p/environ" 2>/dev/null | grep -qx "$0"; then kill -s {signal} "${{p#/proc/}}" 2>/dev/null; fi; done"#);
        a.extend([container.to_owned(), s("sh"), s("-c"), kill, marker.to_owned()]);
        if let Err(e) = self.run(&a, &BTreeMap::new(), Duration::from_secs(20)).await {
            tracing::debug!(%container, %signal, error = %format!("{e:#}"), "a step in a container couldn't be signalled (it may have ended)");
        }
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
/// loopback addresses as a container reaches them; and kiln's server's address, when it isn't on
/// this host ([`PathMap::with_server`]).
#[derive(Debug, Clone, Default)]
pub struct PathMap(Vec<(PathBuf, String)>, Option<String>);

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

/// The names a leg's containers have for the host and for kiln's server (`--add-host`, a
/// created container's `ExtraHosts`): `host.docker.internal` at the host's gateway (on the host's
/// network, its loopback), and [`super::runtime::NOT_GHES_HOST`] where the server is, the host
/// unless `server` says it's elsewhere. What a step's tool fetches from kiln through either
/// (docker's build actions' git context, from BuildKit's own container) reaches it.
pub fn container_hosts(host_net: bool, server: Option<&str>) -> Vec<String> {
    let host = if host_net { "127.0.0.1" } else { "host-gateway" };
    vec![format!("{HOST_ALIAS}:{host}"), format!("{}:{}", super::runtime::NOT_GHES_HOST, server.unwrap_or(host))]
}

/// kiln's server's address as a leg's containers reach it when it isn't on this host: `None` when
/// it is (its URL's host a loopback address or `localhost`), or when its name doesn't resolve to
/// an IPv4 address. Resolving blocks: call it off the runtime's threads.
pub fn server_address(server_url: &str) -> Option<String> {
    use std::net::ToSocketAddrs as _;
    let rest = server_url.split_once("://").map_or(server_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    if host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let ip = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip,
        Err(_) => (host, 0u16).to_socket_addrs().ok()?.map(|a| a.ip()).find(std::net::IpAddr::is_ipv4)?,
    };
    (ip.is_ipv4() && !ip.is_loopback()).then(|| ip.to_string())
}

impl PathMap {
    pub fn new(mut pairs: Vec<(PathBuf, String)>) -> PathMap {
        pairs.sort_by_key(|(h, _)| std::cmp::Reverse(h.as_os_str().len()));
        PathMap(pairs, None)
    }

    /// The map with kiln's server at `address` (its [`server_address`]), when it isn't on this host.
    pub fn with_server(mut self, address: Option<String>) -> PathMap {
        self.1 = address;
        self
    }

    /// kiln's server's address, when it isn't on this host.
    pub fn server(&self) -> Option<&str> {
        self.1.as_deref()
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

    /// A step's environment as a container sees it: each value [`translated`](Self::translate),
    /// and `GH_HOST` too, which names a host and port with no scheme: the host's loopback there is
    /// its alias, as in the URLs beside it (and the checkout's remote), or `gh` finds no remote of
    /// that host.
    pub fn translate_env<'a>(&self, env: impl IntoIterator<Item = (&'a String, &'a String)>) -> BTreeMap<String, String> {
        env.into_iter()
            .map(|(k, v)| {
                let v = if k == "GH_HOST" {
                    ["127.0.0.1:", "localhost:", "[::1]:"].iter().find_map(|lo| v.strip_prefix(lo)).map_or_else(|| self.translate(v), |port| format!("{HOST_ALIAS}:{port}"))
                } else {
                    self.translate(v)
                };
                (k.clone(), v)
            })
            .collect()
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
    /// The name its steps' user goes by (`USER`, `LOGNAME`): kiln's runner image's `runner`, as a
    /// hosted runner's steps have it (scripts lean on it: ORT's action makes its container's paths
    /// with `${PROJECT_PATH/$USER/ort}`). None in another image, where GitHub gives steps none.
    pub login: Option<String>,
    /// The image's `PATH`, to which `GITHUB_PATH` entries are prepended.
    pub path: String,
    /// It has `sh`, which a step's cancel reaches the step through ([`Cli::signal_in`]).
    pub has_sh: bool,
    pub has_bash: bool,
    /// Its C library is musl (Alpine's), so its JavaScript actions get a musl Node (W07-R05).
    pub musl: bool,
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
    /// A contained leg's relay (W18-R08): the network its published port is on, and that port
    /// on the host's loopback.
    pub relay: Option<(String, u16)>,
}

/// A leg whose network is contained (docs/ci.md W18-R08): the static kiln its relay runs, and the
/// token the runner's connections to it carry.
pub struct EgressSpec {
    pub binary: PathBuf,
    pub token: String,
}

/// Why `options:` would take a contained leg out of its network, or give it the host: `None`
/// when it wouldn't.
pub fn escapes(options: &str) -> Option<String> {
    const FLAGS: &[&str] = &["--network", "--net", "--privileged", "--cap-add", "--device", "--pid", "--ipc", "--uts", "--userns", "--cgroupns", "--security-opt", "--add-host", "--dns"];
    for word in split_options(options) {
        let flag = word.split('=').next().unwrap_or_default();
        if FLAGS.contains(&flag) {
            return Some(format!("`{flag}` would let it around its network policy"));
        }
        if word.contains("docker.sock") {
            return Some("the host's Docker socket would let it around its network policy".into());
        }
    }
    None
}

/// The proxy settings a contained leg's containers get: everything through the relay but their
/// own loopback and the leg's services.
pub fn proxy_env<'a>(services: impl Iterator<Item = &'a str>) -> Vec<(String, String)> {
    let url = format!("http://{}:{}", super::egress::ALIAS, super::egress::CLIENT_PORT);
    let mut skip = vec!["localhost".to_owned(), "127.0.0.1".to_owned(), "::1".to_owned()];
    skip.extend(services.map(str::to_owned));
    let none = skip.join(",");
    ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].iter().map(|k| ((*k).to_owned(), url.clone())).chain(["NO_PROXY", "no_proxy"].iter().map(|k| ((*k).to_owned(), none.clone()))).collect()
}

fn s(x: &str) -> String {
    x.to_owned()
}

impl Containers {
    /// Each container's image as named and its content-addressed id, by role (`job`,
    /// `service <key>`), for comparing runs (docs/ci.md W19-R03).
    pub async fn images(&self) -> Vec<(String, String, String)> {
        let mut ids: Vec<(String, String)> = self.job.iter().map(|j| (s("job"), j.id.clone())).collect();
        ids.extend(self.services.iter().map(|v| (format!("service {}", v.key), v.id.clone())));
        let mut out = Vec::new();
        for (role, id) in ids {
            if let Ok(o) = self.cli.run(&[s("inspect"), s("--format"), s("{{.Config.Image}} {{.Image}}"), id], &BTreeMap::new(), CALL).await
                && let Some((name, digest)) = o.trim().split_once(' ')
            {
                out.push((role, name.to_owned(), digest.to_owned()));
            }
        }
        out
    }

    /// Starts the leg's network, its services (awaited until healthy) and its job container.
    /// `say` reports progress for the leg's "Initialize containers" log.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(cli: Cli, leg: &str, map: PathMap, workdir: &str, job: Option<Spec>, services: Vec<(String, Spec)>, egress: Option<&EgressSpec>, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<Containers> {
        // The whole leg id (ids made in the same instant share their start) and a suffix of this
        // start's own, so a retry never meets what an earlier attempt left.
        let start = crate::util::new_id();
        let short = format!("{leg}_{}", start.get(start.len().saturating_sub(6)..).unwrap_or(&start));
        let label = format!("kiln.leg={leg}");
        // A contained leg never shares the host's network: its own leads nowhere.
        let shared = host_network() && egress.is_none();
        let network = if shared { s("host") } else { format!("kiln_{short}") };
        let none = BTreeMap::new();
        if let Some(why) = egress.and_then(|_| services.iter().map(|(_, x)| x).chain(job.iter()).find_map(|x| x.options.as_deref().and_then(escapes).or_else(|| x.volumes.iter().any(|v| v.contains("docker.sock")).then(|| "the host's Docker socket would let it around its network policy".to_owned())))) {
            anyhow::bail!("the leg's network is contained (`network:`, or untrusted code): {why}");
        }
        if !shared {
            let mut create = vec![s("network"), s("create"), s("--label"), label.clone()];
            create.extend(cli.owner_args());
            if egress.is_some() {
                create.push(s("--internal"));
            }
            create.push(network.clone());
            cli.run(&create, &none, CALL).await?;
        }
        let mut c = Containers { cli, network, label, job: None, services: Vec::new(), relay: None };
        // What a start that fails partway made goes with it: half a leg's containers and its
        // network would stay otherwise, the network holding one of the engine's few address pools.
        let filled = async {
            // The relay first, so the leg's way out is there before anything in it starts.
            if let Some(e) = egress {
                let image = job.as_ref().map(|j| j.image.clone()).or_else(|| services.first().map(|(_, x)| x.image.clone())).unwrap_or_default();
                say(format!("Starting the egress relay (in {image})"));
                let image = c.prepare(&image, None, say).await?;
                c.relay = Some(c.start_relay(&short, e, &image).await?);
            }
            let proxy = if egress.is_some() { proxy_env(services.iter().map(|(k, _)| k.as_str())) } else { Vec::new() };
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
                args.extend(c.cli.owner_args());
                if !shared {
                    args.extend([s("--network-alias"), key.clone()]);
                    for p in &spec.ports {
                        args.extend([s("-p"), p.clone()]);
                    }
                }
                let mut env: BTreeMap<String, String> = spec.env.iter().cloned().collect();
                for (k, v) in &proxy {
                    env.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for k in env.keys() {
                    args.extend([s("-e"), k.clone()]);
                }
                for v in &spec.volumes {
                    args.extend([s("-v"), v.clone()]);
                }
                args.extend(spec.options.as_deref().map(split_options).unwrap_or_default());
                args.push(image);
                let id = c.cli.run(&args, &env, CALL).await?;
                c.cli.run(&[s("start"), id.clone()], &none, CALL).await?;
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
                // A job's `container:` made for GitHub's runner user runs as this runner's.
                let as_runner = if user.is_none() { as_runner(&c.cli, &image).await } else { None };
                let mut args = vec![s("create"), s("--name"), format!("kiln_{short}_job"), s("--label"), c.label.clone(), s("--workdir"), workdir.to_owned(), s("--network"), c.network.clone()];
                for h in container_hosts(shared, map.server()) {
                    args.extend([s("--add-host"), h]);
                }
                args.extend(c.cli.owner_args());
                if let Some(u) = &as_runner {
                    args.extend([s("--user"), u.clone()]);
                }
                if shared {
                    // Services by name, as on a network of the leg's own.
                    for svc in &c.services {
                        args.extend([s("--add-host"), format!("{}:127.0.0.1", svc.key)]);
                    }
                }
                if spec.docker_socket && Path::new("/var/run/docker.sock").exists() {
                    let socket = spec.docker_proxy.as_ref().map_or_else(|| s("/var/run/docker.sock"), |p| p.to_string_lossy().into_owned());
                    args.extend([s("-v"), format!("{socket}:/var/run/docker.sock")]);
                    // The runner user reaches it through the socket's group, as the host's user does.
                    if user.is_some()
                        && let Some(gid) = socket_group()
                    {
                        args.extend([s("--group-add"), gid.to_string()]);
                    }
                }
                for (host, inside) in &map.0 {
                    // A workspace of its own size isn't the host's directory.
                    if spec.workspace_size.is_none() || inside != workdir {
                        args.extend([s("-v"), format!("{}:{inside}", host.to_string_lossy())]);
                    }
                }
                if spec.seccomp_unconfined {
                    args.extend([s("--security-opt"), s("seccomp=unconfined")]);
                }
                if let Some(bytes) = spec.workspace_size {
                    args.extend([s("--mount"), format!("type=tmpfs,destination={workdir},tmpfs-size={bytes},tmpfs-mode=1777")]);
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
                let id = c.cli.run(&args, &env, CALL).await?;
                c.cli.run(&[s("start"), id.clone()], &none, CALL).await?;
                let path = c.image_path(&image).await.unwrap_or_else(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
                // Its shells: `sh` (none in a distroless image), and `bash` if it has that too.
                let (has_sh, has_bash) = match c.cli.run(&[s("exec"), id.clone(), s("sh"), s("-c"), s("command -v bash || echo sh")], &none, CALL).await {
                    Ok(o) => (true, !o.is_empty() && o.trim() != "sh"),
                    Err(_) => (false, false),
                };
                let musl = c.cli.run(&[s("exec"), id.clone(), s("sh"), s("-c"), s(MUSL_PROBE)], &none, CALL).await.is_ok_and(|o| o.trim() == "musl");
                let (user, home, login) = match user {
                    Some((uid, gid)) => (Some(format!("{uid}:{gid}")), "/home/runner".to_owned(), Some(if uid == 0 { "root" } else { "runner" }.to_owned())),
                    None => (as_runner, "/github/home".to_owned(), None),
                };
                c.job = Some(JobContainer { id, user, home, login, path, has_sh, has_bash, musl, map });
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
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = filled {
            c.stop().await;
            if egress.is_some() && c.relay.is_none() {
                let _ = c.cli.run(&[s("network"), s("rm"), format!("{}_out", c.network)], &none, CALL).await;
            }
            return Err(e);
        }
        Ok(c)
    }

    /// Whether the engine runs rootless.
    async fn rootless(&self) -> bool {
        let none = BTreeMap::new();
        if self.cli.bin.ends_with("podman") {
            return self.cli.run(&[s("info"), s("--format"), s("{{.Host.Security.Rootless}}")], &none, CALL).await.is_ok_and(|o| o == "true");
        }
        self.cli.run(&[s("info"), s("--format"), s("{{json .SecurityOptions}}")], &none, CALL).await.is_ok_and(|o| o.contains("rootless"))
    }

    /// The image to run `image` as: kiln's runner images are built here ([`runner_image`]);
    /// others are pulled.
    async fn prepare(&self, image: &str, user: Option<(u32, u32)>, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<String> {
        if let Some(tag) = runner_image(&self.cli, image, user, say).await? {
            return Ok(tag);
        }
        self.cli.pull(image, say).await?;
        Ok(image.to_owned())
    }

    async fn image_path(&self, image: &str) -> Option<String> {
        let out = self.cli.run(&[s("image"), s("inspect"), s("--format"), s("{{json .Config.Env}}"), image.to_owned()], &BTreeMap::new(), CALL).await.ok()?;
        let env: Vec<String> = serde_json::from_str(&out).ok()?;
        env.iter().find_map(|e| e.strip_prefix("PATH=")).map(str::to_owned)
    }

    /// Waits for a container's health check to pass, when it has one.
    async fn healthy(&self, key: &str, id: &str, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<()> {
        let fmt = s("{{if .State.Health}}{{.State.Health.Status}}{{end}}");
        let start = std::time::Instant::now();
        let mut wait = Duration::from_secs(1);
        loop {
            // No answer isn't "no health check": ask again.
            let status = self.cli.run(&[s("inspect"), s("--format"), fmt.clone(), id.to_owned()], &BTreeMap::new(), CALL).await.unwrap_or_else(|e| format!("not answering ({e:#})"));
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
        let out = self.cli.run(&[s("port"), id.to_owned()], &BTreeMap::new(), CALL).await.unwrap_or_default();
        parse_ports(&out)
    }

    /// `job.container` and `job.services` (GitHub's contexts).
    pub fn contexts(&self) -> (serde_json::Value, serde_json::Value) {
        let job = self.job.as_ref().map_or(serde_json::Value::Null, |j| serde_json::json!({ "id": j.id, "network": self.network }));
        let services: serde_json::Map<String, serde_json::Value> = self.services.iter().map(|x| (x.key.clone(), serde_json::json!({ "id": x.id, "network": self.network, "ports": x.ports }))).collect();
        (job, serde_json::Value::Object(services))
    }

    /// Removes everything the leg started.
    /// A contained leg's relay (W18-R08): a static kiln in `image`, on a network of its own whose
    /// one port is published on the host's loopback for the runner, and on the leg's network as
    /// `kiln-egress`. Its network and host port.
    async fn start_relay(&self, short: &str, e: &EgressSpec, image: &str) -> anyhow::Result<(String, u16)> {
        let none = BTreeMap::new();
        let out = format!("kiln_{short}_out");
        let mut net = vec![s("network"), s("create"), s("--label"), self.label.clone()];
        net.extend(self.cli.owner_args());
        net.push(out.clone());
        self.cli.run(&net, &none, CALL).await?;
        let mut args = vec![s("create"), s("--name"), format!("kiln_{short}_egress"), s("--label"), self.label.clone()];
        args.extend(self.cli.owner_args());
        args.extend([
            s("--network"), out.clone(),
            s("-p"), format!("127.0.0.1::{}", super::egress::RUNNER_PORT), s("-e"), s("KILN_EGRESS_TOKEN"),
            s("-v"), format!("{}:/kiln-egress:ro", e.binary.display()), s("--entrypoint"), s("/kiln-egress"),
            // Nothing it needs but its two ports.
            s("--user"), s("65534:65534"), s("--read-only"), s("--cap-drop"), s("ALL"), s("--security-opt"), s("no-new-privileges"),
            image.to_owned(), s("egress-relay"),
        ]);
        let env: BTreeMap<String, String> = [(s("KILN_EGRESS_TOKEN"), e.token.clone())].into();
        let id = self.cli.run(&args, &env, CALL).await?;
        self.cli.run(&[s("network"), s("connect"), s("--alias"), s(super::egress::ALIAS), self.network.clone(), id.clone()], &none, CALL).await?;
        self.cli.run(&[s("start"), id.clone()], &none, CALL).await?;
        let ports = parse_ports(&self.cli.run(&[s("port"), id], &none, CALL).await?);
        let port = ports.get(&super::egress::RUNNER_PORT.to_string()).and_then(|p| p.parse().ok()).ok_or_else(|| anyhow::anyhow!("the egress relay's port wasn't published"))?;
        Ok((out, port))
    }

    /// Removes the leg's containers and networks (the relay's among them, all labelled the leg's)
    /// and what the leg made through its own socket ([`super::docker_sock`]), as the end of a
    /// machine of its own would.
    pub async fn stop(&self) {
        let made = super::docker_sock::leg_label(self.label.strip_prefix("kiln.leg=").unwrap_or(&self.label));
        remove_labelled(&self.cli, &[self.label.clone(), made], CALL).await;
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
        cli.pull(i, say).await?;
        return Ok(i.to_owned());
    }
    let none = BTreeMap::new();
    if cli.run(&[s("image"), s("inspect"), tag.to_owned()], &none, CALL).await.is_ok() {
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
pub fn action_run_args(
    name: &str,
    labels: &[String],
    user: Option<&str>,
    network: Option<&str>,
    mounts: &[String],
    server: Option<&str>,
    env: &BTreeMap<String, String>,
    entrypoint: Option<&str>,
    image: &str,
    args: &[String],
) -> Vec<String> {
    let mut a = vec![s("run"), s("--rm"), s("--name"), name.to_owned()];
    for l in labels {
        a.extend([s("--label"), l.clone()]);
    }
    if let Some(u) = user {
        a.extend([s("--user"), u.to_owned()]);
    }
    a.extend([s("--workdir"), s("/github/workspace")]);
    for h in container_hosts(network.is_none() && host_network(), server) {
        a.extend([s("--add-host"), h]);
    }
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

    /// A probe the engine hasn't answered in time is `false` for now, not for good: the one under
    /// way is waited for next time (not another started), and its answer is kept.
    #[test]
    fn a_probe_not_answered_in_time_is_asked_again() {
        static SLOW: Probe = Probe::new();
        static RAN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn slow() -> bool {
            RAN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            true
        }
        assert!(!SLOW.answer_within(Duration::from_millis(20), slow), "not answered yet");
        assert!(SLOW.answer_within(Duration::from_secs(10), slow), "the probe under way answers");
        let at = Instant::now();
        assert!(SLOW.answer_within(Duration::ZERO, slow) && at.elapsed() < Duration::from_millis(100), "kept");
        assert_eq!(RAN.load(std::sync::atomic::Ordering::SeqCst), 1, "one probe");
    }

    /// A start that fails partway takes what it made with it: here the leg's network, made before
    /// a service whose image there is no pulling.
    #[test]
    fn a_failed_start_leaves_nothing() -> anyhow::Result<()> {
        let Some(bin) = find() else { return Ok(()) };
        if host_network() {
            return Ok(());
        }
        let tmp = tempfile::tempdir()?;
        let leg = format!("test-failed-start-{}", std::process::id());
        let cli = Cli::new(&bin, tmp.path().join("config")).owned_by(tmp.path());
        let services = vec![("db".to_owned(), Spec { image: "kiln-test/no-such-image:never".into(), ..Spec::default() })];
        let (failed, left) = crate::rt::block_on(async move {
            let failed = Containers::start(cli.clone(), &leg, PathMap::new(Vec::new()), "/w", None, services, None, &|_| {}).await.is_err();
            let filter = [s("network"), s("ls"), s("-q"), s("--filter"), format!("label=kiln.leg={leg}")];
            Ok((failed, cli.run(&filter, &BTreeMap::new(), CALL).await?))
        })?;
        assert!(failed, "a service with no image can't start");
        assert!(left.is_empty(), "the leg's network is gone: {left}");
        Ok(())
    }

    #[test]
    fn w12_r02_pulls_through_kiln() {
        let cli = Cli::new("docker", PathBuf::from("/tmp/x")).through("kiln.example:7070", vec![("docker.io".into(), "docker.io".into()), ("ghcr.io".into(), "gh".into())]);
        assert_eq!(cli.through_ref("alpine:3.20").as_deref(), Some("kiln.example:7070/docker.io/library/alpine:3.20"));
        let d = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(cli.through_ref(&format!("ghcr.io/acme/app@{d}")), Some(format!("kiln.example:7070/gh/acme/app@{d}")));
        assert_eq!(cli.through_ref("quay.io/acme/app:1"), None, "a registry kiln doesn't mirror");
        assert_eq!(Cli::new("docker", PathBuf::from("/tmp/x")).through("kiln.example", vec![]).through_ref("alpine"), None);
    }

    /// kiln's server where a leg's containers find it: where the host is when it's on the host,
    /// its own address when it's elsewhere.
    #[test]
    fn a_legs_containers_find_the_server() {
        assert_eq!(container_hosts(false, None), ["host.docker.internal:host-gateway", "kiln.localhost:host-gateway"]);
        assert_eq!(container_hosts(true, None), ["host.docker.internal:127.0.0.1", "kiln.localhost:127.0.0.1"]);
        assert_eq!(container_hosts(false, Some("10.0.0.5")), ["host.docker.internal:host-gateway", "kiln.localhost:10.0.0.5"]);
        for on_host in ["http://127.0.0.1:42511", "http://localhost:7070/", "http://[::1]:7070", "https://LOCALHOST"] {
            assert_eq!(server_address(on_host), None, "{on_host}");
        }
        assert_eq!(server_address("http://192.168.49.2:30070/api"), Some("192.168.49.2".to_owned()));
        assert_eq!(server_address("https://user@10.1.2.3"), Some("10.1.2.3".to_owned()));
        assert_eq!(server_address("http://[2001:db8::1]:8080"), None, "an IPv6 server isn't mapped");
        assert_eq!(PathMap::new(Vec::new()).with_server(Some("10.0.0.5".to_owned())).server(), Some("10.0.0.5"));
    }

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
        assert!(full.contains("libssl-dev") && !d.contains("libssl-dev"), "the headers native builds look for (openssl-sys)");
        for tool in [
            "/usr/local/bin/snap",
            "ppa:git-core/ppa",
            "fakeroot",
            "python-is-python3",
            "shellcheck",
            "p7zip-full",
            "xvfb",
            "cmake",
            "clang",
            "maven",
            "ant",
            "sbt",
            "composer",
            "ruby-full",
            "ruby-bundler",
            "leiningen",
            "sh.rustup.rs",
            "nodejs.org/dist/latest-v22.x",
            "JAVA_HOME=",
            "Assume-Yes",
            "docker-buildx",
            "docker-compose",
            "CONDA=/usr/share/miniconda",
            "conda-forge/miniforge",
            "/usr/bin/pwsh",
            "libicu74",
            "cli/cli/releases",
            "https://go.dev/dl/$f",
            "PATH=/usr/local/go/bin:",
            "break-system-packages",
            "podman buildah skopeo",
        ] {
            assert!(full.contains(tool) && !d.contains(tool), "{tool}");
        }
        assert!(full.lines().filter(|l| l.ends_with('\\')).all(|l| l.ends_with(" \\")), "continuations: {full}");
        let resolute = runner_dockerfile("kiln/runner-ubuntu-26.04", None).unwrap_or_default();
        assert!(resolute.starts_with("FROM ubuntu:26.04\n") && resolute.contains(" ImageOS=ubuntu26\n") && resolute.contains("libicu78") && resolute.contains("break-system-packages"), "{resolute}");
        let jammy = runner_dockerfile("kiln/runner-ubuntu-22.04", None).unwrap_or_default();
        assert!(jammy.contains(" ImageOS=ubuntu22\n") && jammy.contains("libicu70") && !jammy.contains("break-system-packages"), "{jammy}");
        assert!(runner_dockerfile("ubuntu:24.04", None).is_none());
        assert!(runner_dockerfile("kiln/runner-centos", None).is_none());
        // The host's user, with sudo; root stays root.
        let u = runner_dockerfile("kiln/runner-ubuntu-24.04", Some((1000, 1000))).unwrap_or_default();
        assert!(u.contains("useradd -m -u 1000 -g 1000 -s /bin/bash runner") && u.contains("NOPASSWD:ALL"), "{u}");
        // `/home/runner/work`, GitHub's hosted runners' work directory, is the runner's: what a step
        // makes beside its repository's directory there is its own.
        assert!(u.contains("install -d -o 1000 -g 1000 /home/runner/work"), "{u}");
        assert!(u.trim_end().ends_with("chown -R 1000:1000 /usr/local/cargo /usr/local/rustup /usr/local/lib/node_modules /usr/local/bin /usr/share/dotnet /usr/share/miniconda"), "{u}");
        assert_eq!(runner_dockerfile("kiln/runner-ubuntu-24.04", Some((0, 0))), runner_dockerfile("kiln/runner-ubuntu-24.04", None));
        // `-android` (opt-in): the full image, then Android's SDK where GitHub's images have it,
        // its licence's acceptance on the image; never a slim one (sdkmanager needs the JDK).
        let android = runner_dockerfile("kiln/runner-ubuntu-24.04-android", None).unwrap_or_default();
        assert!(android.starts_with(&full), "the full image, and more");
        for want in [
            "ANDROID_HOME=/usr/local/lib/android/sdk ANDROID_SDK_ROOT=/usr/local/lib/android/sdk\n",
            "LABEL kiln.android-sdk-license=accepted\n",
            "https://dl.google.com/android/repository/$f",
            "echo \"e025545c62a8e64c7559119566a569fb1dec5f60  /tmp/$f\" | sha1sum -c -",
            "sdkmanager\" --licenses",
            "--install 'ndk;27.3.13750724' platform-tools",
            "ANDROID_NDK_ROOT=/usr/local/lib/android/sdk/ndk/27.3.13750724",
            "PATH=$PATH:/usr/local/lib/android/sdk/cmdline-tools/latest/bin:",
        ] {
            assert!(android.contains(want) && !full.contains(want), "{want}: {android}");
        }
        assert!(android.lines().filter(|l| l.ends_with('\\')).all(|l| l.ends_with(" \\")), "continuations: {android}");
        assert!(runner_dockerfile("kiln/runner-ubuntu-24.04-android", Some((1000, 1000))).is_some_and(|d| d.trim_end().ends_with("chown -R 1000:1000 /usr/local/lib/android")));
        assert_eq!(runner_dockerfile("kiln/runner-ubuntu-24.04-slim-android", None), None);
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

    /// A step's environment in a container: the host's loopback is its alias there, in URLs and
    /// in `GH_HOST` (a host and port alone), so `gh` finds the checkout's remote of that host.
    #[test]
    fn a_containers_environment_names_the_host_by_its_alias() {
        let m = PathMap::new(vec![(PathBuf::from("/w/leg/repo"), "/__w/repo/repo".into())]);
        let env: BTreeMap<String, String> =
            [("GH_HOST", "127.0.0.1:42511"), ("GITHUB_SERVER_URL", "http://127.0.0.1:42511"), ("GITHUB_WORKSPACE", "/w/leg/repo"), ("NO_PROXY", "localhost,127.0.0.1,::1")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        let inside = m.translate_env(&env);
        let get = |k: &str| inside.get(k).map(String::as_str);
        assert_eq!(get("GH_HOST"), Some("host.docker.internal:42511"));
        assert_eq!(get("GITHUB_SERVER_URL"), Some("http://host.docker.internal:42511"));
        assert_eq!(get("GITHUB_WORKSPACE"), Some("/__w/repo/repo"));
        assert_eq!(get("NO_PROXY"), Some("localhost,127.0.0.1,::1"), "a list of hosts isn't a URL");
        let elsewhere: BTreeMap<String, String> = [("GH_HOST".to_owned(), "kiln.example:8443".to_owned())].into_iter().collect();
        assert_eq!(m.translate_env(&elsewhere).get("GH_HOST").map(String::as_str), Some("kiln.example:8443"));
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
