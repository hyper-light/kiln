//! Who may use a repository's actions and reusable workflows, as the server reads it (docs/ci.md
//! W29-R17): GitHub's own setting for a repository on GitHub kiln holds a credential for, else
//! `.kiln/config.yaml`'s `share:` on its default branch, read again at most every 20 seconds.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kiln_ci::sharing::Access;
use parking_lot::Mutex;

use crate::hub::Hub;
use crate::store::Repo;

const FRESH: Duration = Duration::from_secs(20);

static CACHE: Mutex<Option<HashMap<String, (Instant, Access)>>> = Mutex::new(None);

/// `repo`'s Access.
pub async fn access_of(hub: &Hub, repo: &Repo) -> Access {
    if let Some((at, a)) = CACHE.lock().get_or_insert_with(HashMap::new).get(&repo.id)
        && at.elapsed() < FRESH
    {
        return *a;
    }
    let a = match crate::githost::for_repo(hub, repo).await {
        Some(crate::githost::GitHost::Github(app)) => match app.sharing(repo).await {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "GitHub didn't say who may use the repository's actions: its .kiln/config.yaml says");
                super::repo_config::load(hub, repo).await.share()
            }
        },
        _ => super::repo_config::load(hub, repo).await.share(),
    };
    CACHE.lock().get_or_insert_with(HashMap::new).insert(repo.id.clone(), (Instant::now(), a));
    a
}
