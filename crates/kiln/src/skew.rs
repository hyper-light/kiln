//! How far a runner's kiln may be behind the server's (docs/releases.md, "Version skew"), counted
//! in commits of the release branch before the server's own (its first parents), by sha:
//!
//! - **Full**: a runner on the server's commit or one of the `full` before it (kiln.toml
//!   `[updates] skew_full`) does everything.
//! - **Upgrade only**: one on one of the `upgrade` commits before the server's (`skew_upgrade`),
//!   past those, connects to be brought up to date, and takes no new work (jobs, agents,
//!   terminals) until it is.
//! - **Refused**: one further behind is told why and how to catch up, and tries again a minute
//!   later.
//!
//! A runner ahead of the server, on a commit off the branch (a build on purpose) or of a local
//! tree is left alone, as the fleet leaves it (`crate::fleet::Standing`); so is one that says no
//! commit (a kiln from before it said one), which the fleet upgrades as it does any behind.
//! Where the server can't tell (it knows no commit of its own, or can't read its release
//! repository), every runner does everything: kiln never shuts a fleet out on what it can't see.
//! System → Updates sets both tolerances, and lists the commits each takes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::hub::Hub;
use crate::instances::PeerId;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the tolerances chosen in System → Updates are kept (none: kiln.toml's).
const KEY: &str = "updates/skew";

/// The most of either tolerance (commits): a window this deep is read in one git call.
pub const MOST: u32 = 5000;

/// How many commits behind the server's a runner may be, and still do everything (`full`) or
/// connect to be upgraded (`upgrade`, never less than `full`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tolerance {
    pub full: u32,
    pub upgrade: u32,
}

impl Tolerance {
    pub fn of(u: &crate::config::Updates) -> Self {
        Self { full: u.skew_full, upgrade: u.skew_upgrade }.clamped()
    }

    /// Within [`MOST`], and `upgrade` at least `full`.
    #[must_use]
    pub fn clamped(self) -> Self {
        let full = self.full.min(MOST);
        Self { full, upgrade: self.upgrade.clamp(full, MOST) }
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.full <= MOST && self.upgrade <= MOST, "a tolerance is at most {MOST} commits");
        anyhow::ensure!(self.upgrade >= self.full, "the upgrade tolerance ({}) can't be less than the full one ({})", self.upgrade, self.full);
        Ok(())
    }
}

/// The tolerances now: System → Updates', else kiln.toml's.
pub async fn tolerance(hub: &Hub) -> Tolerance {
    if let Some(t) = *hub.updates.skew.lock() {
        return t;
    }
    let chosen = hub.store.kv_get(KEY.to_owned()).await.ok().flatten().and_then(|v| serde_json::from_str::<Tolerance>(&v).ok());
    let t = chosen.unwrap_or_else(|| Tolerance::of(&hub.cfg.updates)).clamped();
    *hub.updates.skew.lock() = Some(t);
    t
}

/// The tolerances were chosen in System → Updates: kiln.toml's don't apply.
pub async fn changed(hub: &Hub) -> bool {
    hub.store.kv_get(KEY.to_owned()).await.ok().flatten().is_some()
}

/// These tolerances from now on (`None`: as kiln.toml says). Every connected runner is weighed
/// again against them.
pub async fn set(hub: &Arc<Hub>, t: Option<Tolerance>) -> anyhow::Result<()> {
    match t {
        Some(t) => {
            t.check()?;
            hub.store.kv_set(KEY.to_owned(), serde_json::to_string(&t)?).await?;
        }
        None => hub.store.kv_delete(KEY.to_owned()).await?,
    }
    *hub.updates.skew.lock() = None;
    hub.updates.skew_seen.lock().clear();
    hub.skew_again().await;
    Ok(())
}

/// Where a runner's kiln stands against the server's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(tag = "standing", rename_all = "snake_case")]
pub enum Skew {
    /// Not weighed yet: it hasn't said its commit since it connected (it does at once).
    #[default]
    Pending,
    /// The server's commit, or one of the `full` before it.
    Full { behind: u32 },
    /// Past `full`, within `upgrade`: it is to be upgraded, and takes no new work until it is.
    Upgrade { behind: u32, why: String },
    /// Past `upgrade`: refused.
    Refused { behind: u32, why: String },
    /// Not held to the tolerances, and why (ahead of the server, off the release branch, a local
    /// build, no commit said, or nothing to tell by).
    Alone { why: String },
}

impl Skew {
    /// Whether it takes new work: jobs, agent sessions, terminals.
    pub fn takes_work(&self) -> bool {
        !matches!(self, Self::Upgrade { .. } | Self::Refused { .. })
    }

    /// Why it takes no new work, when it doesn't.
    pub fn holds(&self) -> Option<&str> {
        match self {
            Self::Upgrade { why, .. } | Self::Refused { why, .. } => Some(why),
            _ => None,
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Where a runner `behind` commits behind the server's stands, under `t` (`runner` and `server`
/// the commits, for what it's told).
pub fn tier(behind: u32, t: Tolerance, runner: &str, server: &str) -> Skew {
    let t = t.clamped();
    if behind <= t.full {
        return Skew::Full { behind };
    }
    if behind <= t.upgrade {
        return Skew::Upgrade {
            behind,
            why: format!(
                "its kiln ({}) is {behind} commits behind the server's ({}), past the {} that do everything: it takes no new work until it's upgraded (System → Updates)",
                short(runner),
                short(server),
                t.full
            ),
        };
    }
    Skew::Refused {
        behind,
        why: format!(
            "this runner's kiln ({}) is {behind} commits behind the server's ({}), past the {} the server lets connect (System → Updates, \"Version skew\"): upgrade it here with a newer kiln (`kiln install --agent --upgrade`), or raise the server's upgrade tolerance",
            short(runner),
            short(server),
            t.upgrade
        ),
    }
}

/// One of the commits the tolerances take, as System → Updates lists them.
#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    /// How many commits before the server's it is (0: the server's own).
    pub behind: u32,
    /// `full` or `upgrade`.
    pub tier: &'static str,
    pub subject: String,
    pub at: i64,
}

/// The server's commit and the release branch's first parents before it the tolerances take,
/// newest first. None where the server knows no commit of its own, or its release repository
/// can't be read now.
pub async fn window(hub: &Hub, t: Tolerance) -> Option<Vec<Commit>> {
    let server = crate::api::COMMIT;
    if server.is_empty() {
        return None;
    }
    let repo = crate::fleet::release_repo(hub).await?;
    let mirror = crate::gitops::ensure_commit(hub, &repo, server).await.ok()?.to_string_lossy().to_string();
    let n = (t.upgrade + 1).to_string();
    let out = crate::gitops::run_git(hub, &["-C", &mirror, "log", "--first-parent", "-n", &n, "--format=%H%x09%ct%x09%s", server], GIT_TIMEOUT).await.ok()?;
    Some(
        String::from_utf8_lossy(&out)
            .lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let mut p = l.splitn(3, '\t');
                let (sha, at, subject) = (p.next()?, p.next()?, p.next().unwrap_or_default());
                let behind = u32::try_from(i).ok()?;
                Some(Commit { sha: sha.to_owned(), behind, tier: if behind <= t.full { "full" } else { "upgrade" }, subject: subject.to_owned(), at: at.parse().unwrap_or(0) })
            })
            .collect(),
    )
}

/// Where a runner on `commit` (None: it said none) stands against the server's kiln now.
pub async fn weigh(hub: &Hub, commit: Option<&str>) -> Skew {
    let server = crate::api::COMMIT;
    let alone = |why: &str| Skew::Alone { why: why.to_owned() };
    let Some(c) = commit.map(str::trim).filter(|c| !c.is_empty()) else {
        return alone("it says no commit (a kiln from before it did): upgraded as any behind");
    };
    if server.is_empty() {
        return alone("the server knows no commit of its own to count from");
    }
    if c.eq_ignore_ascii_case(server) {
        return Skew::Full { behind: 0 };
    }
    let t = tolerance(hub).await;
    if let Some(s) = hub.updates.skew_seen.lock().get(c) {
        return s.clone();
    }
    let s = weigh_on_branch(hub, c, server, t).await;
    // What the repository couldn't tell is asked again next time.
    if !matches!(&s, Skew::Alone { why } if why.contains("can't")) {
        hub.updates.skew_seen.lock().insert(c.to_owned(), s.clone());
    }
    s
}

async fn weigh_on_branch(hub: &Hub, c: &str, server: &str, t: Tolerance) -> Skew {
    let alone = |why: &str| Skew::Alone { why: why.to_owned() };
    let Some(repo) = crate::fleet::release_repo(hub).await else {
        return alone("the server can't read its release repository now");
    };
    let Ok(mirror) = crate::gitops::ensure_commit(hub, &repo, server).await else {
        return alone("the server can't read its release repository now");
    };
    let mirror = mirror.to_string_lossy().to_string();
    if crate::gitops::ensure_commit(hub, &repo, c).await.is_err() {
        return alone("a commit the release repository doesn't have (a build on purpose)");
    }
    if !crate::fleet::is_ancestor(hub, &mirror, c, server).await {
        return if crate::fleet::is_ancestor(hub, &mirror, server, c).await { alone("ahead of the server") } else { alone("off the release branch (a build on purpose)") };
    }
    let range = format!("{c}..{server}");
    let Some(behind) = crate::gitops::run_git(hub, &["-C", &mirror, "rev-list", "--first-parent", "--count", &range], GIT_TIMEOUT)
        .await
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<u32>().ok())
    else {
        return alone("the server can't read its release repository now");
    };
    tier(behind, t, c, server)
}

impl Hub {
    /// Runner process `peer` of `agent` said its kiln's commit: it's weighed against the server's
    /// (in the background: the release repository may have to fetch). Refused, it's told so; the
    /// machine's newest process decides whether the machine takes new work.
    pub(crate) fn skew_check(self: &Arc<Self>, agent: &str, peer: PeerId, commit: Option<String>) {
        let (hub, agent) = (Arc::clone(self), agent.to_owned());
        crate::rt::spawn(async move {
            let s = weigh(&hub, commit.as_deref()).await;
            hub.skew_weighed(&agent, peer, s).await;
        });
    }

    async fn skew_weighed(&self, agent: &str, peer: PeerId, s: Skew) {
        if let Skew::Refused { why, .. } = &s {
            tracing::warn!(%agent, peer, reason = %why, "a runner too far behind the server's kiln: refused");
            self.send_peer(peer, &crate::proto::ServerMsg::Refused { reason: why.clone() }).await;
            return;
        }
        if self.instances.lock().newest(agent) != Some(peer) {
            return;
        }
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(agent) else { return };
            if a.skew == s {
                return;
            }
            if let Some(why) = s.holds() {
                tracing::info!(%agent, %why, "a runner behind the server's kiln takes no new work until it's upgraded");
            }
            a.skew = s;
            a.clone()
        };
        self.emit_agent(&snapshot);
        // Its legs: none while it's to be upgraded (CI's dispatch counts no slots for it).
        self.ci.send(crate::ci::dispatch::Msg::RunnerFacts { agent: agent.to_owned() });
        self.wake.notify();
        self.emit(&serde_json::json!({ "type": "updates" }));
    }

    /// Every connected runner process weighed again (the tolerances changed).
    pub(crate) async fn skew_again(self: &Arc<Self>) {
        let procs: Vec<(String, PeerId, Option<String>)> = {
            let inst = self.instances.lock();
            self.agents.lock().values().filter(|a| a.connected).flat_map(|a| inst.of(&a.name).into_iter().map(|p| (a.name.clone(), p, inst.get(p).and_then(|i| i.kiln.as_ref().map(|k| k.commit.clone()))))).collect()
        };
        for (agent, peer, commit) in procs {
            self.skew_check(&agent, peer, commit);
        }
    }

    /// Why machine `agent` takes no new work now, when it doesn't (its kiln is to be upgraded).
    pub fn skew_holds(&self, agent: &str) -> Option<String> {
        self.agents.lock().get(agent).and_then(|a| a.skew.holds().map(|w| format!("{agent}: {w}")))
    }
}

/// What the tolerances took, by commit, until they change (`crate::updates::Updates::skew_seen`).
pub type Seen = HashMap<String, Skew>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_is_full_within_full_upgrade_only_within_upgrade_and_refused_past_it() {
        let t = Tolerance { full: 3, upgrade: 10 };
        let (r, s) = ("aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb");
        assert_eq!(tier(0, t, r, s), Skew::Full { behind: 0 });
        assert_eq!(tier(3, t, r, s), Skew::Full { behind: 3 });
        let up = tier(4, t, r, s);
        assert!(matches!(&up, Skew::Upgrade { behind: 4, why } if why.contains("4 commits behind") && why.contains("aaaaaaaaaaaa")), "{up:?}");
        assert!(!up.takes_work() && up.holds().is_some());
        assert!(matches!(tier(10, t, r, s), Skew::Upgrade { behind: 10, .. }));
        let refused = tier(11, t, r, s);
        assert!(matches!(&refused, Skew::Refused { behind: 11, why } if why.contains("past the 10")), "{refused:?}");
        assert!(!refused.takes_work());
        // Left alone, or not weighed yet: it does everything.
        assert!(Skew::Pending.takes_work() && Skew::Alone { why: "x".into() }.takes_work() && Skew::Full { behind: 2 }.takes_work());
    }

    #[test]
    fn the_upgrade_tolerance_is_never_less_than_the_full_one() {
        assert_eq!(Tolerance { full: 8, upgrade: 2 }.clamped(), Tolerance { full: 8, upgrade: 8 });
        assert_eq!(Tolerance { full: MOST + 9, upgrade: MOST + 9 }.clamped(), Tolerance { full: MOST, upgrade: MOST });
        assert!(Tolerance { full: 8, upgrade: 2 }.check().is_err());
        assert!(Tolerance { full: 0, upgrade: MOST + 1 }.check().is_err());
        assert!(Tolerance { full: 0, upgrade: 0 }.check().is_ok());
        // Strict: only the server's own commit.
        assert!(matches!(tier(1, Tolerance { full: 0, upgrade: 0 }, "a", "b"), Skew::Refused { behind: 1, .. }));
    }

    #[test]
    fn a_skew_says_where_it_stands_for_the_ui() -> anyhow::Result<()> {
        let v = serde_json::to_value(Skew::Upgrade { behind: 5, why: "w".into() })?;
        assert_eq!(v, serde_json::json!({"standing": "upgrade", "behind": 5, "why": "w"}));
        assert_eq!(serde_json::to_value(Skew::Pending)?, serde_json::json!({"standing": "pending"}));
        Ok(())
    }
}
