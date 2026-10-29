//! Going on on another machine at a usage limit, for a conversation that opted in
//! (`State::move_on_limit`; docs/agents.md, "Usage limits: pause and resume").
//!
//! When such a session pauses for a limit (reached, or its stop point), the server looks for
//! another of your machines to take it over (only the one, and the harness and model, a person
//! chose, when they did: `State::move_plan`): connected and taking sessions over (reach), with the
//! same harness signed in to another account, one kiln doesn't know to be as full. One that has the
//! session's repository already comes first, then the emptiest account; else the repository is
//! copied there and kept in step. A session whose folder isn't a repository kiln knows doesn't go:
//! its files couldn't come along. The session moves there (`relocate`), and there its harness,
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
        Ok(c) => match super::relocate::start(hub, &info.id, &c.name, false, c.kind.clone(), c.model.clone()).await {
            Ok(_) => {
                if let Some(t) = TRIED.lock().get_mut(&info.id).filter(|t| t.since == since) {
                    t.to = Some(c.name);
                }
                return;
            }
            Err(e) => format!("{} couldn't take it over ({e})", c.name),
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
struct Choice {
    name: String,
    /// Another harness it goes on in there, its conversation carried over (None: its own), and
    /// the model it goes on with there.
    kind: Option<String>,
    model: Option<String>,
    /// The session's repository has a copy there already.
    has_repo: bool,
    /// The most used of its account's limits that kiln knows of (0 when it knows none).
    used: f64,
}

/// The best of the machines that can take it over: its own harness first (its conversation goes
/// as it is), then one with the repository, then the emptiest account.
fn rank(mut options: Vec<Choice>) -> Option<Choice> {
    options.sort_by(|a, b| a.kind.is_some().cmp(&b.kind.is_some()).then(b.has_repo.cmp(&a.has_repo)).then(a.used.total_cmp(&b.used)).then(a.name.cmp(&b.name)));
    options.into_iter().next()
}

/// How full an account is, as its sessions' limits say: the most used window that hasn't reset.
fn fullest(limits: &[&Limit], now: i64) -> f64 {
    limits
        .iter()
        .filter(|l| l.unit.is_none() && l.resets_at.is_none_or(|r| r > now))
        .map(|l| if l.status.as_deref() == Some("rejected") { 100.0 } else { l.used_percent.unwrap_or(0.0) })
        .fold(0.0, f64::max)
}

/// Where `info` goes on: the machine (and harness) that takes it over best, or why none can. Its
/// own harness signed in to another account; else another harness its conversation can be carried
/// into (any other kiln runs as a structured session), signed in with room.
async fn choose(hub: &Arc<Hub>, info: &Info) -> Result<Choice, String> {
    use super::kinds::Kind;
    let Some(Location::Machine(from)) = hub.harness.location(&info.id) else { return Err("it runs on the kiln server, not on one of your machines".into()) };
    // Where a person said it goes on, if they did: that machine alone, that harness alone.
    let plan = &info.move_plan;
    let mut others: Vec<_> = hub.reach_directory().into_iter().filter(|r| r.name != from && r.connected).collect();
    if let Some(m) = plan.machine() {
        others.retain(|r| r.name == m);
        if others.is_empty() {
            return Err(if m == from { format!("it runs on {m}, where you chose it to go on") } else { format!("{m}, where you chose it to go on, isn't connected") });
        }
    } else if others.is_empty() {
        return Err("no other machine of yours is connected".into());
    }
    // The harnesses it can go on in: its own, and those its conversation can be carried into.
    let carries = Kind::get(&info.kind).is_some_and(Kind::switchable);
    let fits = |h: &str| (h == info.kind || (carries && Kind::get(h).is_some_and(Kind::switchable))) && plan.kind.as_deref().is_none_or(|k| k == h);
    let named = |h: &str| Kind::get(h).map_or_else(|| h.to_owned(), |k| k.name.to_owned());
    let with_one: Vec<_> = others.into_iter().filter(|r| r.harnesses.iter().any(|h| fits(h))).collect();
    if with_one.is_empty() {
        let want = plan.kind.as_deref().map_or_else(|| if carries { format!("{} or a harness it can go on in", info.kind_name) } else { info.kind_name.clone() }, named);
        return Err(match plan.machine() {
            Some(m) => format!("{m}, where you chose it to go on, doesn't have {want} installed"),
            None => format!("no other connected machine has {want} installed"),
        });
    }
    let taking: Vec<_> = with_one.into_iter().filter(|r| r.reach && hub.instances.lock().newest_with(&r.name, crate::proto::CAP_REACH).is_some()).collect();
    if taking.is_empty() {
        return Err(format!("the machines with {} don't take sessions over from others", info.kind_name));
    }
    let has_repo = repo_machines(hub, &from, &info.cwd).await;
    // Its files must come along: a folder that isn't a repository kiln knows would be left behind,
    // and the agent would go on without its work. A person can still move it (Move to…).
    if has_repo.is_empty() {
        return Err(format!("its folder, {}, isn't a git repository kiln knows on {from}, so its files couldn't come along", info.cwd));
    }
    // As full as this, it would stop again there at once.
    let cap = info.pause_at.map_or(100.0, f64::from);
    let now = crate::util::now_ms();
    let sessions = hub.harness.list();
    let kinds = hub.harness.kinds();
    let mut options = Vec::new();
    let mut why = Vec::new();
    for r in taking {
        let accounts = crate::accounts::status(hub, &format!("agent:{}", r.name)).await.unwrap_or_default();
        for h in r.harnesses.iter().filter(|h| fits(h)) {
            let Some(account) = accounts.iter().find(|a| a.kind == *h && a.signed_in == Some(true)) else {
                why.push(format!("{} isn't signed in on {}", named(h), r.name));
                continue;
            };
            let used = match account.email.as_deref() {
                Some(who) => {
                    if *h == info.kind && info.account.as_deref().and_then(|mine| account_is(mine, h, who)) == Some(true) {
                        why.push(format!("{} on {} is signed in to the same account", named(h), r.name));
                        continue;
                    }
                    let limits: Vec<&Limit> = sessions.iter().filter(|o| o.account.as_deref().and_then(|a| account_is(a, h, who)) == Some(true)).flat_map(|o| o.limits.iter()).collect();
                    fullest(&limits, now)
                }
                // Signed in without an address (an API key): no plan limits to share.
                None => 0.0,
            };
            if used >= cap {
                why.push(format!("the account {} uses on {} is {}% used", named(h), r.name, used.round()));
                continue;
            }
            // Another harness: the model chosen for it, else the first it lists there (the newest
            // of its family).
            let (kind, model) = if *h == info.kind {
                (None, None)
            } else if let Some(m) = plan.model.clone().filter(|_| plan.kind.as_deref() == Some(h.as_str())) {
                (Some(h.clone()), Some(m))
            } else {
                let first = kinds.get(&format!("agent:{}", r.name)).and_then(|ks| ks.iter().find(|k| k.id == *h)).and_then(|k| k.models.first()).map(|m| m.id.clone());
                (Some(h.clone()), first)
            };
            options.push(Choice { name: r.name.clone(), kind, model, has_repo: has_repo.contains(&r.name), used });
        }
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

    fn option(name: &str, has_repo: bool, used: f64) -> Choice {
        Choice { name: name.into(), kind: None, model: None, has_repo, used }
    }

    fn best(options: Vec<Choice>) -> Option<String> {
        rank(options).map(|c| c.name)
    }

    #[test]
    fn a_plan_says_where_and_in_what_and_an_old_command_says_neither() -> anyhow::Result<()> {
        use super::super::model::MovePlan;
        let c: Command = serde_json::from_str(r#"{"type":"move_on_limit","on":true,"to":"agent:mac","kind":"codex","model":"gpt-6.1"}"#)?;
        let Command::MoveOnLimit { on, to, kind, model } = c else { anyhow::bail!("not a MoveOnLimit") };
        let plan = MovePlan { to, kind, model };
        assert!(on);
        assert_eq!(plan.machine(), Some("mac"));
        assert_eq!(plan.kind.as_deref(), Some("codex"));
        // From before a plan could be given: on, and nothing said of where.
        let c: Command = serde_json::from_str(r#"{"type":"move_on_limit","on":true}"#)?;
        let Command::MoveOnLimit { to, kind, model, .. } = c else { anyhow::bail!("not a MoveOnLimit") };
        assert!(MovePlan { to, kind, model }.is_empty());
        // A state always says its plan (so a viewer knows its runner takes one), empty or not.
        let v = serde_json::to_value(super::super::model::State::default())?;
        assert_eq!(v.get("move_plan"), Some(&serde_json::json!({})));
        Ok(())
    }

    #[test]
    fn a_machine_with_the_repository_comes_first_then_the_emptiest_account() {
        assert_eq!(best(vec![option("a", false, 0.0), option("b", true, 60.0)]).as_deref(), Some("b"));
        assert_eq!(best(vec![option("a", true, 40.0), option("b", true, 10.0)]).as_deref(), Some("b"));
        assert_eq!(best(vec![option("b", false, 0.0), option("a", false, 0.0)]).as_deref(), Some("a"));
        assert_eq!(best(Vec::new()), None);
    }

    #[test]
    fn its_own_harness_comes_before_another_its_conversation_is_carried_into() {
        let codex = Choice { kind: Some("codex".into()), model: Some("gpt-6.1-sol".into()), ..option("b", true, 0.0) };
        let picked = rank(vec![codex.clone(), option("a", false, 50.0)]);
        assert_eq!(picked.map(|c| (c.name, c.kind)), Some(("a".to_owned(), None)));
        // With only another harness to go on in, it goes there, with that harness's model.
        let picked = rank(vec![codex]);
        assert_eq!(picked.map(|c| (c.kind, c.model)), Some((Some("codex".into()), Some("gpt-6.1-sol".into()))));
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
