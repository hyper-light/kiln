//! Moving an agent session to another runner, as the server sees to it (`docs/reach.md` §8).
//!
//! The server checks the runner it goes to (connected, the same harness installed, reach on both
//! ends), then sees to the folder: a session working in a git repository goes on in that
//! repository's copy on the other machine, kept in step by sync (one there already, or one made
//! now: a sync task copies the repository over, uncommitted work included, and keeps both copies
//! in step from then on). Then it tells the session's runner to move it there; that runner hands
//! the session over itself, over reach, once its turn is over (`super::session`, `super::moving`).
//!
//! What it does as it goes shows in the session's timeline.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::model::Command;
use super::registry::{Location, Sent};
use crate::hub::Hub;
use crate::sync::places::{Place, Row};
use crate::sync::tasks::{Ask, Kind as TaskKind, Source, Target};

/// How long copying a repository over may take.
const COPY_FOR: Duration = Duration::from_secs(30 * 60);

/// `cwd` is `root` or inside it.
pub(super) fn within(cwd: &str, root: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    if cwd == root {
        return Some(String::new());
    }
    cwd.strip_prefix(root).and_then(|r| r.strip_prefix('/')).map(str::to_owned)
}

/// `path` with `rel` under it.
fn join(path: &str, rel: &str) -> String {
    if rel.is_empty() { path.to_owned() } else { format!("{}/{rel}", path.trim_end_matches('/')) }
}

fn homed(path: &str, home: Option<&str>) -> String {
    match home.and_then(|h| within(path, h)) {
        Some(rel) if rel.is_empty() => "~".to_owned(),
        Some(rel) => format!("~/{rel}"),
        None => path.to_owned(),
    }
}

/// Start moving session `id` to runner `to`: Ok once it is under way (what it does next shows in
/// its timeline), or why it can't. `now`: the turn under way doesn't finish here; it is
/// interrupted once the other runner is ready, and picked up there. `kind`: it goes on there in
/// this other harness, its conversation carried over, with `model` (None: that one's default).
///
/// Its subagents (`Info::subagent`) on the machine it leaves go along, each in its own harness, so
/// they stay on their agent's machine (docs/goals.md, "A plan's steps side by side"); one that
/// can't says why in its timeline. A subagent doesn't move by itself while its agent is there.
pub async fn start(hub: &Arc<Hub>, id: &str, to: &str, now: bool, kind: Option<String>, model: Option<String>) -> Result<serde_json::Value, String> {
    let all = hub.harness.list();
    if let Some(p) = all.iter().find(|i| i.id == id).filter(|i| i.subagent).and_then(|i| i.parent.as_deref()).and_then(|p| all.iter().find(|a| a.id == p)) {
        return Err(format!("it's the subagent of {}, and it goes where its agent goes: move {} and it comes along", p.name, p.name));
    }
    let leaves = hub.harness.location(id);
    let moved = start_one(hub, id, to, now, kind, model).await?;
    let to = to.strip_prefix("agent:").unwrap_or(to);
    for c in all.iter().filter(|c| c.subagent && c.parent.as_deref() == Some(id) && c.status != super::model::Status::Exited && hub.harness.location(&c.id) == leaves) {
        if let Err(e) = start_one(hub, &c.id, to, now, None, None).await {
            say(hub, &c.id, &format!("Couldn't go along with its agent to {to}: {e}")).await;
        }
    }
    Ok(moved)
}

/// [`start`], for session `id` alone.
async fn start_one(hub: &Arc<Hub>, id: &str, to: &str, now: bool, kind: Option<String>, model: Option<String>) -> Result<serde_json::Value, String> {
    let to = to.strip_prefix("agent:").unwrap_or(to).to_owned();
    let info = hub.harness.list().into_iter().find(|i| i.id == id).ok_or_else(|| "no such agent".to_owned())?;
    let location = hub.harness.location(id);
    let local = location == Some(Location::Local);
    let from = match location {
        Some(Location::Machine(m)) => m,
        Some(Location::Local) if hub.harness.never_started(id) => "the kiln server".to_owned(),
        _ => return Err("the kiln server keeps the agents it runs once they've started: only one whose harness never started there moves".into()),
    };
    if from == to {
        return Err(format!("it runs on {to} already"));
    }
    // Its workspace is a volume in its machine's memory: it doesn't follow yet (docs/agentic.md W7).
    if info.workspace.as_ref().is_some_and(|w| w.made.is_some()) {
        return Err(format!("it works in a workspace of its own on {from}, which can't move with it yet: open a change request from its work (or write it to disk), then start an agent on {to}"));
    }
    let connected = hub.agents.lock().get(&to).is_some_and(|a| a.connected && !a.ephemeral);
    if !connected {
        return Err(format!("{to} is not connected"));
    }
    // The harness it goes on in there: its own, or one its conversation can be carried into.
    let kind = kind.filter(|k| *k != info.kind);
    let goes_on_in = kind.clone().unwrap_or_else(|| info.kind.clone());
    let named = super::kinds::Kind::get(&goes_on_in).map_or_else(|| goes_on_in.clone(), |k| k.name.to_owned());
    if kind.is_some() {
        let carried = [info.kind.as_str(), goes_on_in.as_str()].iter().all(|k| super::kinds::Kind::get(k).is_some_and(super::kinds::Kind::switchable));
        if !carried {
            return Err(format!("a conversation can't be carried from {} to {named}", info.kind_name));
        }
    }
    let installed = hub.harness.kinds().get(&format!("agent:{to}")).is_some_and(|ks| ks.iter().any(|k| k.id == goes_on_in && k.available));
    if !installed {
        return Err(format!("{named} is not installed on {to}"));
    }
    if local {
        if kind.is_some() {
            return Err(format!("from the kiln server it goes on in {} alone", info.kind_name));
        }
        let there = homed(&info.cwd, Some(&crate::os::home_dir().to_string_lossy()));
        hub.harness.move_local(hub, id, &to, &there).await?;
        return Ok(serde_json::json!({ "moving": to, "from_server": true }));
    }
    // Its machine is away: it can't hand the session over, so it goes on from the server's copy of
    // its conversation (`docs/conversations.md`, "Resuming on another machine").
    if !hub.agents.lock().get(&from).is_some_and(|a| a.connected) {
        if kind.is_some() {
            return Err(format!("{from} is away, so it can only go on in {} (from the server's copy of its conversation)", info.kind_name));
        }
        let (there, _) = folder(hub, id, &from, &to, &info.cwd, true).await?;
        hub.harness.resume_on(hub, id, &to, Some(there)).await?;
        return Ok(serde_json::json!({ "moving": to, "from_store": true }));
    }
    for m in [&from, &to] {
        if hub.instances.lock().newest_with(m, crate::proto::CAP_REACH).is_none() {
            return Err(format!("{m}'s runner can't hand sessions over (an older runner: upgrade it)"));
        }
    }
    let (hub, id) = (Arc::clone(hub), id.to_owned());
    let to2 = to.clone();
    crate::rt::spawn(async move {
        if let Err(e) = prepare_and_go(&hub, &id, &from, &to2, &info.cwd, now, kind, model).await {
            say(&hub, &id, &format!("Couldn't move to {to2}: {e}")).await;
        }
    });
    Ok(serde_json::json!({ "moving": to, "kind": goes_on_in }))
}

/// Recover every agent of machine `from`, which is gone (not connected): each goes on on another
/// machine from the server's copy of its conversation ([`start`]), on `to` when it's given, else on
/// the machine that suits it best (connected, not a job's, its harness installed there; one of
/// `from`'s platform first, then the one running the fewest agents). Agents that ended stay where
/// they were. What became of each: `{id, name, to?, error?}`.
pub async fn recover(hub: &Arc<Hub>, from: &str, to: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
    use std::collections::HashMap;
    let from = from.strip_prefix("agent:").unwrap_or(from).to_owned();
    let (connected, os) = {
        let g = hub.agents.lock();
        let a = g.get(&from);
        (a.is_some_and(|a| a.connected), a.map(|a| a.os.clone()).unwrap_or_default())
    };
    if connected {
        return Err(format!("{from} is connected: its agents move one at a time (Move to…), its runner handing each over"));
    }
    let to = to.map(|t| t.strip_prefix("agent:").unwrap_or(t).trim().to_owned()).filter(|t| !t.is_empty());
    let all = hub.harness.list();
    let on = |id: &str| match hub.harness.location(id) {
        Some(Location::Machine(m)) => Some(m),
        _ => None,
    };
    // A subagent whose agent is there goes along with it ([`start`]).
    let theirs = |i: &super::session::Info| i.subagent && i.parent.as_deref().is_some_and(|p| all.iter().any(|a| a.id == p));
    let agents: Vec<_> = all.iter().filter(|i| on(&i.id).as_deref() == Some(from.as_str()) && i.status != super::model::Status::Exited && !theirs(i)).cloned().collect();
    let kinds = hub.harness.kinds();
    let runners: Vec<(String, String)> = hub.agents.lock().values().filter(|a| a.connected && !a.ephemeral && !a.outpost && a.name != from).map(|a| (a.name.clone(), a.os.clone())).collect();
    let mut load: HashMap<String, usize> = HashMap::new();
    for i in &all {
        if let Some(m) = on(&i.id) {
            *load.entry(m).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for a in agents {
        let fits = |m: &str| kinds.get(&format!("agent:{m}")).is_some_and(|ks| ks.iter().any(|k| k.id == a.kind && k.available));
        let pick = match &to {
            Some(t) => Some(t.clone()),
            None => runners.iter().filter(|(m, _)| fits(m)).min_by_key(|(m, mos)| (*mos != os, load.get(m).copied().unwrap_or(0), m.clone())).map(|(m, _)| m.clone()),
        };
        let Some(t) = pick else {
            out.push(serde_json::json!({"id": a.id, "name": a.name, "error": format!("no connected machine has {} installed", a.kind_name)}));
            continue;
        };
        match start(hub, &a.id, &t, false, None, None).await {
            Ok(_) => {
                *load.entry(t.clone()).or_default() += 1;
                out.push(serde_json::json!({"id": a.id, "name": a.name, "to": t}));
            }
            Err(e) => out.push(serde_json::json!({"id": a.id, "name": a.name, "to": t, "error": e})),
        }
    }
    Ok(out)
}

async fn say(hub: &Arc<Hub>, id: &str, text: &str) {
    let _ = hub.harness.command(hub, id, Command::Moving { text: text.to_owned() }).await;
}

#[allow(clippy::too_many_arguments)]
async fn prepare_and_go(hub: &Arc<Hub>, id: &str, from: &str, to: &str, cwd: &str, now: bool, kind: Option<String>, model: Option<String>) -> Result<(), String> {
    let (there, note) = folder(hub, id, from, to, cwd, false).await?;
    match hub.harness.command(hub, id, Command::MoveTo { to: to.to_owned(), cwd: there, note, now, kind, model, local: false }).await {
        Sent::Ok | Sent::Taken => Ok(()),
        Sent::Unreachable => Err(format!("{from} is not connected")),
        s => Err(s.failure().map_or_else(|| format!("{from} didn't take it"), |(_, why)| why)),
    }
}

/// Where the session goes on on `to`, and what of its folder came along (in words). `away`: `from`
/// is away, so nothing is copied from it.
async fn folder(hub: &Arc<Hub>, id: &str, from: &str, to: &str, cwd: &str, away: bool) -> Result<(String, Option<String>), String> {
    let view = crate::sync::places::now(hub).await.map_err(|e| format!("{e:#}"))?;
    let home = hub.places.of(from).map(|h| h.home).filter(|h| !h.is_empty());
    // `~/…` for a path under `from`'s home: the same place under `to`'s.
    let mapped = |path: &str| homed(path, home.as_deref());
    let found: Option<(&Row, &Place)> = view.repos.iter().find_map(|r| r.places.iter().find(|p| p.machine == from && within(cwd, &p.path).is_some()).map(|p| (r, p)));
    let Some((row, place)) = found else {
        return Ok((mapped(cwd), Some(format!("Its folder isn't a git repository kiln knows, so its files stay on {from}."))));
    };
    let rel = within(cwd, &place.path).unwrap_or_default();
    // A copy kept in step with this one already there: on in it.
    let group = place.copy.as_ref().map(|c| c.repo.clone());
    if let Some(there) = row.places.iter().find(|p| p.machine == to && p.copy.as_ref().map(|c| c.repo.clone()).is_some_and(|g| Some(g) == group)) {
        return Ok((join(&there.path, &rel), Some(format!("Its repository is kept in step between {from} and {to}."))));
    }
    // Else a copy is made there and kept in step from now on: beside a folder of it already
    // there (never over it). Not from a machine that's away: its files stay there.
    if away {
        return Ok((mapped(cwd), Some(format!("{from} is away, so its repository's files stay there."))));
    }
    let mut target = mapped(&place.path);
    let taken = |t: &str| {
        let whole = hub.places.resolve(to, t);
        row.places.iter().any(|p| p.machine == to && Some(&p.path) == whole.as_ref())
    };
    if taken(&target) {
        target = format!("{target}-{from}");
    }
    say(hub, id, &format!("Moving to {to}: copying {} there, kept in step with {from}…", row.name)).await;
    let ask = Ask {
        kind: TaskKind::Sync,
        key: row.key.clone(),
        from: Some(Source::Machine { machine: from.to_owned(), path: place.path.clone() }),
        branch: None,
        to: vec![Target { machine: to.to_owned(), path: target.clone() }],
        delete: false,
        mode: crate::sync::Mode::Files,
    };
    let tasks = crate::sync::tasks::start(hub, ask).await?;
    let task = tasks.into_iter().next().ok_or_else(|| "the copy was not started".to_owned())?;
    let started = Instant::now();
    let mut said = String::new();
    loop {
        crate::rt::sleep(Duration::from_secs(2)).await;
        let Some(t) = hub.tasks.get(&task.id) else { return Err("the copy's task went away".into()) };
        match t.state.as_str() {
            "done" => break,
            "failed" => return Err(t.error.unwrap_or_else(|| "copying the repository failed".into())),
            _ => {
                let now = if t.detail.is_empty() { t.state.clone() } else { t.detail.clone() };
                if now != said {
                    say(hub, id, &format!("Moving to {to}: copying {} there — {now}", row.name)).await;
                    said = now;
                }
            }
        }
        if started.elapsed() > COPY_FOR {
            return Err(format!("copying {} took more than {} minutes", row.name, COPY_FOR.as_secs() / 60));
        }
    }
    let there = hub.places.resolve(to, &target).unwrap_or(target);
    Ok((join(&there, &rel), Some(format!("Its repository came along, uncommitted work and all, and is kept in step between {from} and {to}."))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_inside_a_repository_maps_to_the_same_place_in_its_copy() {
        assert_eq!(within("/Users/ada/Projects/kiln/crates/kiln", "/Users/ada/Projects/kiln").as_deref(), Some("crates/kiln"));
        assert_eq!(within("/Users/ada/Projects/kiln", "/Users/ada/Projects/kiln/").as_deref(), Some(""));
        assert_eq!(within("/Users/ada/Projects/kiln-old", "/Users/ada/Projects/kiln"), None);
        assert_eq!(join("/home/ada/Projects/kiln", "crates/kiln"), "/home/ada/Projects/kiln/crates/kiln");
        assert_eq!(join("~/Projects/kiln", ""), "~/Projects/kiln");
        assert_eq!(homed("/home/kiln/work/app", Some("/home/kiln")), "~/work/app");
        assert_eq!(homed("/tmp", Some("/tmp")), "~");
        assert_eq!(homed("/srv/app", Some("/home/kiln")), "/srv/app");
        assert_eq!(homed("/srv/app", None), "/srv/app");
    }
}
