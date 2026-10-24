//! The providers that act through kiln's runners (docs/cd-targets.md §2.2): `terraform`,
//! `pulumi`, `job`, `host` and `compose`. Their pure halves are `kiln_cd::providers::{…}`.
//!
//! A release's object becomes a run: a workflow kiln writes for it (or, for `job`, one of the
//! repository's), started at the release's commit, which the apply follows to its end; one that
//! fails fails the apply. `host` and `compose` write a job for each runner the target's selector
//! picks. The same release again starts nothing while its run passed. An object reads from the run
//! kiln last started for it.
//!
//! [`Runners`] is the seam: the server's CI ([`crate::cd::runners`]), or a fake in tests.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::host::Host;
use kiln_cd::providers::{compose, host, microvm, pulumi, terraform};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The longest kiln follows a run.
const RUN: Duration = Duration::from_secs(4 * 3600);

/// How a run stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunState {
    pub id: String,
    pub number: i64,
    /// `pending`, `queued`, `running`; or `succeeded`, `failed`, `cancelled`, `skipped`, `timed_out`.
    pub status: String,
    pub error: Option<String>,
    pub url: String,
}

impl RunState {
    pub fn done(&self) -> bool {
        !matches!(self.status.as_str(), "pending" | "queued" | "running")
    }

    fn value(&self) -> Value {
        json!({ "id": self.id, "number": self.number, "status": self.status, "error": self.error, "url": self.url })
    }
}

/// What a run starts.
pub enum Work<'a> {
    /// A workflow kiln wrote: its name and YAML.
    Written { name: &'a str, yaml: &'a str },
    /// One of the repository's workflows (its path), one of its jobs, with inputs.
    Own { workflow: &'a str, job: Option<&'a str>, inputs: &'a Map<String, Value> },
}

/// kiln's runners, as the providers that act through them use them.
pub trait Runners: Send + Sync {
    /// The connected runners `selector` picks (CI's routing constraints: `label web`,
    /// `os == linux && label web`), by name, with their OS.
    fn pick<'a>(&'a self, selector: &'a str) -> BoxFut<'a, Result<Vec<Host>>>;
    /// Starts `work` in app `app`'s repository at commit `revision` (the app's source's, when
    /// empty): the run's id.
    fn start<'a>(&'a self, app: &'a str, revision: &'a str, work: Work<'a>) -> BoxFut<'a, Result<String>>;
    /// How run `id` stands.
    fn run<'a>(&'a self, id: &'a str) -> BoxFut<'a, Result<RunState>>;
    /// A link a runner fetches an artifact (by its SHA-256, in hex) from, for `secs`.
    fn link(&self, hex: &str, secs: i64) -> Result<String>;
    /// Defines `name` in app `app`'s repository as an environment of `kind` (`microvm`) on the
    /// runners `selector` picks, where the deploy action makes deployments on the runner.
    fn environment<'a>(&'a self, app: &'a str, name: &'a str, kind: &'a str, selector: &'a str) -> BoxFut<'a, Result<()>>;
    /// Takes environment `name`'s deployment of `kind` down on `hosts`.
    fn undeploy<'a>(&'a self, app: &'a str, kind: &'a str, name: &'a str, hosts: &'a [String]) -> BoxFut<'a, Result<()>>;
}

fn runners<'a>(cx: &'a Cx<'_>) -> Result<&'a dyn Runners> {
    cx.files.runners().ok_or_else(|| anyhow::anyhow!("{} acts through kiln's runners, which this kiln doesn't reach from here", cx.conn.def.title))
}

/// The target's selector: which runners take a release (`host`, `compose`).
fn selector<'a>(cx: &'a Cx<'_>) -> Result<&'a str> {
    cx.conn.target.place_value("selector").ok_or_else(|| anyhow::anyhow!("{} names no selector", cx.conn.target.name()))
}

/// Whether a provider's releases go to each runner its selector picks.
fn on_hosts(provider: &str) -> bool {
    matches!(provider, "host" | "compose" | "microvm")
}

/// What a release of `desired` runs, for `hosts`: a workflow kiln writes (its name and YAML), or
/// none for `job`, which runs the repository's own.
fn written(cx: &Cx<'_>, desired: &Value, hosts: &[Host]) -> Result<Option<(String, String)>> {
    let name = obj::name(desired);
    let fail = |e: String| anyhow::anyhow!("{} {name}: {e}", obj::kind(desired));
    Ok(Some(match cx.conn.def.name {
        "terraform" => (format!("kiln: terraform {name}"), terraform::workflow(desired).map_err(fail)?),
        "pulumi" => (format!("kiln: pulumi {name}"), pulumi::workflow(desired).map_err(fail)?),
        "host" => {
            let hex = desired.pointer("/spec/files/artifact").and_then(Value::as_str).unwrap_or_default().trim_start_matches("sha256:").to_owned();
            let link = runners(cx)?.link(&hex, 3600)?;
            (format!("kiln: {name} on its hosts"), host::workflow(desired, hosts, &link).map_err(fail)?)
        }
        "compose" => (format!("kiln: {name} up"), compose::workflow(desired, hosts, false).map_err(fail)?),
        "microvm" => (format!("kiln: {name} on its hosts"), microvm::workflow(desired, hosts).map_err(fail)?),
        _ => return Ok(None),
    }))
}

async fn live(cx: &Cx<'_>, kind: &str, name: &str) -> Result<Option<Value>> {
    let group = cx.conn.def.kinds.iter().find(|k| k.kind == kind).map_or("", |k| k.group);
    let Some(rec) = cx.applied(group, kind, "", name) else { return Ok(None) };
    let run = match rec.made.get("run").and_then(Value::as_str) {
        Some(id) => Some(runners(cx)?.run(id).await?.value()),
        None => None,
    };
    Ok(Some(kiln_cd::providers::runner::live(&rec.object, run.as_ref())))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        out.extend(live(cx, kind, name).await?);
    }
    Ok(out)
}

/// Follows a run to its end: why, if it didn't pass.
async fn follow(cx: &Cx<'_>, id: &str, what: &str) -> Result<RunState> {
    let r = runners(cx)?;
    let state = until(cx.conn, RUN, what, || async {
        let s = r.run(id).await?;
        Ok(s.done().then_some(s))
    })
    .await?;
    if state.status != "succeeded" {
        bail!("{what}: run #{} {}{} ({})", state.number, state.status.replace('_', " "), state.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default(), state.url);
    }
    Ok(state)
}

/// Applies an object: a run of its release, unless the run of this very release passed; the
/// apply follows the run to its end.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let r = runners(cx)?;
    let (group, kind, name) = (obj::group(desired).to_owned(), obj::kind(desired).to_owned(), obj::name(desired).to_owned());
    let app = super::app_of(desired).ok_or_else(|| anyhow::anyhow!("{kind} {name} names no app"))?;
    let revision = desired.pointer("/spec/revision").and_then(Value::as_str).unwrap_or_default().to_owned();
    let hosts = if on_hosts(cx.conn.def.name) { r.pick(selector(cx)?).await? } else { Vec::new() };
    let mut key = desired.get("spec").cloned().unwrap_or(Value::Null);
    if let Value::Object(m) = &mut key {
        m.insert("hosts".into(), json!(hosts.iter().map(|h| h.name.clone()).collect::<Vec<_>>()));
    }
    let want = kiln_cd::obj::hash_value(&key).to_string();
    let prev = cx.applied(&group, &kind, "", &name).map(|a| a.made).unwrap_or(Value::Null);
    if prev.get("release").and_then(Value::as_str) == Some(want.as_str())
        && let Some(id) = prev.get("run").and_then(Value::as_str)
        && r.run(id).await?.status == "succeeded"
    {
        return live(cx, &kind, &name).await?.ok_or_else(|| anyhow::anyhow!("{kind} {name} is gone"));
    }
    let what = format!("{} {name}", cx.conn.def.title);
    // A microVM's jobs are bound to an environment of its own, where the deploy action makes it.
    if cx.conn.def.name == "microvm" {
        r.environment(&app, &microvm::environment(&name), "microvm", selector(cx)?).await?;
    }
    let id = match written(cx, desired, &hosts)? {
        Some((wf, yaml)) => r.start(&app, &revision, Work::Written { name: &wf, yaml: &yaml }).await?,
        None => {
            let workflow = desired.pointer("/spec/workflow").and_then(Value::as_str).unwrap_or_default().to_owned();
            let job = desired.pointer("/spec/job").and_then(Value::as_str).map(str::to_owned);
            let inputs = desired.pointer("/spec/inputs").and_then(Value::as_object).cloned().unwrap_or_default();
            r.start(&app, &revision, Work::Own { workflow: &workflow, job: job.as_deref(), inputs: &inputs }).await?
        }
    };
    let made = json!({ "release": want, "run": id, "hosts": hosts.iter().map(|h| h.name.clone()).collect::<Vec<_>>() });
    cx.record(&group, &kind, "", &name, Some(applied(desired, made))).await?;
    follow(cx, &id, &what).await?;
    live(cx, &kind, &name).await?.ok_or_else(|| anyhow::anyhow!("{kind} {name} is gone"))
}

/// Deletes an object: a Compose project comes down, and a microVM is taken down, on the hosts kiln
/// last brought it up on; what the others made stays (infrastructure is destroyed with its tool,
/// on purpose; a job's work is done; a host's service is the host's).
pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if !matches!(cx.conn.def.name, "compose" | "microvm") {
        tracing::info!(target = %cx.conn.target.name(), object = %key.name, "deployments: kiln leaves what a runner's job made when its object goes");
        return Ok(());
    }
    let Some(rec) = cx.applied(&key.group, &key.kind, "", &key.name) else { return Ok(()) };
    let r = runners(cx)?;
    let were: Vec<String> = rec.made.get("hosts").and_then(|h| serde_json::from_value(h.clone()).ok()).unwrap_or_default();
    if cx.conn.def.name == "microvm" {
        let app = super::app_of(&rec.object).unwrap_or_default();
        return r.undeploy(&app, "microvm", &microvm::environment(&key.name), &were).await;
    }
    let hosts: Vec<Host> = r.pick(selector(cx)?).await?.into_iter().filter(|h| were.contains(&h.name)).collect();
    if hosts.is_empty() {
        return Ok(());
    }
    let yaml = compose::workflow(&rec.object, &hosts, true).map_err(|e| anyhow::anyhow!("{e}"))?;
    let app = super::app_of(&rec.object).unwrap_or_default();
    let id = r.start(&app, "", Work::Written { name: &format!("kiln: {} down", key.name), yaml: &yaml }).await?;
    follow(cx, &id, &format!("taking {} down", key.name)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::MemRecords;
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::fake_runners::FakeRunners;
    use crate::cd::targets::{Conn, Creds, key_of};

    fn conn(provider: &str, place: &Value) -> anyhow::Result<Conn> {
        let (target, _) = kiln_cd::providers::target_of(provider, place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
        let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
        anyhow::ensure!(def.built, "{provider} isn't built");
        Ok(Conn::quick(target, def))
    }

    fn h(o: &Value) -> Option<Health> {
        kiln_cd::providers::health(o).map(|s| s.health)
    }

    /// A Terraform workspace through releases, each applied by a job kiln writes for a runner with
    /// the tool, at the release's commit: the first runs init, the workspace and apply with its
    /// variables; the same release again runs nothing; the next runs at its commit; one whose apply
    /// fails fails and reads as degraded; the release before, rolled back to, applies its commit
    /// again; the workspace stays when its object goes.
    #[test]
    fn workspaces_apply_each_release_on_a_runner() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let fake = FakeRunners::with_hosts(&[("tf-1", &["linux", "terraform"], "linux")]);
            let place = json!({ "account": "runners" });
            let conn = conn("terraform", &place)?;
            let records = MemRecords::default();
            let cx = Cx { conn: &conn, creds: Creds::Runners, records: &records, files: &fake };
            let spec = json!({ "workspace": "prod", "dir": "infra", "binary": "tofu", "vars": { "image": "${{ release.artifacts.image.digest }}" }, "runs-on": ["linux", "terraform"] });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("terraform", &place, spec.clone(), version, &[], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no workspace"))
            };
            let w1 = release("1.0.0", "sha256:aaaa")?;
            let up = apply(&cx, &w1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&w1, &cx.with_applied(up.clone())), "in sync: {up}");
            let run = fake.last();
            assert_eq!(run.revision, "c0ffee1.0.0", "at the release's commit");
            for want in ["runs-on: [\"linux\",\"terraform\"]", "cd 'infra'", "tofu init -input=false -no-color", "tofu workspace select -or-create=true 'prod'", "tofu apply -input=false -no-color -auto-approve -var 'image=sha256:aaaa'"] {
                assert!(run.ran.contains(want), "{want} in {}", run.ran);
            }
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(fake.runs.lock().len(), 1, "the same release again runs nothing");

            apply(&cx, &release("1.1.0", "sha256:bbbb")?).await?;
            assert_eq!((fake.last().revision, fake.runs.lock().len()), ("c0ffee1.1.0".to_owned(), 2));

            let failed = apply(&cx, &release("1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("run #3 failed"), "{failed}");
            assert_eq!(h(&list(&cx, "Workspace", &[(String::new(), "prod".into())]).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));

            let back = apply(&cx, &w1).await?;
            assert_eq!((fake.last().revision, h(&back)), ("c0ffee1.0.0".to_owned(), Some(Health::Healthy)), "the release before applied again");
            delete(&cx, &key_of(&w1)).await?;
            assert_eq!(fake.runs.lock().len(), 4, "nothing destroyed when the object goes");
            Ok(())
        })
    }

    /// A Pulumi stack and a repository's own workflow, each through a release: the stack's job
    /// selects it, sets its config and brings it up; the workflow runs at the release's commit with
    /// the inputs it's given.
    #[test]
    fn stacks_and_workflows_run_for_releases() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let fake = FakeRunners::with_hosts(&[("ci-1", &["linux"], "linux")]);
            let place = json!({ "account": "runners" });
            let records = MemRecords::default();
            let pulumi = conn("pulumi", &place)?;
            let cx = Cx { conn: &pulumi, creds: Creds::Runners, records: &records, files: &fake };
            let spec = json!({ "stack": "acme/shop/prod", "config": { "image": "${{ release.artifacts.image.digest }}" } });
            let s1 = released_to("pulumi", &place, spec, "1.0.0", &[], &[("image", "sha256:aaaa")])?.into_iter().next().unwrap_or_default();
            assert_eq!(obj::name(&s1), "acme.shop.prod");
            let up = apply(&cx, &s1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            for want in ["pulumi stack select 'acme/shop/prod' --create --non-interactive", "pulumi config set --stack 'acme/shop/prod' 'image' 'sha256:aaaa' --non-interactive", "pulumi up --stack 'acme/shop/prod' --yes --skip-preview --non-interactive"] {
                assert!(fake.last().ran.contains(want), "{want} in {}", fake.last().ran);
            }

            let job = conn("job", &place)?;
            let cx = Cx { conn: &job, creds: Creds::Runners, records: &records, files: &fake };
            let spec = json!({ "workflow": ".kiln/migrate.yaml", "inputs": { "version": "${{ release.version }}" } });
            let r1 = released_to("job", &place, spec, "2.0.0", &[], &[])?.into_iter().next().unwrap_or_default();
            assert_eq!(obj::name(&r1), "migrate");
            let ran = apply(&cx, &r1).await?;
            assert_eq!(h(&ran), Some(Health::Healthy), "{ran}");
            let run = fake.last();
            assert_eq!((run.ran.as_str(), run.revision.as_str()), (".kiln/migrate.yaml#* {\"version\":\"2.0.0\"}", "c0ffee2.0.0"));
            Ok(())
        })
    }

    /// A service on the hosts the target's selector picks, through releases: one job a host, a
    /// batch after the one before, each fetching the release's files from a link kiln signs, the
    /// Linux hosts' service a systemd user unit and the Mac's a launch agent, its check waited for;
    /// the same release again runs nothing; a host joining runs it again; one whose service doesn't
    /// start fails and reads as degraded; the release before, rolled back to, runs with its files.
    #[test]
    fn services_roll_across_their_hosts_in_batches() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut fake = FakeRunners::with_hosts(&[("web-1", &["web"], "linux"), ("web-2", &["web"], "linux"), ("mac-1", &["web"], "darwin"), ("db-1", &["db"], "linux")]);
            let place = json!({ "account": "runners", "selector": "label web" });
            let conn = conn("host", &place)?;
            let records = MemRecords::default();
            let (a, b) = ("a".repeat(64), "b".repeat(64));
            let spec = |start: &str| json!({ "service": "shop-web", "files": "${{ release.artifacts.dist.digest }}", "start": start, "env": { "MODE": "prod" }, "check": "http://127.0.0.1:8080/healthz", "batch": 2 });
            let release = |version: &str, hex: &str, start: &str| -> anyhow::Result<Value> {
                released_to("host", &place, spec(start), version, &[], &[("dist", &format!("sha256:{hex}"))])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no service"))
            };
            {
                let cx = Cx { conn: &conn, creds: Creds::Runners, records: &records, files: &fake };
                let s1 = release("1.0.0", &a, "bin/server --port 8080")?;
                let up = apply(&cx, &s1).await?;
                assert_eq!(h(&up), Some(Health::Healthy), "{up}");
                assert!(synced(&s1, &cx.with_applied(up.clone())), "in sync: {up}");
                let y = fake.last().ran;
                for want in [
                    "  host-web-1:\n",
                    "  host-web-2:\n",
                    "  host-mac-1:\n",
                    "    runs-on: \"web-1\"\n",
                    "    needs: [\"host-web-1\",\"host-web-2\"]\n",
                    "    checkout: false\n",
                    &format!("curl -fsSL 'https://kiln.test/_kiln/blob/object-{a}?se=3600&sp=r&sig=fake'"),
                    &format!("R=\"$D/releases/{}\"", &a[..16]),
                    "systemctl --user restart kiln-shop-web.service",
                    "ExecStart=/bin/sh -c \"bin/server --port 8080\"",
                    "Environment=\"MODE=prod\"",
                    "launchctl bootstrap \"gui/$(id -u)\"",
                    "curl -fsS -o /dev/null 'http://127.0.0.1:8080/healthz'",
                ] {
                    assert!(y.contains(want), "{want:?} in {y}");
                }
                assert!(!y.contains("db-1"), "a runner the selector doesn't pick takes nothing");
                apply(&cx, &release("1.0.0", &a, "bin/server --port 8080")?).await?;
                assert_eq!(fake.runs.lock().len(), 1, "the same release again runs nothing");
            }
            // A host joins: the release goes to it too.
            fake.hosts.push(("web-3".into(), vec!["web".into()], "linux".into()));
            let cx = Cx { conn: &conn, creds: Creds::Runners, records: &records, files: &fake };
            apply(&cx, &release("1.0.0", &a, "bin/server --port 8080")?).await?;
            assert!(fake.last().ran.contains("  host-web-3:\n"), "{}", fake.last().ran);
            apply(&cx, &release("1.1.0", &b, "bin/server --port 8080")?).await?;
            assert!(fake.last().ran.contains(&format!("releases/{}", &b[..16])));
            let failed = apply(&cx, &release("1.2.0", &b, "bin/broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("failed"), "{failed}");
            assert_eq!(h(&list(&cx, "Service", &[(String::new(), "shop-web".into())]).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));
            let back = apply(&cx, &release("1.0.0", &a, "bin/server --port 8080")?).await?;
            assert!(fake.last().ran.contains(&format!("releases/{}", &a[..16])) && h(&back) == Some(Health::Healthy), "{back}");
            Ok(())
        })
    }

    /// A Compose project and a microVM on their hosts, through releases: the project's file and
    /// environment written on each host and brought up with `docker compose up --wait`, taken down
    /// again when its object goes; the microVM's jobs bound to an environment of its own, kind
    /// microvm, where the deploy action boots its image; taken down on its hosts when it goes.
    #[test]
    fn compose_projects_and_microvms_come_up_on_their_hosts() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let fake = FakeRunners::with_hosts(&[("box-1", &["docker", "vm"], "linux"), ("box-2", &["docker"], "linux")]);
            let records = MemRecords::default();
            let place = json!({ "account": "runners", "selector": "label docker" });
            let compose = conn("compose", &place)?;
            let cx = Cx { conn: &compose, creds: Creds::Runners, records: &records, files: &fake };
            let file = "services:\n  web:\n    image: ${IMAGE}\n    healthcheck:\n      test: [\"CMD\", \"wget\", \"-qO-\", \"http://localhost/\"]\n";
            let spec = json!({ "project": "shop", "env": { "IMAGE": "ghcr.io/acme/shop@${{ release.artifacts.image.digest }}" } });
            let p1 = released_to("compose", &place, spec, "1.0.0", &[("compose.yaml", file)], &[("image", "sha256:aaaa")])?.into_iter().next().unwrap_or_default();
            let up = apply(&cx, &p1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            let y = fake.last().ran;
            for want in ["  compose-box-1:\n", "  compose-box-2:\n", "    needs: [\"compose-box-1\"]\n", "      services:\n", "      web:\n", "IMAGE=ghcr.io/acme/shop@sha256:aaaa", "docker compose -p 'shop' -f \"$D/compose.yaml\" --env-file \"$D/.env\" up -d --wait --remove-orphans"] {
                assert!(y.contains(want), "{want:?} in {y}");
            }
            delete(&cx, &key_of(&p1)).await?;
            assert!(fake.last().ran.contains("down --remove-orphans") && fake.last().ran.contains("  compose-box-2:\n"), "taken down on both: {}", fake.last().ran);

            let place = json!({ "account": "runners", "selector": "label vm" });
            let vm = conn("microvm", &place)?;
            let cx = Cx { conn: &vm, creds: Creds::Runners, records: &records, files: &fake };
            let spec = json!({ "vm": "api", "image": "ghcr.io/acme/api@${{ release.artifacts.image.digest }}", "ports": ["8080:80"], "health": "http://127.0.0.1:8080/healthz", "env": { "MODE": "prod" }, "memory": "1G" });
            let v1 = released_to("microvm", &place, spec, "1.0.0", &[], &[("image", "sha256:aaaa")])?.into_iter().next().unwrap_or_default();
            let up = apply(&cx, &v1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            let envs = fake.environments.lock().clone();
            assert!(envs.iter().any(|(_, n, k, s)| n == "kiln-api" && k == "microvm" && s == "label vm"), "{envs:?}");
            let y = fake.last().ran;
            for want in ["  vm-box-1:\n", "    environment: \"kiln-api\"\n", "        uses: kiln/deploy@v1\n", "          image: \"ghcr.io/acme/api@sha256:aaaa\"\n", "          ports: |\n            8080:80\n", "          memory: \"1G\"\n"] {
                assert!(y.contains(want), "{want:?} in {y}");
            }
            assert!(!y.contains("box-2"), "{y}");
            delete(&cx, &key_of(&v1)).await?;
            assert_eq!(fake.undeployed.lock().clone(), vec![("microvm".to_owned(), "kiln-api".to_owned(), vec!["box-1".to_owned()])]);
            Ok(())
        })
    }
}
