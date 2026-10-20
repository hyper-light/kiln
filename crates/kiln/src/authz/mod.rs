//! Who calls kiln's API and what each may do (docs/ci.md W10-R01 to R05).
//!
//! - **Principals.** The owner (the admin token, the password, one-time links, MCP tokens:
//!   everything, as before); users, signed in through a sign-in link of their own; personal access
//!   tokens `kiln_pat_<id>_<secret>` and agent tokens `kiln_agt_<id>_<secret>`, each for a user (or
//!   the owner) and carrying a grant of its own, so it does at most what both allow; and job tokens
//!   (`kiln_job_…`), which reach the API for what their job's `permissions` give.
//! - **Capabilities in scopes.** A grant gives a role (a named set of capabilities) or one
//!   capability, or denies it, in a scope: the server, an organization, a repository, a workflow,
//!   an environment, a runner or a pool. A grant reaches what its scope contains (an
//!   organization's repositories, a repository's workflows and environments). A deny wins.
//! - **One route table** ([`ROUTES`]): each API route's capability and where its scope comes from.
//!   Authorization reads it, and so does `/api/openapi.json`. A route it doesn't name needs
//!   `settings.manage` on the server: nothing becomes reachable by being forgotten.

pub mod api;
pub mod cli;
pub mod gate;
pub mod sso;
pub mod tokens;

use std::collections::HashMap;

use http::Method;
use serde::{Deserialize, Serialize};

pub use gate::{Caller, Rights};

/// W10-R02's capabilities, and `binaries.publish` (access.md) for release pipelines.
pub const CAPABILITIES: &[&str] = &[
    "runs.read",
    "runs.trigger",
    "runs.cancel",
    "runs.rerun",
    "runs.debug",
    "runs.approve",
    "logs.read",
    "artifacts.read",
    "artifacts.write",
    "caches.write",
    "workflows.edit",
    "secrets.manage",
    "variables.manage",
    "integrations.manage",
    "runners.read",
    "runners.manage",
    "runners.terminal",
    "pools.manage",
    "images.build",
    "routing.manage",
    "environments.deploy",
    "environments.manage",
    "diffs.read",
    "git.read",
    "diffs.submit",
    "diffs.review",
    "diffs.land",
    "queue.manage",
    "agents.run",
    "agents.attach",
    "experiments.run",
    "settings.manage",
    "tokens.manage",
    "users.manage",
    "audit.read",
    "binaries.publish",
];

/// What a route needs when any signed-in principal may call it (who you are, the schemas).
pub const SIGNED_IN: &str = "signed-in";

/// The built-in roles (W10-R02, access.md): `viewer` reads; `developer` also triggers, reruns,
/// submits and reviews; `maintainer` also lands, attaches agents, debugs, approves, deploys and
/// manages the queue, secrets and variables; `admin` and `owner` have every capability.
pub fn role(name: &str) -> Option<Vec<&'static str>> {
    const VIEWER: &[&str] = &["runs.read", "logs.read", "artifacts.read", "runners.read", "diffs.read"];
    const DEVELOPER: &[&str] = &["runs.trigger", "runs.rerun", "runs.cancel", "artifacts.write", "caches.write", "workflows.edit", "diffs.submit", "diffs.review", "agents.run"];
    const MAINTAINER: &[&str] = &[
        "runs.debug",
        "runs.approve",
        "diffs.land",
        "queue.manage",
        "agents.attach",
        "experiments.run",
        "environments.deploy",
        "variables.manage",
        "secrets.manage",
        "images.build",
    ];
    Some(match name {
        "viewer" => VIEWER.to_vec(),
        "developer" => VIEWER.iter().chain(DEVELOPER).copied().collect(),
        "maintainer" => VIEWER.iter().chain(DEVELOPER).chain(MAINTAINER).copied().collect(),
        "admin" | "owner" => CAPABILITIES.to_vec(),
        _ => return None,
    })
}

/// The built-in roles' names, least first.
pub const ROLES: &[&str] = &["viewer", "developer", "maintainer", "admin", "owner"];

/// What a grant is for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Scope {
    Server,
    Org { org: String },
    Repo { repo: String },
    Workflow { repo: String, path: String },
    Environment { repo: String, environment: String },
    Runner { runner: String },
    Pool { pool: String },
}

impl Scope {
    /// `server`, `org:<o>`, `repo:<o>/<r>`, `workflow:<o>/<r>:<path>`, `environment:<o>/<r>:<env>`
    /// (or `env:`, as secrets write it), `runner:<name>`, `pool:<name>`.
    pub fn parse(s: &str) -> Result<Scope, String> {
        let s = s.trim();
        let bad = || {
            format!("`{s}` is not a scope: server, org:<org>, repo:<owner>/<repo>, workflow:<owner>/<repo>:<path>, environment:<owner>/<repo>:<env>, runner:<name> or pool:<name>")
        };
        if s == "server" {
            return Ok(Scope::Server);
        }
        let (kind, rest) = s.split_once(':').ok_or_else(bad)?;
        let need = |v: &str| if v.is_empty() { Err(bad()) } else { Ok(v.to_owned()) };
        Ok(match kind {
            "org" => Scope::Org { org: need(rest)? },
            "repo" => Scope::Repo { repo: need(rest)? },
            "workflow" => {
                let (repo, path) = rest.split_once(':').ok_or_else(bad)?;
                Scope::Workflow { repo: need(repo)?, path: need(path)? }
            }
            "environment" | "env" => {
                let (repo, env) = rest.split_once(':').ok_or_else(bad)?;
                Scope::Environment { repo: need(repo)?, environment: need(env)? }
            }
            "runner" => Scope::Runner { runner: need(rest)? },
            "pool" => Scope::Pool { pool: need(rest)? },
            _ => return Err(bad()),
        })
    }

    pub fn text(&self) -> String {
        match self {
            Scope::Server => "server".into(),
            Scope::Org { org } => format!("org:{org}"),
            Scope::Repo { repo } => format!("repo:{repo}"),
            Scope::Workflow { repo, path } => format!("workflow:{repo}:{path}"),
            Scope::Environment { repo, environment } => format!("environment:{repo}:{environment}"),
            Scope::Runner { runner } => format!("runner:{runner}"),
            Scope::Pool { pool } => format!("pool:{pool}"),
        }
    }

    /// The repository a scope is inside, if any.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Scope::Repo { repo } | Scope::Workflow { repo, .. } | Scope::Environment { repo, .. } => Some(repo),
            _ => None,
        }
    }

    /// The same scope with its repository named `name` (an id resolved to `owner/repo`).
    pub fn with_repo(self, name: String) -> Scope {
        match self {
            Scope::Repo { .. } => Scope::Repo { repo: name },
            Scope::Workflow { path, .. } => Scope::Workflow { repo: name, path },
            Scope::Environment { environment, .. } => Scope::Environment { repo: name, environment },
            other => other,
        }
    }

    /// Whether a grant in `self` reaches `target`.
    pub fn covers(&self, target: &Scope) -> bool {
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        match self {
            Scope::Server => true,
            Scope::Org { org } => match target {
                Scope::Org { org: o } => same(o, org),
                t => t.repo().and_then(|r| r.split_once('/')).is_some_and(|(o, _)| same(o, org)),
            },
            Scope::Repo { repo } => target.repo().is_some_and(|r| same(r, repo)),
            Scope::Workflow { repo, path } => matches!(target, Scope::Workflow { repo: r, path: p } if same(r, repo) && p == path),
            Scope::Environment { repo, environment } => matches!(target, Scope::Environment { repo: r, environment: e } if same(r, repo) && e == environment),
            Scope::Runner { .. } | Scope::Pool { .. } => self == target,
        }
    }
}

/// One grant: a role or a capability, in a scope; or a deny of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// A role (`developer`) or a capability (`runs.read`).
    pub what: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deny: bool,
}

impl Grant {
    /// `<what>` or `<what>@<scope>`, `!` first for a deny: `developer@org:acme`, `!runs.cancel@repo:acme/app`.
    pub fn parse(s: &str) -> Result<Grant, String> {
        let s = s.trim();
        let (deny, s) = s.strip_prefix('!').map_or((false, s), |rest| (true, rest));
        let (what, scope) = match s.split_once('@') {
            Some((w, sc)) => (w, Scope::parse(sc)?),
            None => (s, Scope::Server),
        };
        if what.is_empty() {
            return Err(format!("`{s}` grants nothing: say a role or a capability, then @scope"));
        }
        Ok(Grant { what: what.to_owned(), scope, deny })
    }

    pub fn text(&self) -> String {
        format!("{}{}@{}", if self.deny { "!" } else { "" }, self.what, self.scope.text())
    }

    pub fn gives(&self, capability: &str, custom: &HashMap<String, Vec<String>>) -> bool {
        self.what == capability || role(&self.what).is_some_and(|caps| caps.contains(&capability)) || custom.get(&self.what).is_some_and(|caps| caps.iter().any(|c| c == capability))
    }
}

/// Whether `what` names a capability or a role (built-in, or one of `custom`).
pub fn known(what: &str, custom: &HashMap<String, Vec<String>>) -> bool {
    CAPABILITIES.contains(&what) || role(what).is_some() || custom.contains_key(what)
}

/// Whether `grants` give `capability` in `target`: some grant reaching it gives it, and no deny
/// reaching it takes it away.
pub fn allows(grants: &[Grant], custom: &HashMap<String, Vec<String>>, capability: &str, target: &Scope) -> bool {
    let reaching = || grants.iter().filter(|g| g.scope.covers(target) && g.gives(capability, custom));
    !reaching().any(|g| g.deny) && reaching().any(|g| !g.deny)
}

/// Where a route's scope comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum At {
    Server,
    /// The repository at this path segment (`owner/repo`, or its id).
    Repo(usize),
    /// The run at this segment: its repository.
    Run(usize),
    /// The job (a v1 job or an engine leg) at this segment: its run's repository.
    Job(usize),
    /// The runner at this segment.
    Runner(usize),
    /// The diff at this segment (`D12`, `12`, or its id): its repository.
    Diff(usize),
    /// The stack at this segment: its repository.
    Stack(usize),
    /// The review comment at this segment: its diff's repository.
    Comment(usize),
    /// The land queue entry at this segment: its repository.
    Entry(usize),
    /// The `scope` query parameter (`repo:acme/app`, …), else the server.
    ScopeQuery,
    /// The `repo` query parameter, else the server.
    RepoQuery,
    /// Anywhere the caller has the capability; the handler narrows what it reads by what the
    /// caller may read (a body naming a repository, a list filtered row by row).
    Anywhere,
    /// The caller's own (tokens): any user may manage theirs, `tokens.manage` anyone's; the
    /// handler tells them apart.
    Own,
}

/// One route's rule: its method (`None`: any), path pattern (`*` one segment, `**` one or more),
/// and the capability it needs where.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Rule {
    pub method: Option<&'static str>,
    pub pattern: &'static str,
    pub capability: &'static str,
    pub at: At,
}

const fn r(method: &'static str, pattern: &'static str, capability: &'static str, at: At) -> Rule {
    Rule { method: Some(method), pattern, capability, at }
}

const fn any(pattern: &'static str, capability: &'static str, at: At) -> Rule {
    Rule { method: None, pattern, capability, at }
}

/// The API behind authentication (W10-R05), first match wins. docs/ci.md §4.1 lists the CI part.
pub static ROUTES: &[Rule] = &[
    r("GET", "/api/openapi.json", SIGNED_IN, At::Server),
    r("GET", "/api/peers/open", SIGNED_IN, At::Server),
    r("POST", "/api/peers/*/frame", SIGNED_IN, At::Server),
    r("GET", "/api/ci/schema/*", SIGNED_IN, At::Server),
    r("GET", "/api/overview", "runs.read", At::Server),
    r("GET", "/api/system", "runners.read", At::Server),
    r("GET", "/api/events", "runs.read", At::Server),
    r("GET", "/api/metrics/history", "runners.read", At::Server),
    // Every conversation store and its series (docs/conversations.md, "Telemetry").
    r("GET", "/api/conversations/status", "runners.read", At::Server),
    r("GET", "/api/conversations/series", "runners.read", At::Server),
    r("GET", "/api/audit", "audit.read", At::Server),
    // Repositories, runs and jobs (v1's routes; the engine's legs are jobs too).
    r("GET", "/api/repos", "runs.read", At::Anywhere),
    r("POST", "/api/repos/*/trigger", "runs.trigger", At::Repo(2)),
    r("POST", "/api/repos/*/poll", "runs.trigger", At::Repo(2)),
    // Diffs and stacks (W14).
    r("GET", "/api/repos/*/diffs", "diffs.read", At::Repo(2)),
    r("GET", "/api/diffs/*", "diffs.read", At::Diff(2)),
    r("GET", "/api/diffs/*/**", "diffs.read", At::Diff(2)),
    r("POST", "/api/diffs/*/ready", "diffs.submit", At::Diff(2)),
    r("POST", "/api/diffs/*/abandon", "diffs.submit", At::Diff(2)),
    r("POST", "/api/diffs/*/reclaim", "diffs.submit", At::Diff(2)),
    r("POST", "/api/diffs/*/commandeer", "diffs.submit", At::Diff(2)),
    r("GET", "/api/stacks/*", "diffs.read", At::Stack(2)),
    r("POST", "/api/diffs/*/comments", "diffs.review", At::Diff(2)),
    r("POST", "/api/diffs/*/review", "diffs.review", At::Diff(2)),
    r("POST", "/api/diffs/*/viewed", "diffs.read", At::Diff(2)),
    r("POST", "/api/diffs/*/land", "diffs.land", At::Diff(2)),
    r("PATCH", "/api/comments/*", "diffs.review", At::Comment(2)),
    r("POST", "/api/comments/*/apply", "diffs.submit", At::Comment(2)),
    r("POST", "/api/repos/*/submit", "diffs.submit", At::Repo(2)),
    r("GET", "/api/review/inbox", "diffs.read", At::Anywhere),
    r("GET", "/api/repos/*/queue", "diffs.read", At::Repo(2)),
    r("DELETE", "/api/queue/*", "queue.manage", At::Entry(2)),
    // Change requests (W14-R04), as diffs.
    r("GET", "/api/repos/*/change-requests", "diffs.read", At::Repo(2)),
    r("POST", "/api/repos/*/change-requests", "diffs.submit", At::Repo(2)),
    r("GET", "/api/change-requests/*", "diffs.read", At::Diff(2)),
    r("GET", "/api/change-requests/*/**", "diffs.read", At::Diff(2)),
    r("POST", "/api/change-requests/*/comments", "diffs.review", At::Diff(2)),
    r("POST", "/api/change-requests/*/review", "diffs.review", At::Diff(2)),
    r("POST", "/api/change-requests/*/viewed", "diffs.read", At::Diff(2)),
    r("POST", "/api/change-requests/*/land", "diffs.land", At::Diff(2)),
    r("POST", "/api/change-requests/*/ready", "diffs.submit", At::Diff(2)),
    r("POST", "/api/change-requests/*/abandon", "diffs.submit", At::Diff(2)),
    r("POST", "/api/change-requests/*/reclaim", "diffs.submit", At::Diff(2)),
    r("POST", "/api/change-requests/*/commandeer", "diffs.submit", At::Diff(2)),
    r("GET", "/api/runs", "runs.read", At::RepoQuery),
    r("GET", "/api/runs/*", "runs.read", At::Run(2)),
    r("POST", "/api/runs/*/cancel", "runs.cancel", At::Run(2)),
    r("POST", "/api/runs/*/retry", "runs.rerun", At::Run(2)),
    r("GET", "/api/jobs/*", "runs.read", At::Job(2)),
    r("GET", "/api/jobs/*/logs", "logs.read", At::Job(2)),
    r("GET", "/api/jobs/*/logs/ws", "logs.read", At::Job(2)),
    r("GET", "/api/jobs/*/artifacts", "artifacts.read", At::Job(2)),
    // The CI engine.
    r("POST", "/api/ci/validate", "runs.read", At::Anywhere),
    r("POST", "/api/ci/attestations/verify", "runs.read", At::Anywhere),
    r("GET", "/api/ci/images", "runs.read", At::RepoQuery),
    r("POST", "/api/ci/plan-preview", "runs.read", At::Anywhere),
    r("GET", "/api/ci/runs", "runs.read", At::RepoQuery),
    r("GET", "/api/ci/runs/*", "runs.read", At::Run(3)),
    r("GET", "/api/ci/runs/*/artifacts/*/download", "artifacts.read", At::Run(3)),
    r("GET", "/api/ci/runs/*/**", "runs.read", At::Run(3)),
    r("POST", "/api/ci/runs/*/cancel", "runs.cancel", At::Run(3)),
    // Who may review is the environment's to say too: the handler checks its reviewers.
    r("POST", "/api/ci/runs/*/approve", "runs.approve", At::Anywhere),
    r("POST", "/api/ci/runs/*/rerun", "runs.rerun", At::Run(3)),
    r("GET", "/api/ci/legs/*", "runs.read", At::Job(3)),
    r("GET", "/api/ci/legs/*/logs", "logs.read", At::Job(3)),
    r("GET", "/api/ci/legs/*/logs/*", "logs.read", At::Job(3)),
    r("GET", "/api/ci/logs/search", "logs.read", At::RepoQuery),
    // A terminal into a leg, and keeping or releasing a failed one (W17); on the `host` executor
    // the terminal also needs `runners.terminal` on its runner, which the handler checks.
    r("GET", "/api/ci/legs/*/terminal", "runs.debug", At::Job(3)),
    r("POST", "/api/ci/legs/*/**", "runs.debug", At::Job(3)),
    r("GET", "/api/ci/legs/*/**", "runs.read", At::Job(3)),
    r("GET", "/api/ci/repos/*/names", "workflows.edit", At::Repo(3)),
    // W23: a repository's CI providers, and its host's pull requests.
    r("PUT", "/api/ci/repos/*/providers", "settings.manage", At::Repo(3)),
    r("POST", "/api/ci/repos/*/providers/sync", "runs.read", At::Repo(3)),
    r("GET", "/api/ci/repos/*/pulls", "diffs.read", At::Repo(3)),
    r("GET", "/api/ci/repos/*/pulls/*", "diffs.read", At::Repo(3)),
    r("POST", "/api/ci/repos/*/pulls/*/comments", "diffs.review", At::Repo(3)),
    r("POST", "/api/ci/repos/*/pulls/*/reviews", "diffs.review", At::Repo(3)),
    r("POST", "/api/ci/repos/*/pulls/*/merge", "diffs.land", At::Repo(3)),
    r("GET", "/api/ci/repos/*/**", "runs.read", At::Repo(3)),
    r("DELETE", "/api/ci/repos/*/caches/*", "caches.write", At::Repo(3)),
    r("POST", "/api/ci/repos/*/**", "runs.trigger", At::Repo(3)),
    any("/api/ci/secrets", "secrets.manage", At::ScopeQuery),
    any("/api/ci/secrets/**", "secrets.manage", At::ScopeQuery),
    any("/api/ci/variables", "variables.manage", At::ScopeQuery),
    any("/api/ci/variables/**", "variables.manage", At::ScopeQuery),
    r("GET", "/api/ci/actions", "runs.read", At::Anywhere),
    r("GET", "/api/ci/actions/policy", "runs.read", At::RepoQuery),
    any("/api/ci/actions/policy", "settings.manage", At::RepoQuery),
    any("/api/ci/actions/**", "settings.manage", At::Server),
    r("GET", "/api/ci/oidc/subject", "runs.read", At::ScopeQuery),
    any("/api/ci/oidc/subject", "settings.manage", At::ScopeQuery),
    any("/api/ci/oidc", "settings.manage", At::Server),
    any("/api/ci/oidc/**", "settings.manage", At::Server),
    r("GET", "/api/ci/environments", "runs.read", At::RepoQuery),
    r("GET", "/api/ci/environments/**", "runs.read", At::RepoQuery),
    any("/api/ci/environments", "environments.manage", At::RepoQuery),
    any("/api/ci/environments/**", "environments.manage", At::RepoQuery),
    r("POST", "/api/ci/routing/explain", "runs.read", At::Anywhere),
    any("/api/ci/routing/rules", "routing.manage", At::ScopeQuery),
    r("GET", "/api/ci/integrations/kinds", "runs.read", At::Anywhere),
    any("/api/ci/integrations", "integrations.manage", At::Server),
    any("/api/ci/integrations/**", "integrations.manage", At::Server),
    any("/api/ci/webhooks/**", "integrations.manage", At::Server),
    r("GET", "/api/ci/insights/repos", "runs.read", At::Server),
    r("GET", "/api/ci/insights/repos/*", "runs.read", At::Repo(4)),
    r("GET", "/api/ci/insights/workflows/*", "runs.read", At::Repo(4)),
    r("GET", "/api/ci/insights/runners", "runners.read", At::Server),
    r("GET", "/api/ci/insights/runners/*", "runners.read", At::Server),
    // Deployments (docs/cd.md).
    r("GET", "/api/cd/**", "runs.read", At::Server),
    any("/api/cd/**", "environments.deploy", At::Server),
    // Deploying by API (docs/deploy-api.md §15): releases are deploys; apps, environments and
    // registrations are their management.
    r("GET", "/api/deploy/**", "runs.read", At::Server),
    r("POST", "/api/deploy/releases", "environments.deploy", At::Server),
    r("POST", "/api/deploy/fleets", "environments.deploy", At::Server),
    r("POST", "/api/deploy/releases/*/approve", "runs.approve", At::Server),
    r("POST", "/api/deploy/releases/*/reject", "runs.approve", At::Server),
    r("POST", "/api/deploy/releases/*/**", "environments.deploy", At::Server),
    // An app's secrets (§3.5) are read as the rest of it (names and versions: no value is read
    // back), and set or deleted by those who manage secrets.
    any("/api/deploy/apps/*/environments/*/secrets/*", "secrets.manage", At::Server),
    r("POST", "/api/deploy/secret-keys/rotate", "secrets.manage", At::Server),
    any("/api/deploy/**", "environments.manage", At::Server),
    // Runners and machines. The list is of the runners the caller may read (`crate::doctor::readable`).
    r("GET", "/api/agents", "runners.read", At::Anywhere),
    r("GET", "/api/reach/runners", "runners.read", At::Server),
    r("GET", "/api/front-doors", "runners.read", At::Server),
    r("GET", "/api/runners/*/front-door", "runners.read", At::Runner(2)),
    any("/api/runners/*/**", "runners.manage", At::Runner(2)),
    r("GET", "/api/agents/*/diagnose", "runners.read", At::Runner(2)),
    r("POST", "/api/agents/*/repair", "runners.manage", At::Runner(2)),
    r("POST", "/api/agents/*/drain", "runners.manage", At::Runner(2)),
    r("DELETE", "/api/agents/*", "runners.manage", At::Runner(2)),
    r("GET", "/api/machines/*/dirs", "runners.terminal", At::Runner(2)),
    any("/api/terminal/**", "runners.terminal", At::Server),
    r("GET", "/api/updates", "runners.read", At::Server),
    // The runners the caller may read against the newest kiln (`updates.list`).
    r("GET", "/api/updates/runners", "runners.read", At::Anywhere),
    r("POST", "/api/updates/binaries", "binaries.publish", At::Server),
    // A runner upgraded now, or its policy (`crate::doctor`'s `updates.retry` and `updates.policy`).
    r("POST", "/api/updates/runners/*", "runners.manage", At::Runner(3)),
    r("PUT", "/api/updates/runners/*", "runners.manage", At::Runner(3)),
    // Agents: the harness, AI sessions, plans, places, voice, model accounts.
    any("/api/harness/**", "agents.run", At::Server),
    any("/api/ai-sessions", "agents.run", At::Server),
    any("/api/ai-sessions/**", "agents.run", At::Server),
    // Attached agents and their incidents (docs/ci.md W20-R08 to R10): the handlers narrow by
    // what each attachment looks after; reporting is an incident's agent's, or a person's who may
    // attach there.
    r("GET", "/api/attachments", "runs.read", At::Anywhere),
    r("GET", "/api/attachments/*", "runs.read", At::Anywhere),
    any("/api/attachments", "agents.attach", At::Anywhere),
    any("/api/attachments/**", "agents.attach", At::Anywhere),
    r("POST", "/api/incidents/*/report", SIGNED_IN, At::Server),
    r("GET", "/api/incidents", "runs.read", At::Anywhere),
    r("GET", "/api/incidents/*", "runs.read", At::Anywhere),
    any("/api/plans", "agents.run", At::Server),
    any("/api/plans/**", "agents.run", At::Server),
    any("/api/places", "agents.run", At::Server),
    any("/api/places/**", "agents.run", At::Server),
    any("/api/voice", "agents.run", At::Server),
    any("/api/voice/**", "agents.run", At::Server),
    // Notifications of agents (docs/notifications.md): each person's own devices and mutes.
    any("/api/push", "agents.run", At::Server),
    any("/api/push/**", "agents.run", At::Server),
    // Identity (W10).
    any("/api/tokens", "tokens.manage", At::Own),
    any("/api/tokens/**", "tokens.manage", At::Own),
    r("GET", "/api/roles", SIGNED_IN, At::Server),
    any("/api/roles/**", "users.manage", At::Server),
    any("/api/users", "users.manage", At::Server),
    any("/api/users/**", "users.manage", At::Server),
    any("/api/grants", "users.manage", At::Server),
    any("/api/grants/**", "users.manage", At::Server),
];

/// What a route needs when the table doesn't name it: repositories' settings, sync, discovery,
/// join requests, updates, the password, accounts, Tailscale and the rest of the server's
/// administration.
pub const UNNAMED: Rule = Rule { method: None, pattern: "/api/**", capability: "settings.manage", at: At::Server };

fn matches(pattern: &str, segs: &[&str]) -> bool {
    let pat: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    for (n, p) in pat.iter().enumerate() {
        if *p == "**" {
            return segs.len() > n;
        }
        match segs.get(n) {
            Some(s) if *p == "*" || p == s => {}
            _ => return false,
        }
    }
    segs.len() == pat.len()
}

/// The rule for a request: the first that matches, else [`UNNAMED`].
pub fn rule(method: &Method, segs: &[&str]) -> &'static Rule {
    // HEAD asks what GET would answer.
    let method = if method == Method::HEAD { "GET" } else { method.as_str() };
    ROUTES.iter().find(|r| r.method.is_none_or(|m| m == method) && matches(r.pattern, segs)).unwrap_or(&UNNAMED)
}

/// The OpenAPI 3.1 document of the table (W10-R05): each route's method and path, the capability
/// it needs (`x-kiln-capability`) and where that is checked (`x-kiln-scope`).
pub fn openapi(version: &str) -> serde_json::Value {
    let mut paths = serde_json::Map::new();
    for r in ROUTES {
        // A parameter is named for the segment before it: `/runs/{run}`, `/repos/{repo}`.
        let segs: Vec<&str> = r.pattern.split('/').collect();
        let path = segs
            .iter()
            .enumerate()
            .map(|(i, s)| match *s {
                "*" => {
                    let before = i.checked_sub(1).and_then(|b| segs.get(b)).copied().unwrap_or("id");
                    format!("{{{}}}", before.strip_suffix('s').unwrap_or(before))
                }
                "**" => "{rest}".to_owned(),
                other => (*other).to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/");
        let methods: Vec<String> = match r.method {
            Some(m) => vec![m.to_ascii_lowercase()],
            None => ["get", "post", "put", "patch", "delete"].map(str::to_owned).to_vec(),
        };
        let entry = paths.entry(path).or_insert_with(|| serde_json::json!({}));
        if let Some(o) = entry.as_object_mut() {
            for m in methods {
                o.entry(m).or_insert_with(|| {
                    serde_json::json!({
                        "x-kiln-capability": r.capability,
                        "x-kiln-scope": format!("{:?}", r.at),
                        "security": [{ "bearer": [] }, { "session": [] }],
                        "responses": { "200": { "description": "OK" }, "401": { "description": "not signed in" }, "403": { "description": "the capability is missing in that scope" } },
                    })
                });
            }
        }
    }
    serde_json::json!({
        "openapi": "3.1.0",
        "info": { "title": "kiln", "version": version, "description": "Every route of kiln's API behind sign-in, with the capability each needs (docs/ci.md W10). Routes not listed need settings.manage." },
        "components": { "securitySchemes": {
            "bearer": { "type": "http", "scheme": "bearer", "description": "the admin token, a personal access token (kiln_pat_…), an agent token (kiln_agt_…) or a job token (kiln_job_…)" },
            "session": { "type": "apiKey", "in": "cookie", "name": "kiln_session" },
        } },
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(p: &str) -> Vec<&str> {
        p.trim_matches('/').split('/').collect()
    }

    #[test]
    fn w10_r02_scopes_reach_what_they_contain() -> Result<(), String> {
        let repo = Scope::parse("repo:acme/app")?;
        assert!(Scope::Server.covers(&repo));
        assert!(Scope::parse("org:acme")?.covers(&repo) && Scope::parse("org:ACME")?.covers(&Scope::parse("workflow:acme/app:.github/workflows/ci.yml")?));
        assert!(!Scope::parse("org:other")?.covers(&repo) && !Scope::parse("org:acme")?.covers(&Scope::Server));
        assert!(repo.covers(&Scope::parse("environment:acme/app:prod")?) && repo.covers(&Scope::parse("env:acme/app:prod")?));
        assert!(!repo.covers(&Scope::parse("repo:acme/other")?) && !repo.covers(&Scope::Server));
        assert!(!Scope::parse("environment:acme/app:prod")?.covers(&repo), "an environment's grant doesn't reach its repository");
        assert!(Scope::parse("runner:mac-1")?.covers(&Scope::parse("runner:mac-1")?) && !Scope::parse("runner:mac-1")?.covers(&Scope::parse("runner:mac-2")?));
        assert!(Scope::parse("galaxy:far").is_err() && Scope::parse("repo:").is_err());
        assert_eq!(Scope::parse("workflow:acme/app:.github/workflows/ci.yml")?.text(), "workflow:acme/app:.github/workflows/ci.yml");
        Ok(())
    }

    #[test]
    fn w10_r02_roles_grants_and_a_deny() -> Result<(), String> {
        let custom: HashMap<String, Vec<String>> = [("releaser".to_owned(), vec!["binaries.publish".to_owned()])].into_iter().collect();
        let app = Scope::Repo { repo: "acme/app".into() };
        let other = Scope::Repo { repo: "acme/other".into() };
        let grants = vec![Grant::parse("developer@org:acme")?, Grant::parse("!runs.cancel@repo:acme/other")?, Grant::parse("releaser")?];
        assert!(allows(&grants, &custom, "runs.trigger", &app));
        assert!(allows(&grants, &custom, "runs.cancel", &app));
        assert!(!allows(&grants, &custom, "runs.cancel", &other), "the deny wins");
        assert!(allows(&grants, &custom, "runs.trigger", &other));
        assert!(!allows(&grants, &custom, "diffs.land", &app), "a developer doesn't land");
        assert!(allows(&grants, &custom, "binaries.publish", &Scope::Server), "a custom role");
        assert!(!allows(&[], &custom, "runs.read", &app), "nothing granted, nothing allowed");
        assert_eq!(Grant::parse("!runs.cancel@repo:acme/other")?.text(), "!runs.cancel@repo:acme/other");
        assert!(Grant::parse("runs.read@galaxy:far").is_err() && Grant::parse("@server").is_err());
        assert!(role("maintainer").is_some_and(|m| role("developer").is_some_and(|d| d.iter().all(|c| m.contains(c)))));
        Ok(())
    }

    /// Every rule is reachable: the request its own pattern describes finds it, not an earlier
    /// rule; and the routes the table doesn't name need `settings.manage`.
    #[test]
    fn w10_r05_every_rule_is_the_one_its_route_finds() {
        for rule in ROUTES {
            let concrete: Vec<&str> = rule.pattern.trim_matches('/').split('/').map(|s| if s == "*" || s == "**" { "x" } else { s }).collect();
            let method = rule.method.map_or(Method::PATCH, |m| Method::from_bytes(m.as_bytes()).unwrap_or(Method::GET));
            let found = super::rule(&method, &concrete);
            assert!(std::ptr::eq(found, rule), "{method} {} finds {} first", rule.pattern, found.pattern);
            assert!(rule.capability == SIGNED_IN || CAPABILITIES.contains(&rule.capability), "{} names no capability", rule.pattern);
        }
        for (m, p, cap) in [
            (Method::POST, "/api/unknown/thing", "settings.manage"),
            (Method::POST, "/api/repos", "settings.manage"),
            (Method::PUT, "/api/settings/password", "settings.manage"),
            (Method::GET, "/api/ci/runs/r1/graph", "runs.read"),
            (Method::POST, "/api/ci/runs/r1/rerun", "runs.rerun"),
            (Method::GET, "/api/ci/actions/policy", "runs.read"),
            (Method::PUT, "/api/ci/actions/policy", "settings.manage"),
            (Method::POST, "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/dispatches", "runs.trigger"),
            (Method::DELETE, "/api/ci/repos/r/caches/9", "caches.write"),
            (Method::DELETE, "/api/updates/binaries/x86_64-linux", "settings.manage"),
            (Method::HEAD, "/api/ci/runs/r1", "runs.read"),
            (Method::GET, "/api/deploy/apps/web/environments/staging/secrets", "runs.read"),
            (Method::GET, "/api/deploy/apps/web/environments/staging/secrets/DB_PASSWORD/versions", "runs.read"),
            (Method::PUT, "/api/deploy/apps/web/environments/staging/secrets/DB_PASSWORD", "secrets.manage"),
            (Method::DELETE, "/api/deploy/apps/web/environments/staging/secrets/DB_PASSWORD", "secrets.manage"),
            (Method::POST, "/api/deploy/secret-keys/rotate", "secrets.manage"),
            (Method::POST, "/api/deploy/apps/web/environments/staging", "environments.manage"),
        ] {
            assert_eq!(super::rule(&m, &segs(p)).capability, cap, "{m} {p}");
        }
        assert!(CAPABILITIES.iter().all(|c| role("owner").unwrap_or_default().contains(c)));
    }

    #[test]
    fn w10_r05_the_openapi_document_comes_from_the_table() {
        let doc = openapi("0.1.0");
        assert_eq!(doc.pointer("/paths/~1api~1ci~1runs~1{run}~1rerun/post/x-kiln-capability").and_then(serde_json::Value::as_str), Some("runs.rerun"));
        assert_eq!(doc.pointer("/paths/~1api~1tokens/delete/x-kiln-capability").and_then(serde_json::Value::as_str), Some("tokens.manage"));
        assert_eq!(doc.get("openapi").and_then(serde_json::Value::as_str), Some("3.1.0"));
        assert!(doc.pointer("/paths/~1api~1ci~1repos~1{repo}~1caches~1{cache}/delete").is_some());
    }
}
