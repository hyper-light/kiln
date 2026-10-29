//! A deployment's commits (`docs/observe.md` §10.7): git's log from the deployment before it to it,
//! read from kiln's mirror of the repository it names (a commit the mirror lacks is fetched).

use super::ledger::{Commit, MAX_COMMITS};
use crate::hub::Hub;
use crate::store::Repo;

/// Whether `s` is a commit's sha (7 to 64 hex digits).
pub fn is_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A repository URL as two are compared: no scheme, user or password, `.git` or slash at its end;
/// `git@host:path` as `host/path`; its host lowercase.
pub fn normal_url(url: &str) -> String {
    let u = url.trim();
    let rest = u.split_once("://").map_or(u, |(_, r)| r);
    // scp's form: `git@github.com:acme/app.git`.
    let rest = match rest.split_once(':') {
        Some((host, path)) if !u.contains("://") && !host.contains('/') && !path.starts_with('/') => format!("{host}/{path}"),
        _ => rest.to_owned(),
    };
    let rest = rest.rsplit_once('@').filter(|(user, _)| !user.contains('/')).map_or(rest.as_str(), |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    match rest.split_once('/') {
        Some((host, path)) => format!("{}/{path}", host.to_ascii_lowercase()),
        None => rest.to_ascii_lowercase(),
    }
}

/// The kiln repository `url` names: its clone URL (as [`normal_url`] compares them) or kiln's name
/// for it (`owner/name`).
pub async fn repo_of(hub: &Hub, url: &str) -> Option<Repo> {
    let repos = hub.store.repos().await.ok()?;
    let want = normal_url(url);
    let name = url.trim();
    repos.into_iter().find(|r| r.name == name || (!want.is_empty() && normal_url(&r.clone_url) == want))
}

/// What git's log says of each commit: its sha, its author's time, its parents, its title and its
/// message's rest, fields apart by a unit separator and commits by a record separator.
const FORMAT: &str = "--format=%H%x1f%at%x1f%P%x1f%s%x1f%b%x1e";

/// The commits from `from` (not itself) to `to` in `repo`, newest first, one more than
/// [`MAX_COMMITS`] at the most (which says there were more): each fetched first when kiln's mirror
/// lacks it.
pub async fn between(hub: &Hub, repo: &Repo, from: &str, to: &str) -> Result<Vec<Commit>, String> {
    if !is_sha(from) || !is_sha(to) {
        return Err(format!("{from:?} and {to:?}: a commit's sha each"));
    }
    crate::gitops::ensure_commit(hub, repo, to).await.map_err(|e| format!("{to} in {}: {e:#}", repo.name))?;
    let path = crate::gitops::ensure_commit(hub, repo, from).await.map_err(|e| format!("{from} in {}: {e:#}", repo.name))?;
    let p = path.to_string_lossy().into_owned();
    let n = format!("-n{}", MAX_COMMITS + 1);
    let range = format!("{from}..{to}");
    let out = crate::gitops::run_git(hub, &["-C", &p, "log", FORMAT, &n, &range], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| format!("git log {range} in {}: {e:#}", repo.name))?;
    Ok(parse_log(&out))
}

/// Git's log as [`FORMAT`] gives it, a commit each; what does not read as one is passed over.
pub fn parse_log(out: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(out)
        .split('\u{1e}')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches(['\n', '\r']).splitn(5, '\u{1f}');
            let sha = f.next()?.trim();
            if !is_sha(sha) {
                return None;
            }
            let author_s = f.next()?.trim().parse::<i64>().ok()?;
            let parents = f.next()?.split_whitespace().count();
            let title = f.next()?.trim().to_owned();
            let reverts = f.next().and_then(reverted_sha);
            Some(Commit { sha: sha.to_ascii_lowercase(), author_s, title, merge: parents > 1, reverts })
        })
        .collect()
}

/// The commit a revert's message names: `This reverts commit <sha>.` (git's own, and GitHub's).
fn reverted_sha(body: &str) -> Option<String> {
    const SAYS: &str = "This reverts commit ";
    let at = body.find(SAYS)?;
    let hex: String = body.get(at + SAYS.len()..)?.chars().take_while(char::is_ascii_hexdigit).collect();
    is_sha(&hex).then(|| hex.to_ascii_lowercase())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn urls_compare_as_one_repository() {
        for u in ["https://github.com/acme/app.git", "https://GitHub.com/acme/app", "git@github.com:acme/app.git", "ssh://git@github.com/acme/app", "https://user:pw@github.com/acme/app/"] {
            assert_eq!(normal_url(u), "github.com/acme/app", "{u}");
        }
        assert_eq!(normal_url("file:///srv/git/app.git"), "/srv/git/app");
        assert_ne!(normal_url("https://gitlab.com/acme/app"), normal_url("https://github.com/acme/app"));
        // A path's case is its own.
        assert_ne!(normal_url("https://github.com/acme/App"), normal_url("https://github.com/acme/app"));
    }

    /// Git's log read: shas, times, merges, titles, the commit a revert names; a record that does not
    /// read is passed over.
    #[test]
    fn a_log_reads_as_its_commits() {
        let a = "a".repeat(40);
        let b = "B".repeat(40);
        let log = format!(
            "{a}\u{1f}1700000000\u{1f}{b}\u{1f}Revert \"Add search\"\u{1f}This reverts commit {}.\n\u{1e}\n{b}\u{1f}1690000000\u{1f}{a} {a}\u{1f}Merge branch 'hotfix/login'\u{1f}\u{1e}\nnot a commit\u{1e}",
            "c".repeat(40)
        );
        let c = parse_log(log.as_bytes());
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].sha.as_str(), c[0].author_s, c[0].merge, c[0].title.as_str()), (a.as_str(), 1_700_000_000, false, "Revert \"Add search\""));
        assert_eq!(c[0].reverts.as_deref(), Some("c".repeat(40).as_str()));
        assert_eq!((c[1].sha.as_str(), c[1].merge, c[1].reverts.as_ref()), ("b".repeat(40).as_str(), true, None));
    }
}
