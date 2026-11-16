//! A person's experiments on their agent definitions (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1), through the
//! operations as REST, MCP and code mode call them (`crate::ops::call`, the route table's check included): every
//! operation, another person's experiments as none, and the draw, from the operation table's own (`assign`) to a
//! session `agentdefs.run` started and what `agentdefs.sessions` says of it.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{Ask, Engine, VariantAsk, assign, create, started};
use crate::authz::{Caller, Grant, Rights, gate::Kind};
use crate::hub::Hub;
use crate::ops::{Cx, Fail};

const GIB: u64 = 1 << 30;

/// A person, `user:<name>`, with `grant`.
fn person(name: &str, grant: &str) -> anyhow::Result<Caller> {
    let g = Grant::parse(grant).map_err(anyhow::Error::msg)?;
    let mut caller = Caller::with(Kind::User, &format!("user:{name}"), Some(Rights { grants: vec![g], ..Default::default() }), None);
    caller.user = Some(name.to_owned());
    Ok(caller)
}

async fn op(hub: &Arc<Hub>, caller: &Caller, name: &str, input: Value) -> Result<Value, Fail> {
    crate::ops::call(Cx { hub: Arc::clone(hub), caller: caller.clone() }, name, input).await
}

fn failed((code, why): Fail) -> anyhow::Error {
    anyhow::anyhow!("{code}: {why}")
}

/// `r` refused with `code`, its words saying `says`.
fn refused(r: Result<Value, Fail>, code: u16, says: &str) -> anyhow::Result<()> {
    match r {
        Err((c, why)) if c.as_u16() == code && why.contains(says) => Ok(()),
        Err((c, why)) => anyhow::bail!("refused {c} ({why}), not {code} saying {says:?}"),
        Ok(v) => anyhow::bail!("answered {v}, not refused {code} saying {says:?}"),
    }
}

fn s(v: &Value, pointer: &str) -> String {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn n(v: &Value, pointer: &str) -> i64 {
    v.pointer(pointer).and_then(Value::as_i64).unwrap_or(-1)
}

/// Its variants' ids and weights, in its order.
fn split(e: &Value) -> Vec<(String, i64)> {
    e.pointer("/experiment/variants").and_then(Value::as_array).map(|vs| vs.iter().map(|v| (s(v, "/id"), n(v, "/weight"))).collect()).unwrap_or_default()
}

/// A microVM variant of kiln's Agentfile for Claude Code, sized `cpus` and `gib`.
fn vm(weight: u32, cpus: u32, gib: u64) -> Value {
    json!({"weight": weight, "agentfile": "harness-claude", "cpus": cpus, "memory": gib * GIB})
}

fn warm(experiment: &str) -> Value {
    json!({"name": "Warm start", "query": format!("avg(last_7d):avg:kiln.session.warm_ms{{experiment:{experiment}}} by {{variant}}"), "op": "<=", "bound": 250})
}

/// P7.1's operations, each: Ada starts an experiment on her definition (two microVM sizes, a criterion), and it's
/// listed by definition as the UI reads it; she renames it and a variant; sets its weights (the split's next
/// version; the same again none; over a version moved on, refused); adds and removes variants (its current one, and
/// below 2, refused; a weighted one's share back to the rest in proportion) and criteria; pauses, resumes and stops
/// it; starts another (refused while one is live), promotes its variant (the definition's next version, with the
/// variant's Agentfile and size) and deletes the first. kiln's own definitions take none, and what isn't a split, a
/// variant kiln runs or a criterion kiln's monitors read is refused, saying why.
#[test]
fn p7_1_an_experiment_round_trips_through_every_operation() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        let ada = person("ada", "developer")?;
        let call = |name: &'static str, input: Value| op(&hub, &ada, name, input);
        let def = call("agentdefs.create", json!({"name": "reviewer", "harness": "claude", "instructions": "Review it."})).await.map_err(failed)?;
        let id = s(&def, "/id");

        // Started: live, split v1, its first variant what the definition runs now, its criterion as written.
        let made = call("agentdefs.experiments.create", json!({"id": id, "name": "Right-size", "variants": [vm(80, 2, 4), {"name": "Large", "weight": 20, "agentfile": "harness-claude", "cpus": 4, "memory": 8 * GIB}], "criteria": [warm("x")]})).await.map_err(failed)?;
        let e = s(&made, "/experiment/id");
        anyhow::ensure!(s(&made, "/experiment/state") == "live" && n(&made, "/experiment/version") == 1 && n(&made, "/experiment/sessions") == 0 && s(&made, "/experiment/agentdef") == id, "{made}");
        anyhow::ensure!(made.pointer("/experiment/variants/0/current") == Some(&json!(true)) && made.pointer("/experiment/variants/1/current") == Some(&json!(false)), "{made}");
        anyhow::ensure!(made.pointer("/experiment/variants/0/name") == Some(&Value::Null) && s(&made, "/experiment/variants/1/name") == "Large", "an unnamed variant's name is null: {made}");
        anyhow::ensure!(s(&made, "/experiment/variants/1/engine") == "microvm" && n(&made, "/experiment/variants/1/cpus") == 4 && made.pointer("/experiment/variants/1/memory") == Some(&json!(8 * GIB)), "{made}");
        anyhow::ensure!(s(&made, "/experiment/criteria/0/op") == "<=" && made.pointer("/experiment/criteria/0/bound") == Some(&json!(250.0)) && s(&made, "/experiment/criteria/0/query").ends_with("by {variant}"), "{made}");
        anyhow::ensure!(made.pointer("/experiment/started_at").and_then(Value::as_i64).is_some_and(|t| t > 1_000_000_000_000), "started_at is Unix ms: {made}");
        let listed = call("agentdefs.experiments.list", json!({})).await.map_err(failed)?;
        anyhow::ensure!(listed.get(&id).and_then(Value::as_array).is_some_and(|l| l.len() == 1) && listed.pointer(&format!("/{id}/0/id")) == Some(&json!(e)), "{listed}");
        let one = call("agentdefs.experiments.list", json!({"id": id})).await.map_err(failed)?;
        anyhow::ensure!(one.as_object().is_some_and(|o| o.len() == 1), "one definition's: {one}");

        // Renamed; a variant renamed, then back to what sets it apart.
        let ids: Vec<String> = split(&made).into_iter().map(|(v, _)| v).collect();
        let (small, large) = (ids.first().cloned().unwrap_or_default(), ids.get(1).cloned().unwrap_or_default());
        let renamed = call("agentdefs.experiments.rename", json!({"id": id, "experiment": e, "name": "Right-size its microVM"})).await.map_err(failed)?;
        anyhow::ensure!(s(&renamed, "/experiment/name") == "Right-size its microVM", "{renamed}");
        let named = call("agentdefs.experiments.rename", json!({"id": id, "experiment": e, "variant": small, "name": "Small"})).await.map_err(failed)?;
        anyhow::ensure!(s(&named, "/experiment/variants/0/name") == "Small", "{named}");
        let unnamed = call("agentdefs.experiments.rename", json!({"id": id, "experiment": e, "variant": large})).await.map_err(failed)?;
        anyhow::ensure!(unnamed.pointer("/experiment/variants/1/name") == Some(&Value::Null), "{unnamed}");
        refused(call("agentdefs.experiments.rename", json!({"id": id, "experiment": e, "name": "  "})).await, 400, "needs its name")?;

        // Weights: the split's next version; the same again none; over a moved-on version, refused.
        let w = call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 50, large.as_str(): 50}, "base": 1})).await.map_err(failed)?;
        anyhow::ensure!(n(&w, "/experiment/version") == 2 && split(&w).iter().all(|(_, w)| *w == 50), "{w}");
        let same = call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 50, large.as_str(): 50}})).await.map_err(failed)?;
        anyhow::ensure!(n(&same, "/experiment/version") == 2, "the same weights are no new version: {same}");
        refused(call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 60, large.as_str(): 40}, "base": 1})).await, 409, "version 2")?;
        refused(call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 60, large.as_str(): 30}})).await, 400, "add up to 100, not 90")?;
        refused(call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 100}})).await, 400, "say every variant's weight")?;
        refused(call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 50, large.as_str(): 40, "nope": 10}})).await, 404, "no variant nope")?;

        // Variants: one added joins at 0, no new version; the current one stays; a weighted one's share goes back.
        let added = call("agentdefs.experiments.variants.add", json!({"id": id, "experiment": e, "engine": "host", "name": "On the host"})).await.map_err(failed)?;
        let host = split(&added).last().map(|(v, _)| v.clone()).unwrap_or_default();
        anyhow::ensure!(split(&added).len() == 3 && split(&added).last().map(|(_, w)| *w) == Some(0) && n(&added, "/experiment/version") == 2, "{added}");
        anyhow::ensure!(s(&added, "/experiment/variants/2/engine") == "host" && added.pointer("/experiment/variants/2/agentfile") == Some(&Value::Null) && added.pointer("/experiment/variants/2/cpus").is_none(), "a host's has no Agentfile or size: {added}");
        refused(call("agentdefs.experiments.variants.remove", json!({"id": id, "experiment": e, "variant": small})).await, 409, "what its agent runs now")?;
        let w = call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 30, large.as_str(): 30, host.as_str(): 40}})).await.map_err(failed)?;
        anyhow::ensure!(n(&w, "/experiment/version") == 3, "{w}");
        let gone = call("agentdefs.experiments.variants.remove", json!({"id": id, "experiment": e, "variant": host})).await.map_err(failed)?;
        anyhow::ensure!(split(&gone) == vec![(small.clone(), 50), (large.clone(), 50)] && n(&gone, "/experiment/version") == 4, "its 40 back to the others in proportion: {gone}");
        refused(call("agentdefs.experiments.variants.remove", json!({"id": id, "experiment": e, "variant": large})).await, 409, "2 variants")?;

        // Criteria: added once kiln's monitors read one, taken out.
        let crit = call("agentdefs.experiments.criteria.add", json!({"id": id, "experiment": e, "name": "Failed sessions", "query": format!("avg(last_7d):avg:kiln.session.failed_pct{{experiment:{e}}} by {{variant}}"), "op": "<", "bound": 10})).await.map_err(failed)?;
        let crits: Vec<String> = crit.pointer("/experiment/criteria").and_then(Value::as_array).map(|c| c.iter().map(|x| s(x, "/id")).collect()).unwrap_or_default();
        anyhow::ensure!(crits.len() == 2 && s(&crit, "/experiment/criteria/1/name") == "Failed sessions", "{crit}");
        refused(call("agentdefs.experiments.criteria.add", json!({"id": id, "experiment": e, "name": "Odd", "query": "not a query", "op": "<", "bound": 1})).await, 400, "kiln's monitors can't read")?;
        refused(call("agentdefs.experiments.criteria.add", json!({"id": id, "experiment": e, "name": "Twice", "query": "avg(last_5m):avg:kiln.session.warm_ms{*} < 3", "op": "<", "bound": 1})).await, 400, "")?;
        let fewer = call("agentdefs.experiments.criteria.remove", json!({"id": id, "experiment": e, "criterion": crits.first().cloned().unwrap_or_default()})).await.map_err(failed)?;
        anyhow::ensure!(fewer.pointer("/experiment/criteria").and_then(Value::as_array).is_some_and(|c| c.len() == 1), "{fewer}");

        // Paused, resumed, stopped: once over, its split no longer changes; its name and criteria still do.
        let paused = call("agentdefs.experiments.pause", json!({"id": id, "experiment": e})).await.map_err(failed)?;
        anyhow::ensure!(s(&paused, "/experiment/state") == "paused", "{paused}");
        let again = call("agentdefs.experiments.pause", json!({"id": id, "experiment": e})).await.map_err(failed)?;
        anyhow::ensure!(s(&again, "/experiment/state") == "paused" && n(&again, "/experiment/state_at") == n(&paused, "/experiment/state_at"), "paused already, as it is: {again}");
        let live = call("agentdefs.experiments.resume", json!({"id": id, "experiment": e})).await.map_err(failed)?;
        anyhow::ensure!(s(&live, "/experiment/state") == "live", "{live}");
        let stopped = call("agentdefs.experiments.stop", json!({"id": id, "experiment": e})).await.map_err(failed)?;
        anyhow::ensure!(s(&stopped, "/experiment/state") == "stopped", "{stopped}");
        refused(call("agentdefs.experiments.resume", json!({"id": id, "experiment": e})).await, 409, "it's over")?;
        refused(call("agentdefs.experiments.weights", json!({"id": id, "experiment": e, "weights": {small.as_str(): 60, large.as_str(): 40}})).await, 409, "it's over")?;
        refused(call("agentdefs.experiments.variants.add", json!({"id": id, "experiment": e, "engine": "host"})).await, 409, "it's over")?;
        call("agentdefs.experiments.rename", json!({"id": id, "experiment": e, "name": "Sizes, done"})).await.map_err(failed)?;
        call("agentdefs.experiments.criteria.add", json!({"id": id, "experiment": e, "name": "Peak", "query": format!("avg(last_7d):avg:kiln.session.peak_memory_gib{{experiment:{e}}} by {{variant}}"), "op": "<", "bound": 3.5})).await.map_err(failed)?;

        // One live at a time; promoted, the variant is the definition's next version and the experiment's over.
        let second = call("agentdefs.experiments.create", json!({"id": id, "name": "Bigger", "variants": [vm(50, 2, 4), vm(50, 8, 16)]})).await.map_err(failed)?;
        let e2 = s(&second, "/experiment/id");
        refused(call("agentdefs.experiments.create", json!({"id": id, "name": "Third", "variants": [vm(50, 2, 4), vm(50, 1, 2)]})).await, 409, "Bigger is live")?;
        refused(call("agentdefs.experiments.resume", json!({"id": id, "experiment": e})).await, 409, "it's over")?;
        let big = split(&second).get(1).map(|(v, _)| v.clone()).unwrap_or_default();
        let promoted = call("agentdefs.experiments.promote", json!({"id": id, "experiment": e2, "variant": big})).await.map_err(failed)?;
        anyhow::ensure!(s(&promoted, "/experiment/state") == "promoted" && s(&promoted, "/experiment/promoted") == big, "{promoted}");
        anyhow::ensure!(promoted.pointer("/experiment/variants/1/current") == Some(&json!(true)) && promoted.pointer("/experiment/variants/0/current") == Some(&json!(false)), "{promoted}");
        anyhow::ensure!(n(&promoted, "/agentdef/version") == 2 && n(&promoted, "/agentdef/cpus") == 8 && promoted.pointer("/agentdef/memory") == Some(&json!(16 * GIB)), "its size the definition's next version: {promoted}");
        anyhow::ensure!(promoted.pointer("/agentdef/agentfile").is_none_or(Value::is_null) && promoted.pointer("/agentdef/engine").is_none(), "its Agentfile and engine as the definition ran them, unchanged: {promoted}");
        let as_is = call("agentdefs.experiments.promote", json!({"id": id, "experiment": e2, "variant": big})).await.map_err(failed)?;
        anyhow::ensure!(n(&as_is, "/agentdef/version") == 2, "promoted already, as it is: {as_is}");
        refused(call("agentdefs.experiments.promote", json!({"id": id, "experiment": e2, "variant": split(&second).first().map(|(v, _)| v.clone()).unwrap_or_default()})).await, 409, "it's over")?;

        // Deleted: gone from the list, the other still there.
        let deleted = call("agentdefs.experiments.delete", json!({"id": id, "experiment": e})).await.map_err(failed)?;
        anyhow::ensure!(deleted == json!({"deleted": true}), "{deleted}");
        let left = call("agentdefs.experiments.list", json!({"id": id})).await.map_err(failed)?;
        anyhow::ensure!(left.get(&id).and_then(Value::as_array).is_some_and(|l| l.len() == 1 && l.first().and_then(|x| x.get("id")) == Some(&json!(e2))), "{left}");
        refused(call("agentdefs.experiments.pause", json!({"id": id, "experiment": e})).await, 404, "no such experiment")?;

        // kiln's own take none, typed; what isn't a split, a variant kiln runs or a criterion is refused, saying why.
        refused(call("agentdefs.experiments.create", json!({"id": "kiln:harness-claude", "name": "x", "variants": [vm(50, 2, 4), vm(50, 4, 8)]})).await, 403, "kiln's own agent definition, and takes no experiment")?;
        let def2 = s(&call("agentdefs.create", json!({"name": "triage", "harness": "claude"})).await.map_err(failed)?, "/id");
        let make = |variants: Value| call("agentdefs.experiments.create", json!({"id": def2, "name": "x", "variants": variants}));
        refused(make(json!([vm(100, 2, 4)])).await, 400, "between 2 and 100 variants")?;
        refused(make(json!([vm(70, 2, 4), vm(20, 4, 8)])).await, 400, "add up to 100, not 90")?;
        refused(make(json!([vm(50, 2, 4), {"weight": 50, "engine": "host", "agentfile": "harness-claude"}])).await, 422, "variant 2: an Agentfile and a size are a microVM's")?;
        refused(make(json!([vm(50, 2, 4), {"weight": 50, "agentfile": "harness-codex", "cpus": 2, "memory": 4 * GIB}])).await, 422, "variant 2: harness-codex is kiln's Agentfile for codex")?;
        refused(make(json!([vm(50, 2, 4), {"weight": 50, "agentfile": "harness-claude"}])).await, 422, "variant 2: say its microVM's vCPUs")?;
        refused(make(json!([vm(50, 2, 4), {"weight": 50, "agentfile": "harness-claude", "cpus": 2, "memory": 1000}])).await, 422, "a whole number of MiB")?;
        refused(make(json!([vm(50, 2, 4), {"weight": 50, "agentfile": "harness-claude", "cpus": 2, "memory": 4 * GIB, "size": 2}])).await, 400, "unknown field")?;
        anyhow::Ok(())
    })
}

/// P7.1 beside P6.2: an agent definition's volumes are kept for its sessions in microVMs alone (a start of one on its
/// machine's host is refused), so a definition on the host engine with volumes, and a host variant of one that has
/// them, are refused as they're checked, saying why; its microVM variants are kept.
#[test]
fn p7_1_a_host_variant_of_a_definition_with_volumes_is_refused() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        let ada = person("ada", "developer")?;
        let call = |name: &'static str, input: Value| op(&hub, &ada, name, input);
        let volumes = json!([{"name": "cargo", "bytes": GIB}]);
        let why = "its volumes are kept only for its sessions in microVMs";
        refused(call("agentdefs.create", json!({"name": "builder", "harness": "claude", "engine": "host", "volumes": volumes})).await, 422, why)?;
        let def = s(&call("agentdefs.create", json!({"name": "builder", "harness": "claude", "volumes": volumes})).await.map_err(failed)?, "/id");
        refused(call("agentdefs.experiments.create", json!({"id": def, "name": "x", "variants": [vm(50, 2, 4), {"weight": 50, "engine": "host"}]})).await, 422, &format!("variant 2: {why}"))?;
        let kept = call("agentdefs.experiments.create", json!({"id": def, "name": "y", "variants": [vm(50, 2, 4), vm(50, 4, 8)]})).await.map_err(failed)?;
        anyhow::ensure!(s(&kept, "/experiment/state") == "live", "{kept}");
        anyhow::Ok(())
    })
}

/// P7.1's authz ("another person's experiment is invisible"): Bo lists none of Ada's, and reads, changes, deletes or
/// starts none on her definition (each as no such definition); his own list is his alone. An admin reaches everyone's,
/// as agent definitions are. Taking `super::reach`'s definition check out lets Bo change hers.
#[test]
fn p7_1_another_persons_experiments_are_as_none() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        let (ada, bo, root) = (person("ada", "developer")?, person("bo", "developer")?, person("root", "admin")?);
        let def = s(&op(&hub, &ada, "agentdefs.create", json!({"name": "reviewer", "harness": "claude"})).await.map_err(failed)?, "/id");
        let made = op(&hub, &ada, "agentdefs.experiments.create", json!({"id": def, "name": "Hers", "variants": [vm(50, 2, 4), vm(50, 4, 8)]})).await.map_err(failed)?;
        let e = s(&made, "/experiment/id");
        let variant = split(&made).first().map(|(v, _)| v.clone()).unwrap_or_default();

        let his = op(&hub, &bo, "agentdefs.experiments.list", json!({})).await.map_err(failed)?;
        anyhow::ensure!(his == json!({}), "Bo lists none of hers: {his}");
        refused(op(&hub, &bo, "agentdefs.experiments.list", json!({"id": def})).await, 404, "no such agent definition")?;
        refused(op(&hub, &bo, "agentdefs.experiments.list", json!({"all": true})).await, 403, "everyone's experiments")?;
        for (name, input) in [
            ("agentdefs.experiments.rename", json!({"id": def, "experiment": e, "name": "Mine now"})),
            ("agentdefs.experiments.weights", json!({"id": def, "experiment": e, "weights": {variant.as_str(): 100}})),
            ("agentdefs.experiments.variants.add", json!({"id": def, "experiment": e, "engine": "host"})),
            ("agentdefs.experiments.criteria.add", json!({"id": def, "experiment": e, "name": "w", "query": "avg(last_5m):avg:kiln.session.warm_ms{*}", "op": "<", "bound": 1})),
            ("agentdefs.experiments.pause", json!({"id": def, "experiment": e})),
            ("agentdefs.experiments.stop", json!({"id": def, "experiment": e})),
            ("agentdefs.experiments.promote", json!({"id": def, "experiment": e, "variant": variant})),
            ("agentdefs.experiments.delete", json!({"id": def, "experiment": e})),
            ("agentdefs.experiments.create", json!({"id": def, "name": "His", "variants": [vm(50, 2, 4), vm(50, 4, 8)]})),
        ] {
            refused(op(&hub, &bo, name, input).await, 404, "no such agent definition").map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        }
        // His own definition's experiment is his, and names hers as none too.
        let mine = s(&op(&hub, &bo, "agentdefs.create", json!({"name": "his", "harness": "claude"})).await.map_err(failed)?, "/id");
        refused(op(&hub, &bo, "agentdefs.experiments.pause", json!({"id": mine, "experiment": e})).await, 404, "no such experiment")?;
        let hers = op(&hub, &ada, "agentdefs.experiments.list", json!({})).await.map_err(failed)?;
        anyhow::ensure!(hers.pointer(&format!("/{def}/0/name")) == Some(&json!("Hers")) && hers.pointer(&format!("/{def}/0/state")) == Some(&json!("live")), "hers as she left it: {hers}");
        let everyone = op(&hub, &root, "agentdefs.experiments.list", json!({"all": true})).await.map_err(failed)?;
        anyhow::ensure!(everyone.get(&def).is_some(), "an admin reaches everyone's: {everyone}");
        anyhow::Ok(())
    })
}

/// P7.1's assignment ("pick a variant by weight, deterministic and testable; record it with the session"): while an
/// experiment is live, each draw of its definition's new sessions is `kiln_ci::split::draw_parts` of its weights, its
/// seed and the draw's index, kept with its split's version; another person's sessions, and kiln's own
/// definitions', draw none; paused, none, and resumed it goes on where it was; its weights changed, the next draws
/// are by them, under its split's next version; stopped, none. A started draw's session counts for its variant, a
/// refused one doesn't.
#[test]
fn p7_1_a_live_experiment_draws_each_new_session_by_its_weights() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        let ada = person("ada", "developer")?;
        let who = crate::agentdefs::Who::of(&ada)?;
        let def = s(&op(&hub, &ada, "agentdefs.create", json!({"name": "triage", "harness": "claude"})).await.map_err(failed)?, "/id");
        let host = |weight: u32| VariantAsk { weight, engine: Some(Engine::Host), ..VariantAsk::default() };
        let sized = VariantAsk { weight: 30, agentfile: Some("harness-claude".into()), cpus: Some(2), memory: Some(4 * GIB), ..VariantAsk::default() };
        let seed = 0x5EED_u64;
        let e = create(&hub, &who, &def, Ask { name: "Two ways".into(), variants: vec![host(70), sized], criteria: Vec::new() }, seed, "ada").await?;
        let ids: Vec<String> = e.variants.iter().map(|v| v.id.clone()).collect();
        let expect = |weights: &[u32], n: u64| kiln_ci::split::draw_parts(weights, seed, n).and_then(|k| ids.get(k).cloned());
        let mut started_of = std::collections::BTreeMap::<String, u64>::new();
        for i in 0..40u64 {
            let d = assign(&hub, &def, "user:ada").await.ok_or_else(|| anyhow::anyhow!("draw {i}: none"))?;
            anyhow::ensure!(Some(d.variant.clone()) == expect(&[70, 30], i) && d.n == i64::try_from(i)? && d.version == 1 && d.experiment == e.id, "draw {i}: {d:?}");
            anyhow::ensure!((d.engine == Engine::Host) == (d.size().is_none()) && (d.engine == Engine::Host || d.size() == Some((2, 4096))), "draw {i}'s deployment: {d:?}");
            // Every third refused (its variant's failure), the rest started.
            let came = if i % 3 == 0 { Err("refused".to_owned()) } else { Ok(format!("session-{i}")) };
            if came.is_ok() {
                *started_of.entry(d.variant.clone()).or_default() += 1;
            }
            started(&hub, &d, came).await;
        }
        let shares: Vec<u64> = ids.iter().map(|v| started_of.get(v).copied().unwrap_or_default()).collect();
        anyhow::ensure!(shares.iter().all(|n| *n > 0), "both drawn: {shares:?}");
        let listed = super::list(&hub, &who, Some(def.as_str()), false).await?;
        let now = listed.get(&def).and_then(|l| l.first()).ok_or_else(|| anyhow::anyhow!("not listed"))?;
        anyhow::ensure!(now.sessions == shares.iter().sum::<u64>() && now.variants.iter().map(|v| v.sessions).collect::<Vec<_>>() == shares, "{now:?} vs {shares:?}");

        anyhow::ensure!(assign(&hub, &def, "user:bo").await.is_none(), "another person's sessions draw none");
        anyhow::ensure!(assign(&hub, "kiln:harness-claude", "user:ada").await.is_none(), "kiln's own draw none");
        super::set_state(&hub, &who, &def, &e.id, super::Move::Pause).await?;
        anyhow::ensure!(assign(&hub, &def, "user:ada").await.is_none(), "paused: none");
        super::set_state(&hub, &who, &def, &e.id, super::Move::Resume).await?;
        let first = ids.first().cloned().unwrap_or_default();
        let second = ids.get(1).cloned().unwrap_or_default();
        super::weights(&hub, &who, &def, &e.id, [(first, 10), (second, 90)].into_iter().collect(), Some(1)).await?;
        for i in 40..60u64 {
            let d = assign(&hub, &def, "user:ada").await.ok_or_else(|| anyhow::anyhow!("draw {i}: none"))?;
            anyhow::ensure!(Some(d.variant.clone()) == expect(&[10, 90], i) && d.n == i64::try_from(i)? && d.version == 2, "draw {i} goes on where it was, by the new weights: {d:?}");
        }
        let exp = e.id.clone();
        let versions: Vec<(i64, i64)> = hub
            .store
            .read(move |c| {
                let mut st = c.prepare("SELECT draw, split_version FROM agent_def_experiment_draws WHERE experiment_id = ?1 ORDER BY draw")?;
                let rows = st.query_map([&exp], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        anyhow::ensure!(versions.len() == 60 && versions.iter().all(|(n, v)| *v == if *n < 40 { 1 } else { 2 }), "each draw kept with its split's version: {versions:?}");
        super::set_state(&hub, &who, &def, &e.id, super::Move::Stop).await?;
        anyhow::ensure!(assign(&hub, &def, "user:ada").await.is_none(), "stopped: none");
        anyhow::Ok(())
    })
}

/// P7.1's assignment through `agentdefs.run`: each session it starts from a definition with a live experiment is a
/// variant drawn by weight, started as that variant (a host variant a process of the server's machine; a microVM
/// variant there refused, the server starting none, and kept as its draw's failure), the variant kept on the session
/// (`experiment` on its info) and said by `agentdefs.sessions` (`experiment`, `variant`). The draws are the
/// experiment's own sequence. Taking `Drawn::of`'s definition draw out of `lineage::start` starts each as the
/// definition is.
#[test]
fn p7_1_agentdefs_run_starts_each_session_as_its_drawn_variant() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| {
            c.terminal.enabled = true;
            c.conversations.off = true;
        })
        .await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let owner = Caller::owner();
        let call = |name: &'static str, input: Value| op(&hub, &owner, name, input);
        let folder = tempfile::tempdir()?;
        let cwd = folder.path().to_string_lossy().into_owned();
        let def = s(&call("agentdefs.create", json!({"name": "runner", "harness": "claude"})).await.map_err(failed)?, "/id");
        let made = call("agentdefs.experiments.create", json!({"id": def, "name": "Where", "variants": [{"weight": 50, "engine": "host"}, {"weight": 30, "engine": "host", "name": "Also host"}, vm(20, 1, 2)]})).await.map_err(failed)?;
        let e = s(&made, "/experiment/id");
        let ids: Vec<String> = split(&made).into_iter().map(|(v, _)| v).collect();
        let exp = e.clone();
        let seed: i64 = hub.store.read(move |c| Ok(c.query_row("SELECT seed FROM agent_def_experiments WHERE id = ?1", [&exp], |r| r.get(0))?)).await?;
        let seed = u64::from_ne_bytes(seed.to_ne_bytes());
        let (mut opened, mut refusals) = (Vec::new(), 0usize);
        for i in 0..8u64 {
            let want = kiln_ci::split::draw_parts(&[50, 30, 20], seed, i).and_then(|k| ids.get(k).cloned()).unwrap_or_default();
            let ran = crate::rt::timeout(std::time::Duration::from_secs(120), call("agentdefs.run", json!({"id": def, "cwd": cwd}))).await?;
            if want == ids.get(2).cloned().unwrap_or_default() {
                anyhow::ensure!(matches!(&ran, Err((c, _)) if *c == http::StatusCode::CONFLICT), "run {i}, the microVM variant, is refused where no microVM starts: {ran:?}");
                refusals += 1;
                continue;
            }
            let ran = ran.map_err(failed)?;
            let agent = s(&ran, "/agents/0/id");
            let info = hub.harness.info(&agent).ok_or_else(|| anyhow::anyhow!("run {i}: no session {agent}: {ran}"))?;
            let kept = info.experiment.clone().ok_or_else(|| anyhow::anyhow!("run {i}: no variant on {agent}"))?;
            anyhow::ensure!(kept.experiment == e && kept.arm == want && !kept.promoted && info.isolation.is_none(), "run {i}: {kept:?}, a host variant's session on its host");
            opened.push((agent, want));
        }
        anyhow::ensure!(!opened.is_empty(), "some host variant was drawn");
        let listed = call("agentdefs.sessions", json!({"id": def})).await.map_err(failed)?;
        for (agent, variant) in &opened {
            let row = listed.get("sessions").and_then(Value::as_array).and_then(|l| l.iter().find(|r| r.get("id") == Some(&json!(agent)))).cloned().unwrap_or_default();
            anyhow::ensure!(row.get("variant") == Some(&json!(variant)) && row.get("experiment") == Some(&json!(e)), "agentdefs.sessions says {agent}'s variant: {row}");
        }
        let now = call("agentdefs.experiments.list", json!({"id": def})).await.map_err(failed)?;
        anyhow::ensure!(now.pointer(&format!("/{def}/0/sessions")).and_then(Value::as_u64) == Some(opened.len() as u64), "only started draws count: {now}");
        let exp = e.clone();
        let kept: (i64, i64, i64) = hub
            .store
            .read(move |c| Ok(c.query_row("SELECT COUNT(*), COUNT(session_id), COUNT(refused) FROM agent_def_experiment_draws WHERE experiment_id = ?1", [&exp], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?))
            .await?;
        anyhow::ensure!(kept == (8, i64::try_from(opened.len())?, i64::try_from(refusals)?), "every draw kept, its session or its refusal: {kept:?}");
        anyhow::Ok(())
    })
}

/// P7.1's routes: each operation's route under `/api/agentdefs/{id}/experiments` is the operation `op_of` takes it
/// for, its path naming the same input, so the console's REST and the operations can't drift apart.
#[test]
fn p7_1_each_route_is_its_operation() -> anyhow::Result<()> {
    let probe = json!({"id": "d1", "experiment": "e1", "variant": "v1", "criterion": "c1"});
    for o in super::ops::list_ops() {
        let (method, segs) = (o.route)(&probe).map_err(failed)?;
        let s: Vec<&str> = segs.iter().map(String::as_str).collect();
        match s.as_slice() {
            ["api", "agentdef-experiments"] => anyhow::ensure!(o.name == "agentdefs.experiments.list" && method == http::Method::GET, "{}", o.name),
            ["api", "agentdefs", "d1", "experiments", rest @ ..] => {
                let (name, named) = super::ops::op_of(&method, rest).ok_or_else(|| anyhow::anyhow!("{} ({method} {s:?}) is no route", o.name))?;
                anyhow::ensure!(name == o.name, "{method} {s:?} is {name}, not {}", o.name);
                for (k, v) in named {
                    anyhow::ensure!(probe.get(k) == Some(&json!(v)), "{}: its path's {k} is {v}", o.name);
                }
            }
            _ => anyhow::bail!("{} routes elsewhere: {s:?}", o.name),
        }
    }
    anyhow::ensure!(super::ops::op_of(&http::Method::GET, &["e1", "nothing"]).is_none());
    Ok(())
}
