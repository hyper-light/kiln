//! Where an agent works, and how its work leaves it (docs/agentic.md, W and C).
//!
//! The machine an agent works on answers questions about its folder ([`Ask`]): the server asks
//! ([`crate::proto::ServerMsg::Workspace`], or in process for the server's own agents), the
//! machine looks at its session's folder itself, so a question can never be about another one.
//!
//! - [`git`]: the folder's repository, and a commit of the agent's work made beside it.
//! - [`change`]: an agent's change request, from its work to review (C1, C6).
//! - [`follow`]: what happens to it, back to the agent (C2, C3).
//! - [`slates`]: a workspace of its own, a Slates volume over its folder (W1–W4).

pub mod change;
pub mod follow;
pub mod git;
pub mod slates;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where an agent works when not in its folder itself (`Spec::workspace`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    /// `volume`: a Slates volume of its own, laid over its folder.
    pub kind: String,
    /// The folder it's laid over: its repository's top (else the folder itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    /// Its folder within that, relative to it (`crates/kiln`); empty for the top itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub: String,
    /// The volume, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made: Option<slates::Made>,
}

/// The kinds of workspace an agent may ask for: its folder itself, or a volume of its own.
pub const KINDS: &[&str] = &["folder", "volume"];

/// Makes the workspace `spec` asks for (on the machine its agent `id` works on), before its
/// harness starts: a volume laid over its folder's repository, mounted, and the agent's folder in
/// it; or, for one made already (the runner started again), mounted again if its mount went away.
/// Nothing for an agent that works in its folder itself.
pub async fn prepare(spec: &mut crate::harness::session::Spec, id: &str) -> anyhow::Result<()> {
    let Some(w) = spec.workspace.clone().filter(|w| w.kind == "volume") else { return Ok(()) };
    if let Some(m) = &w.made {
        return slates::mounted(m).await;
    }
    let cwd = PathBuf::from(crate::harness::session::resolve_cwd(spec.cwd.as_deref()));
    anyhow::ensure!(cwd.is_dir(), "{} isn't a folder on this machine", cwd.display());
    // The whole repository, so git works in it as it does in the folder; the agent where it was asked to be.
    let (over, sub) = match git::facts(&cwd).await {
        Ok(f) => {
            let top = PathBuf::from(&f.top);
            let sub = cwd.strip_prefix(&top).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            (top, sub)
        }
        Err(_) => (cwd.clone(), String::new()),
    };
    // The whole id: kiln's ids start with the time they were made, so two agents started together
    // share their beginnings (Slates' names are unique on the machine).
    let whole: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    let made = slates::make(&format!("kiln-{whole}"), &over).await?;
    let here = if sub.is_empty() { PathBuf::from(&made.mount) } else { PathBuf::from(&made.mount).join(&sub) };
    spec.cwd = Some(here.to_string_lossy().into_owned());
    spec.workspace = Some(Workspace { kind: w.kind, over: Some(over.to_string_lossy().into_owned()), sub, made: Some(made) });
    Ok(())
}

/// Whether `spec` needs its workspace made (or mounted again) before its harness starts.
pub fn needs_preparing(spec: &crate::harness::session::Spec) -> bool {
    spec.workspace.as_ref().is_some_and(|w| w.kind == "volume")
}

/// Ends agent session `spec`'s workspace for good, when it has one of its own: its volume and what
/// it held go with the session.
pub fn release(spec: &crate::harness::session::Spec) {
    let Some(m) = spec.workspace.as_ref().and_then(|w| w.made.clone()) else { return };
    crate::rt::spawn(async move {
        if let Err(e) = slates::remove(&m).await {
            tracing::warn!(volume = %m.volume, error = %format!("{e:#}"), "an ended agent's workspace stayed");
        }
    });
}

/// What the server asks the machine about an agent's folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Ask {
    /// Its repository, as git sees it ([`git::Facts`]).
    Git,
    /// Commit the agent's work and push it ([`git::Made`] and what the push said).
    Change(Change),
    /// Its workspace of its own: what changed on disk under it since the agent changed it.
    Look,
    /// What writing its workspace back to its folder would do: Slates' plan, for a person to read.
    Plan,
    /// Write it back as planned, a person having said yes to that plan (its manifest's hash).
    Land { manifest: String },
    /// One step of a plan run's stack: the agent's work committed on the step before's, and
    /// pushed after the earlier steps' that weren't yet (docs/agentic.md P4).
    Stack(Stacked),
    /// A new version of one step of a plan run's stack, and the steps above it made again on it.
    Restack(Restack),
}

/// A push of one commit to one ref, as one principal.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Push {
    /// The commit (empty: the one just made).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha: String,
    pub to_ref: String,
    /// An `Authorization: Bearer` for a kiln server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

/// One step of a plan run's stack, committed and pushed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stacked {
    /// The commit it goes on (the step before's); `None`: the folder's last commit.
    pub on: Option<String>,
    /// The files it takes (as the agent's changes name them).
    pub paths: Vec<String>,
    pub message: String,
    /// Who commits when the machine's git doesn't say.
    pub fallback: git::Ident,
    /// Where the pushes go (a remote's name or a URL); none: the commit stays in the folder's
    /// repository.
    pub to: Option<String>,
    /// In order: the earlier steps' commits not pushed yet, then this one's.
    pub pushes: Vec<Push>,
}

/// A step's new version, the steps above it restacked.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Restack {
    /// The stack's commits from the step's up, bottom first.
    pub commits: Vec<String>,
    /// The agent's files: as the folder has them now, they go into the first commit.
    pub paths: Vec<String>,
    /// The first commit's new message, when it changes.
    pub message: Option<String>,
    pub fallback: git::Ident,
    pub to: Option<String>,
    /// The ref each new commit goes to, by its place in `commits` (none: not pushed by itself).
    pub refs: Vec<Option<String>>,
    pub bearer: Option<String>,
}

/// A commit of the agent's work, pushed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    /// The files it takes (as the agent's changes name them); `None`: every change git sees.
    pub paths: Option<Vec<String>>,
    pub message: String,
    /// Who commits when the machine's git doesn't say.
    pub fallback: git::Ident,
    /// A remote's name or a URL.
    pub to: String,
    /// The ref it goes to (`refs/heads/agents/x`, `refs/for/main/branch/agents/x`), rewritten.
    pub to_ref: String,
    /// An `Authorization: Bearer` for a kiln server.
    pub bearer: Option<String>,
}

/// How long the server waits for a machine's answer about its folder (a push can be slow).
pub const WAIT: Duration = Duration::from_secs(240);

/// Answers `ask` (JSON) about the folder `cwd` of an agent (and its workspace `ws`, when it has
/// one of its own), on the machine it is on: `{"facts"}`, `{"made", "said"}`, `{"drift"}`,
/// `{"plan"}`, `{"landed"}`, or `{"error"}`.
pub async fn serve(cwd: &Path, ws: Option<&Workspace>, ask: &str) -> Value {
    let ask: Ask = match serde_json::from_str(ask) {
        Ok(a) => a,
        Err(e) => return json!({"error": format!("a question this machine doesn't understand ({e}): its kiln may be older than the server's")}),
    };
    match answer(cwd, ws, ask).await {
        Ok(v) => v,
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// The workspace of its own an ask about one needs: its volume and the folder it's laid over.
fn own(ws: Option<&Workspace>) -> anyhow::Result<(&slates::Made, &str)> {
    let w = ws.ok_or_else(|| anyhow::anyhow!("it works in its folder itself, not a workspace of its own"))?;
    match (&w.made, &w.over) {
        (Some(m), Some(over)) => Ok((m, over.as_str())),
        _ => anyhow::bail!("its workspace isn't made yet"),
    }
}

async fn answer(cwd: &Path, ws: Option<&Workspace>, ask: Ask) -> anyhow::Result<Value> {
    match ask {
        Ask::Look => {
            let (m, over) = own(ws)?;
            Ok(json!({"drift": slates::drift(m).await?, "over": over, "volume": m.volume, "mount": m.mount}))
        }
        Ask::Plan => {
            let (m, over) = own(ws)?;
            let plan = slates::plan_landing(m, over).await?;
            // Why it can't be written from kiln: this machine's Slates takes no go-ahead from it, or
            // the landing would write what git ignores.
            let why_not = match slates::can_take_grants() {
                Err(e) => Some(format!("{e:#}")),
                Ok(()) => slates::writable(m, &plan).await.err(),
            };
            Ok(json!({"plan": plan, "excludes": slates::EXCLUDES, "can_write": why_not.is_none(), "why_not": why_not}))
        }
        Ask::Land { manifest } => {
            let (m, over) = own(ws)?;
            Ok(match slates::land(m, over, &manifest).await? {
                slates::Landed::Written(outcome) => json!({"landed": outcome}),
                slates::Landed::Changed(plan) => {
                    let why_not = slates::writable(m, &plan).await.err();
                    json!({"changed": true, "plan": plan, "excludes": slates::EXCLUDES, "can_write": why_not.is_none(), "why_not": why_not})
                }
            })
        }
        Ask::Git => Ok(json!({"facts": git::facts(cwd).await?})),
        Ask::Change(c) => {
            let facts = git::facts(cwd).await?;
            let top = Path::new(&facts.top);
            let who = git::identity(top, &c.fallback).await;
            let Some(made) = git::commit(top, facts.head.as_deref(), c.paths.as_deref(), &c.message, &who).await? else {
                anyhow::bail!("there's nothing to commit: the agent's files are as the folder's last commit has them");
            };
            let said = git::push(top, &c.to, &format!("+{}:{}", made.sha, c.to_ref), c.bearer.as_deref()).await?;
            Ok(json!({"made": made, "said": said}))
        }
        Ask::Stack(s) => {
            let Some(made) = git::commit_on(cwd, s.on.as_deref(), &s.paths, &s.message, &s.fallback).await? else {
                return Ok(json!({"nothing": true}));
            };
            let facts = git::facts(cwd).await?;
            let pushes: Vec<Push> = s.pushes.into_iter().map(|p| Push { sha: if p.sha.is_empty() { made.sha.clone() } else { p.sha }, ..p }).collect();
            let (pushed, said, error) = push_all(Path::new(&facts.top), s.to.as_deref(), &pushes).await;
            Ok(json!({"made": made, "pushed": pushed, "said": said, "push_error": error}))
        }
        Ask::Restack(r) => {
            let facts = git::facts(cwd).await?;
            let top = Path::new(&facts.top);
            let who = git::identity(top, &r.fallback).await;
            let made = git::restack(top, &r.commits, &r.paths, r.message.as_deref(), &who).await?;
            let pushes: Vec<Push> = made.iter().zip(&r.refs).filter_map(|(m, to)| to.clone().map(|to_ref| Push { sha: m.sha.clone(), to_ref, bearer: r.bearer.clone() })).collect();
            let (pushed, said, error) = push_all(top, r.to.as_deref(), &pushes).await;
            Ok(json!({"made": made, "pushed": pushed, "said": said, "push_error": error}))
        }
    }
}

/// Pushes `pushes` to `to` in order, up to the first that fails: how many went, what was said,
/// and why the one that didn't go didn't.
async fn push_all(top: &Path, to: Option<&str>, pushes: &[Push]) -> (usize, String, Option<String>) {
    let Some(to) = to else { return (0, String::new(), None) };
    let mut said = String::new();
    for (i, p) in pushes.iter().enumerate() {
        match git::push(top, to, &format!("+{}:{}", p.sha, p.to_ref), p.bearer.as_deref()).await {
            Ok(s) => said.push_str(&s),
            Err(e) => return (i, said, Some(format!("{e:#}"))),
        }
    }
    (pushes.len(), said, None)
}
