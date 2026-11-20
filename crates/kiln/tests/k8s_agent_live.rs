//! kiln's Kubernetes engine on a real cluster (docs/agentic.md W10; kiln goal 28e73c34's part 01a1f1249c5e75b5bbf0c92c84385109):
//! an agent started through kiln with `isolation: "pod"` runs in a pod of its own and works in its home runner's Slates
//! volume through kubelet's mount of that runner's export over RPC-with-TLS (never a copy on a node). Then its review,
//! drift and landing are its home's, landing through the person's grant.
//!
//! The cluster is the operator's, made before the test: its nodes run `tlshd` (ktls-utils) and see the kernel's TLS
//! record layer (`modprobe tls` on the host whose kernel they share). The test issues each node's `tlshd` certificate
//! through kiln (`POST /api/kubernetes/node-certificates`, for a key made on the node) and puts it in place over
//! `docker exec`, since a KIND or minikube node is a container on this host. The agents' image holds kiln at
//! `/usr/local/bin/kiln` and the stand-in harness as `claude`, which acts on its first message by writing
//! `pod-wrote.txt` (the pod's hostname in it) and appending to `README.md` in its folder. The server runs here,
//! listening where the pods reach it; the home runner is the server's one Slates fleet node (`f = 0`), listed at the
//! host's address on the cluster's network.
//!
//! Gated by `KILN_K8S_LIVE=1`; without it the test skips, saying so. It also needs:
//! - `KUBECONFIG`: the cluster's kubeconfig, its current context the cluster (also what `kubectl` uses here);
//! - `KILN_K8S_HOST`: this host's address on the cluster's network (the nodes' gateway);
//! - `KILN_K8S_NODES`: the nodes' container names, comma-separated;
//! - `KILN_K8S_IMAGE`: the agents' image, loaded into the cluster;
//! - `KILN_K8S_NAMESPACE` (default `kiln-agents-live`): the namespace, made already, with its `default` service account;
//! - `KILN_K8S_LOGS` (optional): a folder the server's, the home's and the pod's whole logs are copied to as the test ends.
//!
//!   KILN_K8S_LIVE=1 KUBECONFIG=... KILN_K8S_HOST=192.168.58.1 KILN_K8S_NODES=k90-close,k90-close-m02 \
//!   KILN_K8S_IMAGE=k90-close-agent:live cargo test -p kiln --test k8s_agent_live -- --nocapture

#![cfg(target_os = "linux")]

mod common;
mod vm_e2e;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The switch that runs the test; without it it skips, saying so.
const SWITCH: &str = "KILN_K8S_LIVE";
/// The home runner's name.
const HOME: &str = "k8s-home";
/// How long each step may take on a loaded machine: an image's start, a mount's TLS handshake, a session's opening.
const WITHIN: Duration = Duration::from_secs(300);
/// How often a step looks again.
const LOOK: Duration = Duration::from_secs(2);
/// What the agent is asked; the stand-in in the image acts on any first message.
const PROMPT: &str = "write pod-wrote.txt in your folder";

fn need(key: &str) -> anyhow::Result<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("{SWITCH}=1 needs {key} (see this test's header)"))
}

/// A program run here, its output (stdout) on success; `input` on its stdin.
fn run(program: &str, args: &[&str], input: Option<&str>) -> anyhow::Result<String> {
    let mut child = Command::new(program).args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    anyhow::ensure!(out.status.success(), "{program} {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn kubectl(ns: &str, args: &[&str]) -> anyhow::Result<String> {
    let mut all = vec!["-n", ns];
    all.extend_from_slice(args);
    run("kubectl", &all, None)
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut all = vec!["-C".to_owned(), dir.to_string_lossy().into_owned()];
    all.extend(args.iter().map(|a| (*a).to_owned()));
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    Ok(run("git", &all, None)?.trim().to_owned())
}

/// Two consecutive UDP ports free at `host`, the first free over TCP too: a node's base (its export's TCP port and its
/// two UDP ports, W7).
fn base_port(host: &str) -> anyhow::Result<u16> {
    for _ in 0..64 {
        let first = std::net::UdpSocket::bind((host, 0))?;
        let base = first.local_addr()?.port();
        let Some(next) = base.checked_add(1) else { continue };
        if std::net::UdpSocket::bind((host, next)).is_ok() && std::net::TcpListener::bind((host, base)).is_ok() {
            return Ok(base);
        }
    }
    anyhow::bail!("no free base port at {host}")
}

fn until<T>(what: &str, mut f: impl FnMut() -> anyhow::Result<Option<T>>, seen: impl Fn() -> String) -> anyhow::Result<T> {
    let began = Instant::now();
    loop {
        match f() {
            Ok(Some(t)) => {
                eprintln!("PROVEN: {what} ({:.1?})", began.elapsed());
                return Ok(t);
            }
            Ok(None) => {}
            Err(e) if began.elapsed() >= WITHIN => return Err(e.context(format!("{what}\n{}", seen()))),
            Err(_) => {}
        }
        anyhow::ensure!(began.elapsed() < WITHIN, "{what}: not within {WITHIN:?}\n{}", seen());
        std::thread::sleep(LOOK);
    }
}

/// What the test leaves behind as it ends, where `KILN_K8S_LOGS` names a folder: every log of its fleet's folder and its
/// pod's whole log, copied before the fleet's folder goes (it's dropped first, being made after the fleet).
struct Kept {
    root: std::path::PathBuf,
    ns: String,
    pod: std::cell::RefCell<Option<String>>,
}

impl Drop for Kept {
    fn drop(&mut self) {
        let Some(to) = std::env::var_os("KILN_K8S_LOGS").map(std::path::PathBuf::from) else { return };
        if std::fs::create_dir_all(&to).is_err() {
            return;
        }
        for entry in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "log") {
                let _ = std::fs::copy(&path, to.join(entry.file_name()));
            }
        }
        if let Some(pod) = self.pod.borrow().as_deref() {
            let log = kubectl(&self.ns, &["logs", pod, "-c", "agent"]).unwrap_or_else(|e| format!("{e:#}"));
            let _ = std::fs::write(to.join(format!("{pod}.log")), format!("{}\n{log}", pod_story(&self.ns, pod)));
        }
    }
}

/// W10 on a real cluster. Do: start the stand-in through kiln in a pod of its own, its home the fleet's one node; it
/// writes in its folder; ask for its change request, change a file it changed in the folder, land it with the person's
/// grant, end it. Expect: the pod running kiln's runner for that session, its folder kubelet's `nfs4` mount with
/// `xprtsec=mtls` from the home's export; the agent's files in the home's volume and not the folder until it lands; its
/// change request holding them; the file changed in the folder named drift; the landing a plan needing the grant, then
/// written with it, the drifted file left; and the pod, its claim and PersistentVolume gone once it ends.
#[test]
fn w10_an_agent_in_a_pod_works_in_its_homes_volume_through_kubelets_mount() -> anyhow::Result<()> {
    if std::env::var(SWITCH).as_deref() != Ok("1") {
        eprintln!("SKIPPED: {SWITCH} is not 1: this test needs a live Kubernetes cluster whose nodes run tlshd (see its header)");
        return Ok(());
    }
    let (host, image, kubeconfig) = (need("KILN_K8S_HOST")?, need("KILN_K8S_IMAGE")?, need("KUBECONFIG")?);
    let nodes: Vec<String> = need("KILN_K8S_NODES")?.split(',').map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).collect();
    let ns = std::env::var("KILN_K8S_NAMESPACE").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| "kiln-agents-live".to_owned());
    let machine = common::machine(HOME);
    let base = base_port(&host)?;
    let kubernetes = format!(
        "enabled = true\nnamespace = \"{ns}\"\nagent_image = \"{image}\"\nagents_image = \"{image}\"\nservice_account = \"default\"\nimage_pull_policy = \"Never\"\ndefault_cpu = \"100m\"\ndefault_memory = \"512Mi\"\nserver_url = \"http://{host}:{{http}}\"\nserver_quic = \"{host}:{{quic}}\""
    );
    let tables = common::Tables { listen: "0.0.0.0", kubernetes: &kubernetes, ..common::Tables::default() };
    let extra = format!("[slates_fleet]\nf = 0\n[slates_fleet.nodes]\n\"{machine}\" = \"{host}:{base}\"\n");
    let mut fleet = common::Fleet::with_tables(&tables, move |_| extra.clone(), &[("KUBECONFIG", kubeconfig.as_str())])?;

    // The folder: a clone of a bare repository (its change requests' branches are pushed there).
    let root = fleet.root.path().to_path_buf();
    let kept = Kept { root: root.clone(), ns: ns.clone(), pod: std::cell::RefCell::new(None) };
    let bare = root.join("origin.git");
    let project = root.join("project");
    run("git", &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()], None)?;
    run("git", &["clone", "-q", &bare.to_string_lossy(), &project.to_string_lossy()], None)?;
    std::fs::write(project.join("README.md"), "the folder\n")?;
    git(&project, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "README.md"])?;
    git(&project, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "first"])?;
    git(&project, &["push", "-q", "origin", "main"])?;
    let head = git(&project, &["rev-parse", "HEAD"])?;

    // The home: the fleet's one node, with its own Slates instance, the stand-in under each harness's name.
    let instance = format!("kilnk8s{}", std::process::id());
    let dir = root.join("home-stub");
    let mut env = vm_e2e::stub_host_env(&dir)?;
    env.extend([("KILN_MACHINE_ID", HOME.to_owned()), ("KILN_SLATES_INSTANCE", instance.clone()), ("KILN_SLATES_DIRECT", "1".to_owned()), ("KILN_SLATES", String::new())]);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    fleet.start_runner_harnessed(HOME, &env)?;
    let logs = || fleet_logs(&root);
    until("the home's node serves as its fleet's member", || Ok(slates_api::Instance::attach(&instance).and_then(|mut i| i.daemon_status()).ok().filter(|s| !s.fleet.members.is_empty()).map(|_| ())), logs)?;

    // Each node's tlshd certificate, issued by kiln for a key made on the node.
    for node in &nodes {
        let csr = run("docker", &["exec", node, "sh", "-c", &format!("mkdir -p /etc/kiln-tls && openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout /etc/kiln-tls/key.pem -subj /CN={node} 2>/dev/null")], None)?;
        let issued = fleet.call("POST", "/api/kubernetes/node-certificates", Some(&json!({"node": node, "csr": csr})))?;
        let (cert, ca) = (issued.get("certificate").and_then(Value::as_str).unwrap_or_default(), issued.get("ca").and_then(Value::as_str).unwrap_or_default());
        anyhow::ensure!(!cert.is_empty() && !ca.is_empty(), "kiln issued {node} no certificate: {issued}");
        run("docker", &["exec", "-i", node, "sh", "-c", "cat > /etc/kiln-tls/cert.pem"], Some(cert))?;
        run("docker", &["exec", "-i", node, "sh", "-c", "cat > /etc/kiln-tls/ca.pem"], Some(ca))?;
        run("docker", &["exec", node, "systemctl", "restart", "tlshd"], None)?;
        let active = run("docker", &["exec", node, "systemctl", "is-active", "tlshd"], None)?;
        anyhow::ensure!(active.trim() == "active", "{node}'s tlshd: {active}");
    }
    eprintln!("PROVEN: {} node(s) hold kiln-issued tlshd certificates", nodes.len());

    // The agent, in a pod of its own, its home the node.
    let spec = json!({"kind": "claude", "on": machine, "cwd": project, "prompt": PROMPT, "isolation": "pod"});
    let started = until(
        "agents.start with isolation pod answered",
        || {
            let (code, body) = fleet.request("POST", "/api/ops/agents.start", Some(&spec))?;
            let text = String::from_utf8_lossy(&body).into_owned();
            if (200..300).contains(&code) {
                return Ok(Some(serde_json::from_str::<Value>(&text)?));
            }
            anyhow::ensure!(code == 409 && text.contains("try again"), "POST /api/ops/agents.start: {code} {text}");
            Ok(None)
        },
        logs,
    )?;
    let id = started.pointer("/agents/0/id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no session in {started}"))?.to_owned();
    let pod = kiln::k8s::agent::pod_name(&id);
    kept.pod.replace(Some(pod.clone()));
    let agent = |fleet: &common::Fleet| fleet.call("POST", "/api/ops/agents.get", Some(&json!({"agent": id})));
    let seen = || format!("{}\n{}", pod_story(&ns, &pod), fleet_logs(&root));
    until("the agent's pod running", || Ok((kubectl(&ns, &["get", "pod", &pod, "-o", "jsonpath={.status.phase}"])? == "Running").then_some(())), seen)?;

    // The pod's folder is kubelet's NFS mount of the home's export, over RPC-with-TLS.
    let mounts = kubectl(&ns, &["exec", &pod, "-c", "agent", "--", "cat", "/proc/mounts"])?;
    let folder = project.to_string_lossy().into_owned();
    let line = mounts.lines().find(|l| l.split_whitespace().nth(1) == Some(folder.as_str())).ok_or_else(|| anyhow::anyhow!("no mount at {folder} in the pod:\n{mounts}"))?.to_owned();
    anyhow::ensure!(line.split_whitespace().nth(2).is_some_and(|t| t.starts_with("nfs4")) && line.contains("xprtsec=mtls") && line.contains(&host), "the pod's folder isn't the home's export over RPC-with-TLS: {line}");
    eprintln!("PROVEN: the pod's folder is kubelet's mount: {line}");

    // The agent acts on its opening prompt. Where it hasn't within two minutes, its events are shown and the same message
    // sent again: which of the two it acted on says whether a pod session's opening prompt reached its harness.
    let written = format!("{folder}/pod-wrote.txt");
    let acted = |_: &common::Fleet| -> anyhow::Result<bool> { Ok(kubectl(&ns, &["exec", &pod, "-c", "agent", "--", "test", "-f", &written]).is_ok()) };
    let began = Instant::now();
    while !acted(&fleet).unwrap_or(false) && began.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(LOOK);
    }
    if !acted(&fleet).unwrap_or(false) {
        let events = fleet.call("GET", &format!("/api/harness/sessions/{id}/events?limit=200"), None).map(|e| e.to_string()).unwrap_or_else(|e| format!("{e:#}"));
        eprintln!("SEEN: no action on the opening prompt in 120 s; the session's events: {}", events.chars().take(6000).collect::<String>());
        let sent = fleet.call("POST", "/api/ops/agents.send", Some(&json!({"agent": id, "text": PROMPT})));
        eprintln!("SEEN: the prompt sent again: {sent:?}");
        let began = Instant::now();
        while !acted(&fleet).unwrap_or(false) && began.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(LOOK);
        }
        anyhow::bail!(
            "a pod session's opening prompt never reached its harness; sent again, it {}\n{}",
            if acted(&fleet).unwrap_or(false) { "acted on it (the opening prompt is lost)" } else { "didn't act either (no message reaches it)" },
            seen()
        );
    }

    // The agent wrote, in its pod, into its home's volume; the folder is as it was.
    let last_plan = std::cell::RefCell::new(String::new());
    let wrote = until(
        "the agent's files in its volume, by its landing's plan (one created, one replaced)",
        || {
            let plan = fleet.call("POST", "/api/ops/agents.land", Some(&json!({"agent": id})));
            last_plan.replace(match &plan {
                Ok(p) => p.to_string(),
                Err(e) => format!("{e:#}"),
            });
            let plan = plan?;
            // A plan says what it would do by action, counted (the file it creates and the one it replaces).
            let count = |action: &str| plan.pointer("/plan/summary/by_action").and_then(Value::as_array).and_then(|a| a.iter().find(|x| x.get("action").and_then(Value::as_str) == Some(action))).and_then(|x| x.get("count")).and_then(Value::as_u64).unwrap_or(0);
            Ok((count("create") >= 1 && count("replace") >= 1).then_some(plan))
        },
        || {
            let ws = fleet.call("POST", "/api/ops/agents.workspace", Some(&json!({"agent": id}))).map(|w| w.to_string()).unwrap_or_else(|e| format!("{e:#}"));
            format!("its last plan: {}\nits workspace: {ws}\n{}\n{}", last_plan.borrow(), agent(&fleet).map(|a| a.to_string()).unwrap_or_default(), seen())
        },
    )?;
    let in_pod = kubectl(&ns, &["exec", &pod, "-c", "agent", "--", "cat", &format!("{folder}/pod-wrote.txt")])?;
    anyhow::ensure!(in_pod.trim() == format!("written in pod {pod}"), "the pod reads {in_pod:?} back");
    anyhow::ensure!(!project.join("pod-wrote.txt").exists(), "the agent's file reached the folder before it landed");
    anyhow::ensure!(agent(&fleet)?.pointer("/agent/machine").and_then(Value::as_str).is_some_and(|m| m != machine), "the session isn't on its pod's runner: {}", agent(&fleet)?);
    eprintln!("PROVEN: the agent wrote in its pod, into its home's volume: {wrote}");

    // Review: its change request holds what it wrote.
    let change = fleet.call("POST", "/api/ops/agents.change", Some(&json!({"agent": id, "title": "Written in a pod"})))?;
    let branch = change.pointer("/change_request/branch").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no change request: {change}"))?.to_owned();
    let shown = git(&bare, &["show", &format!("{branch}:pod-wrote.txt")])?;
    anyhow::ensure!(shown == format!("written in pod {pod}"), "{branch}'s pod-wrote.txt: {shown:?}");
    eprintln!("PROVEN: review: {branch} holds the pod's file");

    // Drift: README.md, which it changed, changed in the folder too.
    std::fs::write(project.join("README.md"), "the person's\n")?;
    let ws = until(
        "README.md named drift",
        || {
            let ws = fleet.call("POST", "/api/ops/agents.workspace", Some(&json!({"agent": id})))?;
            Ok(ws.get("drift").and_then(Value::as_array).is_some_and(|d| d.iter().any(|x| x.as_str().is_some_and(|s| s.contains("README.md")))).then_some(ws))
        },
        seen,
    )?;
    eprintln!("PROVEN: drift: {}", ws.get("drift").cloned().unwrap_or_default());

    // Landing, through the person's grant: the plan asks for it; with it, written, the drifted file left as the person made it.
    let plan = fleet.call("POST", "/api/ops/agents.land", Some(&json!({"agent": id})))?;
    let manifest = plan.pointer("/approval/manifest").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("the plan holds no grant to approve: {plan}"))?.to_owned();
    let landed = fleet.call("POST", "/api/ops/agents.land", Some(&json!({"agent": id, "manifest": manifest})))?;
    anyhow::ensure!(landed.get("outcome") == Some(&json!("written")), "landed: {landed}");
    anyhow::ensure!(std::fs::read_to_string(project.join("pod-wrote.txt"))?.trim() == format!("written in pod {pod}"), "the folder doesn't hold the landing");
    anyhow::ensure!(std::fs::read_to_string(project.join("README.md"))? == "the person's\n", "the landing overwrote the drifted file");
    anyhow::ensure!(git(&project, &["rev-parse", "HEAD"])? == head, "the folder's HEAD moved");
    eprintln!("PROVEN: landed through the person's grant: {landed}");

    // Its end: the pod, its claim and PersistentVolume go.
    fleet.call("POST", "/api/ops/agents.stop", Some(&json!({"agent": id})))?;
    until(
        "the pod, its claim and PersistentVolume gone",
        || {
            let left = [("pod", false), ("pvc", false), ("pv", true)].iter().filter(|(kind, cluster)| {
                let got = if *cluster { run("kubectl", &["get", kind, &pod, "--ignore-not-found", "-o", "name"], None) } else { kubectl(&ns, &["get", kind, &pod, "--ignore-not-found", "-o", "name"]) };
                got.map_or(true, |o| !o.trim().is_empty())
            });
            Ok((left.count() == 0).then_some(()))
        },
        seen,
    )?;
    fleet.stop_gently();
    Ok(())
}

/// The pod's description and its agent's log, for a failure.
fn pod_story(ns: &str, pod: &str) -> String {
    let describe = kubectl(ns, &["describe", "pod", pod]).unwrap_or_else(|e| format!("{e:#}"));
    let log = kubectl(ns, &["logs", pod, "-c", "agent", "--tail", "80"]).unwrap_or_else(|e| format!("{e:#}"));
    // What runs in it and what its runner keeps: whether its harness started.
    let ps = kubectl(ns, &["exec", pod, "-c", "agent", "--", "sh", "-c", "ps -eo pid,ppid,etime,args 2>&1; find /tmp/.local/share/kiln -maxdepth 4 2>&1 | head -60"]).unwrap_or_else(|e| format!("{e:#}"));
    format!("--- pod {pod}\n{describe}\n--- its agent's log\n{log}\n--- its processes and its runner's state\n{ps}")
}

/// The server's and the home's logs' ends, for a failure.
fn fleet_logs(root: &Path) -> String {
    ["server.log", &format!("{HOME}.log")]
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(root.join(f)).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(60).collect();
            format!("--- {f}\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
