//! W29-R23: the shared source contract on real git (including an annotated and moved tag),
//! with HTTP fixtures at the actual host/registry boundary. No live host is contacted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use kiln_seams::dependencies::contract::{self as contract, FIXTURE_IMAGE, FIXTURE_NEW, FIXTURE_OLD, fixture_archive, fixture_manifest};
use kiln_seams::dependencies::{DependencySourcesFixture, FixtureChange, FixtureCommits};
use crate::http::server::{self as srv, Request, Response};

use super::*;

#[derive(Default)]
struct Host {
    offline: AtomicBool,
    denied: AtomicBool,
    redirected: AtomicBool,
    repo_id: AtomicI64,
    runtime_moved: AtomicBool,
    methods: Mutex<Vec<String>>,
    auth: Mutex<Vec<(String, bool)>>,
}

impl srv::Handler for Host {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        self.methods.lock().push(req.method().to_string());
        let path = req.uri().path();
        self.auth.lock().push((path.to_owned(), req.headers().contains_key(http::header::AUTHORIZATION)));
        if path == "/repos/acme/tools" {
            if self.denied.load(Ordering::SeqCst) { return srv::text(http::StatusCode::FORBIDDEN, "private"); }
            if self.offline.load(Ordering::SeqCst) { return srv::text(http::StatusCode::SERVICE_UNAVAILABLE, "offline"); }
            if self.redirected.load(Ordering::SeqCst) { return srv::text(http::StatusCode::MOVED_PERMANENTLY, "moved"); }
            return srv::json(http::StatusCode::OK, &serde_json::json!({ "owner": { "id": 41 }, "id": self.repo_id.load(Ordering::SeqCst) }));
        }
        // W29-S04's fixture changed-files, a repository of its own.
        if path == "/repos/acme/changed-files" { return srv::json(http::StatusCode::OK, &serde_json::json!({ "owner": { "id": 41 }, "id": 43 })); }
        if matches!(path, "/SHASUMS256.txt" | "/v20.19.5/SHASUMS256.txt" | "/v20.19.6/SHASUMS256.txt") {
            let a = if path.starts_with("/v20.19.6/") || path == "/SHASUMS256.txt" && self.runtime_moved.load(Ordering::SeqCst) { contract::fixture_new_archive() } else { fixture_archive() };
            return srv::text(http::StatusCode::OK, &format!("{}  {}\n", a.sha256, a.file));
        }
        if path.starts_with("/v2/acme/base/manifests/") {
            let m = fixture_manifest();
            return srv::response(http::StatusCode::OK, &m.media_type, srv::full(m.bytes));
        }
        srv::text(http::StatusCode::NOT_FOUND, "missing")
    }
}

async fn serve(host: Arc<Host>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::rt::spawn(async move {
        while let Ok(stream) = tcp.accept().await {
            let (h, peer) = (Arc::clone(&host), stream.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(stream), h, peer));
        }
    });
    Ok(format!("http://{addr}"))
}

pub(crate) async fn git(hub: &Hub, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = crate::gitops::git(hub);
    cmd.current_dir(dir).args(["-c", "user.name=Lock fixture", "-c", "user.email=lock@example.test", "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"]).args(args);
    let output = crate::proc::output(cmd, WITHIN).await?;
    anyhow::ensure!(output.status.success(), "fixture git failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) struct Live {
    pub(crate) sources: Sources,
    pub(crate) commits: FixtureCommits,
    pub(crate) origin: PathBuf,
    host: Arc<Host>,
    _dir: tempfile::TempDir,
}

impl Live {
    pub(crate) fn hub(&self) -> Arc<Hub> { Arc::clone(&self.sources.hub) }
    pub(crate) fn root(&self) -> &Path { self._dir.path() }

    pub(crate) async fn for_locks() -> anyhow::Result<Self> {
        Self::for_locks_with(|_| {}).await
    }

    /// As [`Live::for_locks`], with more of the server's configuration (a notification rule).
    pub(crate) async fn for_locks_with(more: impl FnOnce(&mut crate::config::Config) + Send + 'static) -> anyhow::Result<Self> {
        let mut live = Self::with(false, more).await?;
        if let Some(e) = &mut live.sources.endpoints { e.host = "github.com".into(); }
        Ok(live)
    }

    /// The fixture's hosts for every reader the server makes for itself, as its draft writer does.
    pub(crate) fn share(&self) {
        if let Some(e) = &self.sources.endpoints { super::SHARED.lock().push((Arc::downgrade(&self.sources.hub), e.clone())); }
    }

    async fn new() -> anyhow::Result<Self> {
        Self::with_token(false).await
    }

    async fn with_token(credential: bool) -> anyhow::Result<Self> {
        Self::with(credential, |_| {}).await
    }

    async fn with(credential: bool, more: impl FnOnce(&mut crate::config::Config) + Send + 'static) -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let pattern = root.join("{repo}.git").to_string_lossy().into_owned();
        let token_file = root.join("source-token");
        let token_copy = token_file.clone();
        if credential { crate::rt::blocking(move || std::fs::write(token_copy, "source-fixture-token")).await??; }
        let hub = crate::hub::tests::scratch_hub_with(|cfg| {
            cfg.ci.enabled = true;
            cfg.ci.actions.sources.push(crate::config::ci::ActionSource { owners: vec!["acme".into()], url: Some(pattern), native: false });
            cfg.gitops.github_token_file = credential.then_some(token_file);
            more(cfg);
        }).await?;
        let work = root.join("work");
        let work_copy = work.clone();
        crate::rt::blocking(move || std::fs::create_dir_all(work_copy)).await??;
        git(&hub, &work, &["init", "-q", "-b", "main"]).await?;
        let w = work.clone();
        crate::rt::blocking(move || -> anyhow::Result<()> {
            std::fs::write(w.join("action.yml"), FIXTURE_OLD)?;
            std::fs::write(w.join("big.txt"), vec![b'x'; dep::FILE_MAX.saturating_add(1)])?;
            Ok(())
        }).await??;
        git(&hub, &work, &["add", "."]).await?;
        git(&hub, &work, &["commit", "-qm", "old"]).await?;
        let old = git(&hub, &work, &["rev-parse", "HEAD"]).await?;
        git(&hub, &work, &["tag", "-am", "annotated v1", "v1"]).await?;
        let w = work.clone();
        crate::rt::blocking(move || std::fs::write(w.join("action.yml"), FIXTURE_NEW)).await??;
        git(&hub, &work, &["commit", "-qam", "new"]).await?;
        let new = git(&hub, &work, &["rev-parse", "HEAD"]).await?;
        // A commit object exists, with no ancestry from main: a fork object is no proof.
        let foreign = git(&hub, &work, &["commit-tree", &format!("{old}^{{tree}}"), "-m", "foreign"]).await?;
        let origin = root.join("tools.git");
        git(&hub, &root, &["clone", "-q", "--bare", &work.to_string_lossy(), &origin.to_string_lossy()]).await?;
        let host = Arc::new(Host { repo_id: AtomicI64::new(42), ..Default::default() });
        let base = serve(Arc::clone(&host)).await?;
        let mut sources = Sources::new(hub, "acme/app".into());
        sources.endpoints = Some(TestEndpoints { host: "github.test".into(), api: base.clone(), registry: base.clone(), node: format!("{base}/SHASUMS256.txt") });
        Ok(Self { sources, commits: FixtureCommits { old, new, foreign }, origin, host, _dir: dir })
    }
}

struct Fixture(Result<Live, String>);

impl Fixture {
    fn live(&self) -> Result<&Live, String> { self.0.as_ref().map_err(Clone::clone) }
}

impl DependencySourcesFixture for Fixture {
    fn sources(&self) -> Result<&dyn DependencySources, String> { Ok(&self.live()?.sources) }
    fn commits(&self) -> Result<FixtureCommits, String> { Ok(self.live()?.commits.clone()) }
    fn repository(&self) -> Repository { Repository { name: "acme/tools".into(), native: false } }
    fn change(&self, change: FixtureChange) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let live = self.live()?;
            match change {
                FixtureChange::MoveTag => { git(&live.sources.hub, &live.origin, &["tag", "-fam", "moved", "v1", &live.commits.new]).await.map_err(|e| e.to_string())?; },
                FixtureChange::ReplaceIdentity => { live.host.repo_id.store(99, Ordering::SeqCst); },
                FixtureChange::Denied => { live.host.denied.store(true, Ordering::SeqCst); },
                FixtureChange::MoveRuntime => { live.host.runtime_moved.store(true, Ordering::SeqCst); },
                FixtureChange::ShadowQualifiedRefs => {
                    git(&live.sources.hub, &live.origin, &["tag", "-am", "shadow branch spelling", "refs/heads/main", &live.commits.old]).await.map_err(|e| e.to_string())?;
                    git(&live.sources.hub, &live.origin, &["tag", "-am", "shadow tag spelling", "refs/tags/v1", &live.commits.new]).await.map_err(|e| e.to_string())?;
                    git(&live.sources.hub, &live.origin, &["tag", "-am", "shadow commit spelling", &live.commits.old, &live.commits.new]).await.map_err(|e| e.to_string())?;
                }
                FixtureChange::Offline => {
                    live.host.offline.store(true, Ordering::SeqCst);
                    let origin = live.origin.clone();
                    crate::rt::blocking(move || std::fs::rename(&origin, origin.with_extension("away"))).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        })
    }
}

#[test]
fn w29_r23_real_sources_pass_the_dependency_source_contract() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let report = kiln_seams::conformance::run_within("DependencySources (kiln)", &dep::cases::<Fixture>(), || async { Fixture(Live::new().await.map_err(|e| format!("{e:#}"))) }, || crate::rt::sleep(Duration::from_secs(60))).await;
        anyhow::ensure!(report.is_ok(), "{:?}", report.failed);
        Ok(())
    })
}

#[test]
fn w29_r25_whole_source_comparison_keeps_binary_modes_symlinks_and_bounds() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    crate::rt::block_on(async {
        let live = Live::new().await?;
        let hub = live.hub();
        let repo = Repository { name: "acme/tools".into(), native: false };
        let before = live.sources.resolve(repo.clone(), "main".into()).await?;
        let work = live.root().join("work");
        let files = work.clone();
        crate::rt::blocking(move || -> anyhow::Result<()> {
            std::fs::create_dir_all(files.join("src"))?;
            std::fs::write(files.join("src/index.js"), b"runNewCode();\n")?;
            std::fs::write(files.join("data.bin"), [0, 255, 42])?;
            std::fs::set_permissions(files.join("action.yml"), std::fs::Permissions::from_mode(0o755))?;
            std::os::unix::fs::symlink("src/index.js", files.join("link"))?;
            std::fs::write(files.join(".gitattributes"), "*.js diff=hidden\n")?;
            Ok(())
        }).await??;
        git(&hub, &work, &["add", "."]).await?;
        git(&hub, &work, &["commit", "-qm", "changed code outside action metadata"]).await?;
        git(&hub, &live.origin, &["fetch", "--quiet", &work.to_string_lossy(), "HEAD:refs/heads/main"]).await?;
        let after = live.sources.resolve(repo.clone(), "main".into()).await?;
        let remote = live.sources.remote(&repo, "main")?;
        let mirror = remote.mirror(&hub);
        git(&hub, &mirror, &["config", "diff.hidden.command", "does-not-exist"]).await?;
        let diff = live.sources.compare(before.clone(), after).await?;
        assert_eq!(diff.files.len(), 5);
        assert!(diff.files.iter().any(|f| f.path == "src/index.js" && f.after.as_ref().is_some_and(|b| b.bytes == b"runNewCode();\n")));
        assert!(diff.files.iter().any(|f| f.path == "data.bin" && f.after.as_ref().is_some_and(|b| b.bytes == [0, 255, 42])));
        assert!(diff.files.iter().any(|f| f.path == "link" && f.after.as_ref().is_some_and(|b| b.mode == 0o120000 && b.bytes == b"src/index.js")));
        assert!(diff.files.iter().any(|f| f.path == "action.yml" && f.before.as_ref().is_some_and(|b| b.mode == 0o100644) && f.after.as_ref().is_some_and(|b| b.mode == 0o100755)));
        let files = work.clone();
        crate::rt::blocking(move || std::fs::write(files.join("oversized.bin"), vec![0; dep::DIFFERENCE_MAX.saturating_add(1)])).await??;
        git(&hub, &work, &["add", "."]).await?;
        git(&hub, &work, &["commit", "-qm", "comparison over byte bound"]).await?;
        git(&hub, &live.origin, &["fetch", "--quiet", &work.to_string_lossy(), "HEAD:refs/heads/main"]).await?;
        let after = live.sources.resolve(repo, "main".into()).await?;
        assert_eq!(live.sources.compare(before, after).await, Err(Refusal::TooLarge { what: "code difference", limit: dep::DIFFERENCE_MAX }));
        Ok(())
    })
}

#[test]
fn w29_r23_sources_never_write_the_host_and_return_no_transport_credentials() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::new().await?;
        let r = Repository { name: "acme/tools".into(), native: false };
        let revision = live.sources.resolve(r, "v1".into()).await?;
        assert_eq!(revision.identity, Identity::Github { hostname: "github.test".into(), owner_id: 41, repo_id: 42 });
        let manifest = live.sources.manifest(FIXTURE_IMAGE.into()).await?;
        manifest.validate(FIXTURE_IMAGE)?;
        let runtime = live.sources.runtime(contract::fixture_runtime()).await?;
        assert!(live.host.methods.lock().iter().all(|m| m == "GET"));
        let facts = serde_json::to_string(&(revision, manifest, runtime))?;
        assert!(!facts.contains("127.0.0.1") && !facts.contains("Authorization") && !facts.contains("token"));
        live.host.redirected.store(true, Ordering::SeqCst);
        // An explicit redirect isn't an offline proof: never reuse the old owner's ids for it.
        assert_eq!(live.sources.resolve(Repository { name: "acme/tools".into(), native: false }, "v1".into()).await, Err(Refusal::Invalid("repository identity response")));
        Ok(())
    })
}

#[test]
fn w29_r23_an_alias_gets_identity_from_its_actual_source_without_url_credentials() -> Result<(), Refusal> {
    assert_eq!(github_source("https://test-reader:test-password@github.com/publisher/tool.git")?, ("github.com".into(), "publisher/tool".into()));
    assert_eq!(github_source("https://github.com/extra/path/tool.git"), Err(Refusal::Invalid("source repository")));
    Ok(())
}

#[test]
fn w29_r23_github_credentials_stay_with_their_host_and_out_of_runtime_and_registry_reads() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let mut live = Live::with_token(true).await?;
        if let Some(e) = &mut live.sources.endpoints { e.host = "github.com".into(); }
        let repo = Repository { name: "acme/tools".into(), native: false };
        let revision = live.sources.resolve(repo.clone(), "v1".into()).await?;
        assert!(live.host.auth.lock().iter().any(|(p, auth)| p == "/repos/acme/tools" && *auth));
        live.sources.manifest(FIXTURE_IMAGE.into()).await?;
        live.sources.runtime(contract::fixture_runtime()).await?;
        assert!(live.host.auth.lock().iter().filter(|(p, _)| p != "/repos/acme/tools").all(|(_, auth)| !auth));
        live.host.auth.lock().clear();
        let mut other = Sources::new(Arc::clone(&live.sources.hub), "acme/app".into());
        other.endpoints = live.sources.endpoints.clone().map(|e| TestEndpoints { host: "other.github.test".into(), ..e });
        other.resolve(repo, "v1".into()).await?;
        assert!(live.host.auth.lock().iter().all(|(_, auth)| !auth));
        assert!(!serde_json::to_string(&revision)?.contains("source-fixture-token"));
        Ok(())
    })
}

#[test]
fn w29_r23_a_new_reader_can_prove_an_offline_mirror_with_its_recorded_identity() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let fixture = Fixture(Ok(Live::new().await?));
        let live = fixture.live().map_err(anyhow::Error::msg)?;
        let revision = live.sources.resolve(fixture.repository(), "v1".into()).await?;
        fixture.change(FixtureChange::Offline).await.map_err(anyhow::Error::msg)?;
        let mut reopened = Sources::new(Arc::clone(&live.sources.hub), "acme/app".into());
        reopened.endpoints = live.sources.endpoints.clone();
        let cached = reopened.resolve(fixture.repository(), "v1".into()).await?;
        assert_eq!((&cached.identity, &cached.commit, cached.fresh), (&revision.identity, &revision.commit, false));
        assert_eq!(reopened.file(cached, live.commits.old.clone(), "action.yml".into()).await?, FIXTURE_OLD);
        // The same directory is not evidence for a different configured host.
        let mut changed = Sources::new(Arc::clone(&live.sources.hub), "acme/app".into());
        changed.endpoints = live.sources.endpoints.clone().map(|e| TestEndpoints { host: "other.github.test".into(), ..e });
        assert!(matches!(changed.resolve(fixture.repository(), "v1".into()).await, Err(Refusal::Unavailable(_))));
        Ok(())
    })
}

#[test]
fn w29_r23_native_identity_and_sharing_are_checked_before_cached_files() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::new().await?;
        let hub = &live.sources.hub;
        let publisher = hub.store.upsert_repo(crate::store::Repo {
            id: "native-publisher".into(), name: "acme/tools".into(), clone_url: "unused".into(), branches: vec!["main".into()], poll_seconds: 60,
            enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: Some("main".into()), mirror: None,
        }).await?;
        let native = super::super::hosting::path_of(hub, &publisher.name).ok_or_else(|| anyhow::anyhow!("no native path"))?;
        let parent = native.parent().ok_or_else(|| anyhow::anyhow!("no native parent"))?.to_path_buf();
        crate::rt::blocking(move || std::fs::create_dir_all(parent)).await??;
        git(hub, live._dir.path(), &["clone", "-q", "--bare", &live.origin.to_string_lossy(), &native.to_string_lossy()]).await?;
        let own = Sources::new(Arc::clone(hub), publisher.name.clone());
        let r = Repository { name: publisher.name, native: true };
        let revision = own.resolve(r, "v1".into()).await?;
        assert_eq!(revision.identity, Identity::Native { repo_id: publisher.id });
        assert_eq!(own.file(revision.clone(), live.commits.old.clone(), "action.yml".into()).await?, FIXTURE_OLD);
        let other = Sources::new(Arc::clone(hub), "another/app".into());
        assert_eq!(other.file(revision, live.commits.old, "action.yml".into()).await, Err(Refusal::Denied));
        assert_eq!(other.resolve(Repository { name: "acme/tools".into(), native: true }, "kiln/draft/hidden".into()).await, Err(Refusal::Denied));
        Ok(())
    })
}
