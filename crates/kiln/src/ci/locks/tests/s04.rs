//! W29-S04 (docs/ci.md W29-AC24): the tj-actions replay on real git. A fixture changed-files'
//! `v45` and `v45.0.7` move from A to M after its consumers ran on A. In every step of the
//! scenario no queued leg is given M: an allowed run stays on A, and a refused one starts no job.

use std::collections::HashSet;

use kiln_ci::lock::kiln as native_format;

use super::*;

const CF: &str = "acme/changed-files";
const PARTIAL: &str = ".github/workflows/partial.yml";
const FULL: &str = ".github/workflows/full.yml";
const NATIVE: &str = ".kiln/ci.yaml";

fn workflow(uses: &str) -> String {
    format!("on: push\njobs:\n  check:\n    runs-on: baremetal\n    steps:\n      - uses: {uses}\n")
}

/// The fixture: Live's history as acme/changed-files, `v45` and `v45.0.7` both at A.
async fn changed_files(live: &Live) -> anyhow::Result<std::path::PathBuf> {
    let hub = live.hub();
    let origin = live.root().join("changed-files.git");
    git(&hub, live.root(), &["clone", "-q", "--bare", &live.origin.to_string_lossy(), &origin.to_string_lossy()]).await?;
    for tag in ["v45", "v45.0.7"] {
        git(&hub, &origin, &["tag", "-am", &format!("release {tag}"), tag, &live.commits.old]).await?;
    }
    Ok(origin)
}

/// The attack: both tags moved to `to`, as CVE-2025-30066's were.
async fn move_tags(hub: &Hub, origin: &Path, to: &str) -> anyhow::Result<()> {
    for tag in ["v45", "v45.0.7"] {
        git(hub, origin, &["tag", "-fam", "moved", tag, to]).await?;
    }
    Ok(())
}

/// GitHub's lock of each workflow, its one use changed-files at its ref, locked to `commit`.
fn github_lock(paths: &[(&str, &str)], commit: &str) -> anyhow::Result<String> {
    let mut file = github::File::default();
    for (path, reference) in paths {
        let pin = format!("{CF}@{reference}");
        file.workflows.insert((*path).into(), vec![pin.clone()]);
        file.dependencies.insert(pin, github::Action { git_ref: (*reference).into(), commit: format!("sha1-{commit}"), owner_id: 41, repo_id: 43, ..Default::default() });
    }
    file.write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// kiln's lock of a `.kiln/` workflow using changed-files at `reference`, locked to `commit`.
fn kiln_lock(path: &str, reference: &str, commit: &str) -> anyhow::Result<String> {
    let key = format!("{CF}@{reference}");
    let pin = native_format::Dependency::Git(native_format::Git { git_ref: format!("refs/tags/{reference}"), commit: format!("sha1-{commit}"), repo: "github:41/43".into(), ..Default::default() });
    native_format::File { workflows: [(path.into(), vec![key.clone()])].into(), dependencies: [(key, pin)].into(), ..Default::default() }.write().map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// A push of `path`: its preflight, with the fixture's sources, as the dispatcher's at the run's
/// start, then its plan. Refused before any leg, or the changed-files commit each queued leg is
/// given (what its runner gets), with the snapshot saved for its recovery.
struct Planned {
    refused: Option<String>,
    legs: usize,
    commits: Vec<String>,
    frozen: Option<Frozen>,
}

async fn planned(live: &Live, repo: &Repo, commit: &str, path: &str, text: &str) -> anyhow::Result<Planned> {
    let hub = live.hub();
    let model = model(path, text)?;
    let frozen = match prepare_with(&hub, &live.sources, repo, commit, path, Arc::clone(&model)).await {
        Ok(f) => f,
        Err(why) => return Ok(Planned { refused: Some(why), legs: 0, commits: Vec::new(), frozen: None }),
    };
    let caller = Caller { repo: repo.clone(), commit: commit.into(), model: Arc::clone(&model) };
    let (_, acts) = RunPlan::start(model, input_for(&caller, path), Settings::default(), &SavedSource(&frozen.files)).map_err(anyhow::Error::msg)?;
    let legs: Vec<_> = acts.iter().filter_map(|a| if let PlanAction::Queue(l) = a { Some(l) } else { None }).collect();
    let mut commits = Vec::new();
    for leg in &legs {
        let actions = frozen.actions_for(&leg.defined_in).ok_or_else(|| anyhow::anyhow!("a queued leg has no saved actions"))?;
        commits.extend(actions.iter().filter(|(u, _)| u.starts_with(CF)).map(|(_, a)| a.sha.clone()));
    }
    Ok(Planned { refused: None, legs: legs.len(), commits, frozen: Some(frozen) })
}

impl Planned {
    /// It ran, and every leg was given `commit`.
    fn on(&self, commit: &str) -> bool { self.refused.is_none() && self.legs > 0 && !self.commits.is_empty() && self.commits.iter().all(|c| c == commit) }
    fn refused_with(&self, code: &str) -> bool { self.legs == 0 && self.refused.as_deref().is_some_and(|e| e.contains(code)) }
    fn warned(&self, code: &str) -> bool { self.frozen.as_ref().is_some_and(|f| f.warnings.iter().any(|w| w.contains(code))) }
}

/// Steps 1 and 2: the mirror sees the tags move, the move is flagged and told once; a locked
/// partial version keeps A with KCI4057; a locked full version is refused (KCI4053) before any
/// leg; and a run saved before the move keeps A through recovery and a rerun.
#[test]
fn w29_s04_a_moved_tag_never_reaches_a_job_of_a_locked_github_workflow() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks_with(|c| c.ci.notify.push(crate::ci::notify::Rule { on: vec!["tag_moved".into()], cond: None, to: vec!["inbox:lock-observer".into()], throttle: None })).await?;
        let hub = live.hub();
        let origin = changed_files(&live).await?;
        let (a, m) = (live.commits.old.clone(), live.commits.new.clone());
        let (partial, full) = (workflow(&format!("{CF}@v45")), workflow(&format!("{CF}@v45.0.7")));
        let lock = github_lock(&[(PARTIAL, "v45"), (FULL, "v45.0.7")], &a)?;
        let app = native::publish(&hub, live.root(), "acme/app", vec![(PARTIAL.into(), partial.clone()), (FULL.into(), full.clone()), (github::PATH.into(), lock)], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        // Before the attack: both run A, and kiln records the full version's first resolution.
        let before = planned(&live, &app.repo, &app.commit, PARTIAL, &partial).await?;
        assert!(before.on(&a), "{:?} {:?}", before.commits, before.refused);
        assert!(planned(&live, &app.repo, &app.commit, FULL, &full).await?.on(&a));
        move_tags(&hub, &origin, &m).await?;
        // Step 2: the partial version keeps the locked A, and the run is told the ref moved.
        let kept = planned(&live, &app.repo, &app.commit, PARTIAL, &partial).await?;
        assert!(kept.on(&a) && kept.warned("KCI4057"), "{:?} {:?}", kept.commits, kept.frozen.map(|f| f.warnings));
        // The full version is refused before any leg.
        let refused = planned(&live, &app.repo, &app.commit, FULL, &full).await?;
        assert!(refused.refused_with("KCI4053"), "{:?}", refused.refused);
        // Step 1: flagged on the catalog entry, and told once.
        let moved = moved_tags(&hub, CF, false).await?;
        let flagged = |r: &serde_json::Value| r.get("ref").and_then(serde_json::Value::as_str) == Some("v45.0.7") && r.get("locked").and_then(serde_json::Value::as_str) == Some(a.as_str()) && r.get("now").and_then(serde_json::Value::as_str) == Some(m.as_str());
        assert!(moved.iter().any(flagged), "{moved:?}");
        notify_moves(&hub).await?;
        notify_moves(&hub).await?;
        let told: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_inbox WHERE event = 'tag_moved' AND person = 'user:lock-observer'", [], |r| r.get(0))?)).await?;
        assert_eq!(told, 1);
        // A run saved before the move is recovered, and run again, on A.
        let frozen = before.frozen.ok_or_else(|| anyhow::anyhow!("no snapshot"))?;
        let input = serde_json::json!({ "dependency_snapshot": frozen, "trusted": true });
        let saved = Frozen::saved(&input, &app.repo.name, PARTIAL, &app.commit).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("no saved snapshot"))?;
        assert!(saved.root_actions().map_err(anyhow::Error::msg)?.values().all(|x| x.sha == a));
        crate::ci::runs::create(&hub, &app.repo, crate::ci::runs::NewRun { id: "s04-before".into(), workflow: PARTIAL.into(), name: "s04".into(), event: "push".into(), branch: "main".into(), sha: app.commit.clone(), trusted: true, model: serde_json::to_string(&*model(PARTIAL, &partial)?)?, input: input.to_string(), ..Default::default() }).await?;
        hub.store.call(|c| Ok(c.execute("UPDATE runs SET status = 'succeeded', finished_at = 2 WHERE id = 's04-before'", [])?)).await?;
        let again = prepare_retry(&hub, "s04-before", false).await.map_err(anyhow::Error::msg)?;
        assert!(again.root_actions().map_err(anyhow::Error::msg)?.values().all(|x| x.sha == a));
        Ok(())
    })
}

/// Step 2's other files: a `.kiln/` workflow that GitHub's lock doesn't cover is held by
/// `.kiln/lock`, and a reusable workflow from another repository by its own lock.
#[test]
fn w29_s04_kilns_lock_and_a_callees_own_lock_hold_a_too() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let origin = changed_files(&live).await?;
        let (a, m) = (live.commits.old.clone(), live.commits.new.clone());
        // acme/shared's reusable workflow uses changed-files, locked to A in its own lock.
        let check = ".github/workflows/check.yml";
        let callee = format!("on: workflow_call\njobs:\n  check:\n    runs-on: baremetal\n    steps:\n      - uses: {CF}@v45\n");
        let shared = live.root().join("shared");
        write(&shared, vec![(check.into(), callee), (github::PATH.into(), github_lock(&[(check, "v45")], &a)?)]).await?;
        for args in [&["init", "-q", "-b", "main"][..], &["add", "."], &["commit", "-qm", "callee"], &["tag", "-am", "v1", "v1"]] {
            git(&hub, &shared, args).await?;
        }
        git(&hub, live.root(), &["clone", "-q", "--bare", &shared.to_string_lossy(), &live.root().join("shared.git").to_string_lossy()]).await?;
        let native = workflow(&format!("{CF}@v45"));
        let caller_text = format!("on: push\njobs:\n  shared:\n    uses: acme/shared/{check}@v1\n");
        let app = native::publish(&hub, live.root(), "acme/app", vec![(NATIVE.into(), native.clone()), (native_format::PATH.into(), kiln_lock(NATIVE, "v45", &a)?), (PATH.into(), caller_text.clone())], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        move_tags(&hub, &origin, &m).await?;
        let held = planned(&live, &app.repo, &app.commit, NATIVE, &native).await?;
        assert!(held.on(&a), "{:?} {:?}", held.commits, held.refused);
        let called = planned(&live, &app.repo, &app.commit, PATH, &caller_text).await?;
        assert!(called.on(&a), "{:?} {:?}", called.commits, called.refused);
        Ok(())
    })
}

/// Steps 3 and 4: a lock naming a commit outside the tag's history is refused (KCI4051); without
/// a lock, `enforce` refuses before reading any source (KCI4055), `report` with `moved_tags:
/// refuse` refuses the moved full version (KCI4056), and `report` alone annotates. Each refusal
/// plans no leg.
#[test]
fn w29_s04_unlocked_and_unprovable_runs_are_refused_before_any_job() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let origin = changed_files(&live).await?;
        let (a, m) = (live.commits.old.clone(), live.commits.new.clone());
        let partial = workflow(&format!("{CF}@v45"));
        let full = workflow(&format!("{CF}@v45.0.7"));
        // A lock naming the fork-only commit: no proof from the tag's history.
        let forked = github_lock(&[(PARTIAL, "v45")], &live.commits.foreign)?;
        let mut app = native::publish(&hub, live.root(), "acme/app", vec![(PARTIAL.into(), partial.clone()), (FULL.into(), full.clone()), (github::PATH.into(), forked)], "none").await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let unproved = planned(&live, &app.repo, &app.commit, PARTIAL, &partial).await?;
        assert!(unproved.refused_with("KCI4051"), "{:?}", unproved.refused);
        // Without a lock: the first run records A for the full version.
        app.commit = native::advance(&hub, &app, vec![(github::PATH.into(), github::File::default().write("github.com").map_err(|e| anyhow::anyhow!("{e:?}"))?)]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        assert!(planned(&live, &app.repo, &app.commit, FULL, &full).await?.on(&a));
        move_tags(&hub, &origin, &m).await?;
        let policy = |text: &str| (".kiln/config.yaml".to_owned(), format!("share: none\n{text}"));
        app.commit = native::advance(&hub, &app, vec![policy("require_lock: enforce\n")]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let enforced = planned(&live, &app.repo, &app.commit, FULL, &full).await?;
        assert!(enforced.refused_with("KCI4055"), "{:?}", enforced.refused);
        // report runs what isn't locked, annotated; moved_tags: refuse still refuses the move.
        app.commit = native::advance(&hub, &app, vec![policy("require_lock: report\nmoved_tags: refuse\n")]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let refused = planned(&live, &app.repo, &app.commit, FULL, &full).await?;
        assert!(refused.refused_with("KCI4056"), "{:?}", refused.refused);
        // Unlocked and only reported, a partial version follows its tag, annotated: what a lock,
        // enforce or refuse is there to stop.
        let reported = planned(&live, &app.repo, &app.commit, PARTIAL, &partial).await?;
        assert!(reported.on(&m) && reported.warned("KCI4055"), "{:?} {:?}", reported.commits, reported.frozen.map(|f| f.warnings));
        Ok(())
    })
}

/// Steps 5 to 7: a moved tag is no release and opens no update; a new release at M opens kiln's
/// update draft after the cooldown, M in its lock and given to no run, while the trunk's runs stay
/// on A; and a repository kiln hosts refuses the move itself.
#[test]
fn w29_s04_a_moved_tag_opens_no_update_a_new_release_does_and_kiln_hosts_refuse_the_move() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let live = Live::for_locks().await?;
        let hub = live.hub();
        let origin = changed_files(&live).await?;
        let (a, m) = (live.commits.old.clone(), live.commits.new.clone());
        let full = workflow(&format!("{CF}@v45.0.7"));
        let partial = workflow(&format!("{CF}@v45"));
        let lock = github_lock(&[(PARTIAL, "v45"), (FULL, "v45.0.7")], &a)?;
        let mut app = native::publish(&hub, live.root(), "acme/app", vec![(PARTIAL.into(), partial.clone()), (FULL.into(), full.clone()), (github::PATH.into(), lock)], "none").await?;
        app.commit = native::advance(&hub, &app, vec![(".kiln/config.yaml".into(), "share: none\nupdates:\n  actions: on\n  cooldown: 0\n".into())]).await?;
        crate::gitops::sync_mirror(&hub, &app.repo).await?;
        let ran = planned(&live, &app.repo, &app.commit, FULL, &full).await?;
        assert!(ran.on(&a));
        // What the dispatcher records as the run starts: its uses, for the catalog and updates.
        let actions = ran.frozen.as_ref().map(|f| f.root_actions().cloned()).transpose().map_err(anyhow::Error::msg)?.unwrap_or_default();
        crate::ci::catalog::record(&hub, "s04-full", &app.repo, &actions, &HashMap::new()).await;
        move_tags(&hub, &origin, &m).await?;
        assert!(planned(&live, &app.repo, &app.commit, FULL, &full).await?.refused_with("KCI4053"));
        let pending = || hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_pending_updates", [], |r| r.get::<_, i64>(0))?));
        assert_eq!(pending().await?, 0, "a moved tag is no release");
        // Step 6: a new release at M is due to the consumer that used the full version before it.
        git(&hub, &origin, &["tag", "-am", "release v46.0.0", "v46.0.0", &m]).await?;
        assert_eq!(crate::ci::updates::released(&hub, CF, "v46.0.0").await?, 1);
        assert_eq!(pending().await?, 1);
        // After the cooldown (none here) it opens as kiln's update draft, through the server's own
        // sources: the release in its uses and M in its lock. A draft runs only by its author's hand.
        live.share();
        let runs = || hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, i64>(0))?));
        let ran = runs().await?;
        crate::ci::updates::sweep(&hub).await;
        let (state, note): (String, Option<String>) = hub.store.call(|c| Ok(c.query_row("SELECT state, note FROM ci_pending_updates", [], |r| Ok((r.get(0)?, r.get(1)?)))?)).await?;
        assert_eq!(state, "opened", "{note:?}");
        let id = note.unwrap_or_default();
        let draft = crate::ci::pipeline_drafts::get(&hub, &id).await?.ok_or_else(|| anyhow::anyhow!("no update draft {id:?}"))?;
        let head = crate::ci::pipeline_drafts::head(&hub, &app.repo, &draft).await?;
        assert!(crate::ci::load::blob(&hub, &app.repo, &head, FULL).await?.contains(&format!("{CF}@v46.0.0")));
        let drafted = github::parse(&crate::ci::load::blob(&hub, &app.repo, &head, github::PATH).await?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let commit = |key: &str| drafted.dependencies.get(&format!("{CF}@{key}")).map(|d| d.commit.clone());
        assert_eq!((commit("v46.0.0"), commit("v45"), commit("v45.0.7")), (Some(format!("sha1-{m}")), Some(format!("sha1-{a}")), None), "{drafted:?}");
        assert_eq!(runs().await?, ran, "opening the draft gave M to no run");
        let still = planned(&live, &app.repo, &app.commit, PARTIAL, &partial).await?;
        assert!(still.on(&a), "the trunk's runs stay on A: {:?}", still.commits);
        // Step 7: where kiln hosts the action, the move is refused as it's pushed.
        let moving = crate::ci::hosting::Update { old: a.clone(), new: m.clone(), name: "refs/tags/v45.0.7".into() };
        assert!(crate::ci::hosting::tag_refused(&moving, &HashSet::new()).is_some());
        let floating = crate::ci::hosting::Update { old: a, new: m, name: "refs/tags/v45".into() };
        assert!(crate::ci::hosting::tag_refused(&floating, &HashSet::new()).is_some(), "a floating version is Release's to move");
        Ok(())
    })
}
