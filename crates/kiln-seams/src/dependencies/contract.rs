use futures::future::BoxFuture;

use super::*;
use crate::conformance::{Case, Failure, ensure, fine, same};

#[derive(Clone)]
pub struct FixtureCommits {
    pub old: String,
    pub new: String,
    pub foreign: String,
}

pub enum FixtureChange {
    MoveTag,
    ReplaceIdentity,
    Offline,
    Denied,
    ShadowQualifiedRefs,
    MoveRuntime,
}

/// A fresh source with `main` at `new`, `v1` at `old` (an annotated tag on real git),
/// `new` descending from `old`, and an unrelated `foreign` object available in the mirror.
/// Files: action.yml at old/new, and big.txt over FILE_MAX at old. The manifest and runtime
/// below are available. Changes happen upstream, outside the source interface under test.
pub trait DependencySourcesFixture: Send + Sync {
    fn sources(&self) -> Result<&dyn DependencySources, Failure>;
    fn commits(&self) -> Result<FixtureCommits, Failure>;
    fn repository(&self) -> Repository;
    fn change(&self, change: FixtureChange) -> BoxFuture<'_, Result<(), Failure>>;
}

pub fn fixture_repository() -> Repository { Repository { name: "acme/tools".into(), native: true } }
pub const FIXTURE_IMAGE: &str = "registry.test/acme/base:latest";
pub const FIXTURE_OLD: &[u8] = b"name: old\nruns: {using: composite, steps: []}\n";
pub const FIXTURE_NEW: &[u8] = b"name: new\nruns: {using: composite, steps: []}\n";

pub fn fixture_manifest() -> Manifest {
    let bytes = b"{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\"}".to_vec();
    Manifest { digest: format!("sha256:{}", crate::sha256_hex(&bytes)), media_type: "application/vnd.oci.image.manifest.v1+json".into(), bytes }
}

pub fn fixture_runtime() -> Runtime { Runtime { using: "node20".into(), os: "linux".into(), arch: "amd64".into(), musl: false, version: None } }
pub fn fixture_archive() -> Archive {
    Archive { version: "v20.19.5".into(), url: "https://nodejs.org/dist/v20.19.5/node-v20.19.5-linux-x64.tar.gz".into(), file: "node-v20.19.5-linux-x64.tar.gz".into(), sha256: "a".repeat(64) }
}

pub fn fixture_new_archive() -> Archive {
    Archive { version: "v20.19.6".into(), url: "https://nodejs.org/dist/v20.19.6/node-v20.19.6-linux-x64.tar.gz".into(), file: "node-v20.19.6-linux-x64.tar.gz".into(), sha256: "b".repeat(64) }
}

pub struct SimDependencySourcesFixture {
    source: SimDependencySources,
    commits: FixtureCommits,
    made: Result<(), Refusal>,
}

impl Default for SimDependencySourcesFixture {
    fn default() -> Self {
        let source = SimDependencySources::default();
        let commits = FixtureCommits { old: "a".repeat(40), new: "b".repeat(40), foreign: "c".repeat(40) };
        let made = source.put_repository(fixture_repository(), SimRepository {
            identity: Identity::Native { repo_id: "original".into() }, default_branch: "main".into(),
            refs: [("refs/heads/main".into(), commits.new.clone()), ("refs/tags/v1".into(), commits.old.clone())].into(),
            commits: [(commits.old.clone(), vec![]), (commits.new.clone(), vec![commits.old.clone()]), (commits.foreign.clone(), vec![])].into(),
            files: [((commits.old.clone(), "action.yml".into()), FIXTURE_OLD.to_vec()), ((commits.new.clone(), "action.yml".into()), FIXTURE_NEW.to_vec()), ((commits.old.clone(), "big.txt".into()), vec![b'x'; FILE_MAX.saturating_add(1)])].into(),
        }).and_then(|()| {
            let m = fixture_manifest();
            source.put_manifest(FIXTURE_IMAGE.into(), m.clone())?;
            source.put_manifest(format!("registry.test/acme/base@{}", m.digest), m)?;
            source.put_runtime(fixture_runtime(), fixture_archive())?;
            source.put_runtime(Runtime { version: Some(fixture_archive().version), ..fixture_runtime() }, fixture_archive())
        });
        Self { source, commits, made }
    }
}

impl DependencySourcesFixture for SimDependencySourcesFixture {
    fn sources(&self) -> Result<&dyn DependencySources, Failure> { self.made.clone().map_err(|e| e.to_string()).map(|()| &self.source as &dyn DependencySources) }
    fn commits(&self) -> Result<FixtureCommits, Failure> { Ok(self.commits.clone()) }
    fn repository(&self) -> Repository { fixture_repository() }
    fn change(&self, change: FixtureChange) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            let r = fixture_repository();
            let result = match change {
                FixtureChange::MoveTag => self.source.move_ref(&r, "refs/tags/v1", &self.commits.new),
                FixtureChange::ReplaceIdentity => self.source.replace_identity(&r, Identity::Native { repo_id: "replacement".into() }),
                FixtureChange::Offline => self.source.availability(&r, false, false),
                FixtureChange::Denied => self.source.availability(&r, true, true),
                FixtureChange::ShadowQualifiedRefs => self.source.move_ref(&r, "refs/tags/refs/heads/main", &self.commits.old)
                    .and_then(|()| self.source.move_ref(&r, "refs/tags/refs/tags/v1", &self.commits.new))
                    .and_then(|()| self.source.move_ref(&r, &format!("refs/tags/{}", self.commits.old), &self.commits.new)),
                FixtureChange::MoveRuntime => self.source.put_runtime(fixture_runtime(), fixture_new_archive())
                    .and_then(|()| self.source.put_runtime(Runtime { version: Some(fixture_new_archive().version), ..fixture_runtime() }, fixture_new_archive())),
            };
            fine("change upstream", result)
        })
    }
}

fn c1<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let c = f.commits()?;
        let tag = fine("resolve tag", s.resolve(f.repository(), "v1".into()).await)?;
        fine("valid revision", tag.validate())?;
        same("peeled tag", (&tag.commit, tag.reference.as_str(), tag.requested.as_str(), tag.fresh), (&c.old, "refs/tags/v1", "v1", true))?;
        let head = fine("resolve HEAD", s.resolve(f.repository(), "HEAD".into()).await)?;
        same("default branch", (&head.commit, head.reference.as_str(), head.requested.as_str()), (&c.new, "refs/heads/main", "HEAD"))?;
        same("same source identity", head.identity, tag.identity)?;
        f.change(FixtureChange::ShadowQualifiedRefs).await?;
        let branch = fine("qualified branch", s.resolve(f.repository(), "refs/heads/main".into()).await)?;
        same("qualified branch is not shadowed by a tag", (branch.reference.as_str(), branch.commit), ("refs/heads/main", c.new))?;
        let tag = fine("qualified tag", s.resolve(f.repository(), "refs/tags/v1".into()).await)?;
        same("qualified tag is not shadowed by another tag", (tag.reference.as_str(), tag.commit), ("refs/tags/v1", c.old.clone()))?;
        let commit = fine("literal commit", s.resolve(f.repository(), c.old.clone()).await)?;
        same("a tag named as a SHA cannot replace that object", (commit.reference, commit.commit), (c.old.clone(), c.old))
    })
}

fn c2<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let c = f.commits()?;
        let old = fine("resolve tag", s.resolve(f.repository(), "v1".into()).await)?;
        f.change(FixtureChange::MoveTag).await?;
        let new = fine("resolve moved tag", s.resolve(f.repository(), "v1".into()).await)?;
        same("moved tag", (&old.commit, &new.commit), (&c.old, &c.new))?;
        same("captured history", fine("contains", s.contains(old.clone(), c.new).await)?, false)?;
        same("captured file", fine("file", s.file(old, c.old, "action.yml".into()).await)?, FIXTURE_OLD.to_vec())
    })
}

fn c3<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let c = f.commits()?;
        let head = fine("resolve branch", s.resolve(f.repository(), "main".into()).await)?;
        same("ancestor", fine("contains old", s.contains(head.clone(), c.old).await)?, true)?;
        same("head", fine("contains head", s.contains(head.clone(), c.new).await)?, true)?;
        same("foreign object", fine("contains foreign", s.contains(head.clone(), c.foreign).await)?, false)?;
        ensure(matches!(s.contains(head, "d".repeat(40)).await, Err(Refusal::Missing(_))), || "an unknown object was treated as proven history".into())
    })
}

fn c4<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let c = f.commits()?;
        let head = fine("resolve branch", s.resolve(f.repository(), "main".into()).await)?;
        same("old file", fine("file", s.file(head.clone(), c.old.clone(), "action.yml".into()).await)?, FIXTURE_OLD.to_vec())?;
        same("outside history", s.file(head.clone(), c.foreign, "action.yml".into()).await, Err(Refusal::NotReachable))?;
        ensure(matches!(s.file(head.clone(), c.old.clone(), "missing".into()).await, Err(Refusal::Missing(_))), || "a missing file was accepted".into())?;
        same("path traversal", s.file(head, c.old, "../action.yml".into()).await, Err(Refusal::Invalid("file path")))
    })
}

fn c5<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let head = fine("resolve branch", s.resolve(f.repository(), "main".into()).await)?;
        same("oversized file", s.file(head, f.commits()?.old, "big.txt".into()).await, Err(Refusal::TooLarge { what: "file", limit: FILE_MAX }))
    })
}

fn c6<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let old = fine("resolve", s.resolve(f.repository(), "v1".into()).await)?;
        f.change(FixtureChange::ReplaceIdentity).await?;
        same("old source history", s.contains(old.clone(), f.commits()?.old.clone()).await, Err(Refusal::IdentityChanged))?;
        same("old source file", s.file(old.clone(), f.commits()?.old, "action.yml".into()).await, Err(Refusal::IdentityChanged))?;
        let new = fine("new identity", s.resolve(f.repository(), "v1".into()).await)?;
        ensure(new.identity != old.identity, || "the replacement kept the old source identity".into())
    })
}

fn c7<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let old = fine("resolve", s.resolve(f.repository(), "v1".into()).await)?;
        f.change(FixtureChange::Offline).await?;
        let cached = fine("cached resolution", s.resolve(f.repository(), "v1".into()).await)?;
        same("cached, not current upstream evidence", (&cached.commit, &cached.identity, cached.fresh), (&old.commit, &old.identity, false))?;
        same("cached bytes", fine("cached file", s.file(old, f.commits()?.old, "action.yml".into()).await)?, FIXTURE_OLD.to_vec())
    })
}

fn c8<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.change(FixtureChange::Offline).await?;
        ensure(matches!(f.sources()?.resolve(f.repository(), "v1".into()).await, Err(Refusal::Unavailable(_))), || "a source without a mirror answered while offline".into())
    })
}

fn c9<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let old = fine("resolve", s.resolve(f.repository(), "v1".into()).await)?;
        f.change(FixtureChange::Denied).await?;
        same("cached resolve denied", s.resolve(f.repository(), "v1".into()).await, Err(Refusal::Denied))?;
        same("cached file denied", s.file(old, f.commits()?.old, "action.yml".into()).await, Err(Refusal::Denied))
    })
}

fn c10<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        let m = fine("manifest", s.manifest(FIXTURE_IMAGE.into()).await)?;
        fine("actual digest", m.validate(FIXTURE_IMAGE))?;
        same("pinned manifest", fine("pinned", s.manifest(format!("registry.test/acme/base@{}", m.digest)).await)?, m)?;
        same("credential URL refused", s.manifest("https://user:secret@registry.test/base".into()).await, Err(Refusal::Invalid("image reference")))?;
        let r = fixture_runtime();
        let old = fine("runtime", s.runtime(r.clone()).await)?;
        fine("immutable runtime archive", old.validate(&r))?;
        f.change(FixtureChange::MoveRuntime).await?;
        same("new runtime resolution", fine("new runtime", s.runtime(r.clone()).await)?, fixture_new_archive())?;
        same("old version survives a latest change", fine("old runtime", s.runtime(Runtime { version: Some(old.version.clone()), ..r.clone() }).await)?, old)?;
        same("new version is exact too", fine("exact new runtime", s.runtime(Runtime { version: Some(fixture_new_archive().version), ..r.clone() }).await)?, fixture_new_archive())?;
        same("wrong runtime version", s.runtime(Runtime { version: Some("v24.0.0".into()), ..r.clone() }).await, Err(Refusal::Invalid("runtime version")))?;
        same("unsupported runtime", s.runtime(Runtime { using: "node999".into(), ..r }).await, Err(Refusal::Unsupported("runtime")))
    })
}

fn c11<F: DependencySourcesFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.sources()?;
        same("repository URL refused", s.resolve(Repository { name: "https://user:secret@host/repo".into(), native: false }, "main".into()).await, Err(Refusal::Invalid("repository name")))?;
        same("git revision expression refused", s.resolve(f.repository(), "main^{commit}".into()).await, Err(Refusal::Invalid("ref")))?;
        ensure(matches!(s.resolve(f.repository(), "missing".into()).await, Err(Refusal::Missing(_))), || "a missing ref wasn't refused".into())
    })
}

/// The DependencySources contract, clause by clause, also run on kiln's real adapter.
pub fn cases<F: DependencySourcesFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "DEPENDENCIES-1", says: "a ref resolves to its source identity and peeled commit, retaining the request and qualified ref; HEAD names its default branch", check: c1::<F> },
        Case { id: "DEPENDENCIES-2", says: "moving a tag changes a new resolution, while a captured revision keeps its commit, history and file bytes", check: c2::<F> },
        Case { id: "DEPENDENCIES-3", says: "history means ancestry from the captured commit in that source; an unrelated object is not proof, and an unknown object is refused Missing", check: c3::<F> },
        Case { id: "DEPENDENCIES-4", says: "a file is read at the selected commit within captured history; outside history is refused NotReachable, a missing file Missing, and traversal Invalid", check: c4::<F> },
        Case { id: "DEPENDENCIES-5", says: "a file over the byte bound is refused TooLarge before its bytes are copied into an answer", check: c5::<F> },
        Case { id: "DEPENDENCIES-6", says: "a changed repository identity refuses the old revision as IdentityChanged, even when its name and commit remain", check: c6::<F> },
        Case { id: "DEPENDENCIES-7", says: "an unreachable upstream can answer from a prior mirror, explicitly marked not fresh, preserving its identity and bytes", check: c7::<F> },
        Case { id: "DEPENDENCIES-8", says: "an unreachable source without a prior mirror is refused Unavailable", check: c8::<F> },
        Case { id: "DEPENDENCIES-9", says: "access denial refuses cached resolutions and files as Denied", check: c9::<F> },
        Case { id: "DEPENDENCIES-10", says: "a manifest is bound to its bytes and requested digest, and runtime metadata names an immutable archive and SHA-256; credentials and unsupported runtimes are refused", check: c10::<F> },
        Case { id: "DEPENDENCIES-11", says: "repository URLs and git revision expressions are refused Invalid, and a missing ref is refused Missing", check: c11::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r23_simulated_sources_pass_the_source_contract() -> Result<(), String> {
        futures::executor::block_on(crate::conformance::run("DependencySources", &cases::<SimDependencySourcesFixture>(), || async { SimDependencySourcesFixture::default() })).into_result().map(|_| ())
    }

    #[test]
    fn w29_r23_source_tables_refuse_overflow_without_replacing_existing_data() -> Result<(), Refusal> {
        let s = SimDependencySources::default();
        for i in 0..SIM_REPOSITORIES {
            s.put_repository(Repository { name: format!("acme/r{i}"), native: true }, SimRepository {
                identity: Identity::Native { repo_id: format!("id-{i}") }, refs: BTreeMap::new(), default_branch: "main".into(), commits: BTreeMap::new(), files: BTreeMap::new(),
            })?;
        }
        let extra = SimRepository { identity: Identity::Native { repo_id: "extra".into() }, refs: BTreeMap::new(), default_branch: "main".into(), commits: BTreeMap::new(), files: BTreeMap::new() };
        assert_eq!(s.put_repository(Repository { name: "acme/extra".into(), native: true }, extra.clone()), Err(Refusal::Full { what: "repositories", limit: SIM_REPOSITORIES }));
        s.put_repository(Repository { name: "acme/r0".into(), native: true }, extra)?;
        let mut archive = fixture_archive();
        archive.url = archive.url.replace("/v20.19.5/", "/latest-v20.x/");
        assert_eq!(archive.validate(&fixture_runtime()), Err(Refusal::Invalid("runtime archive metadata")));
        let mut manifest = fixture_manifest();
        manifest.bytes.push(b' ');
        assert_eq!(s.put_manifest(FIXTURE_IMAGE.into(), manifest), Err(Refusal::Integrity("manifest")));
        Ok(())
    }
}
