//! kiln's runners for the providers that act through them (docs/cd-targets.md §2.2): the server's
//! CI. A run starts in the repository behind the app at the release's commit: a workflow kiln wrote
//! (started as CD's own), or one of the repository's (dispatched with its inputs). Runners are
//! picked by CI's routing constraints over what each connected runner reports. Artifacts reach the
//! runners through links CI's blob service signs.

use anyhow::{Context as _, Result};
use kiln_cd::providers::host::Host;

use super::targets::runner::{BoxFut, RunState, Runners, Work};
use crate::hub::Hub;
use crate::store::Repo;

/// The repository behind `app`, and the commit its source is at: the definition's source, as
/// KilnCD holds it.
async fn source(hub: &Hub, app: &str) -> Result<(Repo, String)> {
    let cd = crate::cd::get().context("KilnCD isn't running")?;
    let def = cd.definition(app).with_context(|| format!("no app {app}"))?;
    let src = def.sources.first().with_context(|| format!("{app} has no source"))?;
    let repos = hub.store.repos().await?;
    let name = super::targets::repo_name(&src.repo);
    let repo = repos.into_iter().find(|r| r.clone_url == src.repo || r.name == src.repo || r.name == name).with_context(|| format!("{app}'s source ({}) isn't one of kiln's repositories, which its runners' jobs run from", src.repo))?;
    Ok((repo, src.revision.clone()))
}

impl Runners for Hub {
    fn pick<'a>(&'a self, selector: &'a str) -> BoxFut<'a, Result<Vec<Host>>> {
        Box::pin(async move {
            let c = kiln_ci::sched::routing::Constraint::parse(selector).map_err(|e| anyhow::anyhow!("selector `{selector}`: {e}"))?;
            // A runner whose probes haven't reported is judged by its labels, its facts unknown.
            let unknown = kiln_ci::facts::RunnerFacts::default();
            let mut out: Vec<Host> = self
                .agent_list()
                .await
                .into_iter()
                .filter(|a| a.connected && c.eval(a.facts.as_deref().unwrap_or(&unknown), &a.labels))
                .map(|a| Host { name: a.name.clone(), os: a.os.clone() })
                .collect();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
    }

    fn start<'a>(&'a self, app: &'a str, revision: &'a str, work: Work<'a>) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            let (repo, at) = source(self, app).await?;
            let sha = if revision.is_empty() { at } else { revision.to_owned() };
            match work {
                Work::Written { name, yaml } => {
                    let path = format!(".kiln/cd/{}.yaml", name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>().trim_matches('-'));
                    crate::ci::events::start_written(self, &repo, &sha, &path, yaml, "kiln").await.map_err(|e| anyhow::anyhow!("{}", e.message))
                }
                Work::Own { workflow, job, inputs } => crate::ci::events::dispatch_at(self, &repo, &sha, workflow, job, inputs, "kiln").await.map_err(|e| anyhow::anyhow!("{}", e.message)),
            }
        })
    }

    fn run<'a>(&'a self, id: &'a str) -> BoxFut<'a, Result<RunState>> {
        Box::pin(async move {
            let r = self.store.run(id.to_owned()).await?.with_context(|| format!("no run {id}"))?;
            let base = self.cfg.server.public_url.trim_end_matches('/');
            Ok(RunState { id: id.to_owned(), number: r.number, status: r.status.as_str().to_owned(), error: r.error.clone(), url: format!("{base}/runs/{id}") })
        })
    }

    fn environment<'a>(&'a self, app: &'a str, name: &'a str, kind: &'a str, selector: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let (repo, _) = source(self, app).await?;
            crate::ci::environments::define(self, &repo, name, kind, selector).await
        })
    }

    fn undeploy<'a>(&'a self, app: &'a str, kind: &'a str, name: &'a str, hosts: &'a [String]) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let (repo, _) = source(self, app).await?;
            crate::ci::previews::undeploy_on(self, &repo, kind, name, hosts).await
        })
    }

    fn link(&self, hex: &str, secs: i64) -> Result<String> {
        let digest: [u8; 32] = hex::decode(hex).ok().and_then(|b| b.try_into().ok()).with_context(|| format!("`{hex}` isn't a SHA-256"))?;
        let base = self.cfg.server.public_url.trim_end_matches('/');
        Ok(crate::ci::services::blob::url(self, base, crate::ci::services::blob::Target::Object(digest), "r", secs))
    }
}
