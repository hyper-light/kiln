//! W29-R25: derive both locks from merged workflow bytes. Only objects in kiln's own Git
//! repositories and caches are written here; no branch, review state or accepted history moves.

use kiln_ci::lock::{kiln, replay, regenerate};

use super::*;

pub(crate) fn lock_path(path: &str) -> bool { matches!(path, github::PATH | kiln::PATH) }

struct Raw { github: Option<Vec<u8>>, kiln: Option<Vec<u8>> }
struct Parsed { github: Option<github::File>, kiln: Option<kiln::File> }

impl Raw {
    async fn read(hub: &Hub, path: &Path, commit: &str) -> Result<Self, String> {
        Ok(Self { github: blob(hub, path, None, commit, github::PATH).await?, kiln: blob(hub, path, None, commit, kiln::PATH).await? })
    }

    fn any(&self) -> bool { self.github.is_some() || self.kiln.is_some() }

    fn parse(self) -> Result<Parsed, String> {
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).map_err(|_| "KCI1022 a replay lock is not UTF-8".to_owned());
        Ok(Parsed {
            github: self.github.map(text).transpose()?.map(|s| github::parse(&s).map_err(|ds| ds.iter().map(|d| d.render(github::PATH)).collect::<Vec<_>>().join("\n"))).transpose()?,
            kiln: self.kiln.map(text).transpose()?.map(|s| kiln::parse(&s).map_err(|ds| ds.iter().map(|d| d.render(kiln::PATH)).collect::<Vec<_>>().join("\n"))).transpose()?,
        })
    }
}

impl Parsed {
    fn pair(&self) -> replay::Pair<'_> { replay::Pair { github: self.github.as_ref(), kiln: self.kiln.as_ref() } }
}

/// The workflow/source merge has succeeded, or only the two generated lockfiles conflict.
/// `base`, `onto` and `change` are clean commits, so conflict markers never become source facts.
pub(crate) async fn regenerate_tree(hub: &Hub, path: &str, base: &str, onto: &str, change: &str, tree: &str) -> Result<String, String> {
    let changed_commit = change.to_owned();
    let (base, onto_locks, change) = (Raw::read(hub, Path::new(path), base).await?, Raw::read(hub, Path::new(path), onto).await?, Raw::read(hub, Path::new(path), change).await?);
    if !base.any() && !onto_locks.any() && !change.any() { return Ok(tree.to_owned()); }
    let repo = hub.store.repos().await.map_err(|_| "KCI4051 replay repository could not be read")?.into_iter().find(|r| crate::gitops::git_dir(hub, r) == Path::new(path)).ok_or("KCI4051 replay lock owner could not be identified")?;
    let (base, onto_locks, change) = (base.parse()?, onto_locks.parse()?, change.parse()?);
    let home = Owner { repository: repo.name.clone(), commit: onto.into(), remote: None, revision: None }.home_host(hub, &repo);
    let reviewed = super::approvals::seeds(hub, &repo, &changed_commit).await?;
    let seed = replay::accepted(replay::Input { base: base.pair(), onto: onto_locks.pair(), change: change.pair(), reviewed: replay::Pair { github: reviewed.github.as_ref(), kiln: reviewed.kiln.as_ref() }, home_host: &home }).map_err(|ds| ds.iter().map(|d| d.render(kiln::PATH)).collect::<Vec<_>>().join("\n"))?;
    let github = seed.github.map(|f| f.write(&home).map(String::into_bytes)).transpose().map_err(|ds| ds.iter().map(|d| d.render(github::PATH)).collect::<Vec<_>>().join("\n"))?;
    let native = seed.kiln.map(|f| f.write().map(String::into_bytes)).transpose().map_err(|ds| ds.iter().map(|d| d.render(kiln::PATH)).collect::<Vec<_>>().join("\n"))?;
    let edits = [(github::PATH.into(), github), (kiln::PATH.into(), native)].into();
    // A fixed-date, unreferenced commit lets the existing immutable loader inspect the tree.
    // Its metadata is never published and never enters either generated lockfile.
    let env = [("GIT_AUTHOR_NAME", "kiln"), ("GIT_AUTHOR_EMAIL", "kiln@localhost"), ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost"), ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")];
    let made = super::super::diffs::git_run(hub, &["-C", path, "commit-tree", tree, "-p", onto, "-m", "workflow lock replay"], &env, Vec::new()).await.map_err(|_| "KCI4051 merged workflow tree could not be captured")?;
    if !made.ok { return Err("KCI4051 merged workflow tree could not be captured".into()); }
    let commit = String::from_utf8(made.out).map_err(|_| "KCI4051 replay commit does not read")?.trim().to_owned();
    let mirror = crate::gitops::ensure_commit(hub, &repo, onto).await.map_err(|_| "KCI4051 replay base is unavailable")?;
    if mirror != Path::new(path) {
        crate::gitops::cache_commit_from(hub, &repo, Path::new(path), &commit).await.map_err(|_| "KCI4051 merged workflow tree could not be cached")?;
    }
    let source = super::super::dependency_sources::Sources::borrowed(hub, repo.name.clone());
    let files = super::generate::collect(hub, &source, &repo, &commit, edits, None, regenerate::Mode::Preserve).await?;
    let puts = [(github::PATH, files.github), (kiln::PATH, files.kiln)].into_iter().map(|(path, text)| (path.to_owned(), "100644".to_owned(), text.map(String::into_bytes))).collect();
    super::super::diffs::tree_with(hub, path, tree, super::super::diffs::Edit::Changes(puts)).await
}
