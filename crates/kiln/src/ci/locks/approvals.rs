//! W29-R25/R26: a person's review binds the consumer, version, complete code difference,
//! stable source identity and changed pins. It never blesses a moved tag for another consumer.

use http::StatusCode;
use rusqlite::params;

use super::*;
use crate::authz::{Caller, Scope, gate::Kind};
use crate::ci::{Fail, diffs::{Diff, Version}};

fn refused(why: impl Into<String>) -> Fail { (StatusCode::CONFLICT, why.into()) }

async fn tree(hub: &Hub, repo: &Repo, commit: &str) -> Result<String, String> {
    if !kiln_seams::dependencies::commit_valid(commit) { return Err("KCI4051 invalid reviewed commit".into()); }
    let dir = crate::gitops::ensure_commit(hub, repo, commit).await.map_err(|_| "KCI4051 reviewed commit is unavailable")?;
    let bytes = crate::gitops::run_git(hub, &["-C", &dir.to_string_lossy(), "rev-parse", "--verify", &format!("{commit}^{{tree}}")], crate::gitops::LS_REMOTE_TIMEOUT).await.map_err(|_| "KCI4051 reviewed tree is unavailable")?;
    let sha = String::from_utf8(bytes).map_err(|_| "KCI4051 invalid reviewed tree")?.trim().to_owned();
    if !kiln_seams::dependencies::commit_valid(&sha) { return Err("KCI4051 invalid reviewed tree".into()); }
    Ok(sha)
}

struct Locks { github: Option<github::File>, native: Option<kiln_ci::lock::kiln::File> }

impl Locks {
    async fn read(hub: &Hub, repo: &Repo, commit: &str) -> Result<Self, String> {
        let dir = crate::gitops::ensure_commit(hub, repo, commit).await.map_err(|_| "KCI4051 reviewed locks are unavailable")?;
        let gh = blob(hub, &dir, None, commit, github::PATH).await?;
        let kl = blob(hub, &dir, None, commit, kiln_ci::lock::kiln::PATH).await?;
        Ok(Self {
            github: gh.map(|b| String::from_utf8(b).map_err(|_| "KCI1022 reviewed GitHub lock is not UTF-8")).transpose()?.map(|s| github::parse(&s).map_err(|_| "KCI1022 reviewed GitHub lock does not read")).transpose()?,
            native: kl.map(|b| String::from_utf8(b).map_err(|_| "KCI1022 reviewed kiln lock is not UTF-8")).transpose()?.map(|s| kiln_ci::lock::kiln::parse(&s).map_err(|_| "KCI1022 reviewed kiln lock does not read")).transpose()?,
        })
    }

    fn pin(&self, key: &str) -> Option<kiln_ci::lock::kiln::Git> {
        if let Some(kiln_ci::lock::kiln::Dependency::Git(git)) = self.native.as_ref().and_then(|f| f.dependencies.get(key)) { return Some(git.clone()); }
        self.github.as_ref().and_then(|f| f.dependencies.get(key)).map(|a| kiln_ci::lock::kiln::Git { git_ref: a.git_ref.clone(), commit: a.commit.clone(), repo: format!("github:{}/{}", a.owner_id, a.repo_id), hostname: a.hostname.clone(), uses: a.uses.clone() })
    }
}

fn core_identity(identity: &Identity) -> kiln_ci::lock::kiln::Identity {
    match identity {
        Identity::Github { hostname, owner_id, repo_id } => kiln_ci::lock::kiln::Identity::Github { hostname: hostname.clone(), owner_id: *owner_id, repo_id: *repo_id },
        Identity::Native { repo_id } => kiln_ci::lock::kiln::Identity::Kiln { id: repo_id.clone() },
    }
}

/// A draft's head now: its diff's latest version, or what kiln saved last on its host.
const DRAFT_HEAD: &str = "CASE WHEN d.kind = 'diff' THEN (SELECT v.commit_sha FROM diff_versions v JOIN diffs x ON x.id = v.diff_id WHERE x.kind = 'diff' AND 'D' || x.number = d.name ORDER BY v.n DESC LIMIT 1) ELSE d.head END";

struct Bound {
    proposal: String, draft: String, commit: String, hash: String, tree: String,
    pins: Vec<(String, String, String, String, String)>,
}

struct Authority {
    kind: &'static str, principal: &'static str, by: String, user: Option<String>,
    diff: Option<String>, version: Option<u32>, review: Option<String>,
}

fn person(caller: &Caller) -> bool { matches!(caller.kind, Kind::Owner | Kind::User) && caller.agent.is_none() && caller.token.is_none() }

async fn binding(hub: &Hub, repo: &Repo, draft: &str, head: &str, expected: Option<&str>) -> Result<Option<Bound>, Fail> {
    let (repo_id, draft_id, commit) = (repo.id.clone(), draft.to_owned(), head.to_owned());
    let proposal: Option<(String, String, String, String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT p.id, p.base_sha, p.evidence, p.evidence_sha256, d.path FROM ci_lock_reviews p JOIN ci_pipeline_drafts d ON d.id = p.draft_id WHERE p.consumer_repo_id = ?1 AND p.draft_id = ?2 AND p.commit_sha = ?3 AND d.state IN ('draft', 'review')", params![repo_id, draft_id, commit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?)).await.map_err(|_| refused("lock review evidence could not be read"))?;
    let Some((proposal, base, evidence, hash, workflow)) = proposal else { return Ok(None) };
    if expected.is_some_and(|given| given != hash) { return Err(refused("the upstream code evidence changed; review the current revision")); }
    if evidence.len() > 24 << 20 || kiln_seams::sha256_hex(evidence.as_bytes()) != hash { return Err(refused("KCI4051 reviewed code evidence failed its integrity check")); }
    let differences: BTreeMap<String, kiln_seams::dependencies::Difference> = serde_json::from_str(&evidence).map_err(|_| refused("KCI1022 reviewed code evidence does not read"))?;
    let (before, after) = (Locks::read(hub, repo, &base).await.map_err(refused)?, Locks::read(hub, repo, head).await.map_err(refused)?);
    let home = Owner { repository: repo.name.clone(), commit: head.into(), remote: None, revision: None }.home_host(hub, repo);
    let mut changes = Vec::new();
    let mut prior = BTreeMap::new();
    for (key, difference) in &differences {
        difference.validate().map_err(|e| refused(format!("KCI4051 {key}: {e}")))?;
        if before.pin(key).is_none() {
            let (identity, reference, old) = (serde_json::to_string(&core_identity(&difference.before.identity)).map_err(|_| refused("KCI4051 accepted identity cannot be encoded"))?, difference.after.reference.clone(), difference.before.commit.clone());
            let accepted: bool = hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_lock_refs r JOIN ci_lock_sources s ON s.source = r.source WHERE s.accepted_identity = ?1 AND r.git_ref = ?2 AND r.accepted_commit = ?3)", params![identity, reference, old], |r| r.get(0))?)).await.map_err(|_| refused("prior source acceptance could not be read"))?;
            if accepted && let Some(mut pin) = after.pin(key) { pin.commit = format!("{}-{}", if difference.before.commit.len() == 64 { "sha256" } else { "sha1" }, difference.before.commit); prior.insert(key.clone(), pin); }
        }
        changes.push(kiln_ci::lock::review::Change { key: key.clone(), reference: difference.after.reference.clone(), before: difference.before.commit.clone(), after: difference.after.commit.clone(), identity: core_identity(&difference.after.identity) });
    }
    let approved = kiln_ci::lock::review::bind(kiln_ci::lock::review::Input { before: kiln_ci::lock::replay::Pair { github: before.github.as_ref(), kiln: before.native.as_ref() }, after: kiln_ci::lock::replay::Pair { github: after.github.as_ref(), kiln: after.native.as_ref() }, workflow: &workflow, home_host: &home, changes: &changes, prior: &prior }).map_err(|ds| refused(ds.iter().map(|d| d.render(&workflow)).collect::<Vec<_>>().join("\n")))?;
    let mut pins = Vec::new();
    for (key, difference) in differences.into_iter().filter(|(key, _)| approved.contains_key(key)) {
        let identity = serde_json::to_string(&difference.after.identity).map_err(|_| refused("KCI4051 reviewed identity cannot be encoded"))?;
        pins.push((key, identity, difference.after.reference, difference.before.commit, difference.after.commit));
    }
    Ok(Some(Bound { proposal, draft: draft.into(), commit: head.into(), hash, tree: tree(hub, repo, head).await.map_err(refused)?, pins }))
}

async fn record(hub: &Hub, repo: &Repo, bound: Bound, authority: Authority) -> Result<String, Fail> {
    let (id, repo_id) = (crate::util::new_id(), repo.id.clone());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let current: Option<String> = tx.query_row(&format!("SELECT {DRAFT_HEAD} FROM ci_pipeline_drafts d WHERE d.id = ?1 AND d.state IN ('draft', 'review')"), [&bound.draft], |r| r.get(0)).optional()?.flatten();
        if current.as_deref() != Some(bound.commit.as_str()) { return Err(anyhow::anyhow!("the draft changed during its dependency review")); }
        if let Some(review) = &authority.review {
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reviews r JOIN diff_versions v ON v.diff_id = r.diff_id AND v.n = r.version WHERE r.id = ?1 AND r.diff_id = ?2 AND r.version = ?3 AND r.reviewer = ?4 AND r.reviewer_kind = 'human' AND r.action = 'accept' AND v.commit_sha = ?5)", params![review, authority.diff, authority.version, authority.by, bound.commit], |r| r.get(0))?;
            if !valid { return Err(anyhow::anyhow!("the person's review does not name this dependency revision")); }
        }
        tx.execute("INSERT INTO ci_lock_approvals (id, proposal_id, consumer_repo_id, diff_id, version, commit_sha, tree_sha, human_review_id, kind, principal_kind, user_id, approved_by, evidence_sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) ON CONFLICT DO NOTHING", params![id, bound.proposal, repo_id, authority.diff, authority.version, bound.commit, bound.tree, authority.review, authority.kind, authority.principal, authority.user, authority.by, bound.hash, crate::util::now_unix()])?;
        let id: String = tx.query_row("SELECT id FROM ci_lock_approvals WHERE proposal_id = ?1 AND kind = ?2 AND approved_by = ?3 AND (?2 = 'dependency' OR human_review_id = ?4) ORDER BY created_at DESC, id DESC LIMIT 1", params![bound.proposal, authority.kind, authority.by, authority.review], |r| r.get(0))?;
        for (key, identity, reference, before, now) in bound.pins { tx.execute("INSERT OR IGNORE INTO ci_lock_approved_pins (approval_id, uses_key, source_identity, git_ref, before_commit, commit_sha) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, key, identity, reference, before, now])?; }
        tx.commit()?;
        Ok(id)
    }).await.map_err(|_| refused("the dependency revision changed or its review could not be recorded"))
}

/// A dependency review is a person's explicit word about this code, not an approval of the
/// enclosing diff. It neither starts a run nor changes the repository's merge requirements.
pub async fn accept(hub: &Arc<Hub>, caller: &Caller, key: &str, body: &serde_json::Value) -> Result<serde_json::Value, Fail> {
    if !person(caller) { return Err((StatusCode::FORBIDDEN, "accepting dependency code needs a person's signed-in session".into())); }
    let repo = hub.store.repos().await.map_err(|_| refused("repositories could not be read"))?.into_iter().find(|r| r.id == key || r.name == key).ok_or_else(|| (StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    let scope = Scope::Repo { repo: repo.name.clone() };
    if !["workflows.edit", "diffs.submit", "diffs.review"].iter().all(|what| caller.can(what, &scope)) { return Err((StatusCode::FORBIDDEN, "accepting dependency code needs `workflows.edit`, `diffs.submit` and `diffs.review`".into())); }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ask { draft: String, head: String, evidence_sha256: String }
    let ask: Ask = serde_json::from_value(body.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("dependency review: {e}")))?;
    if !kiln_seams::dependencies::commit_valid(&ask.head) || ask.evidence_sha256.len() != 64 || !ask.evidence_sha256.bytes().all(|b| b.is_ascii_hexdigit()) { return Err((StatusCode::BAD_REQUEST, "dependency review needs the exact head and code evidence hash".into())); }
    let bound = binding(hub, &repo, &ask.draft, &ask.head, Some(&ask.evidence_sha256)).await?.ok_or_else(|| refused("this draft revision has no complete upstream code evidence"))?;
    let authority = Authority { kind: "dependency", principal: if caller.kind == Kind::Owner { "owner" } else { "user" }, by: caller.name.clone(), user: caller.user.clone(), diff: None, version: None, review: None };
    let id = record(hub, &repo, bound, authority).await?;
    hub.store.audit(caller.name.clone(), "ci.locks.accept".into(), format!("{} {} at {}: code evidence {}", repo.name, ask.draft, ask.head, ask.evidence_sha256)).await;
    Ok(serde_json::json!({"approval": id, "draft": ask.draft, "head": ask.head, "evidence_sha256": ask.evidence_sha256, "accepted": true}))
}

pub(crate) async fn reviewed(hub: &Hub, diff: &Diff, version: &Version, caller: &Caller, review: &str) -> Result<(), Fail> {
    if !person(caller) { return Ok(()); }
    if !caller.can("diffs.review", &Scope::Repo { repo: diff.repo.clone() }) { return Err((StatusCode::FORBIDDEN, "reviewing dependency changes needs `diffs.review`".into())); }
    let repo = hub.store.repos().await.map_err(|_| refused("the reviewed repository could not be read"))?.into_iter().find(|r| r.name == diff.repo).ok_or_else(|| refused("the reviewed repository is gone"))?;
    let (name, repo_name) = (diff.name.clone(), repo.name.clone());
    let draft: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT id FROM ci_pipeline_drafts WHERE repo = ?1 AND name = ?2 AND state IN ('draft', 'review')", params![repo_name, name], |r| r.get(0)).optional()?)).await.map_err(|_| refused("the dependency draft could not be read"))?;
    let Some(draft) = draft else { return Ok(()) };
    let Some(bound) = binding(hub, &repo, &draft, &version.commit, None).await? else { return Ok(()) };
    record(hub, &repo, bound, Authority { kind: "review", principal: if caller.kind == Kind::Owner { "owner" } else { "user" }, by: crate::ci::review::who(caller).0, user: caller.user.clone(), diff: Some(diff.id.clone()), version: Some(version.n), review: Some(review.into()) }).await?;
    Ok(())
}

struct Proof { id: String, key: String, identity: String, reference: String, commit: String, reviewed: String, tree: String, latest: String, workflow: String }

/// A person's explicit acceptance, or their accept of the exact version, joined back to that
/// review: a later verdict by them withdraws it (a comment doesn't); new code needs new words.
async fn proofs(hub: &Hub, repo: &Repo, reviewed_tree: Option<&str>, source: Option<(&str, &str, &str)>) -> Result<Vec<Proof>, String> {
    let repo = repo.id.clone();
    let change = reviewed_tree.map(str::to_owned);
    let (identity, reference, pin) = source.map_or((None, None, None), |(identity, reference, pin)| (Some(identity.to_owned()), Some(reference.to_owned()), Some(pin.to_owned())));
    let rows = hub.store.call(move |c| {
        let mut st = c.prepare(&format!("SELECT a.id, p.uses_key, p.source_identity, p.git_ref, p.commit_sha, a.commit_sha, a.tree_sha, COALESCE({DRAFT_HEAD}, ''), d.path FROM ci_lock_approvals a JOIN ci_lock_approved_pins p ON p.approval_id = a.id JOIN ci_lock_reviews e ON e.id = a.proposal_id JOIN ci_pipeline_drafts d ON d.id = e.draft_id LEFT JOIN reviews r ON r.id = a.human_review_id LEFT JOIN diff_versions v ON v.diff_id = a.diff_id AND v.n = a.version WHERE a.consumer_repo_id = ?1 AND (?2 IS NULL OR a.tree_sha = ?2) AND (?3 IS NULL OR p.source_identity = ?3) AND (?4 IS NULL OR p.git_ref = ?4) AND (?5 IS NULL OR p.commit_sha = ?5) AND d.state != 'discarded' AND e.evidence_sha256 = a.evidence_sha256 AND (a.kind = 'dependency' OR (r.diff_id = a.diff_id AND r.version = a.version AND r.reviewer = a.approved_by AND r.reviewer_kind = 'human' AND r.action = 'accept' AND v.commit_sha = a.commit_sha AND NOT EXISTS(SELECT 1 FROM reviews newer WHERE newer.diff_id = r.diff_id AND newer.reviewer = r.reviewer AND newer.action IN ('accept', 'request_changes', 'resign') AND newer.rowid > r.rowid))) LIMIT 4097"))?;
        Ok(st.query_map(params![repo, change, identity, reference, pin], |r| Ok(Proof { id: r.get(0)?, key: r.get(1)?, identity: r.get(2)?, reference: r.get(3)?, commit: r.get(4)?, reviewed: r.get(5)?, tree: r.get(6)?, latest: r.get(7)?, workflow: r.get(8)? }))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(|_| "KCI4051 dependency approvals could not be read")?;
    if rows.len() > MAX_ITEMS { return Err("KCI1022 too many dependency approval facts".into()); }
    Ok(rows)
}

impl Proof {
    async fn current(&self, hub: &Hub, repo: &Repo) -> Result<bool, String> {
        if self.latest == self.reviewed { return Ok(true); }
        // Ready on a host can recommit the same tree to remove [skip ci]. That changes no
        // reviewed code. Any changed file, including a lock or companion, invalidates it.
        Ok(!self.latest.is_empty() && tree(hub, repo, &self.latest).await? == self.tree)
    }
}

pub(super) async fn allowed(hub: &Hub, repo: &Repo, root: &str, identity: &Identity, reference: &str, commit: &str) -> Result<bool, String> {
    let identity = serde_json::to_string(identity).map_err(|_| "KCI4051 source identity could not be encoded")?;
    let (repo_id, identity2, reference2, commit2) = (repo.id.clone(), identity.clone(), reference.to_owned(), commit.to_ascii_lowercase());
    let accepted: bool = hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_lock_acceptances x JOIN ci_lock_approved_pins p ON p.approval_id = x.approval_id AND p.uses_key = x.uses_key WHERE x.consumer_repo_id = ?1 AND p.source_identity = ?2 AND p.git_ref = ?3 AND p.commit_sha = ?4)", params![repo_id, identity2, reference2, commit2], |r| r.get(0))?)).await.map_err(|_| "KCI4051 accepted dependency state could not be read")?;
    if accepted { return Ok(true); }
    let mut own_tree = None;
    for proof in proofs(hub, repo, None, Some((&identity, reference, commit))).await? {
        if !proof.current(hub, repo).await? { continue; }
        let (repo_id, candidate, id, key) = (repo.id.clone(), root.to_owned(), proof.id.clone(), proof.key.clone());
        let registered: bool = hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_lock_candidate_pins WHERE consumer_repo_id = ?1 AND candidate_sha = ?2 AND approval_id = ?3 AND uses_key = ?4) OR EXISTS(SELECT 1 FROM ci_lock_acceptances WHERE consumer_repo_id = ?1 AND approval_id = ?3 AND uses_key = ?4)", params![repo_id, candidate, id, key], |r| r.get(0))?)).await.map_err(|_| "KCI4051 accepted dependency state could not be read")?;
        if registered { return Ok(true); }
        if own_tree.is_none() { own_tree = Some(tree(hub, repo, root).await?); }
        if own_tree.as_deref() == Some(proof.tree.as_str()) { return Ok(true); }
    }
    Ok(false)
}

pub(super) async fn previous(hub: &Hub, repo: &Repo, identity: &Identity, reference: &str) -> Result<Option<String>, String> {
    let (repo, identity, reference) = (repo.id.clone(), serde_json::to_string(identity).map_err(|_| "KCI4051 source identity could not be encoded")?, reference.to_owned());
    hub.store.call(move |c| Ok(c.query_row("SELECT p.commit_sha FROM ci_lock_acceptances x JOIN ci_lock_approved_pins p ON p.approval_id = x.approval_id AND p.uses_key = x.uses_key JOIN ci_lock_approvals a ON a.id = p.approval_id WHERE x.consumer_repo_id = ?1 AND p.source_identity = ?2 AND p.git_ref = ?3 ORDER BY x.landed_at DESC, a.created_at DESC, a.id DESC LIMIT 1", params![repo, identity, reference], |r| r.get(0)).optional()?)).await.map_err(|_| "KCI4051 accepted dependency history could not be read".into())
}

pub(crate) async fn ready(hub: &Hub, repo: &Repo, draft: &crate::ci::pipeline_drafts::Draft, head: &str) -> Result<(), Fail> {
    let Some(review) = super::proposal_for(hub, repo, &draft.id, head).await? else { return Ok(()) };
    let differences: BTreeMap<String, kiln_seams::dependencies::Difference> = serde_json::from_value(review.get("differences").cloned().unwrap_or_default()).map_err(|_| refused("KCI1022 lock review evidence does not read"))?;
    for difference in differences.values() {
        if !allowed(hub, repo, head, &difference.after.identity, &difference.after.reference, &difference.after.commit).await.map_err(refused)? { return Err(refused("review and accept the upstream dependency code before marking this draft Ready")); }
    }
    Ok(())
}

pub(crate) async fn carry_ready(hub: &Hub, repo: &Repo, draft: &crate::ci::pipeline_drafts::Draft, before: &str, after: &str) -> Result<(), Fail> {
    let Some(review) = super::proposal_for(hub, repo, &draft.id, before).await? else { return Ok(()) };
    if tree(hub, repo, before).await.map_err(refused)? != tree(hub, repo, after).await.map_err(refused)? { return Err(refused("Ready changed the dependency draft's reviewed files; save and review its current code")); }
    let differences: BTreeMap<String, kiln_seams::dependencies::Difference> = serde_json::from_value(review.get("differences").cloned().unwrap_or_default()).map_err(|_| refused("KCI1022 lock review evidence does not read"))?;
    let base = review.get("base").and_then(serde_json::Value::as_str).ok_or_else(|| refused("KCI4051 lock review evidence has no base"))?;
    super::keep_proposal(hub, repo, draft, base, after, &differences).await?;
    Ok(())
}

/// Whether anyone accepted a dependency change for this consumer: most never do, and then
/// landing reads no trees for it.
async fn any(hub: &Hub, repo: &Repo) -> Result<bool, String> {
    let repo = repo.id.clone();
    hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_lock_approvals WHERE consumer_repo_id = ?1)", [&repo], |r| r.get(0))?)).await.map_err(|_| "KCI4051 dependency approvals could not be read".into())
}

pub(crate) async fn seeds(hub: &Hub, repo: &Repo, change: &str) -> Result<kiln_ci::lock::replay::Accepted, String> {
    let mut accepted = kiln_ci::lock::replay::Accepted { github: None, kiln: None };
    if !any(hub, repo).await? { return Ok(accepted); }
    let own = tree(hub, repo, change).await?;
    let mut files = None;
    for proof in proofs(hub, repo, Some(&own), None).await? {
        if !proof.current(hub, repo).await? { continue; }
        if files.is_none() { files = Some(Locks::read(hub, repo, change).await?); }
        let Some(locks) = &files else { continue };
        let selected = locks.pin(&proof.key).ok_or_else(|| format!("KCI4050 {}: reviewed pin is missing", proof.key))?;
        let identity: Identity = serde_json::from_str(&proof.identity).map_err(|_| "KCI4051 invalid reviewed source identity")?;
        let home = Owner { repository: repo.name.clone(), commit: change.into(), remote: None, revision: None }.home_host(hub, repo);
        let facts = kiln_ci::lock::verify_kiln::GitFacts { identity: core_identity(&identity), reference: proof.reference.clone(), current: proof.commit.clone(), reachable: [proof.commit.clone()].into(), previous: None };
        let pin = kiln_ci::lock::regenerate::source(kiln_ci::lock::regenerate::SourceCheck { workflow: &proof.workflow, key: &proof.key, accepted: None, current: &selected, facts: &facts, home_host: &home, mode: kiln_ci::lock::regenerate::Mode::Preserve }).map_err(|ds| ds.iter().map(|d| d.render(&proof.workflow)).collect::<Vec<_>>().join("\n"))?;
        if let Some(action) = locks.github.as_ref().and_then(|f| f.dependencies.get(&proof.key)) {
            let mut action = action.clone();
            action.uses.clear();
            accepted.github.get_or_insert_with(github::File::default).dependencies.insert(proof.key.clone(), action);
        }
        if locks.native.as_ref().is_some_and(|f| f.dependencies.contains_key(&proof.key)) { accepted.kiln.get_or_insert_with(kiln_ci::lock::kiln::File::default).dependencies.insert(proof.key, kiln_ci::lock::kiln::Dependency::Git(pin)); }
    }
    Ok(accepted)
}

/// Only the landing path, after its review gate, registers a merged candidate. The registry
/// names an exact immutable consumer commit; other branches cannot borrow its permission.
pub(crate) async fn candidate(hub: &Hub, repo: &Repo, candidate: &str, reviewed: &[String]) -> Result<(), String> {
    if !any(hub, repo).await? { return Ok(()); }
    let mut pins = BTreeSet::new();
    for commit in reviewed {
        let own = tree(hub, repo, commit).await?;
        for proof in proofs(hub, repo, Some(&own), None).await? {
            if proof.current(hub, repo).await? { pins.insert((proof.id, proof.key)); }
        }
    }
    let (repo, candidate) = (repo.id.clone(), candidate.to_owned());
    hub.store.call(move |c| {
        for (approval, key) in pins { c.execute("INSERT OR IGNORE INTO ci_lock_candidate_pins (consumer_repo_id, candidate_sha, approval_id, uses_key) VALUES (?1, ?2, ?3, ?4)", params![repo, candidate, approval, key])?; }
        Ok(())
    }).await.map_err(|_| "KCI4051 candidate dependency permissions could not be recorded".into())
}

/// The successful trunk CAS precedes this record. Promote only the pins actually present in
/// its immutable tree, retaining the proof and leaving global source/tag acceptance alone.
pub(crate) async fn landed(hub: &Hub, repo: &Repo, candidate: &str, landed: &str) -> Result<(), String> {
    let (repo_id, candidate) = (repo.id.clone(), candidate.to_owned());
    let rows: Vec<(String, String, String)> = hub.store.call(move |c| {
        let mut st = c.prepare("SELECT x.approval_id, x.uses_key, p.commit_sha FROM ci_lock_candidate_pins x JOIN ci_lock_approved_pins p ON p.approval_id = x.approval_id AND p.uses_key = x.uses_key WHERE x.consumer_repo_id = ?1 AND x.candidate_sha = ?2 LIMIT 4097")?;
        Ok(st.query_map(params![repo_id, candidate], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }).await.map_err(|_| "KCI4051 landed dependency proof could not be read")?;
    if rows.is_empty() { return Ok(()); }
    if rows.len() > MAX_ITEMS { return Err("KCI1022 too many landed dependency pins".into()); }
    let files = Locks::read(hub, repo, landed).await?;
    let pins: Vec<_> = rows.into_iter().filter(|(_, key, commit)| files.pin(key).is_some_and(|pin| github::digest(&pin.commit) == Some(commit.as_str()))).collect();
    let (repo, landed) = (repo.id.clone(), landed.to_owned());
    hub.store.call(move |c| {
        for (approval, key, _) in pins { c.execute("INSERT OR IGNORE INTO ci_lock_acceptances (consumer_repo_id, approval_id, uses_key, landed_commit, landed_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![repo, approval, key, landed, crate::util::now_unix()])?; }
        Ok(())
    }).await.map_err(|_| "KCI4051 landed dependency acceptance could not be recorded".into())
}

/// Its host merged `reviewed` (a cleared head) into the trunk as `landed`: the pins a person
/// accepted there and that the merged tree keeps become this consumer's.
pub(crate) async fn landed_on_host(hub: &Hub, repo: &Repo, reviewed: &str, landed: &str) -> Result<(), String> {
    candidate(hub, repo, landed, &[reviewed.to_owned()]).await?;
    self::landed(hub, repo, landed, landed).await
}
