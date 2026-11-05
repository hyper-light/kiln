//! The servers a round measures, each started in the server's namespace in a cgroup scope of its
//! own and stopped by the process the lab recorded (plan 01a1a832e9cc steps 3 and 5): kiln today
//! (its own TLS on its own listener, a runner whose `claude` is the stand-in, its terminal on, a lab
//! repository whose job writes stamped log lines), and the reference servers, h2o and Caddy, with
//! the origin they pass streams to. Every server speaks TLS 1.3 with the same lab chain: one
//! ECDSA P-256 leaf for 10.77.0.1 under a lab CA.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::link::{self, Ns};
use super::targets::{Server, Target};
use super::topo::{Lab, Scopes};
use crate::client::Http;
use crate::fleet::machine_key;

/// The lab's TLS chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Certs {
    pub ca: PathBuf,
    pub leaf: PathBuf,
    pub key: PathBuf,
    /// Leaf then CA, as servers send it.
    pub chain: PathBuf,
    /// The leaf's key's SHA-256 (base64), for Chromium's `--ignore-certificate-errors-spki-list`:
    /// Chromium takes no locally added root for QUIC (f31).
    pub spki: String,
}

fn sh(script: &str) -> anyhow::Result<String> {
    let o = Command::new("sh").args(["-c", script]).stdin(Stdio::null()).output()?;
    anyhow::ensure!(o.status.success(), "`{script}` failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// The chain in `dir`, made with openssl the first time.
pub fn certs(dir: &Path) -> anyhow::Result<Certs> {
    std::fs::create_dir_all(dir)?;
    let p = |f: &str| dir.join(f);
    if !p("chain.pem").exists() {
        let d = dir.display();
        sh(&format!(
            "set -e; cd '{d}'; \
             openssl ecparam -name prime256v1 -genkey -noout -out ca.key; \
             openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj '/CN=kiln browser lab CA' -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign' -out ca.pem; \
             openssl ecparam -name prime256v1 -genkey -noout -out leaf.ec; \
             openssl pkcs8 -topk8 -nocrypt -in leaf.ec -out leaf.key; \
             openssl req -new -key leaf.key -subj '/CN=10.77.0.1' -out leaf.csr; \
             printf 'subjectAltName=IP:10.77.0.1,DNS:lab.kiln,{control}\\nbasicConstraints=critical,CA:FALSE\\nkeyUsage=critical,digitalSignature\\nextendedKeyUsage=serverAuth\\n' > leaf.ext; \
             openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 13 -sha256 -extfile leaf.ext -out leaf.pem; \
             cat leaf.pem ca.pem > chain.pem",
            // The control pairs' addresses too: the probe page reaches the control echo there.
            control = (0..64).map(|i| format!("IP:{}", link::control_server_ip(i))).collect::<Vec<_>>().join(","),
        ))?;
    }
    let spki = sh(&format!("openssl x509 -in '{}' -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64", p("leaf.pem").display()))?;
    Ok(Certs { ca: p("ca.pem"), leaf: p("leaf.pem"), key: p("leaf.key"), chain: p("chain.pem"), spki })
}

/// A process of a server's, killed (its group with it) when dropped.
pub struct Proc {
    pub name: String,
    pub child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = Command::new("kill").arg("-KILL").arg(format!("-{}", self.child.id())).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a running target is.
pub struct Running {
    pub procs: Vec<Proc>,
    /// The server's scope, and the origin's (its CPU counted with the server's and shown apart).
    pub scope: PathBuf,
    pub origin_scope: Option<PathBuf>,
    pub kiln: Option<KilnLab>,
    /// The origin's own control socket (replays).
    pub replay_sock: Option<PathBuf>,
}

/// What a lab kiln gives the round: its admin token, its live sessions, its log job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KilnLab {
    pub admin: String,
    pub sessions: Vec<String>,
    pub job: Option<String>,
    pub run: Option<String>,
    pub plain_port: u16,
}

impl Running {
    /// The server's CPU so far (its scope's and its origin's), in µs.
    pub fn cpu_us(&self) -> (u64, u64) {
        let s = Scopes::cpu_us(&self.scope).unwrap_or(0);
        let o = self.origin_scope.as_ref().and_then(|d| Scopes::cpu_us(d).ok()).unwrap_or(0);
        (s, o)
    }

    /// The pids it was started as.
    pub fn pids(&self) -> Vec<(String, u32)> {
        self.procs.iter().map(|p| (p.name.clone(), p.child.id())).collect()
    }
}

/// Starts `argv` in the server's namespace, in scope `scope`, its output to `log`.
fn spawn(lab: &Lab, scope: &Path, name: &str, argv: &[String], env: &[(&str, String)], log: &Path) -> anyhow::Result<Proc> {
    let wrapped = Scopes::wrap(scope, argv);
    let mut cmd = lab.command(Ns::Server, &wrapped)?;
    let out = std::fs::File::create(log)?;
    // What points at another kiln (the runner the lab runs under) first, then the server's own.
    for v in ["KILN_SUPERVISOR", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_SUPERVISE_CHILD", "KILN_TAKE_OVER_FROM", "KILN_UPGRADE_ID", "KILN_REACH_SOCK", "KILN_REACH_FROM", "KILN_HARNESS_SOCK", "KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SERVER", "KILN_JOIN_TOKEN", "KILN_AGENT_NAME", "KILN_STATE_DIR", "KILN_INSTANCE", "KILN_LOG", "KILN_CHAOS", "KILN_MACHINE_ID", "GIT_CONFIG_GLOBAL"] {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out).spawn()?;
    Ok(Proc { name: name.to_owned(), child })
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

fn server_ip() -> Ipv4Addr {
    Ipv4Addr::new(10, 77, 0, 1)
}

/// Until `ok` says so, or `within` passes (then why not).
fn wait(within: Duration, what: &str, mut ok: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        if ok().unwrap_or(false) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{what} didn't come up within {} s", within.as_secs());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether something answers TCP at `port` on the server's address (from the link).
fn answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from((server_ip(), port)), Duration::from_millis(300)).is_ok()
}

/// What starting a target needs.
pub struct Env<'a> {
    pub lab: &'a Lab,
    pub scopes: &'a Scopes,
    pub certs: &'a Certs,
    pub web: &'a Path,
    /// The lab's stand-in programs (`claude`, `kiln-lab-term`, `kiln-lab-log`).
    pub bin: &'a Path,
    pub kiln: Option<&'a Path>,
    pub h2o: Option<&'a Path>,
    pub caddy: Option<&'a Path>,
    pub payloads: Option<&'a Path>,
    /// The terminal stand-in's pace, lines a second (each 96 bytes).
    pub term_per_s: f64,
    /// Whether the lab kiln runs its log job (a job's CI work is the server's work too).
    pub logs: bool,
    /// The stand-in agents' seed: the same seed says the same things in any run.
    pub seed: u64,
}

/// Starts `t` fresh under `dir`.
pub fn start(e: &Env<'_>, t: &Target, dir: &Path, sessions: usize) -> anyhow::Result<Running> {
    std::fs::create_dir_all(dir)?;
    match t.server {
        Server::Kiln => start_kiln(e, t, dir, sessions),
        Server::H2o | Server::Caddy => start_reference(e, t, dir),
    }
}

fn start_kiln(e: &Env<'_>, t: &Target, dir: &Path, sessions: usize) -> anyhow::Result<Running> {
    let kiln = e.kiln.ok_or_else(|| anyhow::anyhow!("kiln today needs --kiln"))?;
    let scope = e.scopes.scope(&format!("srv-{}", t.name))?;
    let data = dir.join("data");
    let home = dir.join("home");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&home)?;
    let port = t.port;
    let quic = port + 1;
    let term = e.bin.join("kiln-lab-term");
    let config = format!(
        r#"[server]
http_addr = "0.0.0.0:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "https://10.77.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = true
https_cert = "{chain}"
https_key = "{key}"

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[terminal]
enabled = true
shell = "{term}"

[kubernetes]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = false
"#,
        data = data.display(),
        chain = e.certs.chain.display(),
        key = e.certs.key.display(),
        term = term.display(),
    );
    std::fs::write(dir.join("kiln.toml"), config)?;
    let mut procs = Vec::new();
    let env = vec![("HOME", home.display().to_string()), ("KILN_LAB_TERM", format!("{}:96", e.term_per_s)), ("KILN_MACHINE_ID", format!("lab server {}", dir.display())), ("KILN_WARM_FLOOR", "0".to_owned()), ("NO_COLOR", "1".to_owned())];
    procs.push(spawn(e.lab, &scope, "kiln server", &[kiln.display().to_string(), s("--config"), dir.join("kiln.toml").display().to_string(), s("server")], &env, &dir.join("server.log"))?);
    let mut admin = String::new();
    wait(Duration::from_secs(120), "the lab kiln", || {
        let t = std::fs::read_to_string(data.join("admin.token"))?;
        admin = t.trim().to_owned();
        let a = Http::at(server_ip(), port, &admin).send("GET", "/api/me", None)?;
        Ok(a.status == 200)
    })?;
    // One runner, beside it, its `claude` the stand-in.
    let join = std::fs::read_to_string(data.join("join.token"))?.trim().to_owned();
    let rhome = dir.join("runner-home");
    std::fs::create_dir_all(&rhome)?;
    let path = std::env::join_paths(std::iter::once(e.bin.to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))?;
    let renv = vec![("HOME", rhome.display().to_string()), ("KILN_LAB_SEED", e.seed.to_string()), ("KILN_MACHINE_ID", "lab-runner-0".to_owned()), ("KILN_WARM_FLOOR", "0".to_owned()), ("NO_COLOR", "1".to_owned()), ("PATH", path.to_string_lossy().into_owned())];
    let rscope = e.scopes.scope(&format!("runner-{}", t.name))?;
    procs.push(spawn(
        e.lab,
        &rscope,
        "kiln runner",
        &[
            kiln.display().to_string(),
            s("agent"),
            s("--server"),
            format!("http://127.0.0.1:{port}"),
            s("--token"),
            join,
            s("--name"),
            s("lab-runner-0"),
            s("--work-dir"),
            dir.join("runner-work").display().to_string(),
            s("--state-dir"),
            dir.join("runner-state").display().to_string(),
        ],
        &renv,
        &dir.join("runner.log"),
    )?);
    let mut http = Http::at(server_ip(), port, &admin);
    let key = machine_key("lab-runner-0");
    wait(Duration::from_secs(120), "the lab runner", || {
        let a = http.send("GET", "/api/agents", None)?;
        let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        Ok(list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(key.as_str()) && a.get("connected").and_then(Value::as_bool) == Some(true)))
    })?;
    let path = format!("/api/runners/{key}/headroom");
    let _ = http.send("PUT", &path, Some(json!({"enabled": false}).to_string().as_bytes()));
    // Sessions with some history, as an agent page opens one.
    let mut ids = Vec::new();
    for i in 0..sessions {
        let cwd = dir.join(format!("work-{i}"));
        std::fs::create_dir_all(&cwd)?;
        let open = json!({"target": format!("agent:{key}"), "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": "lab:prefill:200"}});
        let mut id = None;
        wait(Duration::from_secs(60), "a lab session", || {
            let a = http.send("POST", "/api/harness/sessions", Some(open.to_string().as_bytes()))?;
            let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
            id = v.pointer("/0/id").and_then(Value::as_str).map(str::to_owned);
            Ok(a.status == 200 && id.is_some())
        })?;
        ids.extend(id);
    }
    // Their history in, and idle.
    wait(Duration::from_secs(120), "the sessions' history", || {
        let a = http.send("GET", "/api/harness/sessions", None)?;
        let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("sessions").and_then(Value::as_array).cloned()).unwrap_or_default();
        Ok(ids.iter().all(|id| list.iter().any(|s| s.get("id").and_then(Value::as_str) == Some(id) && s.get("status").and_then(Value::as_str).is_some_and(|st| st != "running" && st != "starting"))))
    })?;
    let (run, job) = match if e.logs { log_job(&mut http, dir, e.bin, e.term_per_s) } else { Err(anyhow::anyhow!("not asked for")) } {
        Ok((r, j)) => (Some(r), Some(j)),
        Err(err) if !e.logs => {
            let _ = err;
            (None, None)
        }
        Err(err) => {
            eprintln!("kiln-lab: the lab kiln has no log job: {err:#}");
            (None, None)
        }
    };
    Ok(Running { procs, scope, origin_scope: Some(rscope), kiln: Some(KilnLab { admin, sessions: ids, job, run, plain_port: port }), replay_sock: None })
}

/// A lab repository whose one job writes stamped lines for fifteen minutes: its run's and its job's
/// ids once it runs.
fn log_job(http: &mut Http, dir: &Path, bin: &Path, lines_per_s: f64) -> anyhow::Result<(String, String)> {
    let repo = dir.join("lab-repo");
    let d = repo.display();
    let log = bin.join("kiln-lab-log").display().to_string();
    let per_s = lines_per_s;
    sh(&format!(
        "set -e; rm -rf '{d}'; mkdir -p '{d}/.kiln'; cd '{d}'; git init -q -b main; \
         printf 'name: lab\\non:\\n  push: {{ branches: [main] }}\\n  manual: true\\njobs:\\n  log:\\n    runs-on: [baremetal]\\n    steps:\\n      - run: {log} 900 {per_s} 96\\n' > .kiln/ci.yaml; \
         git add -A; git -c user.name=lab -c user.email=lab@localhost commit -qm lab"
    ))?;
    let a = http.send("POST", "/api/repos", Some(json!({"name": "lab/lab", "clone_url": format!("file://{d}"), "branches": ["main"]}).to_string().as_bytes()))?;
    anyhow::ensure!(a.status < 300, "the lab repository: {} {}", a.status, String::from_utf8_lossy(&a.body));
    let v: Value = serde_json::from_slice(&a.body)?;
    let id = v.get("id").map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned)).ok_or_else(|| anyhow::anyhow!("no repository id"))?;
    let a = http.send("POST", &format!("/api/repos/{id}/trigger"), Some(json!({"branch": "main", "pipeline": "lab"}).to_string().as_bytes()))?;
    anyhow::ensure!(a.status < 300, "the lab run: {} {}", a.status, String::from_utf8_lossy(&a.body));
    let v: Value = serde_json::from_slice(&a.body)?;
    let run = v.pointer("/0/id").map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned)).ok_or_else(|| anyhow::anyhow!("no run in {v}"))?;
    let mut job = None;
    let mut last = Value::Null;
    let up = wait(Duration::from_secs(120), "the lab job", || {
        let a = http.send("GET", &format!("/api/runs/{run}"), None)?;
        let v: Value = serde_json::from_slice(&a.body)?;
        let j = v.pointer("/jobs/0").cloned().unwrap_or_default();
        job = j.get("id").map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned));
        last = json!({"run": v.pointer("/run/status"), "job": j.get("status"), "said": v.to_string().chars().take(700).collect::<String>()});
        Ok(j.get("status").and_then(Value::as_str) == Some("running"))
    });
    up.map_err(|e| anyhow::anyhow!("{e}: last {last}"))?;
    Ok((run, job.ok_or_else(|| anyhow::anyhow!("no job"))?))
}

fn start_reference(e: &Env<'_>, t: &Target, dir: &Path) -> anyhow::Result<Running> {
    let payloads = e.payloads.ok_or_else(|| anyhow::anyhow!("the reference servers need --payloads (step 3's replay)"))?;
    let docroot = payloads.join("docroot");
    let scope = e.scopes.scope(&format!("srv-{}", t.name))?;
    let oscope = e.scopes.scope(&format!("origin-{}", t.name))?;
    let (sock, ctl) = (dir.join("origin.sock"), dir.join("replay.sock"));
    let mut procs = vec![spawn(e.lab, &oscope, "origin", &[s("node"), e.web.join("origin.mjs").display().to_string(), payloads.display().to_string(), sock.display().to_string(), ctl.display().to_string()], &[], &dir.join("origin.log"))?];
    wait(Duration::from_secs(20), "the origin", || Ok(sock.exists() && ctl.exists()))?;
    let port = t.port;
    let (prog, argv, conf) = match t.server {
        Server::H2o => {
            let bin = e.h2o.ok_or_else(|| anyhow::anyhow!("{} needs --h2o", t.name))?;
            let conf = dir.join("h2o.conf");
            std::fs::write(&conf, h2o_conf(t, e.certs, &docroot, &sock, port))?;
            // h2o started as root becomes `nobody`, which the lab's namespace doesn't map: in a user
            // namespace of its own whose only uid is the lab's own, it is no one's root to drop.
            (PathBuf::from("unshare"), vec![s("--user"), s("--map-user=1000"), s("--map-group=1000"), s("--"), bin.display().to_string(), s("-c"), conf.display().to_string()], conf)
        }
        Server::Caddy => {
            let bin = e.caddy.ok_or_else(|| anyhow::anyhow!("{} needs --caddy", t.name))?;
            let conf = dir.join("Caddyfile");
            std::fs::write(&conf, caddy_conf(t, e.certs, &docroot, &sock, port))?;
            (bin.to_path_buf(), vec![s("run"), s("--config"), conf.display().to_string(), s("--adapter"), s("caddyfile")], conf)
        }
        Server::Kiln => anyhow::bail!("not a reference server"),
    };
    let _ = conf;
    let mut full = vec![prog.display().to_string()];
    full.extend(argv);
    procs.push(spawn(e.lab, &scope, &t.name, &full, &[("HOME", dir.display().to_string()), ("XDG_DATA_HOME", dir.join("xdg").display().to_string()), ("XDG_CONFIG_HOME", dir.join("xdg").display().to_string())], &dir.join("server.log"))?);
    wait(Duration::from_secs(30), &t.name, || Ok(answers(port)))?;
    Ok(Running { procs, scope, origin_scope: Some(oscope), kiln: None, replay_sock: Some(ctl) })
}

/// h2o's config for `t`: TLS 1.3 with the lab chain; static bodies from the docroot, compressed
/// (brotli, then gzip) as h2o does best; streams and writes passed to the origin over its socket;
/// HTTP/3 on the same port when the target is HTTP/3, its congestion controller `t.quic_cc`.
pub fn h2o_conf(t: &Target, c: &Certs, docroot: &Path, sock: &Path, port: u16) -> String {
    let ssl = format!("    ssl:\n      certificate-file: {}\n      key-file: {}\n      minimum-version: TLSv1.3\n", c.chain.display(), c.key.display());
    let mut listen = format!("listen:\n  port: {port}\n  host: 0.0.0.0\n{}", ssl.replace("    ", "  "));
    if t.http3 {
        let cc = t.quic_cc.as_deref().unwrap_or("cubic");
        listen.push_str(&format!("listen:\n  port: {port}\n  host: 0.0.0.0\n  type: quic\n  cc: {cc}\n{}", ssl.replace("    ", "  ")));
    }
    format!(
        "# kiln browser lab: {name} ({about})\n{listen}\
         num-threads: 1\n\
         http2-idle-timeout: 600\n\
         http3-idle-timeout: 600\n\
         proxy.timeout.io: 600000\n\
         hosts:\n  \"10.77.0.1:{port}\":\n    paths:\n      /stream:\n        proxy.reverse.url: \"http://[unix:{sock}]/stream\"\n        compress: [ br, gzip ]\n      /fetch-stream:\n        proxy.reverse.url: \"http://[unix:{sock}]/fetch-stream\"\n        compress: [ br, gzip ]\n      /__lab:\n        proxy.reverse.url: \"http://[unix:{sock}]/__lab\"\n      /api/harness/sessions/lab/command:\n        proxy.reverse.url: \"http://[unix:{sock}]/api/harness/sessions/lab/command\"\n      /:\n        file.dir: {docroot}\n        file.mime.setdefaulttype: application/json\n        file.index: [ index.json ]\n        compress: [ br, gzip ]\n        header.set: \"cache-control: no-store\"\n",
        name = t.name,
        about = t.about,
        sock = sock.display(),
        docroot = docroot.display(),
    )
}

/// Caddy's config for `t`: the same, with HTTP/3 (quic-go, its Cubic: f35) only when the target is.
pub fn caddy_conf(t: &Target, c: &Certs, docroot: &Path, sock: &Path, port: u16) -> String {
    let protocols = if t.http3 { "h1 h2 h3" } else { "h1 h2" };
    format!(
        "# kiln browser lab: {name} ({about})\n{{\n\tadmin off\n\tauto_https off\n\tservers {{\n\t\tprotocols {protocols}\n\t\ttimeouts {{\n\t\t\tidle 10m\n\t\t}}\n\t}}\n}}\n\
         https://10.77.0.1:{port} {{\n\ttls {chain} {key} {{\n\t\tprotocols tls1.3\n\t}}\n\
         \t@origin path /stream/* /fetch-stream/* /__lab/* /api/harness/sessions/lab/command\n\
         \thandle @origin {{\n\t\tencode zstd gzip\n\t\treverse_proxy unix/{sock} {{\n\t\t\tflush_interval -1\n\t\t}}\n\t}}\n\
         \thandle {{\n\t\troot * {docroot}\n\t\theader cache-control no-store\n\t\theader content-type application/json\n\t\tencode zstd gzip\n\t\tfile_server {{\n\t\t\tindex index.json\n\t\t}}\n\t}}\n}}\n",
        name = t.name,
        about = t.about,
        chain = c.chain.display(),
        key = c.key.display(),
        sock = sock.display(),
        docroot = docroot.display(),
    )
}

/// The control echo on the server's control pairs.
pub fn start_control(e: &Env<'_>, dir: &Path) -> anyhow::Result<Proc> {
    let scope = e.scopes.scope("control")?;
    let p = spawn(e.lab, &scope, "control", &[s("node"), e.web.join("control.mjs").display().to_string(), e.certs.chain.display().to_string(), e.certs.key.display().to_string(), s(CONTROL_PORT)], &[], &dir.join("control.log"))?;
    wait(Duration::from_secs(20), "the control echo", || Ok(std::net::TcpStream::connect_timeout(&format!("{}:{CONTROL_PORT}", link::control_server_ip(0)).parse()?, Duration::from_millis(300)).is_ok() || answers(CONTROL_PORT)))?;
    Ok(p)
}

pub const CONTROL_PORT: u16 = 9001;

/// Asks the origin to replay `events` of `kind`, `every_ms` apart.
pub fn replay(sock: &Path, kind: &str, events: u64, every_ms: u64, from: u64) -> anyhow::Result<Value> {
    use std::io::{Read as _, Write as _};
    let body = json!({"kind": kind, "events": events, "every_ms": every_ms, "from": from}).to_string();
    let mut c = std::os::unix::net::UnixStream::connect(sock)?;
    write!(c, "POST /replay HTTP/1.1\r\nHost: origin\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut out = String::new();
    c.read_to_string(&mut out)?;
    let (_, b) = out.split_once("\r\n\r\n").unwrap_or(("", ""));
    anyhow::ensure!(out.starts_with("HTTP/1.1 200"), "replay: {}", out.lines().next().unwrap_or(""));
    Ok(serde_json::from_str(b).unwrap_or_default())
}

/// Sends a lab kiln's session a message (`lab:turn:<n>:<ms>`: n stamped events, ms apart).
pub fn turn(k: &KilnLab, session: &str, n: u64, every_ms: u64, id: u64) -> anyhow::Result<()> {
    let mut http = Http::at(server_ip(), k.plain_port, &k.admin);
    let body = json!({"type": "send", "id": format!("lab-{id}"), "text": format!("lab:turn:{n}:{every_ms}")}).to_string();
    let a = http.send("POST", &format!("/api/harness/sessions/{session}/command"), Some(body.as_bytes()))?;
    anyhow::ensure!(a.status < 300, "a turn: {} {}", a.status, String::from_utf8_lossy(&a.body));
    Ok(())
}
