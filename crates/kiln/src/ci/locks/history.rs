//! W29-R26: accepted resolutions and new observations are different records. A refresh
//! updates what was seen, never the identity or commit a reviewed acceptance established.

use rusqlite::params;

use super::*;

#[derive(Clone)]
pub(super) struct Seen {
    source: String,
    publisher: String,
    native: bool,
    identity: String,
    old_identity: Option<String>,
    reference: String,
    current: String,
    pub previous: Option<String>,
    pub approved: bool,
}

fn binding(hub: &Hub, remote: &Remote, revision: &Revision) -> (String, String, bool) {
    match &revision.identity {
        Identity::Native { .. } => (format!("kiln://{}", revision.repository.name.to_ascii_lowercase()), revision.repository.name.to_ascii_lowercase(), true),
        Identity::Github { hostname, .. } => {
            // Match the actual mapped host/repository, with credentials stripped. A fixture's
            // local transport keeps the identity's host and the requested repository name.
            let publisher = actions::source(hub, &remote.owner, &remote.repo).ok().and_then(|url| crate::githost::split_url(&url))
                .filter(|(host, _)| host == hostname).map_or_else(|| revision.repository.name.to_ascii_lowercase(), |(_, name)| name.to_ascii_lowercase());
            (format!("github://{hostname}/{publisher}"), publisher, false)
        }
    }
}

impl Seen {
    fn full(&self) -> bool {
        self.reference.strip_prefix("refs/tags/").is_some_and(|tag| matches!(kiln_ci::releases::kind_of(tag), kiln_ci::releases::TagKind::Full))
    }

    pub fn accepted_moves(&self, key: &str) -> BTreeMap<String, String> {
        if self.approved { [(key.into(), self.current.clone())].into() } else { BTreeMap::new() }
    }

    pub async fn read(hub: &Hub, source: &dyn DependencySources, remote: &Remote, revision: &Revision) -> Result<Self, String> {
        revision.validate().map_err(|e| format!("KCI4051 {e}"))?;
        let (name, publisher, native) = binding(hub, remote, revision);
        let core_identity = match &revision.identity {
            Identity::Github { hostname, owner_id, repo_id } => kiln_ci::lock::kiln::Identity::Github { hostname: hostname.clone(), owner_id: *owner_id, repo_id: *repo_id },
            Identity::Native { repo_id } => kiln_ci::lock::kiln::Identity::Kiln { id: repo_id.clone() },
        };
        let identity = serde_json::to_string(&core_identity).map_err(|_| "KCI4051 source identity cannot be encoded")?;
        let (key, reference) = (name.clone(), revision.reference.clone());
        type Ref = (String, Option<String>, Option<String>);
        let (old_identity, recorded): (Option<String>, Option<Ref>) = hub.store.call(move |c| {
            let identity = c.query_row("SELECT accepted_identity FROM ci_lock_sources WHERE source = ?1", [&key], |r| r.get(0)).optional()?;
            let recorded = c.query_row("SELECT accepted_commit, accepted_change, accepted_by FROM ci_lock_refs WHERE source = ?1 AND git_ref = ?2", params![key, reference], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            Ok((identity, recorded))
        }).await.map_err(|_| "KCI4051 dependency history could not be read")?;
        let approved = recorded.as_ref().is_some_and(|(sha, change, by)| sha == &revision.commit && change.as_ref().is_some_and(|s| !s.is_empty()) && by.as_ref().is_some_and(|s| !s.is_empty()));
        let mut got = Self { source: name, publisher, native, identity, old_identity, reference: revision.reference.clone(), current: revision.commit.to_ascii_lowercase(), previous: recorded.map(|r| r.0), approved };
        // Old ci_uses rows did not record numeric identities. Seed only with an old accepted
        // commit proved in this same source's captured history, never from a name alone.
        if got.previous.is_none() && got.full() && got.publisher.eq_ignore_ascii_case(&remote.full()) {
            let (publisher, full, bare) = (got.publisher.clone(), got.reference.clone(), got.reference.trim_start_matches("refs/tags/").to_owned());
            let old: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT sha FROM ci_uses WHERE publisher = ?1 COLLATE NOCASE AND git_ref IN (?2, ?3) ORDER BY at, rowid LIMIT 1", params![publisher, full, bare], |r| r.get(0)).optional()?)).await.map_err(|_| "KCI4051 prior dependency uses could not be read")?;
            if let Some(old) = old.filter(|s| kiln_seams::dependencies::commit_valid(s))
                && source.contains(revision.clone(), old.clone()).await.unwrap_or(false)
            { got.previous = Some(old.to_ascii_lowercase()); }
        }
        Ok(got)
    }

    /// Record a successfully verified first resolution, or an observation about an existing
    /// acceptance. A refused first lookup cannot bless its current commit for the next run.
    pub async fn record(&self, hub: &Hub, caller: Option<&Repo>, workflow: &str, accepted: bool) -> Result<Self, String> {
        if !accepted && self.previous.is_none() && self.old_identity.is_none() { return Ok(self.clone()); }
        let (source, publisher, native, identity, reference, current, previous, repo, workflow, full, at) = (
            self.source.clone(), self.publisher.clone(), self.native, self.identity.clone(), self.reference.clone(), self.current.clone(), self.previous.clone(),
            caller.map(|c| c.id.clone()), workflow.to_owned(), self.full(), crate::util::now_unix(),
        );
        let (old_identity, recorded): (Option<String>, Option<String>) = hub.store.call(move |c| {
            let tx = c.transaction()?;
            let old_identity: Option<String> = tx.query_row("SELECT accepted_identity FROM ci_lock_sources WHERE source = ?1", [&source], |r| r.get(0)).optional()?;
            let recorded: Option<String> = tx.query_row("SELECT accepted_commit FROM ci_lock_refs WHERE source = ?1 AND git_ref = ?2", params![source, reference], |r| r.get(0)).optional()?;
            let previous = recorded.or(previous);
            let changed_identity = old_identity.as_ref().is_some_and(|old| old != &identity);
            tx.execute("INSERT OR IGNORE INTO ci_lock_sources (source, publisher, native, first_identity, accepted_identity, first_seen) VALUES (?1, ?2, ?3, ?4, ?4, ?5)", params![source, publisher, native, identity, at])?;
            if accepted || previous.is_some() {
                let first = previous.as_deref().unwrap_or(&current);
                tx.execute("INSERT OR IGNORE INTO ci_lock_refs (source, git_ref, first_commit, accepted_commit, last_commit, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?5)", params![source, reference, first, current, at])?;
                tx.execute("UPDATE ci_lock_refs SET last_commit = ?3, last_seen = ?4 WHERE source = ?1 AND git_ref = ?2", params![source, reference, current, at])?;
            }
            if full && let (Some(before), Some(repo)) = (&previous, repo) && (*before != current || changed_identity) {
                tx.execute("INSERT OR IGNORE INTO ci_lock_moves (source, git_ref, before_commit, now_commit, identity, before_identity, consumer_repo_id, workflow, observed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![source, reference, before, current, identity, old_identity.as_deref().unwrap_or(&identity), repo, workflow, at])?;
            }
            tx.commit()?;
            Ok((old_identity, previous))
        }).await.map_err(|_| "KCI4051 dependency history could not be recorded")?;
        let mut effective = self.clone();
        effective.old_identity = old_identity;
        effective.previous = recorded;
        Ok(effective)
    }
}

impl Capture<'_> {
    pub(super) async fn history(&mut self, remote: &Remote, revision: &Revision) -> Result<Seen, String> {
        let key = Self::ref_key(remote, &revision.reference);
        if let Some(seen) = self.histories.get(&key) { return Ok(seen.clone()); }
        let seen = Seen::read(self.hub, self.source, remote, revision).await?;
        self.histories.insert(key, seen.clone());
        Ok(seen)
    }

    pub(super) async fn remember(&mut self, seen: &Seen, uses: &str, workflow: &str, locked: bool, accepted: bool) -> Result<(), String> {
        let seen = if self.record { seen.record(self.hub, Some(self.caller), workflow, accepted).await? } else { seen.clone() };
        if self.record { notify_pending(self.hub, self.caller).await?; }
        let identity = serde_json::from_str(&seen.identity).map_err(|_| "KCI4051 recorded source identity is invalid")?;
        let accepted_identity = seen.old_identity.as_deref().map(serde_json::from_str).transpose().map_err(|_| "KCI4051 accepted source identity is invalid")?;
        let checked = verify::check_history(verify::HistoryCheck { workflow, uses, reference: &seen.reference, current: &seen.current, previous: seen.previous.as_deref(), identity: &identity, accepted_identity: accepted_identity.as_ref(), locked, policy: self.lock_policy, approved: seen.approved });
        match checked {
            Ok(warnings) => { self.warnings.extend(warnings.iter().map(|d| d.render(workflow))); Ok(()) }
            Err(error) => Err(error.diagnostics.iter().map(|d| d.render(workflow)).collect::<Vec<_>>().join("\n")),
        }
    }
}

/// W29-R26: one claimed event per source/ref/move and consumer. The existing notifier owns
/// declared channel selection, throttling, durable deliveries and their retries.
pub(super) async fn notify_pending(hub: &Hub, caller: &Repo) -> Result<(), String> {
    let repo_id = caller.id.clone();
    let rows: Vec<(i64, String, String, String, String, String, bool)> = hub.store.call(move |c| {
        let mut st = c.prepare("SELECT m.id, s.publisher, m.git_ref, m.before_commit, m.now_commit, m.workflow, m.identity != m.before_identity FROM ci_lock_moves m JOIN ci_lock_sources s ON s.source = m.source WHERE m.consumer_repo_id = ?1 AND m.notified_at IS NULL ORDER BY m.id LIMIT 100")?;
        let rows = st.query_map([repo_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }).await.map_err(|_| "KCI4051 moved-tag notifications could not be read")?;
    for (id, publisher, reference, before, now, workflow, identity_changed) in rows {
        let at = crate::util::now_unix();
        let claimed = hub.store.call(move |c| Ok(c.execute("UPDATE ci_lock_moves SET notified_at = ?2 WHERE id = ?1 AND notified_at IS NULL", params![id, at])? > 0)).await.map_err(|_| "KCI4051 moved-tag notification could not be claimed")?;
        if !claimed { continue; }
        let tag = reference.trim_start_matches("refs/tags/");
        let message = if before == now && identity_changed { format!("The repository identity behind {publisher}@{tag} changed; its code is still {now}. Accepting that source needs a reviewed lock change.") }
            else { format!("{publisher}@{tag} moved from {before} to {now}. Its accepted resolution has not changed; accepting the new code needs a reviewed lock change.") };
        let branch = caller.default_branch.clone().unwrap_or_else(|| "main".into());
        let url = format!("{}/ship/pipelines?repo={}&path={}", hub.cfg.server.public_url.trim_end_matches('/'), actions::encode_path(&caller.name), actions::encode_path(&workflow));
        let subject = super::super::notify::Subject {
            repo: caller.name.clone(), run: format!("tag-move-{id}"), workflow, name: format!("{publisher}@{tag}: tag moved"),
            branch: branch.clone(), sha: now.clone(), message, actor: "kiln".into(),
            url,
            github: serde_json::json!({ "repository": caller.name, "event_name": "tag_moved", "ref_name": branch, "event": { "dependency": publisher, "ref": reference, "before": before, "now": now, "identity_changed": identity_changed } }),
            ..Default::default()
        };
        Box::pin(super::super::notify::emit(hub, caller, "tag_moved", &subject)).await;
    }
    Ok(())
}

/// Only cached observations: opening the catalog never fetches tags or approves them.
pub(super) async fn moved(hub: &Hub, publisher: &str, native: bool) -> anyhow::Result<Vec<serde_json::Value>> {
    let publisher = publisher.to_owned();
    hub.store.call(move |c| {
        let mut st = c.prepare("SELECT m.git_ref, m.before_commit, m.now_commit, MIN(m.observed_at), s.source, MAX(m.identity != m.before_identity) FROM ci_lock_moves m JOIN ci_lock_sources s ON s.source = m.source WHERE s.publisher = ?1 COLLATE NOCASE AND s.native = ?2 GROUP BY s.source, m.git_ref, m.before_commit, m.now_commit ORDER BY m.git_ref, MAX(m.id)")?;
        let rows = st.query_map(params![publisher, native], |r| {
            let reference: String = r.get(0)?;
            Ok(serde_json::json!({ "ref": reference.trim_start_matches("refs/tags/"), "locked": r.get::<_, String>(1)?, "now": r.get::<_, String>(2)?, "at": r.get::<_, i64>(3)?, "source": r.get::<_, String>(4)?, "identity_changed": r.get::<_, bool>(5)? }))
        })?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }).await
}
