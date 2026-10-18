//! Moving an agent session to another runner, as the server sees to it (`docs/reach.md` §9).
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
fn within(cwd: &str, root: &str) -> Option<String> {
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

/// Start moving session `id` to runner `to`: Ok once it is under way (what it does next shows in
/// its timeline), or why it can't.
pub async fn start(hub: &Arc<Hub>, id: &str, to: &str) -> Result<serde_json::Value, String> {
    let to = to.strip_prefix("agent:").unwrap_or(to).to_owned();
    let info = hub.harness.list().into_iter().find(|i| i.id == id).ok_or_else(|| "no such agent".to_owned())?;
    let from = match hub.harness.location(id) {
        Some(Location::Machine(m)) => m,
        _ => return Err("only an agent on a runner moves (this one runs on the kiln server)".into()),
    };
    if from == to {
        return Err(format!("it runs on {to} already"));
    }
    let connected = hub.agents.lock().get(&to).is_some_and(|a| a.connected && !a.ephemeral);
    if !connected {
        return Err(format!("{to} is not connected"));
    }
    let installed = hub.harness.kinds().get(&format!("agent:{to}")).is_some_and(|ks| ks.iter().any(|k| k.id == info.kind && k.available));
    if !installed {
        return Err(format!("{} is not installed on {to}", info.kind_name));
    }
    for m in [&from, &to] {
        if hub.instances.lock().newest_with(m, crate::proto::CAP_REACH).is_none() {
            return Err(format!("{m}'s runner can't hand sessions over (an older runner: upgrade it)"));
        }
    }
    let (hub, id) = (Arc::clone(hub), id.to_owned());
    let to2 = to.clone();
    crate::rt::spawn(async move {
        if let Err(e) = prepare_and_go(&hub, &id, &from, &to2, &info.cwd).await {
            say(&hub, &id, &format!("Couldn't move to {to2}: {e}")).await;
        }
    });
    Ok(serde_json::json!({ "moving": to }))
}

async fn say(hub: &Arc<Hub>, id: &str, text: &str) {
    let _ = hub.harness.command(hub, id, Command::Moving { text: text.to_owned() }).await;
}

async fn prepare_and_go(hub: &Arc<Hub>, id: &str, from: &str, to: &str, cwd: &str) -> Result<(), String> {
    let (there, note) = folder(hub, id, from, to, cwd).await?;
    match hub.harness.command(hub, id, Command::MoveTo { to: to.to_owned(), cwd: there, note }).await {
        Sent::Ok => Ok(()),
        _ => Err(format!("{from} is not connected")),
    }
}

/// Where the session goes on on `to`, and what of its folder came along (in words).
async fn folder(hub: &Arc<Hub>, id: &str, from: &str, to: &str, cwd: &str) -> Result<(String, Option<String>), String> {
    let view = crate::sync::places::now(hub).await.map_err(|e| format!("{e:#}"))?;
    let home = hub.places.of(from).map(|h| h.home).filter(|h| !h.is_empty());
    // `~/…` for a path under `from`'s home: the same place under `to`'s.
    let mapped = |path: &str| match home.as_deref().and_then(|h| within(path, h)) {
        Some(rel) if rel.is_empty() => "~".to_owned(),
        Some(rel) => format!("~/{rel}"),
        None => path.to_owned(),
    };
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
    // there (never over it).
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
    }
}
