//! Kiln CD in observe, live (docs/observe.md §8, §10.7, §13): a kiln server as a process of its own
//! deploys an app's plain manifests from a git repository (kiln's `[[repos]]` mirrors it too) to a
//! real Kubernetes cluster: the one `KOBS_KUBECONFIG` names, or a throwaway one kind or minikube
//! starts. The app follows the repository's `main`; four commits land on it in turn, each synced as
//! a person asks: a ConfigMap and a Deployment that come up; an image no registry has, the rollout
//! past its deadline; that commit reverted (what the first deployed, again); an object the API
//! server refuses. What observe says of each is what kiln CD and the
//! cluster say: a deploy counted by result with the objects it applied, an event each deploy and
//! each change of the app's health or sync, the apps counted by health and sync (a monitor on the
//! degraded ones alerts Slack, then recovers), and DORA's deployments of the app: each commit's
//! lead time, the reverted one a change failure the revert's deploy put right, the refused deploy
//! none.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Fleet, Mock, Tables, now_ms};
use serde_json::{Value, json};

const HOUR: i64 = 3_600;
const APP: &str = "default/web";
/// The name kiln gives the cluster.
const CLUSTER: &str = "kobs";
/// An image no registry serves: its pull fails at once.
const NOWHERE: &str = "kobs.invalid/web:2";

/// `cmd` run to its end: its stdout, or its failure with what it said.
fn output(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.stdin(Stdio::null()).output()?;
    anyhow::ensure!(out.status.success(), "{cmd:?}: {}\n{}{}", out.status, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether `bin` runs here.
fn runs(bin: &str) -> bool {
    Command::new(bin).arg("version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// What started a cluster of the test's own, to delete it.
enum Owner {
    Kind(String),
    Minikube(String),
}

/// The cluster deployed to, and the namespace the app deploys in; the cluster deleted when the
/// test started it, else the namespace.
struct Cluster {
    kubeconfig: PathBuf,
    context: String,
    namespace: String,
    owner: Option<Owner>,
}

impl Cluster {
    fn start(dir: &Path) -> anyhow::Result<Self> {
        let namespace = format!("kobs-cd-{}", std::process::id());
        if let Ok(path) = std::env::var("KOBS_KUBECONFIG") {
            let kubeconfig = PathBuf::from(path);
            let context = match std::env::var("KOBS_KUBE_CONTEXT") {
                Ok(c) => c,
                Err(_) => output(Command::new("kubectl").arg("--kubeconfig").arg(&kubeconfig).args(["config", "current-context"]))?,
            };
            return Ok(Self { kubeconfig, context, namespace, owner: None });
        }
        let kubeconfig = dir.join("kubeconfig");
        let name = namespace.clone();
        if runs("kind") {
            let c = Self { kubeconfig, context: format!("kind-{name}"), namespace, owner: Some(Owner::Kind(name.clone())) };
            output(Command::new("kind").args(["create", "cluster", "--name", &name, "--wait", "180s", "--kubeconfig"]).arg(&c.kubeconfig))?;
            return Ok(c);
        }
        if runs("minikube") {
            let c = Self { kubeconfig, context: name.clone(), namespace, owner: Some(Owner::Minikube(name.clone())) };
            output(
                Command::new("minikube")
                    .args(["start", "-p", &name, "--driver=docker", "--container-runtime=containerd", "--embed-certs", "--memory=2g", "--cpus=2", "--wait=all"])
                    .env("KUBECONFIG", &c.kubeconfig),
            )?;
            return Ok(c);
        }
        anyhow::bail!("no cluster to deploy to: KOBS_KUBECONFIG names one (KOBS_KUBE_CONTEXT its context, else its current one), or kind or minikube starts one")
    }

    /// `kubectl` against it: its stdout.
    fn kubectl(&self, args: &[&str]) -> anyhow::Result<String> {
        output(Command::new("kubectl").arg("--kubeconfig").arg(&self.kubeconfig).arg("--context").arg(&self.context).args(args))
    }

    /// An image the node has: its pause image (the sandbox every pod runs in), which runs until it's
    /// stopped.
    fn local_image(&self) -> anyhow::Result<String> {
        let nodes: Value = serde_json::from_str(&self.kubectl(&["get", "nodes", "-o", "json"])?)?;
        let names = nodes["items"].as_array().into_iter().flatten().flat_map(|n| n["status"]["images"].as_array().into_iter().flatten()).flat_map(|i| i["names"].as_array().into_iter().flatten());
        let pause = names.filter_map(Value::as_str).find(|n| n.contains("/pause:") && !n.contains('@'));
        Ok(pause.map_or_else(|| "registry.k8s.io/pause:3.10".to_owned(), str::to_owned))
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let done = match &self.owner {
            Some(Owner::Kind(name)) => output(Command::new("kind").args(["delete", "cluster", "--name", name, "--kubeconfig"]).arg(&self.kubeconfig)),
            Some(Owner::Minikube(name)) => output(Command::new("minikube").args(["delete", "-p", name]).env("KUBECONFIG", &self.kubeconfig)),
            None => self.kubectl(&["delete", "namespace", &self.namespace, "--wait=false", "--ignore-not-found"]),
        };
        if let Err(e) = done {
            eprintln!("the cluster is left: {e:#}");
        }
    }
}

/// `git` in `dir` with its author and committer times `at` (s): its stdout.
fn git(dir: &Path, at: i64, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", format!("@{at} +0000"))
        .env("GIT_COMMITTER_DATE", format!("@{at} +0000"))
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@example.com")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A commit of `files` (each path and content) titled `title`, at `at`: its sha.
fn commit(dir: &Path, at: i64, files: &[(&str, &str)], title: &str) -> String {
    for (path, what) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, what).unwrap();
        git(dir, at, &["add", path]);
    }
    git(dir, at, &["commit", "-q", "-m", title]);
    git(dir, at, &["rev-parse", "HEAD"])
}

/// The app's Deployment: `image` (pulled only when the node hasn't it), its rollout failed after
/// `deadline` seconds without progress.
fn deployment(image: &str, deadline: u32) -> String {
    format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 1\n  progressDeadlineSeconds: {deadline}\n  selector:\n    matchLabels:\n      app: web\n  template:\n    metadata:\n      labels:\n        app: web\n    spec:\n      terminationGracePeriodSeconds: 0\n      containers:\n        - name: web\n          image: {image}\n          imagePullPolicy: IfNotPresent\n"
    )
}

const CONFIG: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\ndata:\n  color: blue\n";
/// A name the API server refuses (not a DNS label).
const REFUSED: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: Bad_Name\ndata:\n  color: red\n";

/// The app kept in kiln, following `repo`'s `main`.
fn app(repo: &str, namespace: &str) -> Value {
    json!({
        "name": "web", "project": "default",
        "sources": [{ "repo": repo, "path": "deploy", "revision": "main", "chart": null, "as_ref": null, "engine": { "Directory": { "recurse": false, "include": null, "exclude": null } } }],
        "destination": { "cluster": CLUSTER, "namespace": namespace },
        "sync": { "auto": false, "prune": true, "self_heal": false, "allow_empty": false, "options": { "create_namespace": true }, "retry": null },
        "ignore": [], "tracking": "Kiln", "origin": "Api", "environment": null, "labels": { "team": "web-team" },
        "history_limit": 10, "cascade": true, "managed_namespace": null,
    })
}

/// Waits up to `secs` for `done` to say yes, or fails saying `what`, what it last saw and the
/// server's logs.
fn until(f: &Fleet, secs: u64, what: &str, mut done: impl FnMut() -> anyhow::Result<(bool, Value)>) -> anyhow::Result<Value> {
    let start = Instant::now();
    let mut seen = Value::Null;
    while start.elapsed() < Duration::from_secs(secs) {
        match done() {
            Ok((true, v)) => return Ok(v),
            Ok((false, v)) => seen = v,
            Err(e) => seen = json!(format!("{e:#}")),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("never: {what}; last {seen:#}\n{}", f.logs())
}

/// The app as kiln CD has it now.
fn status(f: &Fleet) -> anyhow::Result<Value> {
    f.call("GET", &format!("/api/cd/apps/{APP}"), None)
}

/// The app's deployments, newest first.
fn history(f: &Fleet) -> anyhow::Result<Vec<Value>> {
    Ok(f.call("GET", &format!("/api/cd/apps/{APP}/history"), None)?["history"].as_array().cloned().unwrap_or_default())
}

/// The sum of every value `q` answers over `from` to now, a minute a step.
fn total(f: &Fleet, q: &str, from: i64) -> anyhow::Result<f64> {
    let b = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": now_ms() + 60_000, "step": 60_000 })))?;
    Ok(b["series"].as_array().into_iter().flatten().flat_map(|s| s["values"].as_array().into_iter().flatten().filter_map(Value::as_f64)).sum())
}

/// The last value `q` answers over the last two minutes, of the series that has one latest.
fn last(f: &Fleet, q: &str) -> anyhow::Result<Option<f64>> {
    let b = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": now_ms() - 120_000, "to": now_ms() + 10_000, "step": 2_000 })))?;
    Ok(b["series"].as_array().into_iter().flatten().filter_map(|s| s["values"].as_array()?.iter().rev().find_map(Value::as_f64)).reduce(f64::max))
}

/// kiln's events `q` finds since `from`.
fn events(f: &Fleet, q: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let b = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("source:kiln.event service:kiln.cd {q}"), "from": from, "to": now_ms() + 60_000, "limit": 100 })))?;
    Ok(b["records"].as_array().cloned().unwrap_or_default())
}

/// DORA's deployment of commit `sha`, when it has one.
fn dora(f: &Fleet, sha: &str, from: i64) -> anyhow::Result<Option<Value>> {
    let b = f.call("POST", "/api/observe/v1/dora/deployments", Some(&json!({ "from": from, "to": now_ms() + 60_000, "query": format!("@git.commit_sha:{sha}") })))?;
    let Some(id) = b["data"].as_array().and_then(|d| d.first()).and_then(|d| d["id"].as_str()).map(str::to_owned) else { return Ok(None) };
    Ok(Some(f.call("GET", &format!("/api/observe/v1/dora/deployments/{id}"), None)?["data"].clone()))
}

/// Whether `v` is `n` seconds, give or take `slack`.
fn near(v: &Value, n: i64, slack: i64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    v.as_f64().is_some_and(|x| (x - n as f64).abs() <= slack as f64)
}

/// The app at `sha`, `main` now, synced as a person asks: its deployment, once kiln CD has
/// recorded it.
fn deploy(f: &Fleet, sha: &str, n: usize) -> anyhow::Result<Value> {
    // Read at the commit (kiln CD looks at `main` every 2 s), and no sync under way, before it's
    // synced.
    until(f, 120, &format!("the app read at {sha}"), || {
        let s = status(f)?;
        let running = s["operation"]["phase"].as_str().is_some_and(|p| matches!(p, "DryRun" | "Running" | "Failing" | "Terminating"));
        Ok((s["revision"] == sha && !running, s))
    })?;
    f.call("POST", &format!("/api/cd/apps/{APP}/sync"), Some(&json!({})))?;
    let h = until(f, 180, &format!("deployment {n} recorded"), || {
        let h = history(f)?;
        Ok((h.len() == n, json!(h)))
    })?;
    Ok(h[0].clone())
}

#[test]
#[ignore = "starts a kiln server as a process, and a Kubernetes cluster unless KOBS_KUBECONFIG names one"]
fn what_kiln_cd_deploys_observe_counts_alerts_on_and_dora_measures() -> anyhow::Result<()> {
    let now = now_ms() / 1000;
    let tmp = tempfile::tempdir()?;
    let cluster = Cluster::start(tmp.path())?;
    let ns = cluster.namespace.clone();
    let image = cluster.local_image()?;

    // The repository: the app (c1). Then, each as its deploy comes: its image changed to one nowhere
    // (c2), that reverted (c3), an object the API server refuses added (c4); their authors' times
    // hours before.
    let repo = tmp.path().join("web");
    std::fs::create_dir_all(&repo)?;
    git(&repo, now - 4 * HOUR, &["init", "-q", "-b", "main"]);
    let c1 = commit(&repo, now - 3 * HOUR, &[("deploy/config.yaml", CONFIG), ("deploy/web.yaml", &deployment(&image, 120))], "Add web");
    let url = format!("file://{}", repo.display());

    let tables = Tables { cd: "enabled = true\npoll_seconds = 2", host_seconds: 2 };
    let f = Fleet::with_tables(&tables, |_| format!("[[repos]]\nname = \"acme/web\"\nclone_url = \"{url}\"\nbranches = [\"main\"]\npoll_seconds = 3600\n"), &[])?;
    let t0 = now_ms();

    // The cluster, by its kubeconfig context, connected.
    f.call("POST", "/api/cd/clusters", Some(&json!({ "name": CLUSTER, "context": cluster.context, "kubeconfig": cluster.kubeconfig })))?;
    until(&f, 60, "the cluster connected", || {
        let c = f.call("GET", "/api/cd/clusters", None)?;
        let ok = c["clusters"].as_array().into_iter().flatten().any(|c| c["name"] == CLUSTER && c["connected"] == true);
        Ok((ok, c))
    })?;

    // Slack, told by a monitor of the degraded apps.
    let mock = Mock::start(&[])?;
    let (code, b) = f.request("PUT", "/api/observe/v1/integrations/cd", Some(&json!({ "kind": "slack", "url": mock.url("/slack") })))?;
    anyhow::ensure!(code == 204, "the Slack integration: {code} {}", String::from_utf8_lossy(&b));
    f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "kobs degraded apps",
            "query": "last(last_1m):max:kiln.cd.apps{health:Degraded} > 0",
            "message": "{{#is_alert}}{{value}} app degraded @slack-cd{{/is_alert}}{{#is_recovery}}no app degraded @slack-cd{{/is_recovery}}",
            "options": { "every": 10, "thresholds": { "critical": 0 } },
        })),
    )?;

    // c1: deployed, and up.
    f.call("POST", "/api/cd/apps", Some(&app(&url, &ns)))?;
    let d1 = deploy(&f, &c1, 1)?;
    anyhow::ensure!(d1["succeeded"] == true && d1["revision"] == c1.as_str() && d1["trigger"] == "manual", "the first deployment: {d1:#}\n{}", f.logs());
    let applied = d1["applied"].as_f64().unwrap_or(0.0);
    anyhow::ensure!(applied >= 2.0, "the ConfigMap and the Deployment applied: {d1:#}");
    anyhow::ensure!(cluster.kubectl(&["get", "configmap", "web-config", "-n", &ns, "-o", "jsonpath={.data.color}"])? == "blue", "the ConfigMap is in the cluster");
    until(&f, 120, "the app healthy and synced", || {
        let s = status(&f)?;
        Ok((s["health"] == "Healthy" && s["sync"] == "Synced", s))
    })?;
    until(&f, 60, "the deploy counted, with the objects it applied", || {
        let n = total(&f, &format!("sum:kiln.cd.deploys{{app:{APP},result:succeeded,trigger:manual}}.as_count()"), t0)?;
        let a = last(&f, &format!("max:kiln.cd.deploy.applied{{app:{APP}}}"))?;
        Ok(((n - 1.0).abs() < 1e-9 && a == Some(applied), json!([n, a])))
    })?;
    let e = until(&f, 60, "the deploy's event", || {
        let e = events(&f, "@event.type:cd.deploy", t0)?;
        Ok((e.len() == 1, json!(e)))
    })?;
    let e = &e[0];
    anyhow::ensure!(
        e["status"] == "info" && e["attributes"]["app"] == APP && e["attributes"]["result"] == "succeeded" && e["attributes"]["revision"] == c1.as_str() && e["attributes"]["by"] == d1["by"],
        "the deploy's event says what kiln CD recorded: {e:#}"
    );
    anyhow::ensure!(e["message"].as_str().is_some_and(|m| m.starts_with(&format!("deploy of {APP} succeeded at {}", &c1[..12]))), "{e:#}");
    until(&f, 60, "the app's health and sync, an event", || {
        let e = events(&f, "@event.type:cd.status @health:Healthy @sync:Synced", t0)?;
        Ok((!e.is_empty(), json!(e)))
    })?;
    until(&f, 60, "the healthy app counted", || {
        let n = last(&f, "max:kiln.cd.apps{health:Healthy,sync:Synced}")?;
        Ok((n == Some(1.0), json!(n)))
    })?;

    // c2: deployed, its rollout stuck past its deadline: degraded, and Slack told.
    let c2 = commit(&repo, now - 2 * HOUR, &[("deploy/web.yaml", &deployment(NOWHERE, 10))], "Use web 2");
    let d2 = deploy(&f, &c2, 2)?;
    anyhow::ensure!(d2["succeeded"] == true && d2["revision"] == c2.as_str(), "the second deployment applies: {d2:#}");
    anyhow::ensure!(cluster.kubectl(&["get", "deployment", "web", "-n", &ns, "-o", "jsonpath={.spec.template.spec.containers[0].image}"])? == NOWHERE, "the image changed in the cluster");
    until(&f, 180, "the app degraded", || {
        let s = status(&f)?;
        Ok((s["health"] == "Degraded", s))
    })?;
    let degraded = until(&f, 60, "the degraded app, an event", || {
        let e = events(&f, "@event.type:cd.status @health:Degraded", t0)?;
        Ok((!e.is_empty(), json!(e)))
    })?;
    anyhow::ensure!(degraded[0]["status"] == "warn" && degraded[0]["message"].as_str().is_some_and(|m| m.starts_with(&format!("{APP} is Degraded"))), "{degraded:#}");
    until(&f, 90, "Slack told of the degraded app", || {
        let said: Vec<String> = mock.at("/slack").iter().filter_map(|g| g.json()["text"].as_str().map(str::to_owned)).collect();
        Ok((said.iter().any(|t| t.contains("[Triggered] kobs degraded apps") && t.contains("1 app degraded")), json!(said)))
    })?;

    // c3, the revert: what c1 deployed, again (the render c1's), deployed as c3; up again, no app
    // degraded, and Slack told.
    git(&repo, now - HOUR, &["revert", "--no-edit", &c2]);
    let c3 = git(&repo, now - HOUR, &["rev-parse", "HEAD"]);
    let d3 = deploy(&f, &c3, 3)?;
    anyhow::ensure!(d3["succeeded"] == true && d3["revision"] == c3.as_str(), "the revert deploys: {d3:#}");
    until(&f, 180, "the app healthy again", || {
        let s = status(&f)?;
        Ok((s["health"] == "Healthy" && s["sync"] == "Synced", s))
    })?;
    until(&f, 60, "no app degraded, counted", || {
        let n = last(&f, "max:kiln.cd.apps{health:Degraded}")?;
        Ok((n == Some(0.0), json!(n)))
    })?;
    until(&f, 90, "Slack told the app recovered", || {
        let said: Vec<String> = mock.at("/slack").iter().filter_map(|g| g.json()["text"].as_str().map(str::to_owned)).collect();
        Ok((said.iter().any(|t| t.contains("[Recovered] kobs degraded apps")), json!(said)))
    })?;

    // c4: refused by the API server: a failed deploy, an error event, nothing for DORA.
    let c4 = commit(&repo, now - HOUR / 2, &[("deploy/refused.yaml", REFUSED)], "Add a config");
    let d4 = deploy(&f, &c4, 4)?;
    anyhow::ensure!(d4["succeeded"] == false && d4["revision"] == c4.as_str(), "the refused deployment fails: {d4:#}");
    until(&f, 60, "the failed deploy counted", || {
        let failed = total(&f, &format!("sum:kiln.cd.deploys{{app:{APP},result:failed}}.as_count()"), t0)?;
        let succeeded = total(&f, &format!("sum:kiln.cd.deploys{{app:{APP},result:succeeded}}.as_count()"), t0)?;
        Ok(((failed - 1.0).abs() < 1e-9 && (succeeded - 3.0).abs() < 1e-9, json!([failed, succeeded])))
    })?;
    let failed = until(&f, 60, "the failed deploy's event", || {
        let e = events(&f, "@event.type:cd.deploy @result:failed", t0)?;
        Ok((e.len() == 1, json!(e)))
    })?;
    anyhow::ensure!(failed[0]["status"] == "error" && failed[0]["attributes"]["revision"] == c4.as_str(), "{failed:#}");
    anyhow::ensure!(events(&f, "@event.type:cd.deploy", t0)?.len() == 4, "an event each deploy");

    // DORA: the three that succeeded, each its commit's; c2 a change failure, put right by c3's.
    let from = (now - 10 * HOUR) * 1000;
    let dd1 = until(&f, 60, "DORA's deployment of c1", || Ok(dora(&f, &c1, from)?.map_or((false, Value::Null), |d| (true, d))))?;
    let dd2 = until(&f, 60, "DORA's deployment of c2", || Ok(dora(&f, &c2, from)?.map_or((false, Value::Null), |d| (true, d))))?;
    let dd3 = until(&f, 60, "DORA's deployment of c3", || Ok(dora(&f, &c3, from)?.map_or((false, Value::Null), |d| (true, d))))?;
    anyhow::ensure!(dora(&f, &c4, from)?.is_none(), "a deploy that failed is no deployment");
    for (d, k) in [(&dd1, &d1), (&dd2, &d2), (&dd3, &d3)] {
        let a = &d["attributes"];
        anyhow::ensure!(a["service"] == APP && a["team"] == "web-team" && a["source"] == "kiln.cd" && a["git"]["repository_url"] == url.as_str(), "{d:#}");
        anyhow::ensure!(a["finished_at"] == k["at_ms"] && a["custom_tags"] == json!(["trigger:manual"]), "finished as kiln CD recorded it ({k:#}): {d:#}");
    }
    let (a1, a2, a3) = (&dd1["attributes"], &dd2["attributes"], &dd3["attributes"]);
    anyhow::ensure!(a1["lead"].is_null(), "the first deployment has none: {dd1:#}");
    let since = |at: i64| now_ms() / 1000 - at;
    anyhow::ensure!(
        a2["lead"]["commits"] == 1 && a2["lead"]["mean_s"].as_f64().is_some_and(|m| m >= (2 * HOUR) as f64 && m <= since(now - 2 * HOUR) as f64),
        "c2's lead time, from its author's time: {dd2:#}"
    );
    anyhow::ensure!(a3["lead"]["commits"] == 1 && a3["lead"]["mean_s"].as_f64().is_some_and(|m| m >= HOUR as f64 && m <= since(now - HOUR) as f64), "the revert's: {dd3:#}");
    let recovery = (d3["at_ms"].as_i64().unwrap_or(0) - d2["at_ms"].as_i64().unwrap_or(0)) / 1000;
    anyhow::ensure!(
        a2["change_failure"]["found"] == "revert"
            && a2["change_failure"]["remediation"]["id"] == dd3["id"]
            && a2["change_failure"]["remediation"]["type"] == "rollforward"
            && near(&a2["recovery_s"], recovery, 1),
        "the revert marks c2's deployment failed, recovered as c3's finished ({recovery} s on): {dd2:#}"
    );
    anyhow::ensure!(a1["change_failure"].is_null() && a3["change_failure"].is_null(), "the others stand: {dd1:#} {dd3:#}");
    until(&f, 60, "DORA's deployments counted", || {
        let n = total(&f, &format!("sum:dora.deployments{{service:{APP}}}.as_count()"), from)?;
        Ok(((n - 3.0).abs() < 1e-9, json!(n)))
    })?;

    // Gone with its objects.
    let (code, b) = f.request("DELETE", &format!("/api/cd/apps/{APP}"), None)?;
    anyhow::ensure!((200..300).contains(&code), "the app deleted: {code} {}", String::from_utf8_lossy(&b));
    until(&f, 120, "its objects deleted", || {
        let left = cluster.kubectl(&["get", "deployment,configmap", "-n", &ns, "-o", "name"])?;
        Ok((!left.lines().any(|l| l.ends_with("/web") || l.ends_with("/web-config")), json!(left)))
    })?;
    Ok(())
}
