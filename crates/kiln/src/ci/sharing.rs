//! Who may use a repository's actions and reusable workflows, as the server reads it (docs/ci.md
//! W29-R17): GitHub's own setting for a repository on GitHub kiln holds a credential for, asked
//! again at most every 20 seconds, else `.kiln/config.yaml`'s `share:` on its default branch, as
//! `repo_config` reads it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kiln_ci::sharing::Access;
use parking_lot::Mutex;

use crate::hub::Hub;
use crate::store::Repo;

const FRESH: Duration = Duration::from_secs(20);

/// GitHub's answer by repository, `None` where it gave none.
static CACHE: Mutex<Option<HashMap<String, (Instant, Option<Access>)>>> = Mutex::new(None);

/// `repo`'s Access.
pub async fn access_of(hub: &Hub, repo: &Repo) -> Access {
    let kept = match CACHE.lock().get_or_insert_with(HashMap::new).get(&repo.id) {
        Some((at, said)) if at.elapsed() < FRESH => Some(*said),
        _ => None,
    };
    let said = match kept {
        Some(said) => said,
        None => {
            let said = match crate::githost::for_repo(hub, repo).await {
                Some(crate::githost::GitHost::Github(app)) => match app.sharing(repo).await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "GitHub didn't say who may use the repository's actions: its .kiln/config.yaml says");
                        None
                    }
                },
                _ => None,
            };
            CACHE.lock().get_or_insert_with(HashMap::new).insert(repo.id.clone(), (Instant::now(), said));
            said
        }
    };
    match said {
        Some(a) => a,
        None => super::repo_config::load(hub, repo).await.share(),
    }
}
