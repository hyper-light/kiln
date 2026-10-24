//! `kiln deploy` and `kiln app` (docs/deploy-api.md §12.2), over the server's `/api/deploy`.
//! Environments are `kiln deploy env`: `kiln env` is CI's (a repository's environments, ci.md
//! W15), until the two become one.
//!
//! ```sh
//! kiln deploy env add staging --cluster dev
//! kiln app create web --project shop --owner shop --helm-repo oci://ghcr.io/acme/charts --chart web \
//!     --set 'image.tag=${{ release.artifacts.image.tag }}'
//! kiln app register web --env staging
//! kiln deploy web 1.4.2 --to staging --artifact image=ghcr.io/acme/web:1.4.2
//! kiln deploy web --to staging --values values.yaml
//! kiln deploy rollback web --env staging
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

/// Where the server is, and how to show answers.
#[derive(clap::Args, Clone)]
pub struct Conn {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    /// Print JSON.
    #[arg(long, global = true)]
    json: bool,
}

/// `kiln deploy`: release an app, and follow it.
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DeployArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: Option<DeployCmd>,
    /// The app.
    app: Option<String>,
    /// The version to deploy. Without one, `--values` makes a config release.
    version: Option<String>,
    /// The environment.
    #[arg(long, visible_alias = "env")]
    to: Option<String>,
    /// An artifact the version deploys: `name=repository:tag`, `name=repository@sha256:…`, or both.
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
    /// A config release: this file becomes the environment's values.yaml.
    #[arg(long)]
    values: Option<PathBuf>,
    /// A promotion: what this environment runs, the same version and artifacts.
    #[arg(long = "from")]
    from: Option<String>,
    /// A secrets release: the app's secrets at their versions now, its version and values kept
    /// (after `kiln app secret set --no-release`).
    #[arg(long, conflicts_with_all = ["version", "values", "from"])]
    secrets: bool,
    /// Failed, it isn't reverted: it's held for a person (a migration that can't run backwards).
    #[arg(long)]
    no_revert: bool,
    /// Return once the release is asked for, without following it.
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Subcommand)]
enum DeployCmd {
    /// What runs, what's in flight and what its provider says, for an app in an environment.
    Status {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Releases, newest first.
    History {
        app: Option<String>,
        #[arg(long, visible_alias = "to")]
        env: Option<String>,
        /// Those of one version.
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One release.
    Show { release: i64 },
    /// Go back to an earlier release (by default the newest that succeeded before what runs).
    Rollback {
        app: String,
        #[arg(long)]
        env: String,
        /// The release to go back to.
        #[arg(long)]
        to: Option<i64>,
        #[arg(long)]
        no_wait: bool,
    },
    /// Stop a release that is waiting or being delivered.
    Cancel { release: i64 },
    /// Approve a release waiting for approval.
    Approve {
        release: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Reject a release waiting for approval: it is cancelled.
    Reject {
        release: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Run the app in the environment again, at its last version.
    Enable {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Stop running the app in the environment: what it runs is removed, unless `--keep`.
    Disable {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// What an app runs in an environment, as its provider sees it.
    Resources {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// The manifests it renders (or those `--version` would).
    Manifests {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// What a sync would change (or what deploying `--version` would).
    Diff {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// The last lines of a workload's pods' logs.
    Logs {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        /// Deployment, StatefulSet, Pod, …
        #[arg(long, default_value = "Deployment")]
        kind: String,
        /// The object's name; the app's by default.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        container: Option<String>,
        #[arg(long, default_value_t = 200)]
        tail: u32,
    },
    /// A fleet change: one version, or values set (`--set path=value`), released to many apps of
    /// an environment at once (those given, or of a template, a project or an owner; all registered
    /// there when none is), each through its gates. Or a change in each app's source repository,
    /// one pull request per repository: a kiln-base version (`--base`), or values set in its chart
    /// (`--set … --in-source`); on kiln's own git server a branch and its diff, merged with
    /// `--merge <id>`. With `--show <id>`, how one stands; `--withdraw <id>` closes what's open.
    Fleet(Box<FleetArgs>),
    /// Delivery metrics (DORA's four keys) from the releases: of an app, an environment, or all.
    Metrics {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, visible_alias = "to")]
        env: Option<String>,
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// The versions an app has run in an environment.
    Versions {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// What can be done to an object an app runs (`Deployment/web`), or, with an action, doing it:
    /// restart, pause, resume, scale (`--set replicas=3`), suspend, create-job, delete (a pod) and
    /// whatever else its provider offers.
    Action {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        /// `Kind/name`: `Deployment/web`, `Pod/web-7d9c-abcde`, `CronJob/report`.
        object: String,
        action: Option<String>,
        #[arg(long)]
        namespace: Option<String>,
        /// Its API group, when its kind isn't one kiln knows.
        #[arg(long)]
        group: Option<String>,
        /// What the action takes: `replicas=3`.
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// The environments apps are released to.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
}

/// `kiln app`: apps, and where they're registered.
#[derive(clap::Args)]
pub struct AppArgs {
    #[command(flatten)]
    conn: Conn,
    #[command(subcommand)]
    cmd: AppCmd,
}

#[derive(clap::Subcommand)]
enum AppCmd {
    /// Every app.
    List,
    /// An app, and what runs where.
    Show { app: String },
    /// Create (or replace) an app.
    Create(Box<NewApp>),
    /// Register an app in an environment.
    Register {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Remove an app from an environment: what it runs is removed, unless `--keep`.
    Deregister {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
        #[arg(long)]
        keep: bool,
    },
    /// Turn the provider's automation off for hand edits; the next release turns it back on.
    Pause {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    Resume {
        app: String,
        #[arg(long, visible_alias = "to")]
        env: String,
    },
    /// Change some of an app: owners, properties, contacts, template, source repository.
    Set {
        app: String,
        /// Its owner teams, replacing them: the owner first, then a co-owner and an ops owner.
        #[arg(long = "owner")]
        owners: Vec<String>,
        /// `name=value`; `name=` removes it.
        #[arg(long = "property")]
        properties: Vec<String>,
        /// Who is on call.
        #[arg(long)]
        oncall: Option<String>,
        /// Chat channels its release messages go to, replacing them.
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// `name=url`, added to its links.
        #[arg(long = "link")]
        links: Vec<String>,
        #[arg(long)]
        template: Option<String>,
        /// Its source repository, one kiln has; `--source ''` for none.
        #[arg(long)]
        source: Option<String>,
    },
    /// The app as Backstage's catalog reads it (catalog-info.yaml).
    Catalog { app: String },
    /// An app's secrets in an environment (docs/deploy-api.md §3.5): names and versions. A value
    /// is set, never shown again.
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Delete an app: one registered nowhere, or with `--everywhere` deregistered first.
    Delete {
        app: String,
        #[arg(long)]
        everywhere: bool,
        /// With `--everywhere`: leave what it runs in place.
        #[arg(long)]
        keep: bool,
    },
    /// The templates apps are made from.
    Templates,
    /// Add (or replace) a template from a YAML or JSON file.
    TemplateAdd { file: PathBuf },
    TemplateRm { name: String },
    /// An app's chart on kiln's base chart, written into a repository: charts/<app> (depending on
    /// kiln-base in kiln's registry, or with it vendored) and deploy/<env>.yaml, each environment's
    /// values to start from. What's there is kept unless `--force`; `--base` moves only kiln-base
    /// to the version this kiln ships.
    Init {
        app: String,
        /// The repository to write into.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// The image it runs (`ghcr.io/acme/web`); each release gives its tag or digest.
        #[arg(long)]
        image: Option<String>,
        /// The port it listens on.
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long = "env", default_values_t = ["staging".to_owned(), "production".to_owned()])]
        environments: Vec<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, conflicts_with = "force")]
        base: bool,
        /// The kiln whose registry kiln-base comes from (`kiln.example.com`); the server's URL
        /// (`--url`, KILN_URL) by default.
        #[arg(long)]
        registry: Option<String>,
        /// Copy kiln-base into the chart rather than depend on it in a registry (no kiln needed to
        /// build it).
        #[arg(long, conflicts_with = "registry")]
        vendor: bool,
    },
}

/// `kiln app secret`.
#[derive(clap::Subcommand)]
enum SecretCmd {
    /// Set a secret's next version. Its value comes from stdin (asked for, unechoed, at a
    /// terminal; one trailing newline dropped) or `--from-file` (as it is): never from an
    /// argument, which shell history and `ps` keep.
    Set {
        app: String,
        /// An environment variable's name: `DB_PASSWORD`.
        name: String,
        #[arg(long)]
        env: String,
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Keep it without releasing it: a later `kiln deploy <app> --to <env> --secrets` (or any
        /// release) delivers it.
        #[arg(long)]
        no_release: bool,
        /// If the release delivering it fails, hold it rather than revert it: for a secret rotated
        /// after a leak, which a revert would deliver again.
        #[arg(long)]
        no_revert: bool,
    },
    /// Set each `NAME=value` of a dotenv file, released together.
    Import {
        app: String,
        file: PathBuf,
        #[arg(long)]
        env: String,
        #[arg(long)]
        no_release: bool,
        #[arg(long)]
        no_revert: bool,
    },
    /// An app's secrets in an environment: each one's version, size, who set it and when.
    List {
        app: String,
        #[arg(long)]
        env: String,
    },
    /// Delete a secret. The record of its versions stays.
    Rm {
        app: String,
        name: String,
        #[arg(long)]
        env: String,
        #[arg(long)]
        no_release: bool,
        #[arg(long)]
        no_revert: bool,
    },
    /// A secret's versions, newest first: who set each and when, and whether its value is kept.
    Versions {
        app: String,
        name: String,
        #[arg(long)]
        env: String,
    },
    /// Re-seal every app's secrets under a new data key (the old ones are retired).
    RotateKeys,
    /// Each read of an app's secrets for a delivery: who read them, for what, which versions.
    Uses {
        app: String,
        #[arg(long)]
        env: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

/// `kiln deploy fleet`'s arguments (see [`DeployCmd::Fleet`]).
#[derive(clap::Args)]
struct FleetArgs {
    #[arg(long, visible_alias = "to")]
    env: Option<String>,
    #[arg(long = "app")]
    apps: Vec<String>,
    #[arg(long)]
    template: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long, conflicts_with_all = ["set", "base"])]
    version: Option<String>,
    /// `path=value` in each app's values: `image.pullPolicy=Always`, `replicaCount=2`.
    #[arg(long = "set")]
    set: Vec<String>,
    /// kiln-base at this version in each app's chart, proposed in its source repository.
    #[arg(long, conflicts_with = "set")]
    base: Option<String>,
    /// `--set` in each app's chart in its source repository, proposed there, rather than
    /// released.
    #[arg(long)]
    in_source: bool,
    /// Where each app's chart is in its repository (`charts/{app}`), for a change there.
    #[arg(long)]
    chart: Option<String>,
    /// Its values file (`<chart>/values.yaml`), for a change there.
    #[arg(long)]
    values_file: Option<String>,
    /// Asked again with the same key, the same fleet change.
    #[arg(long)]
    key: Option<String>,
    #[arg(long, conflicts_with_all = ["env", "version", "set", "base"])]
    show: Option<String>,
    /// Merges a source change's proposals on kiln's own git server (`--repo`: one of them).
    #[arg(long, conflicts_with_all = ["env", "version", "set", "base", "show"])]
    merge: Option<String>,
    #[arg(long, requires = "merge")]
    repo: Option<String>,
    /// Withdraws a source change's open proposals.
    #[arg(long, conflicts_with_all = ["env", "version", "set", "base", "show", "merge"])]
    withdraw: Option<String>,
}

/// `kiln app create`: an app and how it is deployed.
#[derive(clap::Args)]
struct NewApp {
    app: String,
    /// The template it is made from: its package, namespace and checks, unless given here.
    #[arg(long)]
    template: Option<String>,
    #[arg(long, default_value = "default")]
    project: String,
    /// Owner teams: the owner first, then a co-owner and an ops owner.
    #[arg(long = "owner")]
    owners: Vec<String>,
    /// A Helm chart's repository (`oci://…` or a Helm repository's URL).
    #[arg(long, conflicts_with = "git_repo")]
    helm_repo: Option<String>,
    #[arg(long, requires = "helm_repo")]
    chart: Option<String>,
    /// The chart's version, when it isn't the release's (`${{ release.version }}`).
    #[arg(long, requires = "helm_repo")]
    chart_version: Option<String>,
    /// A git repository whose path deploys at the release's version (a ref).
    #[arg(long)]
    git_repo: Option<String>,
    /// A platform KilnCD deploys to (aws-lambda, aws-ecs, cloudflare-workers, vercel), with
    /// `--spec`: its definition.
    #[arg(long, conflicts_with_all = ["helm_repo", "git_repo"], requires = "spec")]
    platform: Option<String>,
    /// The platform's definition, a YAML or JSON file, as its key in .kiln/deploy.yaml holds it.
    #[arg(long, requires = "platform")]
    spec: Option<PathBuf>,
    /// Where the platform's own files are (a task definition, wrangler.jsonc), at each
    /// release's version; with `--path`.
    #[arg(long, requires = "platform")]
    files_repo: Option<String>,
    #[arg(long, default_value = "")]
    path: String,
    /// A value: `image.tag=${{ release.artifacts.image.tag }}`.
    #[arg(long = "set")]
    set: Vec<String>,
    #[arg(long)]
    namespace: Option<String>,
    /// Its source repository, one kiln has (`acme/web`); `--source ''` for none. Unset: the one
    /// its template makes from its starter, if it makes one.
    #[arg(long)]
    source: Option<String>,
}

#[derive(clap::Subcommand)]
enum EnvCmd {
    List,
    Show { env: String },
    /// Add (or replace) an environment, deployed from a release repository on kiln's own git
    /// server or on GitHub, through KilnCD or through an Argo CD integration (`--provider
    /// argocd/<name>`).
    Add(Box<EnvAdd>),
    Rm { env: String },
    /// Move an environment to another provider (docs/deploy-api.md §8.4): the new one compares in
    /// shadow and never writes; `--handover` hands what runs over once it agrees (`--force`: even
    /// if not), with nothing made again. Without `--to`, how its move stands.
    Move {
        env: String,
        /// `kilncd`, or `argocd/<integration>`.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        handover: bool,
        #[arg(long, requires = "handover")]
        force: bool,
        /// On Argo CD, kiln writes each Application itself rather than through an ApplicationSet.
        #[arg(long)]
        applications: bool,
        /// Cancel a move that hasn't begun handing over.
        #[arg(long, conflicts_with_all = ["to", "handover"])]
        cancel: bool,
        /// Return once it's asked for.
        #[arg(long)]
        no_wait: bool,
    },
}

/// `kiln deploy env add`'s arguments (boxed in [`EnvCmd`]: the variant is much the largest).
#[derive(clap::Args)]
struct EnvAdd {
    env: String,
    /// `kilncd`, or `argocd/<integration>`.
    #[arg(long, default_value = "kilncd")]
    provider: String,
    /// Where the release repository is: `kiln` (kiln's git server), `github` (through kiln's
    /// GitHub App), `gitlab` (a `[[ci.gitlab]]`) or `bitbucket` (`[ci.bitbucket]`); on another
    /// host, kiln makes it if it isn't there.
    #[arg(long, default_value = "kiln")]
    host: String,
    /// The release repository (`owner/name`; on GitLab `group/…/name`).
    #[arg(long, default_value = "kiln/releases")]
    repo: String,
    #[arg(long, default_value = "main")]
    branch: String,
    #[arg(long, default_value = "")]
    root: String,
    /// A cluster it deploys to (KilnCD's name); none: the cluster kiln runs in.
    #[arg(long = "cluster")]
    clusters: Vec<String>,
    /// On Argo CD, the KilnCD cluster that reaches one of Argo CD's, where kiln writes an app's
    /// secrets (docs/deploy-api.md §3.5): `<Argo CD's name>=<KilnCD's>`, as `in-cluster=prod-k`.
    #[arg(long = "kilncd", value_name = "ARGO=KILNCD")]
    kilncd: Vec<String>,
    /// Don't delete what git no longer holds.
    #[arg(long)]
    no_prune: bool,
    /// Undo hand edits.
    #[arg(long)]
    self_heal: bool,
    /// How long a release has to land, in seconds.
    #[arg(long, default_value_t = 900)]
    timeout: u64,
    /// How long a landed release must stay synced and healthy, in seconds.
    #[arg(long, default_value_t = 30)]
    settle: u64,
    /// How long the provider may refuse a release before it fails and its commit is
    /// reverted, in seconds.
    #[arg(long, default_value_t = 60)]
    refused: u64,
    /// A release asked for while another is in flight waits, rather than being refused.
    #[arg(long)]
    queue: bool,
    /// Agents may release here (docs/cd-targets.md D6); only a person sets it.
    #[arg(long)]
    agents: bool,
    /// An app's owner teams may release it here without environments.deploy on it
    /// (docs/deploy-api.md §15).
    #[arg(long)]
    owners_release: bool,
    /// The gates its releases pass (docs/deploy-api.md §9.1), a YAML or JSON file: after (an
    /// environment of a linked server as `staging@eu`), soak, approvals, windows, checks (built
    /// in by name, or `{name, http: {url}}`, `{name, workflow: {repository, workflow, ref}}`),
    /// require_provenance, freeze.
    #[arg(long)]
    gates: Option<PathBuf>,
    /// On Argo CD, kiln writes each Application itself rather than through an ApplicationSet
    /// (for an Argo CD that doesn't allow them).
    #[arg(long)]
    applications: bool,
}

struct Api {
    client: Client,
    url: String,
    token: String,
    json: bool,
}

impl Api {
    async fn new(cfg: &Config, c: &Conn) -> Result<Self> {
        let t = crate::access::resolve(cfg, c.url.as_deref(), c.token.as_deref()).await?;
        Ok(Self { client: Client::public()?, url: t.url, token: t.token, json: c.json })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}/api/deploy{path}", self.url)).bearer(&self.token).timeout(Duration::from_secs(60));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes(16 << 20).await?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        if !(200..300).contains(&status) {
            bail!("{}", v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }

    fn print(&self, v: &Value, human: impl FnOnce(&Value)) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        } else {
            human(v);
        }
    }
}

/// `kiln app secret`.
async fn secret(api: &Api, cmd: SecretCmd) -> Result<()> {
    let at = |app: &str, env: &str| format!("/apps/{}/environments/{}", enc(app), enc(env));
    let when = |v: &Value, k: &str| crate::ci::cli::when(v.get(k).and_then(Value::as_i64));
    match cmd {
        SecretCmd::Set { app, name, env, from_file, no_release, no_revert } => {
            let body = match from_file {
                Some(p) => match String::from_utf8(std::fs::read(&p).with_context(|| format!("read {}", p.display()))?) {
                    Ok(text) => json!({ "value": text }),
                    Err(not_text) => {
                        use base64::Engine as _;
                        json!({ "base64": base64::engine::general_purpose::STANDARD.encode(not_text.as_bytes()) })
                    }
                },
                None => json!({ "value": secret_from_stdin(&name)? }),
            };
            let v = api.call(Method::Put, &format!("{}/secrets/{}{}", at(&app, &env), enc(&name), held(no_release, no_revert)), Some(body)).await?;
            api.print(&v, |v| println!("{name} set for {app} in {env}: version {}; {}", v.get("version").and_then(Value::as_u64).unwrap_or(0), s(v, "next")));
        }
        SecretCmd::Import { app, file, env, no_release, no_revert } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            let pairs = crate::ci::cli::dotenv(&text);
            // Released once, with the last.
            let mut last = Value::Null;
            for (n, (name, value)) in pairs.iter().enumerate() {
                let hold = no_release || n + 1 < pairs.len();
                last = api.call(Method::Put, &format!("{}/secrets/{}{}", at(&app, &env), enc(name), held(hold, no_revert)), Some(json!({ "value": value }))).await.with_context(|| name.clone())?;
            }
            if !api.json {
                println!("{} secrets set for {app} in {env}; {}", pairs.len(), s(&last, "next"));
            }
        }
        SecretCmd::List { app, env } => {
            let v = api.call(Method::Get, &format!("{}/secrets", at(&app, &env)), None).await?;
            api.print(&v, |v| {
                let list = v.get("secrets").and_then(Value::as_array).cloned().unwrap_or_default();
                if list.is_empty() {
                    println!("{app} has no secrets in {env}: kiln app secret set {app} NAME --env {env}");
                }
                for m in &list {
                    let (version, size) = (m.get("version").and_then(Value::as_u64).unwrap_or(0), m.get("size").and_then(Value::as_u64).unwrap_or(0));
                    println!("{:<32} v{version:<4} {size:>6} B  set by {} at {}", s(m, "name"), s(m, "set_by"), when(m, "set_at"));
                }
            });
        }
        SecretCmd::Rm { app, name, env, no_release, no_revert } => {
            let v = api.call(Method::Delete, &format!("{}/secrets/{}{}", at(&app, &env), enc(&name), held(no_release, no_revert)), None).await?;
            api.print(&v, |v| println!("{name} deleted for {app} in {env}; {}", s(v, "next")));
        }
        SecretCmd::Versions { app, name, env } => {
            let v = api.call(Method::Get, &format!("{}/secrets/{}/versions", at(&app, &env), enc(&name)), None).await?;
            api.print(&v, |v| {
                for x in v.get("versions").and_then(Value::as_array).into_iter().flatten() {
                    let flag = |k: &str| x.get(k).and_then(Value::as_bool).unwrap_or(false);
                    let what = if flag("deleted") {
                        "deleted"
                    } else if flag("destroyed") {
                        "set, its value no longer kept"
                    } else {
                        "set"
                    };
                    println!("v{:<4} {what} by {} at {}", x.get("version").and_then(Value::as_u64).unwrap_or(0), s(x, "set_by"), when(x, "set_at"));
                }
            });
        }
        SecretCmd::RotateKeys => {
            let v = api.call(Method::Post, "/secret-keys/rotate", Some(json!({}))).await?;
            api.print(&v, |v| println!("re-sealed {} values under a new data key", v.get("resealed").and_then(Value::as_u64).unwrap_or(0)));
        }
        SecretCmd::Uses { app, env, limit } => {
            let v = api.call(Method::Get, &format!("{}/secret-uses?limit={limit}", at(&app, &env)), None).await?;
            api.print(&v, |v| {
                let reads = v.get("reads").and_then(Value::as_array).cloned().unwrap_or_default();
                if reads.is_empty() {
                    println!("nothing has read {app}'s secrets in {env}");
                }
                for r in &reads {
                    println!("{}  {} for {}: {}", when(r, "at"), s(r, "by"), s(r, "purpose"), s(r, "versions"));
                }
            });
        }
    }
    Ok(())
}

/// The query that keeps a secret's change from being released.
/// The query a secret's change goes with: `release=false` keeps it for a later release,
/// `revert=false` holds that release if it fails rather than delivering the old value again.
fn held(hold: bool, no_revert: bool) -> String {
    match (hold, no_revert) {
        (false, false) => String::new(),
        (true, false) => "?release=false".into(),
        (false, true) => "?revert=false".into(),
        (true, true) => "?release=false&revert=false".into(),
    }
}

/// A secret's value from stdin: asked for, unechoed, at a terminal; otherwise read to its end, one
/// trailing newline dropped (`echo` adds one).
fn secret_from_stdin(name: &str) -> Result<String> {
    use std::io::{IsTerminal as _, Read as _};
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password(format!("{name}: "))?);
    }
    let mut v = String::new();
    std::io::stdin().read_to_string(&mut v)?;
    if v.ends_with('\n') {
        v.pop();
        if v.ends_with('\r') {
            v.pop();
        }
    }
    Ok(v)
}

/// A duration as people say it: `45s`, `3m 20s`, `2h 5m`, `1d 3h`.
fn took(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m {}s", s / 60, s % 60),
        3_600..=86_399 => format!("{}h {}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3_600),
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn enc(s: &str) -> String {
    kiln_cd::kube::enc(s)
}

/// `name=repository:tag`, `name=repository@digest` or `name=repository:tag@digest`.
fn artifact(spec: &str) -> Result<(String, Value)> {
    let (name, image) = spec.split_once('=').with_context(|| format!("{spec:?}: name=repository:tag[@digest]"))?;
    let (image, digest) = image.split_once('@').map_or((image, ""), |(i, d)| (i, d));
    // A tag follows the last ':' after the last '/', so a registry's port isn't one.
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    let (repository, tag) = match image.get(slash..).and_then(|last| last.rfind(':')) {
        Some(i) => (image.get(..slash + i).unwrap_or(image), image.get(slash + i + 1..).unwrap_or("")),
        None => (image, ""),
    };
    Ok((name.to_owned(), json!({ "repository": repository, "tag": tag, "digest": digest })))
}

/// What a release does, in a few words: `deploy 1.4.2`, `secrets: DB_PASSWORD v4`.
fn what(r: &Value) -> String {
    let what = match s(r, "kind") {
        "deploy" => format!("deploy {}", s(r, "version")),
        "rollback" => format!("rollback to {}{}", s(r, "version"), r.get("rollback_of").and_then(Value::as_i64).map(|t| format!(" (release {t})")).unwrap_or_default()),
        "config" if r.get("values").is_none_or(Value::is_null) && !s(r, "secrets").is_empty() => format!("secrets: {}", s(r, "secrets")),
        "revert" => format!("revert of release {}", r.get("reverts").and_then(Value::as_i64).unwrap_or(0)),
        k => k.to_owned(),
    };
    // A release that took on secrets' changes while it waited says so.
    match s(r, "secrets") {
        x if x.is_empty() || what.starts_with("secrets:") => what,
        x => format!("{what} (and secrets: {x})"),
    }
}

fn line(r: &Value) -> String {
    let what = what(r);
    let commit = s(r, "commit").get(..12).unwrap_or("");
    let msg = s(r, "message");
    format!(
        "{:>6}  {:<10} {:<12} {}  {}{}{}",
        r.get("id").and_then(Value::as_i64).unwrap_or(0),
        s(r, "environment"),
        s(r, "state"),
        s(r, "app"),
        what,
        if commit.is_empty() { String::new() } else { format!("  {commit}") },
        if msg.is_empty() { String::new() } else { format!("  — {msg}") }
    )
}

/// Follows a release to its end, printing each state it takes; whether it succeeded.
async fn follow(api: &Api, mut r: Value) -> Result<bool> {
    let id = r.get("id").and_then(Value::as_i64).context("the server answered without a release")?;
    let mut last = String::new();
    loop {
        let state = s(&r, "state").to_owned();
        let shown = format!("{state} {}", s(&r, "message"));
        if shown != last && !api.json {
            println!("{}", line(&r));
            if state == "waiting_approval" {
                println!("        approve it: kiln deploy approve {id}");
            }
            last = shown;
        }
        match state.as_str() {
            "succeeded" => return Ok(true),
            "failed" | "cancelled" | "rolled_back" | "superseded" => return Ok(false),
            _ => {}
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
        r = api.call(Method::Get, &format!("/releases/{id}"), None).await?;
    }
}

/// Runs `kiln deploy`.
pub async fn deploy(args: DeployArgs, cfg: &Config) -> Result<()> {
    let api = Api::new(cfg, &args.conn).await?;
    let ask = match args.cmd {
        Some(DeployCmd::Status { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                let running = v.get("running").filter(|r| !r.is_null());
                println!("{app} in {env}: {}", running.map_or_else(|| "nothing released yet".to_owned(), |r| format!("release {} ({})", r.get("id").and_then(Value::as_i64).unwrap_or(0), what(r))));
                if let Some(why) = v.pointer("/registration/degraded").and_then(Value::as_str) {
                    println!("needs a person: {why}");
                }
                if let Some(f) = v.get("in_flight").filter(|r| !r.is_null()) {
                    println!("in flight: {}", line(f));
                }
                let p = v.pointer("/provider/status").cloned().unwrap_or(Value::Null);
                let provider = match v.pointer("/provider/kind").and_then(Value::as_str) {
                    Some("argocd") => format!("Argo CD {}", v.pointer("/provider/integration").and_then(Value::as_str).unwrap_or("")),
                    _ => "KilnCD".to_owned(),
                };
                let app = v.pointer("/provider/app").and_then(Value::as_str).unwrap_or("");
                match (p.get("error").and_then(Value::as_str), p.is_null()) {
                    (Some(e), _) => println!("{provider} ({app}): can't be asked: {e}"),
                    (None, true) => println!("{provider} ({app}): nothing there"),
                    (None, false) => println!("{provider} ({app}): {} / {}", s(&p, "sync"), s(&p, "health")),
                }
            });
            return Ok(());
        }
        Some(DeployCmd::History { app, env, version, limit }) => {
            let mut q = vec![format!("limit={limit}")];
            q.extend(app.map(|a| format!("app={}", enc(&a))));
            q.extend(env.map(|e| format!("environment={}", enc(&e))));
            q.extend(version.map(|v| format!("version={}", enc(&v))));
            let v = api.call(Method::Get, &format!("/releases?{}", q.join("&")), None).await?;
            api.print(&v, |v| {
                for r in v.get("releases").and_then(Value::as_array).into_iter().flatten() {
                    println!("{}", line(r));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Show { release }) => {
            let v = api.call(Method::Get, &format!("/releases/{release}"), None).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Env { cmd }) => return env(&api, cmd).await,
        Some(DeployCmd::Fleet(f)) => {
            let FleetArgs { env, apps, template, project, owner, version, set, base, in_source, chart, values_file, key, show, merge, repo, withdraw } = *f;
            let v = match (show, merge, withdraw) {
                (Some(id), _, _) => api.call(Method::Get, &format!("/fleets/{}", enc(&id)), None).await?,
                (_, Some(id), _) => api.call(Method::Post, &format!("/fleets/{}/merge", enc(&id)), Some(json!({ "repo": repo }))).await?,
                (_, _, Some(id)) => api.call(Method::Delete, &format!("/fleets/{}", enc(&id)), None).await?,
                _ => {
                    let mut sets = serde_json::Map::new();
                    for kv in &set {
                        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --set path=value"))?;
                        sets.insert(k.to_owned(), json!(v));
                    }
                    let body = if base.is_some() || in_source {
                        let source = json!({ "base": base, "set": if in_source { Value::Object(sets) } else { json!({}) }, "chart": chart, "values": values_file });
                        json!({ "environment": env.unwrap_or_default(), "apps": apps, "template": template, "project": project, "owner": owner, "source": source, "idempotency_key": key })
                    } else {
                        let env = env.context("--env: the environment the fleet change goes to (or --base, --in-source: a change in the apps' sources)")?;
                        json!({ "environment": env, "apps": apps, "template": template, "project": project, "owner": owner, "version": version, "set": sets, "idempotency_key": key })
                    };
                    api.call(Method::Post, "/fleets", Some(body)).await?
                }
            };
            api.print(&v, |v| {
                if v.get("source").and_then(Value::as_bool) == Some(true) {
                    println!("fleet change {} in apps' sources", s(v, "fleet"));
                    for p in v.get("proposals").and_then(Value::as_array).into_iter().flatten() {
                        let at = p.get("url").and_then(Value::as_str).map_or_else(|| format!("branch {}", s(p, "branch")), str::to_owned);
                        let apps: Vec<&str> = p.get("apps").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                        println!("{:<28} {:<10} {}  ({})", s(p, "repo"), s(p, "state"), at, apps.join(", "));
                        if let Some(note) = p.get("note").and_then(Value::as_str) {
                            println!("    {note}");
                        }
                        if let Some(diff) = p.get("diff").and_then(Value::as_str) {
                            println!("{diff}");
                        }
                    }
                    for k in v.get("skipped").and_then(Value::as_array).into_iter().flatten() {
                        println!("{}  not changed: {}", s(k, "app"), s(k, "error"));
                    }
                    if let Some(states) = v.get("states").and_then(Value::as_object) {
                        let parts: Vec<String> = states.iter().map(|(k, n)| format!("{n} {k}")).collect();
                        println!("{}{}", parts.join(", "), if v.get("done").and_then(Value::as_bool) == Some(true) { " · done" } else { "" });
                    }
                    return;
                }
                println!("fleet change {} in {}", s(v, "fleet"), s(v, "environment"));
                for r in v.get("releases").and_then(Value::as_array).into_iter().flatten() {
                    match r.get("release") {
                        Some(rel) => println!("{}  {}", s(rel, "app"), line(rel)),
                        None if r.get("error").is_some() => println!("{}  not released: {}", s(r, "app"), s(r, "error")),
                        None => println!("{}", line(r)),
                    }
                }
                if let Some(states) = v.get("states").and_then(Value::as_object) {
                    let parts: Vec<String> = states.iter().map(|(k, n)| format!("{n} {k}")).collect();
                    println!("{}{}", parts.join(", "), if v.get("done").and_then(Value::as_bool) == Some(true) { " · done" } else { "" });
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Metrics { app, env, days }) => {
            let mut q = vec![format!("days={days}")];
            q.extend(app.map(|a| format!("app={}", enc(&a))));
            q.extend(env.map(|e| format!("environment={}", enc(&e))));
            let v = api.call(Method::Get, &format!("/metrics?{}", q.join("&")), None).await?;
            api.print(&v, |v| {
                let dur = |p: &str| v.pointer(p).and_then(Value::as_i64).map_or_else(|| "-".to_owned(), took);
                let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
                println!("over {} days: {} changes, {} landed ({:.2} a day)", n("days"), n("changes"), n("landed"), v.get("per_day").and_then(Value::as_f64).unwrap_or(0.0));
                println!("lead time (asked to landed): p50 {}, p90 {}", dur("/lead_time_secs/p50"), dur("/lead_time_secs/p90"));
                match v.get("change_failure_rate").and_then(Value::as_f64) {
                    Some(r) => println!("change failure rate: {:.0}% ({} failed or rolled back)", r * 100.0, n("failed")),
                    None => println!("change failure rate: no changes yet"),
                }
                println!("time to restore: p50 {}, p90 {}", dur("/time_to_restore_secs/p50"), dur("/time_to_restore_secs/p90"));
            });
            return Ok(());
        }
        Some(DeployCmd::Versions { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/versions", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for x in v.get("versions").and_then(Value::as_array).into_iter().flatten() {
                    let runs = if x.get("runs_now").and_then(Value::as_bool) == Some(true) { "  ← runs now" } else { "" };
                    let n = |k: &str| x.get(k).and_then(Value::as_i64).unwrap_or(0);
                    println!("{:<20} {} release(s), last #{}{runs}", s(x, "version"), n("releases"), n("last_release"));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Resources { app, env }) => {
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/resources", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for r in v.get("resources").and_then(Value::as_array).into_iter().flatten() {
                    println!("{:<12} {:<10} {:<28} {:<40} {}", s(r, "sync"), s(r, "health"), s(r, "kind"), format!("{}/{}", s(r, "namespace"), s(r, "name")), s(r, "message"));
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Manifests { app, env, version }) => {
            let q = version.map(|v| format!("?version={}", enc(&v))).unwrap_or_default();
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/manifests{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for m in v.get("manifests").and_then(Value::as_array).into_iter().flatten() {
                    println!("---\n{}", s(m, "yaml").trim_end());
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Diff { app, env, version }) => {
            let q = version.map(|v| format!("?version={}", enc(&v))).unwrap_or_default();
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/diff{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                // `changes` from a server before both providers answered `diffs`.
                let all = v.get("diffs").or_else(|| v.get("changes")).and_then(Value::as_array).cloned().unwrap_or_default();
                let flag = |c: &Value, f: &str| c.get(f).and_then(Value::as_bool).unwrap_or(false);
                let changes: Vec<Value> = all.into_iter().filter(|c| c.get("hunks").and_then(Value::as_array).is_some_and(|h| !h.is_empty()) || flag(c, "missing") || flag(c, "requires_pruning")).collect();
                if changes.is_empty() {
                    println!("nothing would change");
                }
                for c in changes {
                    let k = |f: &str| c.pointer(&format!("/key/{f}")).and_then(Value::as_str).unwrap_or("").to_owned();
                    let note = if flag(&c, "missing") { " (not there yet: a sync makes it)" } else if flag(&c, "requires_pruning") { " (no longer in git: a sync that prunes deletes it)" } else { "" };
                    println!("--- {} {}/{}{note}", k("kind"), k("namespace"), k("name"));
                    for h in c.get("hunks").and_then(Value::as_array).into_iter().flatten() {
                        let n = |f: &str| h.get(f).and_then(Value::as_u64).unwrap_or(0);
                        println!("@@ -{},{} +{},{} @@", n("old_start"), n("old_lines"), n("new_start"), n("new_lines"));
                        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                            println!("{l}");
                        }
                    }
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Logs { app, env, kind, name, container, tail }) => {
            let name = name.unwrap_or_else(|| app.clone());
            let mut q = format!("?kind={}&name={}&tail={tail}", enc(&kind), enc(&name));
            if let Some(c) = container {
                q.push_str(&format!("&container={}", enc(&c)));
            }
            let v = api.call(Method::Get, &format!("/apps/{}/environments/{}/logs{q}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |v| {
                for l in v.get("lines").and_then(Value::as_array).into_iter().flatten() {
                    println!("{} {}", s(l, "pod"), s(l, "text").trim_end());
                }
            });
            return Ok(());
        }
        Some(DeployCmd::Action { app, env, object, action, namespace, group, set }) => {
            let (kind, name) = object.split_once('/').ok_or_else(|| anyhow::anyhow!("{object}: an object is Kind/name, as Deployment/web"))?;
            let path = format!("/apps/{}/environments/{}/actions", enc(&app), enc(&env));
            match action {
                Some(action) => {
                    let params: BTreeMap<String, String> = set.iter().map(|p| p.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).ok_or_else(|| anyhow::anyhow!("{p}: name=value"))).collect::<Result<_>>()?;
                    let v = api.call(Method::Post, &path, Some(json!({ "kind": kind, "name": name, "namespace": namespace, "group": group, "action": action, "params": params }))).await?;
                    api.print(&v, |v| println!("{}", s(v, "done")));
                }
                None => {
                    let mut q = format!("?kind={}&name={}", enc(kind), enc(name));
                    if let Some(n) = &namespace {
                        q.push_str(&format!("&namespace={}", enc(n)));
                    }
                    if let Some(g) = &group {
                        q.push_str(&format!("&group={}", enc(g)));
                    }
                    let v = api.call(Method::Get, &format!("{path}{q}"), None).await?;
                    api.print(&v, |v| {
                        for a in v.get("actions").and_then(Value::as_array).into_iter().flatten() {
                            let params: Vec<&str> = a.get("params").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("name").and_then(Value::as_str)).collect();
                            let off = if a.get("disabled").and_then(Value::as_bool) == Some(true) { "  (not now)" } else { "" };
                            println!("{}{}{off}", s(a, "name"), if params.is_empty() { String::new() } else { format!(" --set {}=…", params.join("=… --set ")) });
                        }
                    });
                }
            }
            return Ok(());
        }
        Some(DeployCmd::Approve { release, note }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/approve"), Some(json!({ "note": note }))).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Reject { release, note }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/reject"), Some(json!({ "note": note }))).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Cancel { release }) => {
            let v = api.call(Method::Post, &format!("/releases/{release}/cancel"), None).await?;
            api.print(&v, |v| println!("{}", line(v)));
            return Ok(());
        }
        Some(DeployCmd::Rollback { app, env, to, no_wait }) => (json!({ "app": app, "environment": env, "rollback": { "to": to } }), no_wait),
        Some(DeployCmd::Enable { app, env }) => (json!({ "app": app, "environment": env, "enabled": true }), false),
        Some(DeployCmd::Disable { app, env, keep }) => (json!({ "app": app, "environment": env, "enabled": false, "keep": keep }), false),
        None => {
            let app = args.app.context("which app? kiln deploy <app> <version> --to <env>")?;
            let env = args.to.context("which environment? --to <env>")?;
            if let Some(from) = args.from {
                if args.version.is_some() || args.values.is_some() {
                    bail!("--from promotes what another environment runs: no version or --values with it");
                }
                let ask = json!({ "app": app, "environment": env, "promote_from": from, "no_revert": args.no_revert });
                let r = api.call(Method::Post, "/releases", Some(ask)).await?;
                if args.no_wait {
                    api.print(&r, |r| println!("{}", line(r)));
                    return Ok(());
                }
                if !follow(&api, r).await? {
                    bail!("the release didn't succeed");
                }
                return Ok(());
            }
            if args.secrets {
                let ask = json!({ "app": app, "environment": env, "secrets": "", "no_revert": args.no_revert });
                let r = api.call(Method::Post, "/releases", Some(ask)).await?;
                if args.no_wait {
                    api.print(&r, |r| println!("{}", line(r)));
                    return Ok(());
                }
                if !follow(&api, r).await? {
                    bail!("the release didn't succeed");
                }
                return Ok(());
            }
            let ask = match (args.version, args.values) {
                (Some(v), None) => {
                    let mut arts = serde_json::Map::new();
                    for a in &args.artifacts {
                        let (name, spec) = artifact(a)?;
                        arts.insert(name, spec);
                    }
                    json!({ "app": app, "environment": env, "version": v, "artifacts": arts, "no_revert": args.no_revert })
                }
                (None, Some(path)) => {
                    let values = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
                    json!({ "app": app, "environment": env, "values": values, "no_revert": args.no_revert })
                }
                (Some(_), Some(_)) => bail!("a version or --values, not both: deploy the version first, then change its values"),
                (None, None) => bail!("what to release? a version (kiln deploy <app> <version> --to <env>), --values <file>, or --secrets"),
            };
            (ask, args.no_wait)
        }
    };
    let (ask, no_wait) = ask;
    let r = api.call(Method::Post, "/releases", Some(ask)).await?;
    if no_wait {
        api.print(&r, |r| println!("{}", line(r)));
        return Ok(());
    }
    if !follow(&api, r).await? {
        bail!("the release didn't succeed");
    }
    Ok(())
}

/// Runs `kiln app`.
pub async fn app(args: AppArgs, cfg: &Config) -> Result<()> {
    // Writes files where it's run: no server is asked. kiln-base comes from the registry of the
    // kiln named, or is vendored when none is.
    if let AppCmd::Init { app, dir, image, port, environments, force, base, registry, vendor } = &args.cmd {
        let host = match (vendor, registry, &args.conn.url) {
            (true, _, _) => None,
            (false, Some(r), _) => Some(r.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_owned()),
            (false, None, Some(url)) => Some(crate::githost::host_of(url)),
            (false, None, None) => None,
        };
        return init(app, dir, image.as_deref(), *port, environments, *force, *base, host.as_deref());
    }
    let api = Api::new(cfg, &args.conn).await?;
    match args.cmd {
        // Done above, before any server.
        AppCmd::Init { .. } => {}
        AppCmd::List => {
            let v = api.call(Method::Get, "/apps", None).await?;
            api.print(&v, |v| {
                for a in v.get("apps").and_then(Value::as_array).into_iter().flatten() {
                    let owners: Vec<&str> = a.get("owners").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    println!("{:<24} {:<12} {}", s(a, "name"), s(a, "project"), owners.join(", "));
                }
            });
        }
        AppCmd::Show { app } => {
            let v = api.call(Method::Get, &format!("/apps/{}", enc(&app)), None).await?;
            api.print(&v, |v| {
                println!("{app} ({})", v.pointer("/app/project").and_then(Value::as_str).unwrap_or(""));
                if let Some(r) = v.pointer("/app/source").and_then(Value::as_str) {
                    println!("  source       {r}");
                }
                for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
                    let env = e.pointer("/registration/environment").and_then(Value::as_str).unwrap_or("");
                    let running = e.get("running").filter(|r| !r.is_null()).map_or_else(|| "nothing released".to_owned(), |r| format!("release {} ({})", r.get("id").and_then(Value::as_i64).unwrap_or(0), what(r)));
                    println!("  {env:<12} {running}");
                }
            });
        }
        AppCmd::Create(new) => {
            let NewApp { app, template, project, owners, helm_repo, chart, chart_version, git_repo, platform, spec, files_repo, path, set, namespace, source } = *new;
            let mut sets = BTreeMap::new();
            for kv in &set {
                let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --set name=value"))?;
                sets.insert(k.to_owned(), v.to_owned());
            }
            let package = match (helm_repo, git_repo, platform) {
                (Some(repo), None, None) => json!({ "kind": "helm", "repo": repo, "chart": chart.context("--chart: the chart's name")?, "chart_version": chart_version, "set": sets }),
                (None, Some(repo), None) => json!({ "kind": "git", "repo": repo, "path": path, "set": sets }),
                (None, None, Some(provider)) => {
                    let file = spec.context("--spec: the platform's definition")?;
                    let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
                    let spec: Value = serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", file.display()))?;
                    json!({ "kind": "platform", "provider": provider, "spec": spec, "repo": files_repo, "path": path })
                }
                (None, None, None) if template.is_some() => Value::Null,
                _ => bail!("how is it deployed? --helm-repo and --chart, --git-repo (and --path), --platform and --spec, or a --template that says"),
            };
            let body = json!({ "name": app, "project": project, "owners": owners, "package": package, "namespace": namespace, "template": template, "source": source });
            let v = api.call(Method::Post, "/apps", Some(body)).await?;
            api.print(&v, |v| {
                let from = v.get("source").and_then(Value::as_str).map(|r| format!(", its source in {r}")).unwrap_or_default();
                println!("app {}{from} ready: register it in an environment (kiln app register {} --env <env>)", s(v, "name"), s(v, "name"));
            });
        }
        AppCmd::Register { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} registered in {env}: deploy it (kiln deploy {app} <version> --to {env})"));
        }
        AppCmd::Deregister { app, env, keep } => {
            api.call(Method::Delete, &format!("/apps/{}/environments/{}{}", enc(&app), enc(&env), if keep { "?keep=true" } else { "" }), None).await?;
            if !api.json {
                println!("{app} deregistered from {env}{}", if keep { "; what it ran is kept" } else { "" });
            }
        }
        AppCmd::Pause { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}/pause", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} in {env}: automation paused until the next release (kiln app resume {app} --env {env})"));
        }
        AppCmd::Resume { app, env } => {
            let v = api.call(Method::Post, &format!("/apps/{}/environments/{}/resume", enc(&app), enc(&env)), None).await?;
            api.print(&v, |_| println!("{app} in {env}: automation resumed"));
        }
        AppCmd::Delete { app, everywhere, keep } => {
            api.call(Method::Delete, &format!("/apps/{}?everywhere={everywhere}&keep={keep}", enc(&app)), None).await?;
            if !api.json {
                println!("{app} deleted");
            }
        }
        AppCmd::Set { app, owners, properties, oncall, channels, links, template, source } => {
            let mut patch = serde_json::Map::new();
            if !owners.is_empty() {
                patch.insert("owners".into(), json!(owners));
            }
            if !properties.is_empty() {
                let mut props = serde_json::Map::new();
                for kv in &properties {
                    let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --property name=value"))?;
                    props.insert(k.to_owned(), if v.is_empty() { Value::Null } else { json!(v) });
                }
                patch.insert("properties".into(), Value::Object(props));
            }
            if oncall.is_some() || !channels.is_empty() || !links.is_empty() {
                let now = api.call(Method::Get, &format!("/apps/{}", enc(&app)), None).await?;
                let mut contacts = now.pointer("/app/contacts").cloned().unwrap_or_else(|| json!({}));
                if let Some(m) = contacts.as_object_mut() {
                    if let Some(o) = oncall {
                        m.insert("oncall".into(), json!(o));
                    }
                    if !channels.is_empty() {
                        m.insert("channels".into(), json!(channels));
                    }
                    for kv in &links {
                        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: --link name=url"))?;
                        let entry = m.entry("links").or_insert_with(|| json!({}));
                        if let Some(l) = entry.as_object_mut() {
                            l.insert(k.to_owned(), json!(v));
                        }
                    }
                }
                patch.insert("contacts".into(), contacts);
            }
            if let Some(t) = template {
                patch.insert("template".into(), json!(t));
            }
            if let Some(r) = source {
                patch.insert("source".into(), json!(r));
            }
            let v = api.call(Method::Patch, &format!("/apps/{}", enc(&app)), Some(Value::Object(patch))).await?;
            api.print(&v, |v| println!("{} updated", s(v, "name")));
        }
        AppCmd::Catalog { app } => {
            let v = api.call(Method::Get, &format!("/apps/{}/catalog-info.yaml", enc(&app)), None).await?;
            print!("{}", v.as_str().unwrap_or_default());
        }
        AppCmd::Secret { cmd } => secret(&api, cmd).await?,
        AppCmd::Templates => {
            let v = api.call(Method::Get, "/templates", None).await?;
            api.print(&v, |v| {
                for t in v.get("templates").and_then(Value::as_array).into_iter().flatten() {
                    println!("{:<20} {}", s(t, "name"), s(t, "description"));
                }
            });
        }
        AppCmd::TemplateAdd { file } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            let t: Value = serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", file.display()))?;
            let name = s(&t, "name").to_owned();
            let v = api.call(Method::Put, &format!("/templates/{}", enc(&name)), Some(t)).await?;
            api.print(&v, |v| println!("template {} ready", s(v, "name")));
        }
        AppCmd::TemplateRm { name } => {
            api.call(Method::Delete, &format!("/templates/{}", enc(&name)), None).await?;
            if !api.json {
                println!("template {name} removed");
            }
        }
    }
    Ok(())
}

/// Runs `kiln deploy env`.
async fn env(api: &Api, cmd: EnvCmd) -> Result<()> {
    match cmd {
        EnvCmd::List => {
            let v = api.call(Method::Get, "/environments", None).await?;
            api.print(&v, |v| {
                for e in v.get("environments").and_then(Value::as_array).into_iter().flatten() {
                    let clusters: Vec<&str> = e.pointer("/targets/kubernetes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    println!(
                        "{:<16} {:<18} {}@{}  {}",
                        s(e, "name"),
                        s(e, "provider"),
                        e.pointer("/release_repo/repo").and_then(Value::as_str).unwrap_or(""),
                        e.pointer("/release_repo/branch").and_then(Value::as_str).unwrap_or(""),
                        if clusters.is_empty() { "in-cluster".to_owned() } else { clusters.join(", ") }
                    );
                }
            });
        }
        EnvCmd::Show { env } => {
            let v = api.call(Method::Get, &format!("/environments/{}", enc(&env)), None).await?;
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        EnvCmd::Add(a) => {
            let EnvAdd { env, provider, host, repo, branch, root, clusters, kilncd, no_prune, self_heal, timeout, settle, refused, queue, agents, owners_release, gates, applications } = *a;
            let mut reaches = serde_json::Map::new();
            for pair in &kilncd {
                let (argo, ours) = pair.split_once('=').filter(|(a, k)| !a.is_empty() && !k.is_empty()).with_context(|| format!("--kilncd {pair}: <Argo CD's cluster>=<KilnCD's cluster>"))?;
                reaches.insert(argo.to_owned(), Value::String(ours.to_owned()));
            }
            let gates: Value = match gates {
                Some(path) => {
                    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
                    serde_yaml::from_str(&text).with_context(|| format!("{} doesn't read as YAML", path.display()))?
                }
                None => Value::Null,
            };
            let body = json!({
                "name": env,
                "provider": provider,
                "release_repo": { "host": host, "repo": repo, "branch": branch, "root": root },
                "targets": { "kubernetes": clusters, "kilncd": reaches },
                "sync": { "prune": !no_prune, "self_heal": self_heal },
                "watch": { "timeout_secs": timeout, "settle_secs": settle, "refused_secs": refused },
                "concurrency": if queue { "queue" } else { "reject" },
                "agents": { "deploy": agents },
                "owners": { "release": owners_release },
                "gates": gates,
                "argocd": { "applications": applications },
            });
            let v = api.call(Method::Post, "/environments", Some(body)).await?;
            api.print(&v, |v| println!("environment {} ready", s(v, "name")));
        }
        EnvCmd::Rm { env } => {
            api.call(Method::Delete, &format!("/environments/{}", enc(&env)), None).await?;
            if !api.json {
                println!("environment {env} removed");
            }
        }
        EnvCmd::Move { env, to, handover, force, applications, cancel, no_wait } => {
            let path = format!("/environments/{}/move", enc(&env));
            let mut v = match (cancel, to) {
                (true, _) => api.call(Method::Delete, &path, None).await?,
                (false, Some(to)) => api.call(Method::Post, &path, Some(json!({ "to": to, "handover": handover, "force": force, "argocd": { "applications": applications } }))).await?,
                (false, None) => api.call(Method::Get, &path, None).await?,
            };
            let mut said = String::new();
            while !no_wait && matches!(s(&v, "phase"), "comparing" | "handing_over") {
                let now = format!("{}: {}", s(&v, "phase").replace('_', " "), s(&v, "message"));
                if now != said && !api.json {
                    eprintln!("{env} → {}: {}", s(&v, "to"), s(&v, "phase").replace('_', " "));
                    said = now;
                }
                crate::rt::sleep(Duration::from_secs(3)).await;
                v = api.call(Method::Get, &path, None).await?;
            }
            api.print(&v, print_move);
        }
    }
    Ok(())
}

/// How a move stands, a registration a line.
fn print_move(v: &Value) {
    println!("{} → {}: {}", s(v, "environment"), s(v, "to"), s(v, "phase").replace('_', " "));
    for r in v.get("registrations").and_then(Value::as_array).into_iter().flatten() {
        let agrees = match r.get("agrees").and_then(Value::as_bool) {
            Some(true) => "agrees".to_owned(),
            Some(false) => format!("differs on {}", r.get("differences").and_then(Value::as_array).map_or(0, Vec::len)),
            None => "not compared".to_owned(),
        };
        let remade = match r.get("remade").and_then(Value::as_array) {
            Some(m) if m.is_empty() => format!(", nothing made again ({} objects)", r.get("uids").and_then(Value::as_object).map_or(0, serde_json::Map::len)),
            Some(m) => format!(", made again: {}", m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            None => String::new(),
        };
        println!("  {:<24} {agrees}{remade}", s(r, "app"));
        for d in r.get("differences").and_then(Value::as_array).into_iter().flatten() {
            let k = |f: &str| d.pointer(&format!("/key/{f}")).and_then(Value::as_str).unwrap_or("").to_owned();
            println!("    {} {}/{}", k("kind"), k("namespace"), k("name"));
            for h in d.get("hunks").and_then(Value::as_array).into_iter().flatten() {
                for l in h.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    println!("      {l}");
                }
            }
        }
    }
    if !s(v, "message").is_empty() {
        println!("{}", s(v, "message"));
    }
}

/// `kiln app init`: an app's chart on kiln's base chart, written into the repository at `dir`.
#[allow(clippy::too_many_arguments)]
fn init(app: &str, dir: &std::path::Path, image: Option<&str>, port: u16, environments: &[String], force: bool, base: bool, registry: Option<&str>) -> Result<()> {
    let label = !app.is_empty() && app.len() <= 63 && app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !app.starts_with('-') && !app.ends_with('-');
    if !label {
        bail!("{app:?}: an app's name is lowercase letters, digits and '-', as Kubernetes names things");
    }
    let image = image.map_or_else(|| format!("registry.example.com/{app}"), str::to_owned);
    let chart = std::path::Path::new("charts").join(app);
    let (wrote, kept) = if base {
        // Only kiln-base moves: its dependency in Chart.yaml, and (vendored) its files.
        let path = dir.join(&chart).join("Chart.yaml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let repository = registry.map(super::init::charts_repository);
        let moved = super::init::rebase(&text, repository.as_deref()).with_context(|| format!("{} doesn't depend on kiln-base", path.display()))?;
        let mut files = vec![(chart.join("Chart.yaml"), moved)];
        let vendored = dir.join(&chart).join("charts/kiln-base");
        match registry {
            None => files.extend(super::init::vendored(app)),
            // From the registry now: the copy vendored before goes.
            Some(_) if vendored.is_dir() => {
                std::fs::remove_dir_all(&vendored).with_context(|| format!("remove {}", vendored.display()))?;
                println!("removed {} (kiln-base comes from kiln's registry now)", vendored.display());
            }
            Some(_) => {}
        }
        super::init::write(dir, &files, true).with_context(|| format!("write into {}", dir.display()))?
    } else {
        let files = super::init::files(&super::init::Init { app, image: &image, port, environments, registry });
        super::init::write(dir, &files, force).with_context(|| format!("write into {}", dir.display()))?
    };
    for p in &wrote {
        println!("wrote {}", p.display());
    }
    for p in &kept {
        println!("kept {} (it was there: --force writes over it)", p.display());
    }
    match registry {
        Some(host) if !base => println!(
            "\nkiln-base comes from {} (anyone may pull it): `helm dependency build charts/{app}` fetches it{}.",
            super::init::charts_repository(host),
            if host.starts_with("127.") || host.starts_with("localhost") { " (`--plain-http` for a kiln without TLS)" } else { "" }
        ),
        None if !base => println!("\nkiln-base is vendored in charts/{app}/charts/kiln-base (no kiln named: --url, KILN_URL or --registry depends on it in kiln's registry instead)."),
        _ => {}
    }
    if !base {
        println!(
            "\nNext: commit it, then\n  kiln app create {app} --git-repo <this repository's URL> --path charts/{app} --set 'image.tag=${{{{ release.version }}}}'\n  kiln app register {app} --env {env}\n  kiln deploy {app} <a tag or commit your CI built> --to {env}",
            env = environments.first().map_or("staging", String::as_str)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_read_with_ports_tags_and_digests() -> Result<()> {
        let (name, a) = artifact("image=ghcr.io/acme/web:1.4.2")?;
        assert_eq!((name.as_str(), s(&a, "repository"), s(&a, "tag")), ("image", "ghcr.io/acme/web", "1.4.2"));
        let (_, a) = artifact("image=registry:5000/web@sha256:abc")?;
        assert_eq!((s(&a, "repository"), s(&a, "tag"), s(&a, "digest")), ("registry:5000/web", "", "sha256:abc"));
        let (_, a) = artifact("image=registry:5000/web:2.0@sha256:abc")?;
        assert_eq!((s(&a, "repository"), s(&a, "tag")), ("registry:5000/web", "2.0"));
        assert!(artifact("no-equals").is_err());
        Ok(())
    }
}
