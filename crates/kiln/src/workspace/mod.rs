//! Where an agent works, and how its work leaves it (docs/agentic.md, W and C).
//!
//! The machine an agent works on answers questions about its folder ([`Ask`]): the server asks
//! ([`crate::proto::ServerMsg::Workspace`], or in process for the server's own agents), the
//! machine looks at its session's folder itself, so a question can never be about another one.
//!
//! - [`git`]: the folder's repository, and a commit of the agent's work made beside it.
//! - [`change`]: an agent's change request, from its work to review (C1, C6).
//! - [`follow`]: what happens to it, back to the agent (C2, C3).

pub mod change;
pub mod follow;
pub mod git;

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What the server asks the machine about an agent's folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Ask {
    /// Its repository, as git sees it ([`git::Facts`]).
    Git,
    /// Commit the agent's work and push it ([`git::Made`] and what the push said).
    Change(Change),
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

/// Answers `ask` (JSON) about the folder `cwd`, on the machine it is on: `{"facts"}`, `{"made",
/// "said"}`, or `{"error"}`.
pub async fn serve(cwd: &Path, ask: &str) -> Value {
    let ask: Ask = match serde_json::from_str(ask) {
        Ok(a) => a,
        Err(e) => return json!({"error": format!("a question this machine doesn't understand ({e}): its kiln may be older than the server's")}),
    };
    match answer(cwd, ask).await {
        Ok(v) => v,
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

async fn answer(cwd: &Path, ask: Ask) -> anyhow::Result<Value> {
    match ask {
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
    }
}
