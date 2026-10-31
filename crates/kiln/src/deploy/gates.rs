//! The gates a release passes before its commit (docs/deploy-api.md §9.1): a freeze, the
//! environment's windows, built-in checks, the provenance of what it deploys, the upstream
//! environments it must run in first (`after`, here or on a linked server, §11) and how long
//! (`soak`), external checks (a URL, a workflow) and people's approvals. Each gate's result is
//! recorded on the release with its evidence. A gate that can pass by waiting holds the release
//! (`waiting`, or `waiting_approval` for people); one that can't refuses it, at once.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use super::Deploy;
use super::external::{self, Outcome};
use super::link::{Link, Unsure};
use super::model::{App, Artifact, Check, CheckSpec, Environment, GateResult, Gates, Kind, Provenance, Release, State, Window};
use super::store;
use crate::ci::attest::{self, Provenanced};

/// What the gates say of a release now.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every gate passed: commit it.
    Go(Vec<GateResult>),
    /// A gate holds it: look again at `until` (unix seconds) or when something moves; `approval`:
    /// it waits for people.
    Hold { results: Vec<GateResult>, why: String, approval: bool, until: Option<i64> },
    /// A gate it can't pass by waiting.
    Refuse { results: Vec<GateResult>, why: String },
}

/// How soon a held release is looked at again, when nothing says sooner.
const AGAIN_SECS: i64 = 15;
/// How soon a linked server that couldn't be asked, or provenance not there yet, is looked for
/// again.
const LOOK_AGAIN_SECS: i64 = 30;
/// How long a release waits for kiln's provenance of what it deploys (its build may still be
/// attesting it) before it's refused.
const PROVENANCE_WAIT_SECS: i64 = 30 * 60;

/// The checks built in, by name.
pub const BUILT_IN: &[&str] = &["no-snapshots", "owners-set"];

fn result(gate: &str, passed: bool, detail: impl Into<String>, evidence: Value) -> GateResult {
    GateResult { gate: gate.to_owned(), passed, detail: detail.into(), evidence }
}

/// The person a caller is or acts for, as reviewers name people: `user:ada`, a token of hers
/// (`pat:<id> of user:ada`) and an agent acting for her (`agent:fix for user:ada`) are `ada`; the
/// owner is `admin`.
pub use kiln_ci::env::person;

/// U10: all deployment boundaries use the canonical window evaluator. Preserve the release
/// surface's existing refusal text, including its noun, for clients and recorded evidence.
fn closed(windows: &[Window], now: i64) -> Result<Option<(i64, String)>, String> {
    kiln_ci::env::closed(windows, now).map(|closed| closed.map(|(until, why)| (until, why.replace("holds deployments until", "holds releases until"))))
}

/// An external check (a URL or a workflow), as described.
fn external_spec(c: &Check) -> Option<&CheckSpec> {
    match c {
        Check::Spec(s) if s.http.is_some() || s.workflow.is_some() => Some(s),
        _ => None,
    }
}

/// What a check built in says of `r`.
fn built_in(name: &str, r: &Release, app: &App) -> (bool, String) {
    match name {
        "no-snapshots" => match r.version.as_deref() {
            Some(v) if v.to_ascii_lowercase().contains("snapshot") => (false, format!("{v} is a snapshot")),
            _ => (true, "not a snapshot".to_owned()),
        },
        "owners-set" if app.owners.is_empty() => (false, format!("{} has no owner team", app.name)),
        "owners-set" => (true, format!("owned by {}", app.owners.join(", "))),
        other => (false, format!("kiln has no check `{other}` ({}); an external one is a URL (`http`) or a workflow (`workflow`)", BUILT_IN.join(", "))),
    }
}

/// The checks a release of `app` in `env` passes: the environment's, then its template's (one of
/// the same name is the environment's).
async fn checks_of(d: &Deploy, env: &Environment, app: &App) -> anyhow::Result<Vec<Check>> {
    let mut checks = env.gates.checks.clone();
    if let Some(name) = &app.template
        && let Some(t) = store::template(&*d.records, name).await?
    {
        checks.extend(t.checks.into_iter().filter(|c| !env.gates.checks.iter().any(|e| e.name() == c.name())));
    }
    Ok(checks)
}

/// What the gates say of `r` now.
pub async fn evaluate(d: &Deploy, r: &Release, env: &Environment, app: &App) -> anyhow::Result<Verdict> {
    use crate::ci::environments::{actions,policy};
    let checks = checks_of(d, env, app).await?;
    let identity=kiln_ci::env::EnvironmentId{scope:kiln_ci::env::EnvironmentScope::Server,name:env.name.clone()};
    let shared=match r.provider.get("protected_action").and_then(Value::as_str) {
        Some(id)=>Some(actions::get(&d.hub.store,id).await?.ok_or_else(||anyhow::anyhow!("protected Flow no longer exists"))?),None=>None,
    };
    let decisions=if let Some(flow)=&shared {
        // U10/U19: a shared action cannot outlive an unreadable policy authority.
        let Some(repo)=d.hub.store.repos().await?.into_iter().find(|repo|repo.id==flow.repo) else {
            return Ok(Verdict::Refuse{results:Vec::new(),why:"protected Flow repository no longer exists".into()});
        };
        if let Err(error)=crate::ci::environments::source_ready(&d.hub,&repo).await {
            return Ok(Verdict::Refuse{results:Vec::new(),why:format!("{error:#}")});
        }
        if flow.environment.key()?!=identity.key()? || flow.app!=r.app || Some(flow.version.as_str())!=r.version.as_deref() || r.kind!=Kind::Deploy {
            return Ok(Verdict::Refuse{results:Vec::new(),why:"protected Flow does not authorize this environment/app/version".into()});
        }
        if let actions::FlowBinding::Refused(why)=actions::bind_release(&d.hub.store,flow,&identity,&r.app,r.version.as_deref(),r.id,&r.artifacts).await? {return Ok(Verdict::Refuse{results:Vec::new(),why});}
        actions::reviews(&d.hub.store,&flow.id).await?
    }else{r.approvals.iter().map(|a|kiln_ci::env::Review{by:a.by.clone(),approved:a.approved,comment:a.note.clone().unwrap_or_default(),at:a.at}).collect()};
    if let Some(review)=decisions.iter().find(|r|!r.approved) {
        return Ok(Verdict::Refuse{results:Vec::new(),why:format!("rejected by {}{}",review.by,if review.comment.is_empty(){String::new()}else{format!(": {}",review.comment)})});
    }
    let actor=shared.as_ref().map_or(r.requested_by.as_str(),|f|f.actor.as_str());
    let v = keep(judge(d, r, env, app, &checks,&decisions,actor).await?,r,&checks);
    if let Verdict::Go(mut results)=v {
        if r.id>0 {
            actions::restore_release_holders(&d.hub.store,&*d.records,&env.name).await?;
            actions::reap_releases(&d.hub.store,&*d.records).await?;
            let capacity=policy::get(&*d.records,&identity).await?.and_then(|p|p.concurrency);
            let action=shared.as_ref().map_or_else(||format!("release:{}",r.id),|f|format!("flow:{}",f.id));
            match actions::reserve(&d.hub.store,&identity,&action,"release",&r.id.to_string(),&store::environment_policy(env)?.protection,crate::util::now_unix()).await? {
                actions::Admission::Granted=>{},
                actions::Admission::Held(detail)=>{
                    let why=format!("{}: {detail}",env.name);
                    results.push(result("concurrency",false,why.clone(),json!({"limit":capacity})));
                    return Ok(Verdict::Hold{results,why,approval:false,until:Some(crate::util::now_unix()+AGAIN_SECS)});
                }
                actions::Admission::Refused(why)=>return Ok(Verdict::Refuse{results,why}),
            }
        }
        return Ok(Verdict::Go(results));
    }
    Ok(v)
}

/// An external check's earlier result stays with the release while a gate before it holds the
/// release (a workflow's run, a URL's answer): it isn't asked again when that gate lets it go.
fn keep(v: Verdict, r: &Release, checks: &[Check]) -> Verdict {
    let carry = |results: &mut Vec<GateResult>| {
        for c in checks.iter().filter_map(external_spec) {
            let gate = format!("check:{}", c.name);
            if !results.iter().any(|g| g.gate == gate)
                && let Some(p) = r.gates.iter().find(|g| g.gate == gate)
            {
                results.push(p.clone());
            }
        }
    };
    match v {
        Verdict::Hold { mut results, why, approval, until } => {
            carry(&mut results);
            Verdict::Hold { results, why, approval, until }
        }
        Verdict::Refuse { mut results, why } => {
            carry(&mut results);
            Verdict::Refuse { results, why }
        }
        go => go,
    }
}

async fn judge(d: &Deploy, r: &Release, env: &Environment, app: &App, checks: &[Check], decisions:&[kiln_ci::env::Review], approval_actor:&str) -> anyhow::Result<Verdict> {
    let g = &env.gates;
    let now = crate::util::now_unix();
    let mut results = Vec::new();

    if let Some(why) = &g.freeze {
        results.push(result("freeze", false, format!("frozen: {why}"), json!({ "reason": why })));
        return Ok(Verdict::Hold { results, why: format!("{} is frozen: {why}", env.name), approval: false, until: Some(now + 60) });
    }

    match closed(&g.windows, now) {
        Ok(Some((until, why))) => {
            results.push(result("windows", false, why.clone(), json!({ "until": until })));
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(until) });
        }
        Ok(None) if !g.windows.is_empty() => results.push(result("windows", true, "a window lets it go", Value::Null)),
        Ok(None) => {}
        Err(why) => {
            results.push(result("windows", false, why.clone(), Value::Null));
            return Ok(Verdict::Refuse { results, why });
        }
    }

    // The checks built in: the environment's, then the app's template's.
    for c in checks.iter().filter(|c| external_spec(c).is_none()) {
        let (passed, detail) = built_in(c.name(), r, app);
        let gate = format!("check:{}", c.name());
        if c.report_only() {
            results.push(result(&gate, passed, if passed { detail } else { format!("{detail} (report only)") }, Value::Null));
            continue;
        }
        results.push(result(&gate, passed, detail.clone(), Value::Null));
        if !passed {
            return Ok(Verdict::Refuse { results, why: detail });
        }
    }

    // What a new version deploys, or a rollback puts back, has kiln's provenance (W18-R04).
    if let Some(p) = &g.require_provenance
        && matches!(r.kind, Kind::Deploy | Kind::Rollback)
    {
        match provenance(d, r, env, p).await? {
            Proven::Yes(detail, evidence) => results.push(result("provenance", true, detail, evidence)),
            Proven::Waits(why, evidence) => {
                results.push(result("provenance", false, why.clone(), evidence));
                return Ok(Verdict::Hold { results, why, approval: false, until: Some(now + LOOK_AGAIN_SECS) });
            }
            Proven::No(why, evidence) => {
                results.push(result("provenance", false, why.clone(), evidence));
                return Ok(Verdict::Refuse { results, why });
            }
        }
    }

    // A new version runs upstream first; rollbacks, config and enabling run what passed before.
    if r.kind == Kind::Deploy && !g.after.is_empty() {
        let soak = match g.soak.as_deref() {
            Some(s) => Some(kiln_ci::trigger::cron::seconds(s).and_then(|x| i64::try_from(x).ok()).ok_or_else(|| anyhow::anyhow!("soak `{s}` isn't a duration (`30m`, `2h`, `1d`)"))?),
            None => None,
        };
        let version = r.version.clone().unwrap_or_default();
        let mut soaking: Option<(i64, String)> = None;
        for upstream in &g.after {
            let gate = format!("after:{upstream}");
            let up = match upstream.split_once('@') {
                Some((there, server)) => match linked(d, r, there, server).await {
                    Ok(up) => up,
                    Err(Unsure::Unreachable(why)) => {
                        let why = format!("can't ask {server} what runs in {there}: {why}; asking again");
                        results.push(result(&gate, false, why.clone(), json!({ "environment": upstream })));
                        return Ok(Verdict::Hold { results, why, approval: false, until: Some(now + LOOK_AGAIN_SECS) });
                    }
                    Err(Unsure::Refused(why)) => {
                        let why = format!("{upstream}: {why}");
                        results.push(result(&gate, false, why.clone(), json!({ "environment": upstream })));
                        return Ok(Verdict::Refuse { results, why });
                    }
                },
                None => local(d, &r.app, upstream).await?,
            };
            let mut evidence = up.evidence.clone();
            evidence.insert("environment".into(), json!(upstream));
            match up.running.as_ref().filter(|x| x.version.as_deref() == Some(version.as_str()) && same_artifacts(&x.artifacts, &r.artifacts)) {
                Some(x) => {
                    evidence.insert("release".into(), json!(x.release));
                    evidence.insert("since".into(), json!(x.since));
                    results.push(result(&gate, true, format!("{version} runs in {upstream} (release {})", x.release), Value::Object(evidence)));
                    if let kiln_ci::env::PrerequisiteGate::Waiting { until: Some(until), .. } = kiln_ci::env::prerequisite_gate(&kiln_ci::env::Prerequisite::Running { since: x.since }, g.soak.as_deref(), now).map_err(anyhow::Error::msg)?
                        && soaking.as_ref().is_none_or(|(previous, _)| until > *previous)
                    {
                        soaking = Some((until, upstream.clone()));
                    }
                }
                None => {
                    // On its way there: wait for it.
                    if let Some((id, _, state)) = up.coming.as_ref().filter(|c| c.1.as_deref() == Some(version.as_str())) {
                        let why = format!("waits for {upstream} to run {version} (release {id} is {state})");
                        evidence.insert("release".into(), json!(id));
                        results.push(result(&gate, false, why.clone(), Value::Object(evidence)));
                        return Ok(Verdict::Hold { results, why, approval: false, until: Some(now + AGAIN_SECS) });
                    }
                    let there = up.running.as_ref().and_then(|x| x.version.clone()).unwrap_or_else(|| "nothing".to_owned());
                    let why = if !up.registered {
                        format!("{} isn't registered in {upstream}, which {} releases must run in first", r.app, env.name)
                    } else if up.running.as_ref().is_some_and(|x| x.version.as_deref() == Some(version.as_str())) {
                        format!("{version} runs in {upstream} with other artifacts: release the same build there first")
                    } else {
                        format!("{version} doesn't run in {upstream} ({there} does): release it there first")
                    };
                    evidence.insert("runs".into(), json!(there));
                    results.push(result(&gate, false, why.clone(), Value::Object(evidence)));
                    return Ok(Verdict::Refuse { results, why });
                }
            }
        }
        if let Some((until, upstream)) = soaking {
            let why = format!("soaking in {upstream} until {}", super::repo::rfc3339(until));
            results.push(result("soak", false, why.clone(), json!({ "until": until })));
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(until) });
        }
        if soak.is_some() {
            results.push(result("soak", true, "soaked long enough upstream", Value::Null));
        }
    }

    // External checks, asked once the release is recorded, so what they're told names it.
    let external: Vec<&CheckSpec> = checks.iter().filter_map(external_spec).collect();
    if !external.is_empty() {
        if r.id == 0 {
            let names: Vec<&str> = external.iter().map(|s| s.name.as_str()).collect();
            let why = format!("its checks run: {}", names.join(", "));
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(now) });
        }
        let mut waiting: Option<(i64, String)> = None;
        for s in external {
            let gate = format!("check:{}", s.name);
            let prior = r.gates.iter().find(|x| x.gate == gate);
            match external::run(d, r, s, prior).await {
                Outcome::Pass { detail, evidence } => results.push(result(&gate, true, detail, evidence)),
                Outcome::Fail { detail, evidence } if s.report_only => {
                    let detail = if detail.ends_with("(report only)") { detail } else { format!("{detail} (report only)") };
                    results.push(result(&gate, false, detail, evidence));
                }
                Outcome::Fail { detail, evidence } => {
                    let why = format!("the check `{}` failed: {detail}", s.name);
                    results.push(result(&gate, false, detail, evidence));
                    return Ok(Verdict::Refuse { results, why });
                }
                Outcome::Pending { detail, evidence, again } => {
                    results.push(result(&gate, false, detail.clone(), evidence));
                    if !s.report_only && waiting.as_ref().is_none_or(|(at, _)| again < *at) {
                        waiting = Some((again, detail));
                    }
                }
            }
        }
        if let Some((until, why)) = waiting {
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(until) });
        }
    }

    if let Some(a) = &g.approvals {
        let count = a.count.max(1);
        let people = kiln_ci::env::approved_people(a, approval_actor, decisions.iter().filter(|x|x.approved).map(|x| x.by.as_str()));
        let have = u32::try_from(people.len()).unwrap_or(u32::MAX);
        let evidence = json!({ "count": count, "approved_by": people, "reviewers": a.reviewers, "self_approve": a.self_approve });
        // U10: the common gate engine decides counted approval admission. This service
        // retains the release's evidence and its established refusal/waiting messages.
        let policy = kiln_ci::env::Protection { approvals: Some(a.clone()), ..Default::default() };
        let reviews: Vec<kiln_ci::env::Review> = people.iter().map(|by| kiln_ci::env::Review { by: (*by).to_owned(), approved: true, comment: String::new(), at: now }).collect();
        if !matches!(kiln_ci::env::gate(&env.name, &policy, "", r.created_at, now, &reviews), kiln_ci::env::Gate::Open) {
            let who = if a.reviewers.is_empty() { "anyone who may approve releases here".to_owned() } else { a.reviewers.join(", ") };
            let why = format!("waits for {} more approval{} from {who}", count.saturating_sub(have), if count.saturating_sub(have) == 1 { "" } else { "s" });
            results.push(result("approvals", false, why.clone(), evidence));
            return Ok(Verdict::Hold { results, why, approval: true, until: None });
        }
        results.push(result("approvals", true, format!("approved by {}", people.join(", ")), evidence));
    }
    Ok(Verdict::Go(results))
}

/// What the provenance gate says of a release.
enum Proven {
    Yes(String, Value),
    /// kiln has none yet about one of its artifacts: it waits for it (the build that attests it
    /// may still run).
    Waits(String, Value),
    No(String, Value),
}

/// Whether each artifact `r` deploys has kiln's provenance, its signature verified, meeting `p`.
async fn provenance(d: &Deploy, r: &Release, env: &Environment, p: &Provenance) -> anyhow::Result<Proven> {
    let wanted = serde_json::to_value(p).unwrap_or(Value::Null);
    if r.artifacts.is_empty() {
        return Ok(Proven::No(format!("{} deploys only artifacts with kiln's provenance (require_provenance): this release names none, by digest", env.name), json!({ "requirement": wanted })));
    }
    let req = attest::Requirement { builder: p.builder.clone(), workflows: p.workflows.clone(), branches: p.branches.clone() };
    let mut verified = Map::new();
    for (name, a) in &r.artifacts {
        if a.digest.is_empty() {
            return Ok(Proven::No(format!("{name} isn't pinned by digest, and provenance is checked by digest"), json!({ "artifact": name, "requirement": wanted })));
        }
        match attest::provenance_of(&d.hub, &a.digest, &req).await? {
            Provenanced::Verified(id) => {
                verified.insert(name.clone(), json!({ "digest": a.digest, "attestation": id }));
            }
            Provenanced::None(why) => {
                let evidence = json!({ "artifact": name, "digest": a.digest, "verified": verified, "requirement": wanted });
                if crate::util::now_unix() - r.created_at > PROVENANCE_WAIT_SECS {
                    return Ok(Proven::No(format!("{why}, {} minutes after it was asked for", PROVENANCE_WAIT_SECS / 60), evidence));
                }
                return Ok(Proven::Waits(format!("waits for kiln's provenance of {name}: {why}"), evidence));
            }
            Provenanced::Unmet(why) => {
                return Ok(Proven::No(format!("{name}'s provenance doesn't meet {}'s requirement: {why}", env.name), json!({ "artifact": name, "digest": a.digest, "verified": verified, "requirement": wanted })));
            }
        }
    }
    let names: Vec<&String> = verified.keys().collect();
    let detail = format!("kiln's signed provenance of {} meets the requirement", names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "));
    Ok(Proven::Yes(detail, json!({ "artifacts": verified, "requirement": wanted })))
}

/// What runs of an app in an upstream environment, and what's on its way there.
struct Upstream {
    registered: bool,
    running: Option<Ran>,
    /// A release on its way there: its id, version and state.
    coming: Option<(i64, Option<String>, String)>,
    /// Where it was read, for the gate's evidence: a linked server's signed statement.
    evidence: Map<String, Value>,
}

/// A release that runs, as an upstream says.
struct Ran {
    release: i64,
    version: Option<String>,
    artifacts: BTreeMap<String, Artifact>,
    since: i64,
}

/// An upstream environment of this server's.
async fn local(d: &Deploy, app: &str, upstream: &str) -> anyhow::Result<Upstream> {
    let st = &*d.records;
    let reg = store::registration(st, app, upstream).await?;
    let running = match reg.as_ref().and_then(|x| x.running) {
        Some(id) => store::release(st, id).await?,
        None => None,
    };
    let coming = store::in_flight(st, app, upstream).await?;
    Ok(Upstream {
        registered: reg.is_some(),
        running: running.map(|x| Ran { release: x.id, since: x.ended_at.unwrap_or(x.created_at), version: x.version, artifacts: x.artifacts }),
        coming: coming.map(|x| (x.id, x.version, x.state.as_str().to_owned())),
        evidence: Map::new(),
    })
}

/// An upstream environment of a linked server's (`there@server`), as its signed statement says.
async fn linked(d: &Deploy, r: &Release, there: &str, server: &str) -> Result<Upstream, Unsure> {
    let link = Link::of(&d.hub, server).await.map_err(Unsure::Refused)?;
    let (claims, statement) = link.running(&d.hub, &r.app, there).await?;
    let of = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    let running = claims.get("running").filter(|v| !v.is_null()).map(|v| Ran {
        release: of(v, "release").as_i64().unwrap_or(0),
        version: of(v, "version").as_str().map(str::to_owned),
        artifacts: serde_json::from_value(of(v, "artifacts")).unwrap_or_default(),
        since: of(v, "since").as_i64().unwrap_or(i64::MAX),
    });
    let coming = claims.get("coming").filter(|v| !v.is_null()).map(|v| (of(v, "release").as_i64().unwrap_or(0), of(v, "version").as_str().map(str::to_owned), of(v, "state").as_str().unwrap_or("on its way").to_owned()));
    let mut evidence = Map::new();
    evidence.insert("server".into(), of(&claims, "server"));
    evidence.insert("issuer".into(), of(&claims, "iss"));
    evidence.insert("statement".into(), json!(statement));
    Ok(Upstream { registered: true, running, coming, evidence })
}

/// Whether two releases deploy the same build: each artifact the second names has the same digest
/// in the first, or (without digests) the same repository and tag.
fn same_artifacts(a: &BTreeMap<String, Artifact>, b: &BTreeMap<String, Artifact>) -> bool {
    b.iter().all(|(name, x)| match a.get(name) {
        Some(y) if !x.digest.is_empty() && !y.digest.is_empty() => x.digest == y.digest,
        Some(y) => x.repository == y.repository && x.tag == y.tag,
        None => false,
    })
}

/// Whether a release's state is one the gates hold it in.
pub fn holding(s: State) -> bool {
    matches!(s, State::Waiting | State::WaitingApproval)
}

/// What's wrong with gates as written, if anything: checks kiln has or can ask, linked servers it
/// knows, a provenance requirement it can check. Said when they're put, not when a release meets
/// them.
pub async fn check(d: &Deploy, g: &Gates) -> Result<(), String> {
    if g.concurrency_limit==Some(0) {return Err("an environment concurrency limit is 1 or more".into());}
    check_checks(d, &g.checks).await?;
    for upstream in &g.after {
        let Some((there, server)) = upstream.split_once('@') else { continue };
        if there.is_empty() || server.is_empty() || server.contains('@') {
            return Err(format!("after: `{upstream}`: an environment on a linked server is `<environment>@<server>`"));
        }
        match crate::ci::integrations::get(&d.hub, server).await.map_err(|e| format!("{e:#}"))? {
            Some(i) if i.kind == super::link::KIND => {}
            Some(i) => return Err(format!("after: `{upstream}`: {server} is a {} integration, not a linked kiln server", i.kind)),
            None => return Err(format!("after: `{upstream}`: no linked server {server} (Settings → Integrations, kind {})", super::link::KIND)),
        }
    }
    if let Some(p) = &g.require_provenance
        && (p.builder.as_deref().is_some_and(|b| b.trim().is_empty()) || p.workflows.iter().chain(&p.branches).any(|x| x.trim().is_empty()))
    {
        return Err("require_provenance: a builder, workflow or branch that's empty".into());
    }
    Ok(())
}

/// What's wrong with checks as written (an environment's, a template's), if anything.
pub async fn check_checks(d: &Deploy, checks: &[Check]) -> Result<(), String> {
    let mut names = HashSet::new();
    for c in checks {
        let name = c.name();
        if !names.insert(name) {
            return Err(format!("two checks named `{name}`"));
        }
        let Some(s) = external_spec(c) else {
            if !BUILT_IN.contains(&name) {
                return Err(format!("kiln has no check `{name}` ({}); an external one is a URL (`http`) or a workflow (`workflow`)", BUILT_IN.join(", ")));
            }
            continue;
        };
        if BUILT_IN.contains(&name) {
            return Err(format!("`{name}` is a built-in check: an external check has a name of its own"));
        }
        if name.is_empty() || name.len() > 63 || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(format!("check `{name}`: a name of lowercase letters, digits and '-'"));
        }
        match (&s.http, &s.workflow) {
            (Some(_), Some(_)) => return Err(format!("check `{name}`: a URL (`http`) or a workflow (`workflow`), not both")),
            (Some(h), None) => {
                let uri: http::Uri = h.url.parse().map_err(|e| format!("check `{name}`: `{}` isn't a URL: {e}", h.url))?;
                if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
                    return Err(format!("check `{name}`: `{}` isn't an http or https URL", h.url));
                }
                if let Some(t) = &h.timeout
                    && kiln_ci::trigger::cron::seconds(t).is_none_or(|s| s == 0 || s > 120)
                {
                    return Err(format!("check `{name}`: timeout `{t}`: a duration up to 2m (`10s`, `1m`)"));
                }
            }
            (None, Some(w)) => {
                if w.workflow.trim().is_empty() {
                    return Err(format!("check `{name}`: which workflow of {} (its path)", w.repository));
                }
                let repos = d.hub.store.repos().await.map_err(|e| format!("{e:#}"))?;
                if !repos.iter().any(|r| r.name == w.repository) {
                    return Err(format!("check `{name}`: kiln has no repository {}", w.repository));
                }
            }
            (None, None) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(cron: &str, duration: &str) -> Window {
        Window { allow: None, deny: Some(cron.into()), duration: duration.into(), timezone: None }
    }

    fn allow(cron: &str, duration: &str) -> Window {
        Window { allow: Some(cron.into()), deny: None, duration: duration.into(), timezone: None }
    }

    #[test]
    fn windows_hold_releases_while_they_deny_and_until_one_allows() -> Result<(), String> {
        // Saturday 2026-10-17 12:00 UTC.
        let sat_noon = 1_792_238_400;
        let weekend = deny("0 0 * * 6", "48h");
        let held = closed(std::slice::from_ref(&weekend), sat_noon)?.ok_or("held on Saturday")?;
        assert_eq!(held.0, sat_noon - 12 * 3600 + 48 * 3600, "until Monday 00:00");
        assert!(closed(&[weekend], sat_noon + 3 * 86_400)?.is_none(), "free on Tuesday");
        // Weekdays 9 to 17 only.
        let office = allow("0 9 * * 1-5", "8h");
        let (until, why) = closed(std::slice::from_ref(&office), sat_noon)?.ok_or("not on Saturday")?;
        assert!(why.contains("next opens") && until > sat_noon);
        assert!(closed(&[office], sat_noon + 2 * 86_400 - 12 * 3600 + 10 * 3600)?.is_none(), "Monday 10:00 is open");
        assert!(closed(&[Window { allow: Some("0 0 * * *".into()), deny: Some("0 0 * * *".into()), duration: "1h".into(), timezone: None }], sat_noon).is_err());
        Ok(())
    }

    #[test]
    fn a_reviewer_is_the_person_a_caller_is_or_acts_for() {
        assert_eq!(person("user:ada"), "ada");
        assert_eq!(person("agent:fix-it for user:ada"), "ada");
        assert_eq!(person("pat:01a15248f432 of user:ada"), "ada");
        assert_eq!(person("pat:01a15248f432 of admin"), "admin");
        assert_eq!(person("admin"), "admin");
    }

    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use base64::Engine as _;
    use http::StatusCode;

    use super::super::model::{Package, Registration};
    use crate::http::server::{self as srv, Request, Response};

    async fn deploy_with(f: impl FnOnce(&mut crate::config::Config)) -> anyhow::Result<Arc<Deploy>> {
        let hub = crate::hub::tests::scratch_hub_with(f).await?;
        let root = tempfile::tempdir()?.keep();
        Ok(Arc::new(Deploy { records: Arc::clone(&hub.records), stand_in: None, hosted: super::super::host::Hosted::new(&root), owner: "test".into(), wake: crate::rt::Notify::new(), changes: crate::rt::Broadcast::new(16), argo: Default::default(), remote: Default::default(), hub }))
    }

    fn app(name: &str) -> anyhow::Result<App> {
        let package: Package = serde_json::from_value(json!({ "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "web" }))?;
        Ok(App { name: name.into(), project: "shop".into(), owners: vec!["payments".into()], package, namespace: None, properties: BTreeMap::new(), template: None, contacts: Default::default(), source: None, archived: false, registries: None })
    }

    fn environment(name: &str, gates: Value) -> anyhow::Result<Environment> {
        Ok(serde_json::from_value(json!({ "name": name, "gates": gates }))?)
    }

    /// A deploy of web `version` in `env`, its image pinned to `digest` (none when empty).
    fn release(id: i64, env: &str, version: &str, digest: &str) -> anyhow::Result<Release> {
        let artifacts = if digest.is_empty() { json!({}) } else { json!({ "web": { "repository": "registry.example/acme/web", "tag": version, "digest": digest } }) };
        Ok(serde_json::from_value(json!({ "id": id, "app": "web", "environment": env, "kind": "deploy", "state": "waiting", "version": version, "artifacts": artifacts, "requested_by": "user:ada", "created_at": 0 }))?)
    }

    /// U10: canonical admission reads the numbered release in the same database as policy.
    async fn numbered(d: &Deploy, release: &Release) -> anyhow::Result<Release> {
        let (recorded, new) = store::insert_release(&*d.records, release).await?;
        anyhow::ensure!(new, "a fresh fixture release is numbered");
        Ok(recorded)
    }

    #[test]
    fn u10_combined_ci_to_release_flow_requires_both_scopes_and_prompts_only_once()->anyhow::Result<()> {
        crate::rt::block_on(async {
            use crate::ci::environments::{self,actions};
            use crate::authz::{Caller,Grant,Rights};
            let d=deploy_with(|_|{}).await?;
            let env=environment("release-prod",json!({"approvals":{"count":1,"reviewers":["alice"]},"concurrency_limit":1}))?;
            store::put_environment(&*d.records,&env).await?;
            let app=app("web")?;
            store::put_app(&*d.records,&app).await?;
            store::put_registration(&*d.records,&Registration{app:"web".into(),environment:env.name.clone(),enabled:true,paused:false,running:None,created_at:0,degraded:None}).await?;
            d.hub.store.call(|c|{
                c.execute("INSERT INTO repos(id,name,clone_url,created_at) VALUES('flow-repo','acme/flow','local',0)",[])?;
                c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at,actor) VALUES('flow-run','flow-repo',1,'ship','push','main','sha','queued',0,'builder')",[])?;
                c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,leg) VALUES('flow-job','flow-run','ship','ship','queued','[]','[]',?1,?1)",crate::sql::params![r#"{"environment":{"name":"prod"}}"#])?;
                c.execute("INSERT INTO ci_environments(repo_id,name,protection,source,created_at,created_by) VALUES('flow-repo','prod',?1,'kiln',0,'admin')",crate::sql::params![r#"{"policy":{"scope":{"kind":"server"},"name":"release-prod"}}"#])?;
                Ok(())
            }).await?;
            // U10/U19: establish a readable default tree with explicit config absence.
            let hosted=super::super::host::Hosted::new(&crate::ci::hosting::root(&d.hub));
            hosted.ensure("acme/flow","main")?;
            hosted.commit("acme/flow","main",None,&[super::super::host::Change{path:"README.md".into(),content:Some(b"Flow fixture".to_vec())}],"U10 source fixture",&super::super::host::Author::default()).map_err(|e|anyhow::anyhow!("{e}"))?;
            d.hub.store.call(|c|{c.execute("UPDATE repos SET mode='native' WHERE id='flow-repo'",[])?;Ok(())}).await?;
            let query=std::collections::HashMap::from([("repo".into(),"flow-repo".into())]);
            let owner=Caller::owner();
            let created=environments::route(&d.hub,&http::Method::POST,&["prod","flows"],&query,json!({"run":"flow-run","app":"web","version":"v1"}),Some(&owner)).await.map_err(|e|anyhow::anyhow!("{}: {}",e.0,e.1))?;
            assert_eq!(created.status(),http::StatusCode::CREATED);
            let flow=actions::for_job(&d.hub.store,"flow-run","prod").await?.ok_or_else(||anyhow::anyhow!("declared Flow"))?;
            let caller=|both:bool|->anyhow::Result<Caller>{
                let mut grants=vec![Grant::parse("runs.approve@environment:acme/flow:prod").map_err(anyhow::Error::msg)?];
                if both{grants.push(Grant::parse("runs.approve@server").map_err(anyhow::Error::msg)?);}
                Ok(Caller::with(crate::authz::gate::Kind::User,"user:alice",Some(Rights{grants,..Default::default()}),None))
            };
            let review=json!({"environments":["prod"]});
            let denied=environments::approve(&d.hub,"flow-run",review.clone(),Some(&caller(false)?)).await;
            assert!(matches!(denied,Err((http::StatusCode::FORBIDDEN,_))));
            assert!(actions::reviews(&d.hub.store,&flow.id).await?.is_empty());
            environments::approve(&d.hub,"flow-run",review,Some(&caller(true)?)).await.map_err(|e|anyhow::anyhow!("{}: {}",e.0,e.1))?;
            let mut requested=release(0,"release-prod","v1","")?;
            requested.provider=json!({"protected_action":flow.id});
            let (bound_release,_)=store::insert_release(&*d.records,&requested).await?;
            let repo=d.hub.store.repos().await?.into_iter().find(|r|r.id=="flow-repo").ok_or_else(||anyhow::anyhow!("repository"))?;
            let def=environments::defs(&d.hub,&repo).await?.remove("prod").ok_or_else(||anyhow::anyhow!("bound environment"))?;
            let identity=actions::identity(&repo.id,&def);
            assert_eq!(actions::reserve(&d.hub.store,&identity,&format!("flow:{}",flow.id),"job","flow-job",&def.protection,1).await?,actions::Admission::Granted);
            assert!(matches!(evaluate(&d,&bound_release,&env,&app).await?,Verdict::Go(_)),"the release reuses the authorized Flow review and capacity");
            let unrelated=release(0,"release-prod","v1","")?;
            assert!(matches!(evaluate(&d,&unrelated,&env,&app).await?,Verdict::Hold{approval:true,..}),"another release cannot inherit the Flow review");
            Ok(())
        })
    }

    fn claims_of(jwt: &str) -> anyhow::Result<Value> {
        let body = jwt.split('.').nth(1).ok_or_else(|| anyhow::anyhow!("not a JWT"))?;
        Ok(serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body)?)?)
    }

    fn listener() -> anyhow::Result<(hyper_rt::tcp::TcpListener, SocketAddr)> {
        let tcp = hyper_rt::tcp::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((tcp, addr))
    }

    fn serve<H: srv::Handler>(tcp: hyper_rt::tcp::TcpListener, h: Arc<H>) {
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let remote = stream.peer_addr().ok();
                crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(stream), Arc::clone(&h), remote));
            }
        });
    }

    /// A URL a check asks: it answers what `said` holds, and keeps what it was asked with.
    struct Asked {
        said: parking_lot::Mutex<String>,
        asked: parking_lot::Mutex<Vec<(String, Value)>>,
    }

    impl srv::Handler for Asked {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or_default().to_owned();
            let body: Value = srv::body_json(req.into_body(), 1 << 20).await.unwrap_or(Value::Null);
            self.asked.lock().push((auth, body));
            let said = self.said.lock().clone();
            srv::json(StatusCode::OK, &json!({ "result": said, "detail": format!("the smoke test said {said}"), "retry_after": 5 }))
        }
    }

    /// §9.1: a URL is asked once the release is recorded, with kiln's token for it; it holds the
    /// release while pending, lets it go on pass (its answer the evidence, kept while another gate
    /// holds it, never asked again), refuses it on fail, only records when report-only, and holds
    /// while it can't be reached.
    #[test]
    fn an_external_check_holds_a_release_until_its_url_answers_and_isn_t_asked_again() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let d = deploy_with(|c| c.ci.allow_private_targets = vec!["127.0.0.1".into()]).await?;
            let asked = Arc::new(Asked { said: parking_lot::Mutex::new("pending".into()), asked: parking_lot::Mutex::new(Vec::new()) });
            let (tcp, addr) = listener()?;
            serve(tcp, Arc::clone(&asked));
            let url = format!("http://{addr}/check");
            let env = environment("production", json!({ "checks": ["no-snapshots", { "name": "smoke", "http": { "url": url } }] }))?;
            store::put_environment(&*d.records, &env).await?;
            let web = app("web")?;
            // Asked for: it waits for its check, which is asked once the release is recorded.
            let mut r = release(0, "production", "1.2.0", "")?;
            let Verdict::Hold { why, .. } = evaluate(&d, &r, &env, &web).await? else { anyhow::bail!("held for its check") };
            anyhow::ensure!(why == "its checks run: smoke", "{why}");
            anyhow::ensure!(asked.asked.lock().is_empty(), "not asked before the release is recorded");
            // Recorded: asked, and held while the URL says pending.
            r = numbered(&d, &r).await?;
            let Verdict::Hold { results, why, until, .. } = evaluate(&d, &r, &env, &web).await? else { anyhow::bail!("held while pending") };
            anyhow::ensure!(why.contains("the smoke test said pending") && until.is_some(), "{why}");
            let (auth, body) = asked.asked.lock().first().cloned().ok_or_else(|| anyhow::anyhow!("not asked"))?;
            anyhow::ensure!(body.pointer("/release/id") == Some(&json!(r.id)) && body.pointer("/release/version") == Some(&json!("1.2.0")) && body.get("check") == Some(&json!("smoke")), "{body}");
            let token = claims_of(auth.strip_prefix("Bearer ").unwrap_or_default())?;
            anyhow::ensure!(token.get("aud") == Some(&json!(url)) && token.get("sub") == Some(&json!("deploy:production:web")), "kiln's token for that URL: {token}");
            // It passes: the release goes, the URL's answer its evidence.
            r.gates = results;
            *asked.said.lock() = "pass".into();
            let Verdict::Go(results) = evaluate(&d, &r, &env, &web).await? else { anyhow::bail!("goes once it passes") };
            let smoke = results.iter().find(|g| g.gate == "check:smoke").ok_or_else(|| anyhow::anyhow!("no result for smoke"))?;
            anyhow::ensure!(smoke.passed && smoke.evidence.pointer("/answer/result") == Some(&json!("pass")) && smoke.evidence.get("url") == Some(&json!(url)), "{smoke:?}");
            // Said once: a freeze put on after it keeps its answer, and once lifted it stands.
            r.gates = results;
            *asked.said.lock() = "fail".into();
            let frozen = environment("production", json!({ "freeze": "an incident", "checks": [{ "name": "smoke", "http": { "url": url } }] }))?;
            store::put_environment(&*d.records, &frozen).await?;
            let Verdict::Hold { results: held, .. } = evaluate(&d, &r, &frozen, &web).await? else { anyhow::bail!("frozen") };
            anyhow::ensure!(held.iter().any(|g| g.gate == "check:smoke" && g.passed), "its answer stays with it: {held:?}");
            store::put_environment(&*d.records, &env).await?;
            anyhow::ensure!(matches!(evaluate(&d, &r, &env, &web).await?, Verdict::Go(_)));
            anyhow::ensure!(asked.asked.lock().len() == 2, "asked twice: pending, then pass");
            // Another release it fails is refused.
            let Verdict::Refuse { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.1", "")?).await?, &env, &web).await? else { anyhow::bail!("refused on fail") };
            anyhow::ensure!(why.contains("the check `smoke` failed: the smoke test said fail"), "{why}");
            // Report only: recorded, and the release goes.
            let noted = environment("production", json!({ "checks": [{ "name": "smoke", "http": { "url": url }, "report_only": true }] }))?;
            store::put_environment(&*d.records, &noted).await?;
            let Verdict::Go(results) = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.1", "")?).await?, &noted, &web).await? else { anyhow::bail!("report only lets it go") };
            anyhow::ensure!(results.iter().any(|g| g.gate == "check:smoke" && !g.passed && g.detail.ends_with("(report only)")), "{results:?}");
            // A URL that can't be reached holds it, saying so.
            let closed = SocketAddr::from((Ipv4Addr::LOCALHOST, crate::frontdoor::test_port()?));
            let away = environment("production", json!({ "checks": [{ "name": "smoke", "http": { "url": format!("http://{closed}/check"), "timeout": "2s" } }] }))?;
            store::put_environment(&*d.records, &away).await?;
            let Verdict::Hold { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.1", "")?).await?, &away, &web).await? else { anyhow::bail!("held while it can't be asked") };
            anyhow::ensure!(why.starts_with("can't ask"), "{why}");
            Ok(())
        })
    }

    /// W18-R04: a new version waits for kiln's provenance of what it deploys, goes once kiln has it
    /// (the attestation its evidence), and is refused provenance that doesn't meet the requirement,
    /// an artifact not pinned by digest, or none named; a config release runs what passed before.
    #[test]
    fn a_release_waits_for_kiln_s_provenance_of_what_it_deploys() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let d = deploy_with(|_| {}).await?;
            let web = app("web")?;
            let env = environment("production", json!({ "require_provenance": { "workflows": [".kiln/build.yaml"], "branches": ["main"] } }))?;
            store::put_environment(&*d.records, &env).await?;
            let base = d.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
            let built = |branch: &str| kiln_ci::attest::Facts {
                builder_id: format!("{base}/ci@v1"),
                repository: format!("{base}/acme/web"),
                workflow_path: ".kiln/build.yaml".into(),
                git_ref: format!("refs/heads/{branch}"),
                sha: "abc".into(),
                ..kiln_ci::attest::Facts::default()
            };
            let image = |hex: &str| kiln_ci::attest::Subject { name: "registry.example/acme/web".into(), digest: [("sha256".to_owned(), hex.to_owned())].into() };
            let (a, b) = ("a".repeat(64), "b".repeat(64));
            // None yet: it waits (the build that attests it may still run), for half an hour.
            let mut r = release(0, "production", "1.2.0", &format!("sha256:{a}"))?;
            r.created_at = crate::util::now_unix();
            let r = numbered(&d, &r).await?;
            let Verdict::Hold { why, .. } = evaluate(&d, &r, &env, &web).await? else { anyhow::bail!("waits for its provenance") };
            anyhow::ensure!(why.contains("waits for kiln's provenance of web"), "{why}");
            let stale = Release { created_at: r.created_at - 3600, ..r.clone() };
            let Verdict::Refuse { why, .. } = evaluate(&d, &stale, &env, &web).await? else { anyhow::bail!("refused once it has waited long enough") };
            anyhow::ensure!(why.contains("kiln has no provenance of") && why.contains("30 minutes after"), "{why}");
            // The build attests it: it goes.
            let id = crate::ci::attest::keep_provenance(&d.hub, &[image(&a)], &built("main")).await?;
            let Verdict::Go(results) = evaluate(&d, &r, &env, &web).await? else { anyhow::bail!("goes with its provenance") };
            let p = results.iter().find(|g| g.gate == "provenance").ok_or_else(|| anyhow::anyhow!("no provenance result"))?;
            anyhow::ensure!(p.passed && p.evidence.pointer("/artifacts/web/attestation") == Some(&json!(id)), "{p:?}");
            // Built from another branch: refused at once.
            crate::ci::attest::keep_provenance(&d.hub, &[image(&b)], &built("feature")).await?;
            let Verdict::Refuse { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.1", &format!("sha256:{b}"))?).await?, &env, &web).await? else { anyhow::bail!("refused") };
            anyhow::ensure!(why.contains("doesn't meet") && why.contains("refs/heads/feature"), "{why}");
            // Not pinned by digest, or nothing named: refused.
            let mut loose = release(0, "production", "1.2.2", &format!("sha256:{a}"))?;
            if let Some(x) = loose.artifacts.get_mut("web") {
                x.digest.clear();
            }
            let loose = numbered(&d, &loose).await?;
            anyhow::ensure!(matches!(evaluate(&d, &loose, &env, &web).await?, Verdict::Refuse { ref why, .. } if why.contains("isn't pinned by digest")));
            anyhow::ensure!(matches!(evaluate(&d, &numbered(&d, &release(0, "production", "1.2.3", "")?).await?, &env, &web).await?, Verdict::Refuse { ref why, .. } if why.contains("names none")));
            // A config release runs what passed before.
            let mut config = release(0, "production", "1.2.3", "")?;
            config.kind = Kind::Config;
            let config = numbered(&d, &config).await?;
            anyhow::ensure!(matches!(evaluate(&d, &config, &env, &web).await?, Verdict::Go(_)));
            Ok(())
        })
    }

    /// A linked server, as far as one is asked: where it is, what runs of an app in one of its
    /// environments (signed), and its issuer's documents; only to its reader's token.
    struct Linked {
        d: Arc<Deploy>,
    }

    impl srv::Handler for Linked {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let path = req.uri().path().to_owned();
            let q = srv::query(&req);
            let hub = Arc::clone(&self.d.hub);
            if path.starts_with("/oidc/") {
                let doc = if path.ends_with("openid-configuration") { "discovery" } else { "jwks" };
                return crate::ci::oidc::document(&hub, doc).await.unwrap_or_else(|(s, m)| srv::json(s, &json!({ "error": m })));
            }
            if srv::header_str(&req, &http::header::AUTHORIZATION) != Some("Bearer reader") {
                return srv::json(StatusCode::UNAUTHORIZED, &json!({ "error": "who's asking?" }));
            }
            let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            match parts.as_slice() {
                ["api", "deploy", "link"] => srv::json(StatusCode::OK, &super::super::link::about(&hub)),
                ["api", "deploy", "apps", app, "environments", env, "statement"] => match super::super::link::statement(&self.d, app, env, q.get("audience").map_or("", String::as_str)).await {
                    Ok(Some(s)) => srv::json(StatusCode::OK, &json!({ "statement": s })),
                    Ok(None) => srv::json(StatusCode::NOT_FOUND, &json!({ "error": format!("{app} isn't registered in {env}") })),
                    Err(e) => srv::json(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": format!("{e:#}") })),
                },
                _ => srv::json(StatusCode::NOT_FOUND, &json!({ "error": "no such route" })),
            }
        }
    }

    /// §11: `after` names an environment of a linked server (`staging@eu`), which says what runs
    /// there in a statement signed by its OIDC issuer for this server: checked against its keys, it
    /// proves the gate (its soak too) and is kept as the evidence, checkable again; another version,
    /// other artifacts, an app it doesn't have or a token it doesn't take are refused; one on its way
    /// there, or a server that can't be asked, holds the release.
    #[test]
    fn an_upstream_on_a_linked_server_proves_after_with_what_it_signs() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // There: `staging`, running web 1.2.0 for ten minutes.
            let (tcp, addr) = listener()?;
            let up = deploy_with(|c| c.server.public_url = format!("http://{addr}")).await?;
            let digest = format!("sha256:{}", "a".repeat(64));
            store::put_app(&*up.records, &app("web")?).await?;
            store::put_environment(&*up.records, &environment("staging", json!({}))?).await?;
            let mut ran = release(0, "staging", "1.2.0", &digest)?;
            ran.state = State::Succeeded;
            ran.ended_at = Some(crate::util::now_unix() - 600);
            let (ran, _) = store::insert_release(&*up.records, &ran).await?;
            store::put_registration(&*up.records, &Registration { app: "web".into(), environment: "staging".into(), enabled: true, paused: false, running: Some(ran.id), created_at: 0, degraded: None }).await?;
            serve(tcp, Arc::new(Linked { d: Arc::clone(&up) }));
            // Here: `production` runs what runs there first, for five minutes.
            let d = deploy_with(|_| {}).await?;
            let integration = |name: &str, url: String, token: &str| {
                let hub = Arc::clone(&d.hub);
                let (name, token) = (name.to_owned(), token.to_owned());
                async move { crate::ci::integrations::create(&hub, "kiln", &name, json!({ "url": url }), crate::ci::integrations::Secrets { secret: Some(&token), signing: None }, None, "test").await.map_err(anyhow::Error::msg) }
            };
            integration("eu", format!("http://{addr}"), "reader").await?;
            let gates = json!({ "after": ["staging@eu"], "soak": "5m" });
            check(&d, &serde_json::from_value(gates.clone())?).await.map_err(anyhow::Error::msg)?;
            let env = environment("production", gates)?;
            store::put_environment(&*d.records, &env).await?;
            let web = app("web")?;
            let Verdict::Go(results) = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.0", &digest)?).await?, &env, &web).await? else { anyhow::bail!("1.2.0 runs there") };
            let after = results.iter().find(|g| g.gate == "after:staging@eu").ok_or_else(|| anyhow::anyhow!("no result for staging@eu"))?;
            anyhow::ensure!(after.passed && after.evidence.get("release") == Some(&json!(ran.id)), "{after:?}");
            // Its evidence: the statement, signed by the issuer there for this server, checkable again.
            let statement = after.evidence.get("statement").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no statement kept"))?;
            let (issuer, here) = (crate::ci::oidc::issuer(&up.hub), crate::ci::oidc::issuer(&d.hub));
            anyhow::ensure!(after.evidence.get("issuer") == Some(&json!(issuer)), "{after:?}");
            let client = crate::http::client::Client::public()?;
            let now = crate::util::now_unix();
            let claims = super::super::link::verify(&client, statement, &issuer, &here, now).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(claims.pointer("/running/version") == Some(&json!("1.2.0")) && claims.get("sub") == Some(&json!("deploy:staging:web")), "{claims}");
            // Not for another server, not from another issuer, not changed, not once it's expired.
            anyhow::ensure!(matches!(super::super::link::verify(&client, statement, &issuer, "https://elsewhere.example/oidc", now).await, Err(Unsure::Refused(_))));
            anyhow::ensure!(matches!(super::super::link::verify(&client, statement, &here, &here, now).await, Err(Unsure::Refused(_))));
            let mut parts: Vec<String> = statement.split('.').map(str::to_owned).collect();
            if let Some(body) = parts.get_mut(1) {
                let mut c = claims.clone();
                if let Some(o) = c.as_object_mut() {
                    o.insert("running".into(), json!({ "release": 99, "version": "6.6.6", "since": 0 }));
                }
                *body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c)?);
            }
            anyhow::ensure!(matches!(super::super::link::verify(&client, &parts.join("."), &issuer, &here, now).await, Err(Unsure::Refused(ref why)) if why.contains("doesn't verify")));
            anyhow::ensure!(matches!(super::super::link::verify(&client, statement, &issuer, &here, now + 3600).await, Err(Unsure::Refused(ref why)) if why.contains("expired")));
            // Held for its soak there.
            let long = environment("production", json!({ "after": ["staging@eu"], "soak": "30m" }))?;
            store::put_environment(&*d.records, &long).await?;
            let Verdict::Hold { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.2.0", &digest)?).await?, &long, &web).await? else { anyhow::bail!("soaking") };
            anyhow::ensure!(why.starts_with("soaking in staging@eu until"), "{why}");
            store::put_environment(&*d.records, &env).await?;
            // Another version: refused. Other artifacts: refused. On its way there: held.
            let Verdict::Refuse { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.3.0", &digest)?).await?, &env, &web).await? else { anyhow::bail!("1.3.0 doesn't run there") };
            anyhow::ensure!(why.contains("1.3.0 doesn't run in staging@eu (1.2.0 does)"), "{why}");
            let other = format!("sha256:{}", "b".repeat(64));
            anyhow::ensure!(matches!(evaluate(&d, &numbered(&d, &release(0, "production", "1.2.0", &other)?).await?, &env, &web).await?, Verdict::Refuse { ref why, .. } if why.contains("with other artifacts")));
            let mut coming = release(0, "staging", "1.3.0", &digest)?;
            coming.state = State::Delivering;
            store::insert_release(&*up.records, &coming).await?;
            let Verdict::Hold { why, .. } = evaluate(&d, &numbered(&d, &release(0, "production", "1.3.0", &digest)?).await?, &env, &web).await? else { anyhow::bail!("waits for 1.3.0 there") };
            anyhow::ensure!(why.contains("waits for staging@eu to run 1.3.0"), "{why}");
            // An app it doesn't have there.
            let mut api = release(0, "production", "1.2.0", &digest)?;
            api.app = "api".into();
            let api = numbered(&d, &api).await?;
            anyhow::ensure!(matches!(evaluate(&d, &api, &env, &app("api")?).await?, Verdict::Refuse { ref why, .. } if why.contains("api isn't registered in staging")));
            // A token it doesn't take; a server that can't be asked.
            integration("eu-guest", format!("http://{addr}"), "guest").await?;
            let guest = environment("production", json!({ "after": ["staging@eu-guest"] }))?;
            store::put_environment(&*d.records, &guest).await?;
            anyhow::ensure!(matches!(evaluate(&d, &numbered(&d, &release(0, "production", "1.2.0", &digest)?).await?, &guest, &web).await?, Verdict::Refuse { ref why, .. } if why.contains("may not read releases")));
            integration("away", format!("http://127.0.0.1:{}", crate::frontdoor::test_port()?), "reader").await?;
            let away = environment("production", json!({ "after": ["staging@away"] }))?;
            store::put_environment(&*d.records, &away).await?;
            anyhow::ensure!(matches!(evaluate(&d, &numbered(&d, &release(0, "production", "1.2.0", &digest)?).await?, &away, &web).await?, Verdict::Hold { ref why, .. } if why.starts_with("can't ask away what runs in staging")));
            Ok(())
        })
    }

    /// Checks are a built-in's name or a description, read with what's wrong named; what's wrong
    /// with gates is said when they're put.
    #[test]
    fn checks_are_named_or_described_and_what_s_wrong_is_said_when_they_re_put() -> anyhow::Result<()> {
        let g: Gates = serde_json::from_value(json!({ "checks": ["no-snapshots", { "name": "smoke", "http": { "url": "https://checks.example/smoke" }, "report_only": true }], "require-provenance": { "branches": ["main"] } }))?;
        anyhow::ensure!(g.checks.first().map(Check::name) == Some("no-snapshots") && g.checks.get(1).is_some_and(Check::report_only));
        anyhow::ensure!(g.require_provenance.as_ref().is_some_and(|p| p.branches == ["main"]));
        let back = serde_json::to_value(&g)?;
        anyhow::ensure!(back.pointer("/checks/0") == Some(&json!("no-snapshots")) && back.pointer("/require_provenance/branches/0") == Some(&json!("main")), "{back}");
        let typo = serde_json::from_value::<Gates>(json!({ "checks": [{ "name": "smoke", "htp": { "url": "https://x.example" } }] })).err().map(|e| e.to_string()).unwrap_or_default();
        anyhow::ensure!(typo.contains("unknown field `htp`"), "{typo}");
        crate::rt::block_on(async {
            let d = deploy_with(|_| {}).await?;
            let wrong = |checks: Value| {
                let d = Arc::clone(&d);
                async move {
                    let c: Vec<Check> = serde_json::from_value(checks)?;
                    Ok::<_, anyhow::Error>(check_checks(&d, &c).await.err().unwrap_or_default())
                }
            };
            anyhow::ensure!(wrong(json!(["no-snapshots", "no-snapshots"])).await?.contains("two checks named"));
            anyhow::ensure!(wrong(json!(["vulnerabilities"])).await?.contains("kiln has no check `vulnerabilities`"));
            anyhow::ensure!(wrong(json!([{ "name": "smoke", "http": { "url": "https://x.example" }, "workflow": { "repository": "acme/web", "workflow": "smoke.yaml" } }])).await?.contains("not both"));
            anyhow::ensure!(wrong(json!([{ "name": "smoke", "http": { "url": "ftp://x.example/" } }])).await?.contains("isn't an http or https URL"));
            anyhow::ensure!(wrong(json!([{ "name": "smoke", "http": { "url": "https://x.example/", "timeout": "5m" } }])).await?.contains("up to 2m"));
            anyhow::ensure!(wrong(json!([{ "name": "e2e", "workflow": { "repository": "acme/none", "workflow": "e2e.yaml" } }])).await?.contains("kiln has no repository acme/none"));
            anyhow::ensure!(wrong(json!([{ "name": "owners-set", "http": { "url": "https://x.example/" } }])).await?.contains("is a built-in check"));
            anyhow::ensure!(wrong(json!(["owners-set", { "name": "smoke", "http": { "url": "https://x.example/" } }])).await?.is_empty());
            let after = |a: &str| serde_json::from_value::<Gates>(json!({ "after": [a] }));
            anyhow::ensure!(check(&d, &after("staging@nowhere")?).await.is_err_and(|e| e.contains("no linked server nowhere")));
            anyhow::ensure!(check(&d, &after("staging@")?).await.is_err_and(|e| e.contains("<environment>@<server>")));
            anyhow::ensure!(check(&d, &after("staging")?).await.is_ok(), "an environment of this server's isn't asked about");
            Ok(())
        })
    }
}
