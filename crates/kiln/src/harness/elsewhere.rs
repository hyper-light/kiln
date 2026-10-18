//! Going on on another machine at a usage limit, for a conversation that opted in
//! (`State::move_on_limit`; docs/agents.md, "Usage limits: pause and resume").
//!
//! When such a session pauses for a limit (reached, or its stop point), the server looks for
//! another of your machines to take it over: connected and taking sessions over (reach), with the
//! same harness signed in to another account, one kiln doesn't know to be as full. One that has the
//! session's repository already comes first, then the emptiest account; else the repository is
//! copied there and kept in step. The session moves there (`relocate`), and there its harness,
//! signed in as someone else, ends the pause and goes on with the turn the limit cut off. With
//! nowhere to go it waits for the reset where it is, its timeline says why, and kiln looks again
//! every few minutes.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use super::model::{account_is, Command, Limit, Status};
use super::registry::Location;
use super::session::Info;
use crate::hub::Hub;

/// A paused session with nowhere to go is looked at again this often.
const AGAIN_MS: i64 = 5 * 60_000;
/// A session that arrived is given this long to end its pause there (its harness says who it's
/// signed in as) before kiln looks for somewhere else.
const SETTLE_MS: i64 = 3 * 60_000;
/// A move that hasn't arrived (its repository is being copied over) is waited for this long.
const MOVE_FOR_MS: i64 = 35 * 60_000;

/// What was last done for a paused session.
#[derive(Debug, Clone)]
struct Tried {
    /// The pause it was for (`Pause::since`).
    since: i64,
    at: i64,
    /// Where it went, once it went.
    to: Option<String>,
    /// When it was there.
    arrived: Option<i64>,
    /// Why it couldn't, as last said (said again only when it changes).
    why: String,
}

static TRIED: LazyLock<Mutex<HashMap<String, Tried>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every few seconds (`goals::run`): the paused sessions that opted in look for a machine to go on
/// on, once a pause, then again now and then.
pub fn tick(hub: &Arc<Hub>, all: &[Info], now: i64) {
    let mut tried = TRIED.lock();
    tried.retain(|id, _| all.iter().any(|i| &i.id == id && i.pause.is_some() && i.move_on_limit));
    for info in all {
        let Some(p) = info.pause.as_ref() else { continue };
        if !info.move_on_limit || matches!(info.status, Status::Exited) {
            continue;
        }
        if let Some(t) = tried.get_mut(&info.id).filter(|t| t.since == p.since && t.arrived.is_none())
            && let Some(to) = t.to.as_deref()
            && matches!(hub.harness.location(&info.id), Some(Location::Machine(m)) if m == to)
        {
            t.arrived = Some(now);
        }
        let due = match tried.get(&info.id) {
            None => true,
            Some(t) if t.since != p.since => true,
            Some(Tried { to: Some(_), arrived: Some(at), .. }) => now - at >= SETTLE_MS,
            Some(t) if t.to.is_some() => now - t.at >= MOVE_FOR_MS,
            Some(t) => now - t.at >= AGAIN_MS,
        };
        if !due {
            continue;
        }
        let said = tried.get(&info.id).filter(|t| t.since == p.since).map(|t| t.why.clone()).unwrap_or_default();
        tried.insert(info.id.clone(), Tried { since: p.since, at: now, to: None, arrived: None, why: said.clone() });
        let (hub, info, since) = (Arc::clone(hub), info.clone(), p.since);
        crate::rt::spawn(async move { go(&hub, &info, since, &said).await });
    }
}

async fn go(hub: &Arc<Hub>, info: &Info, since: i64, said: &str) {
    let why = match choose(hub, info).await {
        Ok(to) => match super::relocate::start(hub, &info.id, &to, false).await {
            Ok(_) => {
                if let Some(t) = TRIED.lock().get_mut(&info.id).filter(|t| t.since == since) {
                    t.to = Some(to);
                }
                return;
            }
            Err(e) => format!("{to} couldn't take it over ({e})"),
        },
        Err(why) => why,
    };
    if let Some(t) = TRIED.lock().get_mut(&info.id).filter(|t| t.since == since) {
        t.why.clone_from(&why);
    }
    if why != said {
        let text = format!("No other machine can take it over now: {why}. It waits for the reset here; kiln looks again every few minutes.");
        let _ = hub.harness.command(hub, &info.id, Command::Moving { text }).await;
    }
}

/// A machine that could take a session over, as `rank` weighs it.
#[derive(Debug, Clone, PartialEq)]
struct Option_ {
    name: String,
    /// The session's repository has a copy there already.
    has_repo: bool,
    /// The most used of its account's limits that kiln knows of (0 when it knows none).
    used: f64,
}

/// The best of the machines that can take it over: one with the repository first, then the
/// emptiest account.
fn rank(mut options: Vec<Option_>) -> Option<String> {
    options.sort_by(|a, b| b.has_repo.cmp(&a.has_repo).then(a.used.total_cmp(&b.used)).then(a.name.cmp(&b.name)));
    options.into_iter().next().map(|o| o.name)
}

/// How full an account is, as its sessions' limits say: the most used window that hasn't reset.
fn fullest(limits: &[&Limit], now: i64) -> f64 {
    limits
        .iter()
        .filter(|l| l.unit.is_none() && l.resets_at.is_none_or(|r| r > now))
        .map(|l| if l.status.as_deref() == Some("rejected") { 100.0 } else { l.used_percent.unwrap_or(0.0) })
        .fold(0.0, f64::max)
}

/// Where `info` goes on: the machine that takes it over best, or why none can.
async fn choose(hub: &Arc<Hub>, info: &Info) -> Result<String, String> {
    let Some(Location::Machine(from)) = hub.harness.location(&info.id) else { return Err("it runs on the kiln server, not on one of your machines".into()) };
    let others: Vec<_> = hub.reach_directory().into_iter().filter(|r| r.name != from && r.connected).collect();
    if others.is_empty() {
        return Err("no other machine of yours is connected".into());
    }
    let with_it: Vec<_> = others.into_iter().filter(|r| r.harnesses.contains(&info.kind)).collect();
    if with_it.is_empty() {
        return Err(format!("no other connected machine has {} installed", info.kind_name));
    }
    let taking: Vec<_> = with_it.into_iter().filter(|r| r.reach && hub.instances.lock().newest_with(&r.name, crate::proto::CAP_REACH).is_some()).collect();
    if taking.is_empty() {
        return Err(format!("the machines with {} don't take sessions over from others", info.kind_name));
    }
    let has_repo = repo_machines(hub, &from, &info.cwd).await;
    // As full as this, it would stop again there at once.
    let cap = info.pause_at.map_or(100.0, f64::from);
    let now = crate::util::now_ms();
    let sessions = hub.harness.list();
    let mut options = Vec::new();
    let mut why = Vec::new();
    for r in taking {
        let account = crate::accounts::status(hub, &format!("agent:{}", r.name)).await.ok().and_then(|a| a.into_iter().find(|a| a.kind == info.kind));
        let Some(account) = account.filter(|a| a.signed_in == Some(true)) else {
            why.push(format!("{} isn't signed in on {}", info.kind_name, r.name));
            continue;
        };
        let used = match account.email.as_deref() {
            Some(who) => {
                if info.account.as_deref().and_then(|mine| account_is(mine, &info.kind, who)) == Some(true) {
                    why.push(format!("{} on {} is signed in to the same account", info.kind_name, r.name));
                    continue;
                }
                let limits: Vec<&Limit> = sessions.iter().filter(|o| o.account.as_deref().and_then(|a| account_is(a, &info.kind, who)) == Some(true)).flat_map(|o| o.limits.iter()).collect();
                fullest(&limits, now)
            }
            // Signed in without an address (an API key): no plan limits to share.
            None => 0.0,
        };
        if used >= cap {
            why.push(format!("the account {} uses on {} is {}% used", info.kind_name, r.name, used.round()));
            continue;
        }
        options.push(Option_ { has_repo: has_repo.contains(&r.name), name: r.name, used });
    }
    rank(options).ok_or_else(|| why.join("; "))
}

/// The machines with a copy of the repository `cwd` (on `from`) is in.
async fn repo_machines(hub: &Arc<Hub>, from: &str, cwd: &str) -> Vec<String> {
    let Ok(view) = crate::sync::places::now(hub).await else { return Vec::new() };
    view.repos
        .iter()
        .find(|r| r.places.iter().any(|p| p.machine == from && super::relocate::within(cwd, &p.path).is_some()))
        .map(|r| r.places.iter().map(|p| p.machine.clone()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(name: &str, has_repo: bool, used: f64) -> Option_ {
        Option_ { name: name.into(), has_repo, used }
    }

    #[test]
    fn a_machine_with_the_repository_comes_first_then_the_emptiest_account() {
        assert_eq!(rank(vec![option("a", false, 0.0), option("b", true, 60.0)]).as_deref(), Some("b"));
        assert_eq!(rank(vec![option("a", true, 40.0), option("b", true, 10.0)]).as_deref(), Some("b"));
        assert_eq!(rank(vec![option("b", false, 0.0), option("a", false, 0.0)]).as_deref(), Some("a"));
        assert_eq!(rank(Vec::new()), None);
    }

    #[test]
    fn an_account_is_as_full_as_its_fullest_window_that_hasnt_reset() {
        let window = |used: f64, resets_at: i64, status: Option<&str>| Limit { id: "w".into(), used_percent: Some(used), resets_at: Some(resets_at), status: status.map(str::to_owned), ..Limit::default() };
        let (a, b, c) = (window(40.0, 2_000, None), window(97.0, 500, None), window(80.0, 2_000, Some("rejected")));
        assert_eq!(fullest(&[&a, &b], 1_000), 40.0);
        assert_eq!(fullest(&[&a, &c], 1_000), 100.0);
        assert_eq!(fullest(&[], 1_000), 0.0);
    }

    #[test]
    fn an_account_id_says_whether_it_is_someone_without_saying_who() {
        let id = super::super::model::account_id("claude", "Max", Some("Ada@Example.com"));
        assert_eq!(account_is(&id, "claude", "ada@example.com"), Some(true));
        assert_eq!(account_is(&id, "claude", "someone@else.com"), Some(false));
        assert_eq!(account_is(&id, "codex", "ada@example.com"), Some(false));
        assert_eq!(account_is(&super::super::model::account_id("claude", "API key", None), "claude", "ada@example.com"), None);
    }
}
