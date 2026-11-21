//! An environment opened for a session's harness (docs/agents.md, "Where they apply"): what its runner is
//! given ([`Resolved`]), each `secret` read from its owner's own as it starts, and opened again whenever
//! the server starts that harness again or finds the runner process that holds the session without it. A
//! session's stack is opened into one ([`open_stack`], docs/agents.md "Scopes, stacks, and applied on its own"):
//! its runner holds one environment, as runners from before stacks do.
//!
//! Where the server starts a harness, each opens it: a start (`harness::lineage::start`), a restart
//! (`harness::registry::Registry::dispatch`'s `Restart`, given the environment first), one resumed from
//! kiln's copy of its conversation (`Registry::resume_on`), one moved from the server to a runner
//! (`Registry::move_local`). A runner keeps a session's environment in its memory alone, so a session that
//! goes on in another runner process (moved over reach, handed to a newer process, its runner started
//! again) is there without it: that process says so as it announces the session (`Info::agentenv`), and is
//! given it, its harness started again on it ([`opened`]).

use std::sync::Arc;

use super::{Agentenv, R, Refusal};
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
                Set::Secret(value.clone())
            }
            VarFrom::Unset => Set::Unset,
        };
        vars.push((v.name.clone(), set));
    }
    Ok(Resolved { id: env.id.clone(), name: env.name.clone(), version: u64::try_from(env.version).unwrap_or_default(), base: env.base, vars })
}

/// A session's stack `envs`, top first, opened as one ([`open`] each) for its runner: its id the stack's ids, top first,
/// joined by `+` ([`super::stack_id`]); its name theirs joined by ` + `; its version 0 (one alone: its own); its base
/// the top one's; its variables the bottom one's first and the top one's last, so where two set one the higher wins.
/// None for none: its machine's user's. In a microVM, refused naming every `secret` variable of the stack.
pub async fn open_stack(hub: &Hub, envs: &[Agentenv], microvm: bool, by: &str, for_what: &str) -> R<Option<Resolved>> {
    if microvm {
        let secret: Vec<String> = envs.iter().flat_map(|e| super::secret_vars(&e.vars)).collect();
        if !secret.is_empty() {
            return Err(Refusal::SecretInMicrovm(secret));
        }
    }
    let mut opened = Vec::with_capacity(envs.len());
    for e in envs {
        opened.push(open(hub, e, microvm, by, for_what).await?);
    }
    Ok(flatten(opened))
}

/// Opened environments, top first, as one ([`open_stack`]).
pub fn flatten(opened: Vec<Resolved>) -> Option<Resolved> {
    if opened.len() < 2 {
        return opened.into_iter().next();
    }
    let id = super::stack_id(opened.iter().map(|r| r.id.as_str()));
    let name = opened.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(" + ");
    let base = opened.first().map(|r| r.base).unwrap_or_default();
    let vars = opened.into_iter().rev().flat_map(|r| r.vars).collect();
    Some(Resolved { id, name, version: 0, base, vars })
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

/// The stack session `session` runs with, opened again as its harness starts again (each its newest version, its
/// secrets read again, by its owner), `microvm` as it's held, `why` saying what starts it: none where it runs with its
/// machine's user's. One its owner since deleted (possible only while kiln didn't run the session) is taken out of its
/// stack.
pub async fn again(hub: &Hub, session: &str, microvm: bool, why: &str) -> R<Option<Resolved>> {
    let links = super::stack(hub, session);
    if links.is_empty() {
        return Ok(None);
    }
    let held = super::of_session(hub, session).await?;
    if held.len() != links.len() {
        let kept: Vec<super::Link> = held.iter().map(|(e, auto)| super::Link { id: e.id.clone(), auto: *auto }).collect();
        super::set_stack(hub, session, &kept, "kiln").await?;
    }
    let what = format!("agent session {session}, {why}");
    let mut opened = Vec::with_capacity(held.len());
    for (env, _) in &held {
        opened.push(open(hub, env, microvm, &env.owner, &what).await?);
    }
    Ok(flatten(opened))
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

/// A runner process announced session `session` (`Update::Open`, its info `info`), running with `info.agentenv`.
/// Where kiln keeps that it runs with another stack (by its id, [`super::stack_id`]; none, where it holds one), and that process speaks
/// `agentenv` and wasn't given it yet (a move, a hand-over to a newer process, its runner started again or upgraded
/// in place: a runner keeps an environment in memory alone), it's given it, opened again (`Command::Agentenv`,
/// `restart` false). Once for each process: one that was given it and still says otherwise (an announcement that came
/// before it took it) isn't sent it twice. A session new to the server (`new`) that a person branched from another
/// (`Info::variant_of`) runs with that one's whole stack, kept for it too, unless it says one of its own.
pub fn opened(hub: &Arc<Hub>, from: &Location, session: &str, info: &crate::harness::session::Info, new: bool) {
    let Location::Machine(machine) = from else { return };
    let branched = info.variant_of.as_deref().map(|v| v.split_once('/').map_or(v, |(s, _)| s));
    let own = super::stack(hub, session);
    let want = if own.is_empty() { branched.filter(|_| new && info.agentenv.is_none()).map(|b| super::stack(hub, b)).unwrap_or_default() } else { own };
    let want_id = (!want.is_empty()).then(|| super::stack_id(want.iter().map(|l| l.id.as_str())));
    if info.agentenv.as_ref().map(|b| &b.id) == want_id.as_ref() {
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
    let (hub, session, microvm, branched) = (Arc::clone(hub), session.to_owned(), info.isolation.is_some(), branched.map(str::to_owned));
    drop(crate::rt::spawn(async move {
        // A branch kept as running with its source's stack, as a start's is.
        if !want.is_empty() && !super::stacked(&hub, &session) {
            let by = format!("kiln (branched from {})", branched.as_deref().unwrap_or_default());
            if let Err(e) = super::set_stack(&hub, &session, &want, &by).await {
                tracing::warn!(target: "kiln.agentenvs", %session, error = %e, "a branch's environments couldn't be kept");
                return;
            }
        }
        // None where its stack was emptied while that process held another (one applied on its own left it then).
        match again(&hub, &session, microvm, "its runner process holding it without it").await {
            Ok(r) => {
                let sent = hub.harness.send_agentenv(&hub, &session, r, false).await;
                if let Some((_, why)) = sent.failure() {
                    tracing::warn!(target: "kiln.agentenvs", %session, %why, "a session's environment couldn't be given to the runner process that holds it");
                }
            }
            Err(e) => tracing::warn!(target: "kiln.agentenvs", %session, error = %e, "a session's environment couldn't be opened for the runner process that holds it"),
        }
    }));
}
