//! An environment opened for a session's harness (docs/agents.md, "Where they apply"): what its runner is
//! given ([`Resolved`]), each `secret` read from its owner's own as it starts, and opened again whenever
//! the server starts that harness again or finds the runner process that holds the session without it.
//!
//! Where the server starts a harness, each opens it: a start (`harness::lineage::start`), a restart
//! (`harness::registry::Registry::dispatch`'s `Restart`, given the environment first), one resumed from
//! kiln's copy of its conversation (`Registry::resume_on`), one moved from the server to a runner
//! (`Registry::move_local`). A runner keeps a session's environment in its memory alone, so a session that
//! goes on in another runner process (moved over reach, handed to a newer process, its runner started
//! again) is there without it: that process says so as it announces the session (`Info::agentenv`), and is
//! given it, its harness started again on it ([`opened`]).

use std::sync::Arc;

use super::{Agentenv, Brief, R, Refusal};
use crate::harness::agentenv::{From as VarFrom, Resolved, Set};
use crate::harness::registry::Location;
use crate::hub::Hub;

/// `env` opened for a session's runner: each `secret` read from its owner's own (marked used, audited by
/// name as `by`'s use of it `for_what`), refused naming those its owner holds none of; in a microVM
/// (`microvm`), refused naming its `secret` variables, whose values never go there.
pub async fn open(hub: &Hub, env: &Agentenv, microvm: bool, by: &str, for_what: &str) -> R<Resolved> {
    if microvm {
        let secret = super::secret_vars(&env.vars);
        if !secret.is_empty() {
            return Err(Refusal::SecretInMicrovm(secret));
        }
    }
    let names = super::secrets_of(&env.vars);
    let opened = if names.is_empty() {
        std::collections::BTreeMap::new()
    } else {
        let what = format!("{for_what} (environment {} {})", env.name, env.id);
        crate::secrets::mine::for_session(hub, &env.owner, &names, by, &what).await?.map_err(Refusal::SecretMissing)?
    };
    let mut vars = Vec::with_capacity(env.vars.len());
    for v in &env.vars {
        let set = match v.from {
            VarFrom::Machine => Set::Machine(v.of.clone().unwrap_or_else(|| v.name.clone())),
            VarFrom::Value => Set::Value(v.value.clone().unwrap_or_default()),
            VarFrom::Secret => {
                let name = v.secret.as_deref().and_then(|s| kiln_ci::secrets::check_name(s).ok()).unwrap_or_default();
                let value = opened.get(&name).ok_or_else(|| Refusal::SecretMissing(vec![name.clone()]))?;
                Set::Value(value.clone())
            }
            VarFrom::Unset => Set::Unset,
        };
        vars.push((v.name.clone(), set));
    }
    Ok(Resolved { id: env.id.clone(), name: env.name.clone(), version: u64::try_from(env.version).unwrap_or_default(), base: env.base, vars })
}

/// Whether a session on `target` (`local`, `agent:<machine>`) in a microVM or not (`microvm`) can run with
/// `env`: its runner must name feature `agentenv`, and a microVM takes no `secret` variable.
pub fn runs_there(hub: &Hub, env: &Agentenv, target: &str, microvm: bool) -> R<()> {
    if microvm {
        let secret = super::secret_vars(&env.vars);
        if !secret.is_empty() {
            return Err(Refusal::SecretInMicrovm(secret));
        }
    }
    match target.strip_prefix("agent:") {
        None => Ok(()),
        Some(machine) if speaks(hub, machine) => Ok(()),
        Some(machine) => Err(Refusal::Unsupported(format!("{}'s kiln runs no session with an environment: upgrade it (System → Updates), or start it with none", hub.shown(machine)))),
    }
}

/// Whether machine `machine`'s newest runner process names feature `agentenv`.
pub fn speaks(hub: &Hub, machine: &str) -> bool {
    let inst = hub.instances.load();
    inst.newest(machine).and_then(|p| inst.get(p)).is_some_and(|i| crate::proto::feature::speaks(&i.features, crate::proto::feature::AGENTENV))
}

/// The environment session `session` runs with, opened again as its harness starts again (its newest
/// version, its secrets read again), `microvm` as it's held, `why` saying what starts it: none where it
/// runs with its machine's user's. One its owner since deleted (possible only while kiln didn't run the
/// session) is none, and the session no longer names it.
pub async fn again(hub: &Hub, session: &str, microvm: bool, why: &str) -> R<Option<Resolved>> {
    let Some(id) = super::linked(hub, session) else { return Ok(None) };
    let Some(env) = super::any(hub, &id).await? else {
        super::link(hub, session, None, "kiln").await?;
        return Ok(None);
    };
    open(hub, &env, microvm, &env.owner, &format!("agent session {session}, {why}")).await.map(Some)
}

/// The runner process of machine `machine` that holds session `session` (or would take it: the newest),
/// as said once for each process: `machine/instance/pid/build`, none while none is connected.
fn process_of(hub: &Hub, machine: &str, session: &str) -> Option<String> {
    let inst = hub.instances.load();
    let peer = inst.reaches(machine, &crate::instances::Owned::Agent(session.to_owned()))?;
    let i = inst.get(peer)?;
    Some(format!("{machine}/{}/{}/{}", i.id.as_deref().unwrap_or("main"), i.pid.unwrap_or_default(), i.build.as_deref().unwrap_or_default()))
}

/// Session `session` on `target` was given its environment now, in the runner process that holds it.
pub fn given(hub: &Hub, session: &str, target: &str) {
    if let Some(p) = target.strip_prefix("agent:").and_then(|m| process_of(hub, m, session)) {
        hub.agentenvs.given_in(session, p);
    }
}

/// A runner process announced session `session` (`Update::Open`) running with `reported` (its
/// `Info::agentenv`): where kiln keeps that it runs with another, and that process speaks `agentenv` and
/// wasn't given it yet (a move, a hand-over, its runner started again), it's given it, opened again, its
/// harness started again on it. Once for each process: a process that was given it and still says
/// otherwise (an announcement that came before it took it) isn't sent it twice.
pub fn opened(hub: &Arc<Hub>, from: &Location, session: &str, reported: Option<&Brief>) {
    let Location::Machine(machine) = from else { return };
    let Some(want) = super::linked(hub, session) else { return };
    if reported.is_some_and(|b| b.id == want) {
        return;
    }
    if !speaks(hub, machine) {
        return;
    }
    let Some(process) = process_of(hub, machine, session) else { return };
    if hub.agentenvs.was_given_in(session, &process) {
        return;
    }
    hub.agentenvs.given_in(session, process);
    let (hub, session) = (Arc::clone(hub), session.to_owned());
    drop(crate::rt::spawn(async move {
        let microvm = hub.harness.info(&session).is_some_and(|i| i.isolation.is_some());
        match again(&hub, &session, microvm, "its runner process holding it without it").await {
            Ok(Some(r)) => {
                let sent = hub.harness.send_agentenv(&hub, &session, Some(r), true).await;
                if let Some((_, why)) = sent.failure() {
                    tracing::warn!(target: "kiln.agentenvs", %session, %why, "a session's environment couldn't be given to the runner process that holds it");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "kiln.agentenvs", %session, error = %e, "a session's environment couldn't be opened for the runner process that holds it"),
        }
    }));
}
