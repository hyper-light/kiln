//! W29-R24: caller locks precede a callee's immutable seed. Selecting the caller's pin also
//! selects the source metadata and the policy subject; no later mutable ref overrides it.

use kiln_ci::lock::{kiln, verify_kiln};

use super::*;

#[derive(Debug, Clone)]
pub(super) struct Layer {
    pub repository: String,
    pub workflow: String,
    pub home: String,
    pub file: Arc<kiln::File>,
}

impl Capture<'_> {
    pub(super) async fn native_lock(&mut self, owner: &Owner, workflow: &str) -> Result<Option<Layer>, String> {
        let key = format!("{}:{}@{}", if owner.remote.is_some() { "remote" } else { "own" }, owner.repository, owner.commit);
        let file = match self.native_locks.get(&key) {
            Some(file) => file.clone(),
            None => {
                let file = if let Some(bytes) = owner.file(self.hub, self.source, kiln::PATH).await? {
                    self.count(bytes.len())?;
                    let text = String::from_utf8(bytes).map_err(|_| format!("KCI1022 {}:{} is not UTF-8", owner.repository, kiln::PATH))?;
                    Some(Arc::new(kiln::parse(&text).map_err(|ds| ds.iter().map(|d| format!("{}: {}", owner.repository, d.render(kiln::PATH))).collect::<Vec<_>>().join("\n"))?))
                } else { None };
                self.native_locks.insert(key, file.clone());
                file
            }
        };
        Ok(file.filter(|f| f.workflows.contains_key(workflow)).map(|file| Layer { repository: owner.repository.clone(), workflow: workflow.into(), home: owner.home_host(self.hub, self.caller), file }))
    }

    pub(super) async fn native_selection(&mut self, uses: &str, remote: &Remote, native: &[Layer]) -> Result<Option<(String, Option<Revision>, bool)>, String> {
        for layer in native {
            let Some((key, entry)) = verify_kiln::pin_for(&layer.file, &layer.workflow, uses) else { continue };
            remote.usable_by(self.hub, &self.caller.name).await?;
            let wanted = github::digest(&entry.commit).ok_or_else(|| format!("KCI1022 {uses}: invalid locked commit"))?.to_ascii_lowercase();
            let ref_key = Self::ref_key(remote, &entry.git_ref);
            let revision = match self.captured(remote, &entry.git_ref) {
                Some(revision) => revision,
                None => {
                    let revision = self.source.resolve(Repository { name: remote.full(), native: remote.native }, entry.git_ref.clone()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))?;
                    if self.legacy.get(&ref_key).and_then(|r| r.as_ref().ok()).is_some_and(|(old, _)| *old != revision.commit) {
                        return Err(format!("KCI4051 {uses}: the source ref changed during preflight; start the run again"));
                    }
                    self.legacy.insert(ref_key.clone(), Ok((revision.commit.clone(), revision.reference.clone())));
                    self.origins.insert(ref_key.clone(), remote.clone());
                    self.revisions.insert(ref_key, revision.clone());
                    revision
                }
            };
            super::proposals::check_pin(self.hub, self.caller, &revision.identity, &wanted).await?;
            let reachable = self.source.contains(revision.clone(), wanted.clone()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))?;
            let identity = match &revision.identity {
                Identity::Github { hostname, owner_id, repo_id } => kiln::Identity::Github { hostname: hostname.clone(), owner_id: *owner_id, repo_id: *repo_id },
                Identity::Native { repo_id } => kiln::Identity::Kiln { id: repo_id.clone() },
            };
            let seen = self.history(remote, &revision).await?;
            let accepted_moves = seen.accepted_moves(key);
            let checked = verify_kiln::check_git(verify_kiln::Check {
                workflow: &layer.workflow, key, entry, facts: &verify_kiln::GitFacts { identity, reference: revision.reference.clone(), current: revision.commit.clone(), reachable: if reachable { [wanted].into() } else { BTreeSet::new() }, previous: seen.previous.clone() },
                home_host: &layer.home, policy: self.lock_policy, accepted_moves: &accepted_moves,
            });
            self.remember(&seen, uses, &layer.workflow, true, checked.is_ok()).await?;
            let checked = checked.map_err(|e| e.diagnostics.iter().map(|d| format!("{}: {}", layer.repository, d.render(kiln::PATH))).collect::<Vec<_>>().join("\n"))?;
            self.warnings.extend(checked.warnings.iter().map(|d| format!("{}: {}", layer.repository, d.render(kiln::PATH))));
            self.allowed_pinned(&format!("{}@{}", remote.named(), checked.selection.commit))?;
            return Ok(Some((checked.selection.commit, Some(revision), true)));
        }
        Ok(None)
    }
}
