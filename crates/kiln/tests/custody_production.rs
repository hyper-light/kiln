//! W27-R08 (docs/ci.md; the Transfers plan's step 14, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): custody to production, on a test cluster. A leg on fedora-2
//! builds a multi-platform image (an index of pause 3.10's linux/amd64 and linux/arm64 manifests)
//! from acme/app's commit and pushes it to kiln's registry; the same bytes go to a registry on the
//! cluster's node; production releases web, whose Deployment names the image by its index's
//! digest, through KilnCD. Each pod's record walks back, unbroken, through the node's resolution of
//! the index to its platform's manifest, the cluster's receipt of what CD handed it, CD's receipt of
//! the image from kiln's registry, kiln's receipt of what the leg made, the leg, and kiln's hand-on
//! of the commit. Then production requires a verified chain, and one pod's image is swapped (a test
//! hook: `kubectl set image` on the pod, to pause 3.10.1): the break is recorded at that pod's receipt, and
//! production no longer counts the deploy healthy (its registration needs a person, saying why).
//!
//! It needs a test cluster, never production's: `KILN_CUSTODY_KUBECONFIG` (and
//! `KILN_CUSTODY_CONTEXT`, else the kubeconfig's current context), the registry.k8s.io and
//! docker.io registries reachable from here and from its node, and `kubectl` and `curl`:
//! `KILN_CUSTODY_KUBECONFIG=/tmp/kilncd-e2e/kubeconfig KILN_CUSTODY_CONTEXT=kilncd-e2e cargo test -p
//! kiln --test custody_production -- --ignored`.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, file_url, http, start_cd};
use serde_json::{Value, json};
use sha2::Digest as _;

const TALK: Talk = Talk { lines: 1, every_ms: 10 };
const PAUSE: &str = "https://registry.k8s.io/v2/pause";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// `cmd` run to its end: its stdout, or why it failed.
fn output(cmd: &mut Command) -> anyhow::Result<Vec<u8>> {
    let out = cmd.stdin(Stdio::null()).output()?;
    anyhow::ensure!(out.status.success(), "{cmd:?}: {}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
    Ok(out.stdout)
}

fn sha256(b: &[u8]) -> String {
    sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Waits up to `secs` for `f` to give a value: it, or why not.
fn until<T>(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Result<T, String>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            Ok(t) => return Ok(t),
            Err(why) if Instant::now() >= deadline => anyhow::bail!("{what}: {why}"),
            Err(_) => std::thread::sleep(Duration::from_millis(1000)),
        }
    }
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn send(s: &Server, method: &str, path: &str, body: &Value) -> anyhow::Result<Value> {
    let (st, said) = http(s.port, method, path, &s.admin, Some(body))?;
    anyhow::ensure!((200..300).contains(&st), "{method} {path}: {st} {said}\n{}", s.log());
    Ok(serde_json::from_str(&said).unwrap_or(Value::Null))
}

/// The test cluster, and a namespace of the test's own, deleted after it.
struct Cluster {
    kubeconfig: PathBuf,
    context: String,
    namespace: String,
}

impl Cluster {
    fn named() -> anyhow::Result<Self> {
        let kubeconfig = PathBuf::from(std::env::var("KILN_CUSTODY_KUBECONFIG").map_err(|_| anyhow::anyhow!("KILN_CUSTODY_KUBECONFIG names no test cluster"))?);
        let context = match std::env::var("KILN_CUSTODY_CONTEXT") {
            Ok(c) => c,
            Err(_) => String::from_utf8(output(Command::new("kubectl").arg("--kubeconfig").arg(&kubeconfig).args(["config", "current-context"]))?)?.trim().to_owned(),
        };
        anyhow::ensure!(context != "kiln", "context `kiln` is production's cluster: name a test cluster's");
        let c = Self { kubeconfig, context, namespace: format!("kiln-custody-{}", std::process::id()) };
        c.kubectl(&["create", "namespace", &c.namespace])?;
        Ok(c)
    }

    fn kubectl(&self, args: &[&str]) -> anyhow::Result<String> {
        Ok(String::from_utf8(output(Command::new("kubectl").arg("--kubeconfig").arg(&self.kubeconfig).arg("--context").arg(&self.context).args(args))?)?.trim().to_owned())
    }

    fn apply(&self, yaml: &str) -> anyhow::Result<()> {
        let mut child = Command::new("kubectl")
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .arg("--context")
            .arg(&self.context)
            .args(["apply", "-n", &self.namespace, "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut i) = child.stdin.take() {
            use std::io::Write as _;
            i.write_all(yaml.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "kubectl apply: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }

    /// The first node's address, as this machine reaches it.
    fn node_ip(&self) -> anyhow::Result<String> {
        self.kubectl(&["get", "nodes", "-o", "jsonpath={.items[0].status.addresses[?(@.type==\"InternalIP\")].address}"])
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        if let Err(e) = self.kubectl(&["delete", "namespace", &self.namespace, "--wait=false", "--ignore-not-found"]) {
            eprintln!("namespace {} is left: {e:#}", self.namespace);
        }
    }
}

/// An image to push: its blobs and its platforms' manifests by SHA-256, and its index.
struct Image {
    blobs: BTreeMap<String, Vec<u8>>,
    manifests: Vec<(String, String, Vec<u8>)>,
    index: Vec<u8>,
}

impl Image {
    fn digest(&self) -> String {
        sha256(&self.index)
    }
}

fn curl(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    output(Command::new("curl").args(["-fsSL", "--retry", "3"]).args(args))
}

/// pause 3.10's linux/amd64 and linux/arm64 images, from registry.k8s.io, as one index of ours: a
/// multi-platform image that runs, and whose amd64 node pulls one manifest of two.
fn pause() -> anyhow::Result<Image> {
    let list: Value = serde_json::from_slice(&curl(&["-H", &format!("Accept: {INDEX}, application/vnd.docker.distribution.manifest.list.v2+json"), &format!("{PAUSE}/manifests/3.10")])?)?;
    let (mut blobs, mut manifests, mut entries) = (BTreeMap::new(), Vec::new(), Vec::new());
    for (arch, variant) in [("amd64", None), ("arm64", Some("v8"))] {
        let entry = list
            .get("manifests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|m| m.pointer("/platform/os") == Some(&json!("linux")) && m.pointer("/platform/architecture") == Some(&json!(arch)))
            .ok_or_else(|| anyhow::anyhow!("pause 3.10 has no linux/{arch}: {list}"))?;
        let (digest, media_type) = (text(entry, "digest"), text(entry, "mediaType"));
        let bytes = curl(&["-H", &format!("Accept: {media_type}"), &format!("{PAUSE}/manifests/{digest}")])?;
        let hex = sha256(&bytes);
        anyhow::ensure!(digest == format!("sha256:{hex}"), "pause's {arch} manifest came as {hex}");
        let m: Value = serde_json::from_slice(&bytes)?;
        let parts = std::iter::once(m.get("config").cloned().unwrap_or_default()).chain(m.get("layers").and_then(Value::as_array).cloned().unwrap_or_default());
        for b in parts {
            let d = text(&b, "digest").to_owned();
            let got = curl(&[&format!("{PAUSE}/blobs/{d}")])?;
            anyhow::ensure!(d == format!("sha256:{}", sha256(&got)), "pause's blob {d} came otherwise");
            blobs.insert(sha256(&got), got);
        }
        let mut platform = json!({ "os": "linux", "architecture": arch });
        if let (Some(v), Some(p)) = (variant, platform.as_object_mut()) {
            p.insert("variant".into(), json!(v));
        }
        entries.push(json!({ "mediaType": media_type, "digest": digest, "size": bytes.len(), "platform": platform }));
        manifests.push((hex, media_type.to_owned(), bytes));
    }
    let index = serde_json::to_vec(&json!({ "schemaVersion": 2, "mediaType": INDEX, "manifests": entries }))?;
    Ok(Image { blobs, manifests, index })
}

/// The image's files in a repository's tree (`image/`), and the script a leg pushes them with to
/// kiln's registry, saying the index's digest as the step's output.
fn lay(img: &Image, tree: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(tree.join("image/blobs"))?;
    std::fs::create_dir_all(tree.join("image/manifests"))?;
    for (hex, b) in &img.blobs {
        std::fs::write(tree.join("image/blobs").join(hex), b)?;
    }
    for (hex, media_type, b) in &img.manifests {
        std::fs::write(tree.join("image/manifests").join(format!("{hex}.json")), b)?;
        std::fs::write(tree.join("image/manifests").join(format!("{hex}.type")), media_type)?;
    }
    std::fs::write(tree.join("image/index.json"), &img.index)?;
    std::fs::write(
        tree.join("image/push.sh"),
        r#"set -eu
host="http://${KILN_REGISTRY_HOST}"
reg="$host/v2/acme/app/web"
for b in image/blobs/*; do
  d=$(basename "$b")
  loc=$(curl -fsS -u "x:${KILN_TOKEN}" -X POST -D - -o /dev/null "$reg/blobs/uploads/" | tr -d '\r' | awk 'tolower($1)=="location:" {print $2}')
  case "$loc" in http*) ;; *) loc="$host$loc" ;; esac
  case "$loc" in *\?*) sep='&' ;; *) sep='?' ;; esac
  curl -fsS -u "x:${KILN_TOKEN}" -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$b" "$loc${sep}digest=sha256:$d"
done
for m in image/manifests/*.json; do
  d=$(basename "$m" .json)
  curl -fsS -u "x:${KILN_TOKEN}" -X PUT -H "Content-Type: $(cat "image/manifests/$d.type")" --data-binary @"$m" "$reg/manifests/sha256:$d"
done
curl -fsS -u "x:${KILN_TOKEN}" -X PUT -H 'Content-Type: application/vnd.oci.image.index.v1+json' --data-binary @image/index.json "$reg/manifests/v1"
echo "pushed sha256:$(sha256sum image/index.json | cut -d' ' -f1)"
echo "digest=sha256:$(sha256sum image/index.json | cut -d' ' -f1)" >> "$GITHUB_OUTPUT"
"#,
    )?;
    Ok(())
}

/// The same bytes pushed to the registry at `base` (no credentials): what the cluster's node pulls.
fn push(base: &str, img: &Image, dir: &Path) -> anyhow::Result<()> {
    let reg = format!("{base}/v2/acme/app/web");
    let file = |name: &str, b: &[u8]| -> anyhow::Result<String> {
        let p = dir.join(name);
        std::fs::write(&p, b)?;
        Ok(format!("@{}", p.display()))
    };
    for (hex, b) in &img.blobs {
        let head = String::from_utf8(curl(&["-X", "POST", "-D", "-", "-o", "/dev/null", &format!("{reg}/blobs/uploads/")])?)?;
        let loc = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.trim().to_owned())).ok_or_else(|| anyhow::anyhow!("no upload: {head}"))?;
        let loc = if loc.starts_with("http") { loc } else { format!("{base}{loc}") };
        let sep = if loc.contains('?') { '&' } else { '?' };
        curl(&["-X", "PUT", "-H", "Content-Type: application/octet-stream", "--data-binary", &file(hex, b)?, &format!("{loc}{sep}digest=sha256:{hex}")])?;
    }
    for (hex, media_type, b) in &img.manifests {
        curl(&["-X", "PUT", "-H", &format!("Content-Type: {media_type}"), "--data-binary", &file(hex, b)?, &format!("{reg}/manifests/sha256:{hex}")])?;
    }
    curl(&["-X", "PUT", "-H", &format!("Content-Type: {INDEX}"), "--data-binary", &file("index.json", &img.index)?, &format!("{reg}/manifests/v1")])?;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

const BUILD: &str = r#"on: push
jobs:
  build:
    runs-on: self-hosted
    permissions:
      contents: read
      packages: write
    steps:
      - id: push
        run: sh image/push.sh
      - uses: kiln/attest@v1
        with:
          subject-name: acme/app/web
          subject-digest: ${{ steps.push.outputs.digest }}
"#;

/// web's Deployment: two pods of the image by its index's digest, from the node's own registry.
fn deployment(image: &str) -> String {
    format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 2\n  selector:\n    matchLabels: {{ app: web }}\n  template:\n    metadata:\n      labels: {{ app: web }}\n    spec:\n      containers:\n        - name: web\n          image: {image}\n"
    )
}

/// The registry the node pulls from: a pod on its host network, on `port`.
fn registry(port: u16) -> String {
    format!(
        "apiVersion: v1\nkind: Pod\nmetadata:\n  name: registry\n  labels: {{ app: registry }}\nspec:\n  hostNetwork: true\n  containers:\n    - name: registry\n      image: registry:2\n      env:\n        - {{ name: REGISTRY_HTTP_ADDR, value: \"0.0.0.0:{port}\" }}\n"
    )
}

/// Each hop of a chain kiln walked: (kind, custodian, taken_for).
fn hops(chain: &Value) -> Vec<(String, String, String)> {
    chain.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (text(h, "kind").to_owned(), text(h, "custodian").to_owned(), text(h, "taken_for").to_owned())).collect()
}

#[test]
#[ignore = "needs a test cluster (KILN_CUSTODY_KUBECONFIG), and registry.k8s.io and docker.io"]
fn w27_r08_custody_to_production() -> anyhow::Result<()> {
    let cluster = Cluster::named()?;
    let ns = cluster.namespace.clone();
    let node_ip = cluster.node_ip()?;
    let port = 31000 + u16::try_from(std::process::id() % 2000)?;
    cluster.apply(&registry(port))?;
    cluster.kubectl(&["wait", "-n", &ns, "--for=condition=Ready", "pod/registry", "--timeout=300s"])?;
    let img = pause()?;
    let index = img.digest();
    let dir = tempfile::tempdir()?;
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs)?;
    push(&format!("http://{node_ip}:{port}"), &img, &blobs)?;

    // acme/app, built from its commit by a leg on fedora-2, which pushes the image to kiln's registry
    // and attests it.
    let s = start_cd(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", extra: "" })?;
    let (_runner, log) = agents::runner_with(dir.path(), s.port, "fedora-2", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(180))?;
    let repo = send(&s, "POST", "/api/repos", &json!({ "name": "acme/app", "mode": "native" }))?;
    let repo_id = text(&repo, "id").to_owned();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".kiln"))?;
    git(&work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join(".kiln/build.yaml"), BUILD)?;
    lay(&img, &work)?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "web"])?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&work, &["push", "-q", &push, "HEAD:main"])?;
    // A repository's first poll takes its branches as they are, building none: the commit after it
    // is the one built.
    send(&s, "POST", &format!("/api/repos/{repo_id}/poll"), &json!({}))?;
    std::fs::write(work.join("README.md"), "web\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "web, built"])?;
    let commit = git(&work, &["rev-parse", "HEAD"])?;
    git(&work, &["push", "-q", &push, "HEAD:main"])?;
    send(&s, "POST", &format!("/api/repos/{repo_id}/poll"), &json!({}))?;
    let run = until(600, "acme/app's run at its commit", || {
        let runs = get(&s, "/api/ci/runs?repo=acme/app&limit=20")?;
        let found = runs.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| text(r, "sha") == commit && r.get("finished_at").is_some_and(|f| !f.is_null())).cloned();
        Ok(found.ok_or_else(|| format!("{runs}")))
    })?;
    anyhow::ensure!(text(&run, "conclusion") == "success", "the build: {run}\n{}", s.log());

    // web's manifests, in a repository of its own, tagged v1: the image by its index's digest, from
    // the node's registry.
    let image = format!("localhost:{port}/acme/app/web@sha256:{index}");
    let deploy = dir.path().join("deploy");
    std::fs::create_dir_all(deploy.join("deploy"))?;
    git(&deploy, &["init", "-q", "-b", "main"])?;
    std::fs::write(deploy.join("deploy/web.yaml"), deployment(&image))?;
    git(&deploy, &["add", "."])?;
    git(&deploy, &["commit", "-qm", "web"])?;
    git(&deploy, &["tag", "v1"])?;

    // Released to production through KilnCD, on the test cluster.
    send(&s, "POST", "/api/cd/clusters", &json!({ "name": "e2e", "context": cluster.context, "kubeconfig": cluster.kubeconfig }))?;
    until(120, "the test cluster connected", || {
        let c = get(&s, "/api/cd/clusters")?;
        let ok = c.get("clusters").and_then(Value::as_array).into_iter().flatten().any(|c| text(c, "name") == "e2e" && c.get("connected") == Some(&json!(true)));
        Ok(if ok { Ok(()) } else { Err(c.to_string()) })
    })?;
    send(&s, "POST", "/api/deploy/environments", &json!({ "name": "production", "provider": "kilncd", "targets": { "kubernetes": ["e2e"] }, "gates": { "require_chain": false } }))?;
    send(&s, "POST", "/api/deploy/apps", &json!({ "name": "web", "project": "acme", "namespace": ns, "package": { "kind": "git", "repo": file_url(&deploy), "path": "deploy" } }))?;
    send(&s, "POST", "/api/deploy/apps/web/environments/production", &json!({}))?;
    let artifacts = json!({ "web": { "repository": format!("localhost:{port}/acme/app/web"), "tag": "v1", "digest": format!("sha256:{index}") } });
    let r = send(&s, "POST", "/api/deploy/releases", &json!({ "app": "web", "environment": "production", "version": "v1", "artifacts": artifacts }))?;
    let id = r.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("no release: {r}"))?;
    until(900, "the release landed", || {
        let r = get(&s, &format!("/api/deploy/releases/{id}"))?;
        match text(&r, "state") {
            "succeeded" => Ok(Ok(())),
            "failed" | "refused" | "cancelled" => anyhow::bail!("release {id}: {r}\n{}", s.log()),
            _ => Ok(Err(r.to_string())),
        }
    })?;

    // Each pod's container runs what the deploy handed on, and its chain walks back to the commit.
    let app = "/api/cd/apps/acme/web-production/custody";
    let custody = until(300, "both pods verified", || {
        let v = get(&s, app)?;
        let ok = v.pointer("/verdict/state") == Some(&json!("verified")) && v.pointer("/verdict/pods") == Some(&json!(2));
        Ok(if ok { Ok(v) } else { Err(v.to_string()) })
    })?;
    let pods: Vec<Value> = custody.get("pods").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(pods.len() == 2, "{custody:#}");
    let amd64 = img.manifests.first().map(|m| m.0.clone()).unwrap_or_default();
    for p in &pods {
        assert_eq!((text(p, "platform"), text(p, "wants"), text(p, "runs"), p.get("follows")), ("linux/amd64", amd64.as_str(), amd64.as_str(), Some(&json!(true))), "{p:#}");
        assert!(text(p, "image_id").contains(&index) || text(p, "image_id").contains(&amd64), "the runtime reports the index, or its platform's manifest: {p:#}");
        let chain = get(&s, &format!("/api/custody?digest={}", text(p, "record")))?;
        assert_eq!(chain.get("unbroken"), Some(&json!(true)), "{chain:#}");
        let h = hops(&chain);
        let (pod, node) = (format!("kubernetes://e2e/{ns}/pod/{}", text(p, "pod")), format!("kubernetes://e2e/node/{}", text(p, "node")));
        for (kind, custodian, taken_for) in [
            ("received", "kiln://hyper-light/server", pod.as_str()),
            ("taken", "kiln://hyper-light/server", node.as_str()),
            ("received", "kiln://hyper-light/server", node.as_str()),
            ("taken", "kiln://hyper-light/server", "kubernetes://e2e"),
            ("received", "kiln://hyper-light/server", "kubernetes://e2e"),
            ("handed-on", "kiln://hyper-light/server/cd", ""),
            ("received", "kiln://hyper-light/server/cd", ""),
            ("handed-on", "kiln://hyper-light/server", ""),
            ("received", "kiln://hyper-light/server", ""),
        ] {
            assert!(h.contains(&(kind.to_owned(), custodian.to_owned(), taken_for.to_owned())), "{kind} by {custodian} for {taken_for:?} in {h:#?}");
        }
        assert!(h.iter().any(|(_, c, _)| c.ends_with("/leg/build")), "the leg that built it: {h:#?}");
        // Through the index's resolution: the node's hand-on of its platform's manifest, made from its
        // receipt of the index.
        let resolved = chain.get("hops").and_then(Value::as_array).into_iter().flatten().find(|x| text(x, "kind") == "taken" && text(x, "taken_for") == node).cloned().unwrap_or_default();
        assert_eq!(resolved.pointer("/subject/0/digest/sha256").and_then(Value::as_str), Some(amd64.as_str()), "{resolved:#}");
        assert_eq!(resolved.get("made_from").and_then(Value::as_array).map(Vec::len), Some(1), "{resolved:#}");
        // The node's receipt says what it pulls: the index's amd64 manifest.
        let pulled = chain.get("hops").and_then(Value::as_array).into_iter().flatten().find(|x| text(x, "kind") == "received" && text(x, "taken_for") == node).cloned().unwrap_or_default();
        assert_eq!(pulled.pointer("/subject/0/digest/sha256").and_then(Value::as_str), Some(index.as_str()), "the image the deploy named: {pulled:#}");
        assert_eq!(pulled.pointer("/subject/0/annotations/pulls").and_then(Value::as_str), Some(format!("sha256:{amd64}").as_str()), "{pulled:#}");
        // Back to kiln's hand-on of the commit to fedora-2's runner, where the image's chain began.
        let began: Vec<&Value> = chain.get("hops").and_then(Value::as_array).into_iter().flatten().filter(|x| x.get("began") == Some(&json!(true))).collect();
        assert!(began.iter().any(|x| x.get("subject").and_then(Value::as_array).into_iter().flatten().any(|t| t.pointer("/digest/gitCommit") == Some(&json!(commit)))), "back to the commit: {chain:#}");
    }

    // Walked as `kiln custody walk` walks it (step 15): from a pod back to the commit, each hop in
    // words; and with one hop's record removed, then changed where kiln keeps it, the break named
    // at that hop.
    let first = pods.first().map(|p| text(p, "record").to_owned()).unwrap_or_default();
    let walked = common::custody::walked_and_broken(s.port, &s.admin, &dir.path().join("data/kiln.db"), &first)?;
    let words: Vec<&str> = walked.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| text(h, "words")).collect();
    assert!(words.first().is_some_and(|w| w.starts_with(&format!("kiln for pod {ns}/")) && w.contains(" from node ")), "{words:#?}");
    assert!(words.iter().any(|w| w.starts_with("kiln's CD handed ")), "{words:#?}");
    assert!(words.iter().any(|w| w.contains("'s leg build on fedora-2")), "{words:#?}");

    // Production requires a verified chain now; one pod's image is swapped under it.
    let mut env = get(&s, "/api/deploy/environments/production")?;
    if let Some(g) = env.get_mut("gates").and_then(Value::as_object_mut) {
        g.insert("require_chain".into(), json!(true));
    }
    send(&s, "PUT", "/api/deploy/environments/production", &env)?;
    let swapped = text(pods.first().unwrap_or(&Value::Null), "pod").to_owned();
    cluster.kubectl(&["set", "image", "-n", &ns, &format!("pod/{swapped}"), "web=registry.k8s.io/pause:3.10.1"])?;
    let broken = until(300, "the swapped pod's break", || {
        let v = get(&s, app)?;
        let ok = v.pointer("/verdict/state") == Some(&json!("broken")) && v.pointer("/verdict/why").and_then(Value::as_str).is_some_and(|w| w.contains(&swapped));
        Ok(if ok { Ok(v) } else { Err(v.to_string()) })
    })?;
    let record = broken.get("pods").and_then(Value::as_array).into_iter().flatten().find(|p| text(p, "pod") == swapped && p.get("follows") == Some(&json!(false))).map(|p| text(p, "record").to_owned()).unwrap_or_default();
    let breaks = get(&s, "/api/custody/breaks")?;
    let at = breaks.get("breaks").and_then(Value::as_array).into_iter().flatten().find(|b| text(b, "record") == record).cloned().unwrap_or_default();
    assert_eq!(text(&at, "kind"), "changed", "recorded at the pod's receipt: {breaks:#}");
    assert!(text(&at, "detail").contains(&amd64), "naming what it should run: {at:#}");
    let chain = get(&s, &format!("/api/custody?digest={record}"))?;
    assert_eq!(chain.get("unbroken"), Some(&json!(false)), "{chain:#}");
    // Production doesn't count the deploy healthy: its registration needs a person, saying why.
    until(180, "production's registration degraded", || {
        let reg = get(&s, "/api/deploy/apps/web/environments/production")?;
        let why = reg.pointer("/registration/degraded").or_else(|| reg.get("degraded")).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(if why.contains("requires a verified chain") && why.contains(&swapped) { Ok(()) } else { Err(reg.to_string()) })
    })?;
    Ok(())
}
