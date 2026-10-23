//! W13-R07 (docs/ci.md): a `k8s` pool's executors are pods this server starts, each its own
//! runner for one leg. Against a stand-in for Kubernetes's API that runs each pod's job container
//! here, as a kubelet would (its init container aside: the pod's `kiln` is this build's).

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

/// A pod the stand-in keeps: its object, and the process its job container is.
struct Pod {
    object: Value,
    child: Option<Child>,
}

#[derive(Default)]
struct Seen {
    created: Vec<Value>,
    deleted: Vec<String>,
    /// The most pods alive at once.
    peak: usize,
    /// The pods alive now, by name: their processes, which go when the stand-in does.
    alive: BTreeMap<String, u32>,
}

struct Kube {
    port: u16,
    seen: Arc<Mutex<Seen>>,
}

/// The pods still alive as a test ends go with it, as a cluster's would with the test's namespace:
/// left, each would go on trying for a server that's gone.
impl Drop for Kube {
    fn drop(&mut self) {
        let alive: Vec<u32> = self.seen.lock().map(|s| s.alive.values().copied().collect()).unwrap_or_default();
        for pid in alive {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while let Some(&c) = b.get(i) {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (c, hex) {
            (b'%', Some(h)) => {
                out.push(h);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A pod's phase, from its process.
fn phase(p: &mut Pod) -> &'static str {
    match p.child.as_mut().map(Child::try_wait) {
        Some(Ok(None)) => "Running",
        Some(Ok(Some(s))) if s.success() => "Succeeded",
        Some(_) => "Failed",
        None => "Pending",
    }
}

fn with_status(p: &mut Pod) -> Value {
    let mut o = p.object.clone();
    let ph = phase(p);
    if let Some(m) = o.as_object_mut() {
        m.insert("status".into(), json!({ "phase": ph }));
    }
    o
}

/// Runs a pod's job container here: its command with `/kiln-bin/kiln` this build's, `/work` a
/// folder of its own, its environment.
fn run(dir: &Path, pod: &Value) -> Option<Child> {
    let name = pod.pointer("/metadata/name").and_then(Value::as_str)?;
    let c = pod.pointer("/spec/containers/0")?;
    let home = dir.join("pods").join(name);
    let work = home.join("work");
    std::fs::create_dir_all(&work).ok()?;
    let command: Vec<String> =
        c.get("command").and_then(Value::as_array)?.iter().filter_map(Value::as_str).map(|a| if a == "/work" { work.to_string_lossy().into_owned() } else { a.to_owned() }).collect();
    let (prog, args) = command.split_first()?;
    let prog: PathBuf = if prog == "/kiln-bin/kiln" { PathBuf::from(env!("CARGO_BIN_EXE_kiln")) } else { PathBuf::from(prog) };
    let log = std::fs::File::create(home.join("log")).ok()?;
    let mut cmd = Command::new(prog);
    cmd.args(args).current_dir(&work).env("HOME", &home).env("NO_COLOR", "1").stdin(Stdio::null()).stdout(log.try_clone().ok()?).stderr(log);
    // Not the test's server, nor this machine's login shell (a container's terminal opens the
    // image's).
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_CHAOS", "KUBECONFIG", "SHELL"] {
        cmd.env_remove(var);
    }
    // Nor this machine's container engine: a pod has none, its own sidecars aside.
    cmd.env("DOCKER_HOST", "unix:///nonexistent/docker.sock").env("CONTAINER_HOST", "unix:///nonexistent/podman.sock");
    for e in c.get("env").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(k), Some(v)) = (e.get("name").and_then(Value::as_str), e.get("value").and_then(Value::as_str)) {
            cmd.env(k, v);
        }
    }
    // A Docker sidecar (W13-R07) is this machine's engine, and the CLI it copies in is this
    // machine's.
    let sidecar = pod.pointer("/spec/initContainers").and_then(Value::as_array).is_some_and(|cs| cs.iter().any(|c| c.get("name").and_then(Value::as_str) == Some("docker")));
    if sidecar {
        match std::env::var_os("DOCKER_HOST") {
            Some(h) => cmd.env("DOCKER_HOST", h),
            None => cmd.env_remove("DOCKER_HOST"),
        };
        cmd.env_remove("CONTAINER_HOST");
        if let Some(cli) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|d| d.join("docker")).find(|p| p.is_file()) {
            cmd.env("KILN_POD_DOCKER", cli);
        }
    }
    cmd.spawn().ok()
}

impl Kube {
    /// The stand-in, answering `/api/v1/namespaces/<ns>/pods[/<name>]`: create, get, list by
    /// label, delete.
    fn start(dir: &Path) -> anyhow::Result<Kube> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (keep, dir) = (Arc::clone(&seen), dir.to_path_buf());
        std::thread::spawn(move || {
            let mut pods: BTreeMap<String, Pod> = BTreeMap::new();
            for mut stream in listener.incoming().flatten() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let (head, body) = loop {
                    let Ok(n) = stream.read(&mut chunk) else { break (String::new(), String::new()) };
                    if n == 0 {
                        break (String::new(), String::new());
                    }
                    buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if rest.len() >= len {
                            break (head.to_owned(), rest.to_owned());
                        }
                    }
                };
                let mut words = head.split_whitespace();
                let (method, target) = (words.next().unwrap_or_default().to_owned(), words.next().unwrap_or_default().to_owned());
                let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
                let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
                let (status, answer) = match (method.as_str(), parts.as_slice()) {
                    ("GET", ["api", "v1", "namespaces", _, "pods"]) => {
                        let selector = query.split('&').find_map(|kv| kv.strip_prefix("labelSelector=")).map(decode).unwrap_or_default();
                        let want: Vec<(String, String)> = selector.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
                        let items: Vec<Value> = pods
                            .values_mut()
                            .filter(|p| want.iter().all(|(k, v)| p.object.pointer("/metadata/labels").and_then(|l| l.get(k)).and_then(Value::as_str) == Some(v.as_str())))
                            .map(with_status)
                            .collect();
                        (200, json!({ "apiVersion": "v1", "kind": "PodList", "items": items }))
                    }
                    ("POST", ["api", "v1", "namespaces", _, "pods"]) => {
                        let object: Value = serde_json::from_str(&body).unwrap_or_default();
                        let name = object.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let child = run(&dir, &object);
                        if let Ok(mut s) = keep.lock() {
                            s.created.push(object.clone());
                            if let Some(c) = &child {
                                s.alive.insert(name.clone(), c.id());
                            }
                        }
                        pods.insert(name, Pod { object: object.clone(), child });
                        (201, object)
                    }
                    ("GET", ["api", "v1", "namespaces", _, "pods", name]) => match pods.get_mut(*name) {
                        Some(p) => (200, with_status(p)),
                        None => (404, json!({ "kind": "Status", "reason": "NotFound", "message": format!("pods \"{name}\" not found") })),
                    },
                    ("DELETE", ["api", "v1", "namespaces", _, "pods", name]) => match pods.remove(*name) {
                        Some(mut p) => {
                            if let Some(c) = p.child.as_mut() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            if let Ok(mut s) = keep.lock() {
                                s.deleted.push((*name).to_owned());
                                s.alive.remove(*name);
                            }
                            (200, json!({ "kind": "Status", "status": "Success" }))
                        }
                        None => (404, json!({ "kind": "Status", "reason": "NotFound", "message": format!("pods \"{name}\" not found") })),
                    },
                    _ => (404, json!({ "kind": "Status", "reason": "NotFound", "message": format!("no {method} {path}") })),
                };
                let alive = pods.values_mut().map(phase).filter(|ph| *ph == "Running").count();
                if let Ok(mut s) = keep.lock() {
                    s.peak = s.peak.max(alive);
                }
                let text = answer.to_string();
                let _ = stream.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes());
            }
        });
        Ok(Kube { port, seen })
    }

    fn kubeconfig(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let path = dir.join("kubeconfig");
        std::fs::write(
            &path,
            format!(
                "apiVersion: v1\nkind: Config\ncurrent-context: test\nclusters:\n- name: test\n  cluster:\n    server: http://127.0.0.1:{}\nusers:\n- name: test\n  user:\n    token: stand-in\ncontexts:\n- name: test\n  context:\n    cluster: test\n    user: test\n    namespace: ci\n",
                self.port
            ),
        )?;
        Ok(path)
    }
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

const WORKFLOW: &str = r#"on: push
jobs:
  a:
    runs-on: pod
    steps:
      - run: echo "ran in $RUNNER_NAME"
  b:
    runs-on: pod
    steps:
      - run: echo "ran in $RUNNER_NAME"
  c:
    runs-on: pod
    steps:
      - run: echo "ran in $RUNNER_NAME"
"#;

/// Three legs on a `k8s` pool of at most two pods: each runs in a pod of its own (its runner the
/// pod's, bound to its executor), never more than two at once; each pod goes with its leg, and the
/// pool is empty after.
#[test]
fn w13_r07_k8s_pool_runs_each_leg_in_a_pod() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let kube = Kube::start(dir.path())?;
    let kubeconfig = kube.kubeconfig(dir.path())?;
    let pool = "\n[[ci.pools]]\nname = \"pods\"\nexecutor = \"k8s\"\nlabels = [\"pod\"]\nmax = 2\nidle_timeout = \"30s\"\n";
    let extra = "[kubernetes]\nenabled = true\nnamespace = \"ci\"\nagent_image = \"kiln-agent:test\"\n";
    let s = start_env(dir.path(), &Options { agent: false, ci: pool, extra }, false, &[("KUBECONFIG", &kubeconfig.to_string_lossy())])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".github/workflows/pods.yml"), WORKFLOW)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "pods"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 240, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then(|| body.clone()))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let pods_log = std::fs::read_dir(dir.path().join("pods"))
        .map(|d| d.flatten().map(|e| std::fs::read_to_string(e.path().join("log")).unwrap_or_default()).collect::<Vec<_>>().join("\n---\n"))
        .unwrap_or_default();
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{detail}\n{pods_log}\n{}", s.log());
    let legs: Vec<Value> = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut runners: Vec<String> = legs.iter().filter_map(|l| l.get("runner").and_then(Value::as_str).map(str::to_owned)).collect();
    assert_eq!(runners.len(), 3, "{detail}");
    assert!(runners.iter().all(|r| r.starts_with("kiln-x-pool-pods-")), "each on its pod's runner: {runners:?}");
    runners.sort();
    runners.dedup();
    assert_eq!(runners.len(), 3, "a pod a leg");
    // The pods: as the pool says, at most two at once, each gone with its leg.
    until(&s, 60, "the pods to go", || {
        let seen = kube.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok((seen.deleted.len() < 3).then(|| format!("{} of {} deleted", seen.deleted.len(), seen.created.len())))
    })?;
    {
        let seen = kube.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        assert_eq!(seen.created.len(), 3, "{:?}", seen.created);
        assert!(seen.peak <= 2, "max 2 at once: {}", seen.peak);
        let pod = seen.created.first().cloned().unwrap_or_default();
        assert_eq!(pod.pointer("/spec/initContainers/0/image"), Some(&json!("kiln-agent:test")), "{pod}");
        let command: Vec<&str> = pod.pointer("/spec/containers/0/command").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        assert_eq!(command.get(1..3), Some(&["agent", "--ephemeral"][..]), "{pod}");
        assert!(command.windows(2).any(|w| w == ["--labels", "pod"]), "{pod}");
        assert!(pod.pointer("/metadata/annotations/kiln.dev~1executor").and_then(Value::as_str).is_some_and(|e| e.starts_with("pool-pods-")), "{pod}");
    }
    until(&s, 60, "the pool to empty", || {
        let (_, body) = http(s.port, "GET", "/api/ci/pools/pods", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        let n = v.pointer("/pool/executors").or_else(|| v.get("executors")).and_then(Value::as_array).map_or(0, Vec::len);
        Ok((n > 0).then_some(body))
    })?;
    Ok(())
}

/// A leg with `container:` and `services:` on a `k8s` pool (W13-R07): a pod made for it, its job
/// container the pod's main one (its image evaluated with the leg's matrix), its service a native
/// sidecar `hostAliases` names, and its steps run where the pod's runner is, with no container of
/// the runner's own: the service awaited on its port, its `job.services` ports as the pod has them.
#[test]
fn w13_r07_a_leg_with_containers_runs_in_a_pod_made_for_it() -> anyhow::Result<()> {
    // The service's sidecar (the stand-in runs only a pod's job container): a listener on its port.
    let sidecar = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = sidecar.local_addr()?.port();
    std::thread::spawn(move || {
        for c in sidecar.incoming() {
            drop(c);
        }
    });
    let dir = tempfile::tempdir()?;
    let kube = Kube::start(dir.path())?;
    let kubeconfig = kube.kubeconfig(dir.path())?;
    let pool = "\n[[ci.pools]]\nname = \"pods\"\nexecutor = \"k8s\"\nlabels = [\"pod\"]\nmax = 2\n";
    let extra = "[kubernetes]\nenabled = true\nnamespace = \"ci\"\nagent_image = \"kiln-agent:test\"\n";
    let s = start_env(dir.path(), &Options { agent: false, ci: pool, extra }, false, &[("KUBECONFIG", &kubeconfig.to_string_lossy())])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let workflow = format!(
        "on: push\njobs:\n  db:\n    runs-on: pod\n    strategy:\n      matrix:\n        v: [\"3.20\"]\n    container:\n      image: alpine:${{{{ matrix.v }}}}\n      env:\n        FROM_CONTAINER: yes\n    services:\n      cache:\n        image: redis:7\n        ports: [{port}]\n    steps:\n      - run: |\n          test ! -e /.dockerenv\n          echo \"port=${{{{ job.services.cache.ports['{port}'] }}}} env=$FROM_CONTAINER\"\n"
    );
    std::fs::write(work.join(".github/workflows/db.yml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "db"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 240, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then(|| body.clone()))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{detail}\n{log}\n{}", s.log());
    assert!(log.contains(&format!("port={port} env=yes")), "{log}");
    let seen = kube.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    let pod = seen.created.first().cloned().unwrap_or_default();
    assert_eq!(pod.pointer("/spec/containers/0/image"), Some(&json!("alpine:3.20")), "the job container, its matrix evaluated: {pod}");
    let env: Vec<(String, String)> = pod
        .pointer("/spec/containers/0/env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.get("name")?.as_str()?.to_owned(), e.get("value")?.as_str()?.to_owned())))
        .collect();
    assert!(env.contains(&("FROM_CONTAINER".to_owned(), "yes".to_owned())) && env.iter().any(|(k, _)| k == "KILN_POD_CONTAINERS"), "{env:?}");
    let side = pod.pointer("/spec/initContainers/1").cloned().unwrap_or_default();
    assert_eq!((side.get("name"), side.get("image"), side.get("restartPolicy")), (Some(&json!("svc-cache")), Some(&json!("redis:7")), Some(&json!("Always"))), "a native sidecar: {pod}");
    assert_eq!(side.pointer("/ports/0/containerPort"), Some(&json!(port)), "{side}");
    assert_eq!(pod.pointer("/spec/hostAliases/0"), Some(&json!({ "ip": "127.0.0.1", "hostnames": ["cache"] })), "{pod}");
    Ok(())
}

/// W17-R01 on `k8s`: a terminal into a leg running in a pod is that pod's shell, not a machine's,
/// so a maintainer (`runs.debug`, no `runners.terminal`) opens one: in the leg's workspace, with
/// its environment and its secret masked, ending when its shell does.
#[test]
fn w17_r01_a_terminal_into_a_leg_in_a_pod() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let kube = Kube::start(dir.path())?;
    let kubeconfig = kube.kubeconfig(dir.path())?;
    let pool = "\n[[ci.pools]]\nname = \"pods\"\nexecutor = \"k8s\"\nlabels = [\"pod\"]\nmax = 1\n";
    let extra = "[kubernetes]\nenabled = true\nnamespace = \"ci\"\nagent_image = \"kiln-agent:test\"\n";
    let s = start_env(dir.path(), &Options { agent: false, ci: pool, extra }, false, &[("KUBECONFIG", &kubeconfig.to_string_lossy())])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/WORD?scope=repo:acme/app", &s.admin, Some(&json!({ "value": "plums-in-a-pod" })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "lead", "grants": ["maintainer@repo:acme/app"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "lead", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let lead = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let workflow = "on: push\njobs:\n  hold:\n    runs-on: pod\n    env:\n      WORD: ${{ secrets.WORD }}\n      FRUIT: pears\n    steps:\n      - run: sleep 120\n";
    std::fs::write(work.join(".github/workflows/hold.yml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "hold"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // Its step running in its pod (the leg is `running` as soon as the pod's runner has it).
    let (mut run, mut leg) = (String::new(), String::new());
    until(&s, 240, "the leg's step to run in its pod", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        leg = serde_json::from_str::<Value>(&detail).unwrap_or_default().pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, one) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let steps = serde_json::from_str::<Value>(&one).unwrap_or_default().get("steps").cloned().unwrap_or_default();
        let sleeping = steps.as_array().into_iter().flatten().any(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.contains("sleep 120")) && x.get("status").and_then(Value::as_str) == Some("running"));
        Ok((!sleeping).then_some(one))
    })?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["debug", "terminal", &leg])
        .env("HOME", &home)
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("KILN_ADMIN_TOKEN", &lead)
        .env_remove("KILN_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"test \"$PWD\" = \"$GITHUB_WORKSPACE\" && echo in-the-work\"\"space\necho \"word=$WORD fruit=$FRUIT-$((6*7))\"\nexit\n")?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("the terminal didn't end with its shell\n{}", s.log());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let o = child.wait_with_output()?;
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    let _ = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})));
    assert!(o.status.success(), "{out}\n{err}\n{}", s.log());
    assert!(out.contains("in-the-workspace"), "it opens in the leg's workspace: {out}\n{err}");
    assert!(out.contains("word=*** fruit=pears-42") && !out.contains("plums-in-a-pod"), "its environment, its secret masked: {out}\n{err}");
    // The pod goes with its leg.
    until(&s, 60, "the pod to go", || {
        let seen = kube.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok(seen.deleted.is_empty().then(|| format!("{} created", seen.created.len())))
    })?;
    Ok(())
}

/// W13-R07's Docker actions in pods: on a pool whose pods have no Docker, a job with a Docker
/// step fails as it's queued with KCI4032, saying where they run, and no pod is made for it; on a
/// pool with a Docker sidecar (`docker` in its `[[ci.pools]]`), each pod has the Docker CLI copied
/// in, the engine as a privileged native sidecar its job container shares a socket and the work
/// directory with, and the step runs there.
#[test]
fn w13_r07_docker_actions_in_pods_need_the_pools_sidecar() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let kube = Kube::start(dir.path())?;
    let kubeconfig = kube.kubeconfig(dir.path())?;
    let pools = "\n[[ci.pools]]\nname = \"pods\"\nexecutor = \"k8s\"\nlabels = [\"pod\"]\nmax = 1\n\n[[ci.pools]]\nname = \"dpods\"\nexecutor = \"k8s\"\nlabels = [\"dpod\"]\nmax = 1\n[ci.pools.docker]\nengine = \"podman\"\n";
    let extra = "[kubernetes]\nenabled = true\nnamespace = \"ci\"\nagent_image = \"kiln-agent:test\"\n";
    let s = start_env(dir.path(), &Options { agent: false, ci: pools, extra }, false, &[("KUBECONFIG", &kubeconfig.to_string_lossy())])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let workflow = "on: push\njobs:\n  without:\n    runs-on: pod\n    steps:\n      - uses: docker://alpine:3.20\n        with:\n          args: echo never\n  with:\n    runs-on: dpod\n    steps:\n      - uses: docker://alpine:3.20\n        with:\n          args: echo from-the-sidecar\n";
    std::fs::write(work.join(".github/workflows/docker.yml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "docker"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 300, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then(|| body.clone()))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut legs = BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let leg = l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, one) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let one: Value = serde_json::from_str(&one)?;
        let key = one.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        let notes: Vec<String> = one.get("annotations").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
        let mut log = String::new();
        for step in 0..4 {
            let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
            log.push_str(&text);
        }
        legs.insert(key, (one.pointer("/leg/conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), notes.join("\n"), log));
    }
    let (without, why, _) = legs.get("without").cloned().unwrap_or_default();
    assert_eq!(without, "failure", "{detail}");
    assert!(why.contains("KCI4032") && why.contains("`docker://alpine:3.20` needs Docker") && why.contains("pool `pods`") && why.contains("`docker` executor"), "{why}");
    let (with, notes, log) = legs.get("with").cloned().unwrap_or_default();
    let pods_log = std::fs::read_dir(dir.path().join("pods"))
        .map(|d| d.flatten().map(|e| std::fs::read_to_string(e.path().join("log")).unwrap_or_default()).collect::<Vec<_>>().join("\n---\n"))
        .unwrap_or_default();
    assert_eq!(with, "success", "{notes}\n{log}\n{detail}\n{pods_log}\n{}", s.log());
    assert!(log.contains("from-the-sidecar"), "{log}");
    let seen = kube.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    assert_eq!(seen.created.len(), 1, "a pod for the job with the sidecar only: {:?}", seen.created);
    let pod = seen.created.first().cloned().unwrap_or_default();
    let init: Vec<&str> = pod.pointer("/spec/initContainers").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("name").and_then(Value::as_str)).collect();
    assert_eq!(init, ["kiln-bin", "docker-cli", "docker"], "{pod}");
    let engine = pod.pointer("/spec/initContainers/2").cloned().unwrap_or_default();
    assert_eq!((engine.get("image"), engine.get("restartPolicy"), engine.pointer("/securityContext/privileged")), (Some(&json!("quay.io/podman/stable")), Some(&json!("Always")), Some(&json!(true))), "{engine}");
    assert!(engine.pointer("/startupProbe/exec/command").is_some_and(|c| c.to_string().contains("test -S /run/kiln-docker/docker.sock")), "{engine}");
    let mounts = |c: &Value| -> Vec<String> { c.get("volumeMounts").and_then(Value::as_array).into_iter().flatten().filter_map(|m| Some(format!("{}:{}", m.get("name")?.as_str()?, m.get("mountPath")?.as_str()?))).collect() };
    let job = pod.pointer("/spec/containers/0").cloned().unwrap_or_default();
    for shared in ["docker-sock:/run/kiln-docker", "work:/work"] {
        assert!(mounts(&engine).iter().any(|m| m == shared) && mounts(&job).iter().any(|m| m == shared), "{shared} in both: {pod}");
    }
    let env: Vec<(String, String)> = job.get("env").and_then(Value::as_array).into_iter().flatten().filter_map(|e| Some((e.get("name")?.as_str()?.to_owned(), e.get("value")?.as_str()?.to_owned()))).collect();
    assert!(env.contains(&("DOCKER_HOST".to_owned(), "unix:///run/kiln-docker/docker.sock".to_owned())) && env.contains(&("KILN_POD_DOCKER".to_owned(), "/kiln-bin/docker".to_owned())), "{env:?}");
    Ok(())
}
