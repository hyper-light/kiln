//! A revision kiln commits for someone (docs/ci.md W26-R04, R10, R12): a suggestion applied, a
//! conflict resolved. Every such commit goes through [`commit`], which signs it as kiln's server
//! (W27-R03, [`sign_as_kiln`]): it's made by kiln, whoever asked for it, and whoever asked signs
//! their own act (a verdict, an acceptance). [`Made`] names the signer; the machine's own git
//! configuration never signs it with a key that isn't kiln's. [`record`] keeps the revision's why
//! with its version.

use rusqlite::params;

use crate::hub::Hub;

/// A commit kiln makes for someone.
pub(crate) struct Commit<'a> {
    pub tree: &'a str,
    /// Its parent; a merge's two, the change's head first.
    pub parents: &'a [&'a str],
    /// Its author (`Name <email>`, or a principal) and, on an amend, the date git keeps for it
    /// (`GIT_AUTHOR_DATE`).
    pub author: (&'a str, Option<&'a str>),
    /// Whoever makes it, as the audit names them (`user:ada`, `agent:review for user:ada`): its
    /// committer, and whom W27-R03 signs it as.
    pub by: &'a str,
    /// Who else wrote it (a suggestion's suggester), principals: `Co-authored-by` trailers, GitHub's
    /// way. None for its author, or twice.
    pub co_authors: &'a [&'a str],
    pub message: &'a str,
}

/// A commit made: its SHA, its message as committed (its co-authors' trailers in it), and who
/// signed it (`None`: unsigned, until W27-R03 signs it).
#[derive(Debug, Clone)]
pub(crate) struct Made {
    pub sha: String,
    pub message: String,
    pub signer: Option<String>,
}

/// `Name <email>` for a principal as kiln writes them (as [`super::diffs::make_commit`] names
/// people): `user:ada` (or a token of hers, `pat:… of user:ada`) is `ada <ada@kiln>`; `agent:review
/// for user:ada` is `agent:review <agent-review@kiln>`, the agent itself (W27 names its person in its
/// signature). `Name <email>` stays as it is.
pub(crate) fn ident(principal: &str) -> String {
    let p = principal.trim();
    if p.contains('<') {
        return p.to_owned();
    }
    let p = p.rsplit(" of ").next().unwrap_or(p);
    let p = p.split(" for ").next().unwrap_or(p).trim();
    let name = p.strip_prefix("user:").unwrap_or(p);
    format!("{name} <{}@kiln>", name.replace([' ', ':'], "-"))
}

/// Makes `c` in the repository at `path`: its author, its committer (whoever made it), its
/// co-authors' trailers, unsigned (`--no-gpg-sign`) until W27-R03 signs it as its committer.
pub(crate) async fn commit(hub: &Hub, path: &str, c: &Commit<'_>) -> Result<Made, String> {
    let (author, committer) = (ident(c.author.0), ident(c.by));
    let co: Vec<String> = c.co_authors.iter().map(|p| ident(p)).collect();
    let message = kiln_ci::suggest::with_co_authors(c.message, &author, &co);
    let split = |who: &str| -> (String, String) {
        let (name, email) = who.rsplit_once('<').unwrap_or((who, ""));
        (name.trim().to_owned(), email.trim_end_matches('>').trim().to_owned())
    };
    let ((an, ae), (cn, ce)) = (split(&author), split(&committer));
    let mut env = vec![("GIT_AUTHOR_NAME", an.as_str()), ("GIT_AUTHOR_EMAIL", ae.as_str()), ("GIT_COMMITTER_NAME", cn.as_str()), ("GIT_COMMITTER_EMAIL", ce.as_str())];
    if let Some(d) = c.author.1.filter(|d| !d.is_empty()) {
        env.push(("GIT_AUTHOR_DATE", d));
    }
    let mut args = vec!["-C", path, "commit-tree", "--no-gpg-sign", c.tree];
    for p in c.parents {
        args.extend(["-p", p]);
    }
    args.extend(["-F", "-"]);
    let r = super::diffs::git_run(hub, &args, &env, message.as_bytes().to_vec()).await.map_err(|e| format!("{e:#}"))?;
    if !r.ok {
        return Err(String::from_utf8_lossy(&r.err).trim().to_owned());
    }
    let unsigned = String::from_utf8_lossy(&r.out).trim().to_owned();
    match sign_as_kiln(hub, path, &unsigned, "server").await {
        Ok((sha, signer)) => Ok(Made { sha, message, signer: Some(signer) }),
        Err(e) => {
            tracing::warn!(commit = %unsigned, error = %format!("{e:#}"), "a commit kiln made isn't signed");
            Ok(Made { sha: unsigned, message, signer: None })
        }
    }
}

/// Signs commit `sha` of the repository at `path` as kiln's `kind` (`server`, or `cd`), W27-R03:
/// the commit written again with its `gpgsig` header in git's x509 form, its new SHA and the
/// signer's identity. The key stays in its file ([`crate::identities::sign_own`]).
pub(crate) async fn sign_as_kiln(hub: &Hub, path: &str, sha: &str, kind: &str) -> anyhow::Result<(String, String)> {
    let raw = super::diffs::git_run(hub, &["-C", path, "cat-file", "commit", sha], &[], Vec::new()).await?;
    anyhow::ensure!(raw.ok, "git cat-file: {}", String::from_utf8_lossy(&raw.err).trim());
    let signed = crate::identities::signed_commit(&raw.out, |m| crate::identities::sign_own(&hub.pki, kind, m))?;
    let w = super::diffs::git_run(hub, &["-C", path, "hash-object", "-t", "commit", "-w", "--stdin"], &[], signed).await?;
    anyhow::ensure!(w.ok, "git hash-object: {}", String::from_utf8_lossy(&w.err).trim());
    let actor = if kind == "cd" { kiln_ci::identity::chain::Actor::Cd } else { kiln_ci::identity::chain::Actor::Server };
    let signer = kiln_ci::identity::chain::Identity::new(hub.pki.organization(), actor).uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((String::from_utf8_lossy(&w.out).trim().to_owned(), signer))
}

/// Why a revision exists (W26-R04), kept with its version.
pub(crate) struct Why<'a> {
    /// `suggestion applied`, `conflict resolved`.
    pub why: &'a str,
    /// The thread it answers.
    pub thread: Option<&'a str>,
    /// A suggestion's comment, and which of the comment's suggestions it was.
    pub comment: Option<(&'a str, u32)>,
    /// The note its version shows, where it has none yet.
    pub note: Option<&'a str>,
}

/// Records why version `n` of diff `diff_id` exists, if its commit is `commit` (once made, by
/// [`super::diffs::add_version`] or [`super::cr::submit`]): whether it did.
pub(crate) async fn record(hub: &Hub, diff_id: &str, n: u32, commit: &str, why: &Why<'_>) -> anyhow::Result<bool> {
    let (id, sha, w, thread) = (diff_id.to_owned(), commit.to_owned(), why.why.to_owned(), why.thread.map(str::to_owned));
    let (comment, which, note) = (why.comment.map(|(c, _)| c.to_owned()), why.comment.map(|(_, i)| i), why.note.map(str::to_owned));
    hub.store
        .call(move |c| {
            let changed = c.execute(
                "UPDATE diff_versions SET why = ?4, thread = ?5, comment = ?6, suggestion = ?7, note = COALESCE(note, ?8) WHERE diff_id = ?1 AND n = ?2 AND commit_sha = ?3",
                params![id, n, sha, w, thread, comment, which, note],
            )?;
            Ok(changed > 0)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w26_r10_a_revisions_people_by_name() {
        assert_eq!(ident("user:ada"), "ada <ada@kiln>");
        assert_eq!(ident("pat:3f2a of user:ada"), "ada <ada@kiln>");
        assert_eq!(ident("agent:review for user:ada"), "agent:review <agent-review@kiln>");
        assert_eq!(ident("agent:review"), "agent:review <agent-review@kiln>");
        assert_eq!(ident("Sam Doe <sam@example.com>"), "Sam Doe <sam@example.com>");
        assert_eq!(ident("admin"), "admin <admin@kiln>");
    }
}
