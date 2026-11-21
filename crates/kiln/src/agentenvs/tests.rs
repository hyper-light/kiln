//! Environments kept and reached by their owner alone, opened for a session's harness, and refused where
//! docs/agents.md says, through the operation table as every surface calls it (`crate::ops::call`: the route
//! table, the input's type, the audit) and through an agent's start. No test value is a real secret.

use std::collections::HashMap;

use http::StatusCode;
use serde_json::{Value, json};

use super::*;
use crate::authz::gate::{Kind, Rights};
use crate::authz::{Caller, Grant};
use crate::harness::agentenv::Set;

/// A person, `user:<id>`, holding `grants` on the server.
fn person(id: &str, grants: &[&str]) -> Caller {
    let rights = Rights { grants: grants.iter().filter_map(|g| Grant::parse(g).ok()).collect(), custom: HashMap::new() };
    let mut c = Caller::with(Kind::User, &format!("user:{id}"), Some(rights), None);
    c.user = Some(id.to_owned());
    c
}

/// An agent session acting for person `id`, as kiln's MCP server makes one.
fn agent_for(id: &str) -> Caller {
    let mut c = person(id, &["developer"]);
    c.kind = Kind::Agent;
    c.name = format!("agent:s1 for user:{id}");
    c.agent = Some("s1".into());
    c
}

async fn op(hub: &std::sync::Arc<Hub>, caller: &Caller, name: &str, input: Value) -> Result<Value, (StatusCode, String)> {
    crate::ops::call(crate::ops::Cx { hub: std::sync::Arc::clone(hub), caller: caller.clone() }, name, input).await
}

fn text_of(v: &Value, pointer: &str) -> String {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// A fake value no machine has (never a real secret).
const FAKE: &str = "sk-test-not-a-real-value-0001";

fn vars() -> Value {
    json!([
        {"name": "PATH_EXTRA", "from": "machine", "of": "HOME"},
        {"name": "MODE", "from": "value", "value": "fast"},
        {"name": "ANTHROPIC_API_KEY", "from": "secret", "secret": "anthropic_key"},
        {"name": "NO_PROXY", "from": "unset"}
    ])
}

#[test]
fn the_migration_makes_its_tables() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let store = crate::store::Store::in_memory()?;
        let columns: Vec<Vec<String>> = store
            .read(|c| {
                let mut out = Vec::new();
                for table in ["agentenvs", "agentenv_stacks"] {
                    out.push(c.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?.query_map([table], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?);
                }
                Ok(out)
            })
            .await?;
        anyhow::ensure!(
            columns == [vec!["id", "owner", "name", "description", "base", "vars", "tags", "version", "created_at", "updated_at", "scope", "auto"], vec!["session", "position", "agentenv_id", "auto", "applied_at", "applied_by"]],
            "{columns:?}"
        );
        let dup = store
            .call(|c| {
                c.execute("INSERT INTO agentenvs (id, owner, name, base, vars, created_at, updated_at) VALUES ('a', 'user:u1', 'x', 'user', '[]', 0, 0)", [])?;
                c.execute("INSERT INTO agentenvs (id, owner, name, base, vars, created_at, updated_at) VALUES ('b', 'user:u2', 'x', 'user', '[]', 0, 0)", [])?;
                Ok(c.execute("INSERT INTO agentenvs (id, owner, name, base, vars, created_at, updated_at) VALUES ('c', 'user:u1', 'x', 'user', '[]', 0, 0)", []).is_err())
            })
            .await?;
        anyhow::ensure!(dup, "a name once per owner");
        let defaults: (String, bool) = store.read(|c| Ok(c.query_row("SELECT scope, auto FROM agentenvs WHERE id = 'a'", [], |r| Ok((r.get(0)?, r.get(1)?)))?)).await?;
        anyhow::ensure!(defaults == ("[]".to_owned(), false), "none scoped, none applied on its own: {defaults:?}");
        let once = store
            .call(|c| {
                c.execute("INSERT INTO agentenv_stacks (session, position, agentenv_id, applied_at, applied_by) VALUES ('s', 0, 'a', 0, 'x')", [])?;
                Ok((
                    c.execute("INSERT INTO agentenv_stacks (session, position, agentenv_id, applied_at, applied_by) VALUES ('s', 1, 'a', 0, 'x')", []).is_err(),
                    c.execute("INSERT INTO agentenv_stacks (session, position, agentenv_id, applied_at, applied_by) VALUES ('s', 0, 'b', 0, 'x')", []).is_err(),
                ))
            })
            .await?;
        anyhow::ensure!(once == (true, true), "a stack holds each once, one at each place: {once:?}");
        Ok(())
    })
}

/// Each `from` takes what it says and nothing else; a name is a portable one; a refusal's code is read off its words.
#[test]
fn a_variable_takes_what_its_from_says() -> anyhow::Result<()> {
    let ok: Vec<Var> = serde_json::from_value(vars())?;
    assert_eq!(var_problem(&ok), None);
    for bad in [
        json!([{"name": "1A", "from": "unset"}]),
        json!([{"name": "A", "from": "value"}]),
        json!([{"name": "A", "from": "value", "value": "v", "secret": "S"}]),
        json!([{"name": "A", "from": "machine", "of": "B-C"}]),
        json!([{"name": "A", "from": "machine", "value": "v"}]),
        json!([{"name": "A", "from": "secret"}]),
        json!([{"name": "A", "from": "secret", "secret": "sk-ant-api03-pasted"}]),
        json!([{"name": "A", "from": "unset", "of": "B"}]),
    ] {
        let v: Vec<Var> = serde_json::from_value(bad.clone())?;
        let said = var_problem(&v);
        assert!(said.is_some(), "{bad}");
        assert!(!said.unwrap_or_default().contains("sk-ant-api03-pasted"), "a value put where a name goes isn't repeated");
    }
    assert_eq!(secrets_of(&ok), ["ANTHROPIC_KEY"]);
    let r = Refusal::SecretMissing(vec!["X".into()]);
    assert_eq!(typed(&r.said()), Some(("agentenv_secret_missing", r.to_string().as_str())));
    assert_eq!(typed("no such thing: here"), None);
    assert_eq!(Refusal::Unsupported("x".into()).said(), "x");
    Ok(())
}

/// Each person keeps their own: one name once among theirs, listed, read, changed over its version and deleted by them
/// alone; another's is none, an admin's look included; an agent changes none. Each change is told to its owner alone.
#[test]
fn a_person_keeps_their_own() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let (ada, bo) = (person("u-ada", &["developer"]), person("u-bo", &["admin"]));
        crate::ci::secrets::set(&hub, &crate::ci::secrets::user_scope("user:u-ada"), "ANTHROPIC_KEY", FAKE, "user:u-ada").await.map_err(|e| anyhow::anyhow!(e))?;
        let mut events = hub.subscribe_events()?;
        let made = op(&hub, &ada, "agentenvs.create", json!({"name": "work", "base": "user", "vars": vars()})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = text_of(&made, "/id");
        anyhow::ensure!(text_of(&made, "/owner") == "user:u-ada" && made.get("version") == Some(&json!(1)) && made.get("sessions") == Some(&json!([])), "{made}");
        anyhow::ensure!(!made.to_string().contains(FAKE), "a secret's value never shows: {made}");
        // Its name once among hers; a secret she holds none of, and a bad variable, refused with their codes.
        let again = op(&hub, &ada, "agentenvs.create", json!({"name": "work", "base": "clean", "vars": []})).await;
        anyhow::ensure!(matches!(&again, Err((StatusCode::CONFLICT, m)) if m.starts_with("agentenv_name_taken: ")), "{again:?}");
        let missing = op(&hub, &ada, "agentenvs.create", json!({"name": "other", "base": "user", "vars": [{"name": "T", "from": "secret", "secret": "gh_token"}]})).await;
        anyhow::ensure!(matches!(&missing, Err((StatusCode::BAD_REQUEST, m)) if m.starts_with("agentenv_secret_missing: ") && m.contains("GH_TOKEN")), "{missing:?}");
        let bad = op(&hub, &ada, "agentenvs.create", json!({"name": "other", "base": "user", "vars": [{"name": "A B", "from": "unset"}]})).await;
        anyhow::ensure!(matches!(&bad, Err((StatusCode::BAD_REQUEST, m)) if m.starts_with("agentenv_bad_var: ")), "{bad:?}");
        // Changed over its version; one made over an older version is refused.
        let v2 = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 1, "base": "clean", "description": "for work"})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(v2.get("version") == Some(&json!(2)) && v2.get("base") == Some(&json!("clean")) && text_of(&v2, "/description") == "for work", "{v2}");
        let stale = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 1, "name": "late"})).await;
        anyhow::ensure!(matches!(&stale, Err((StatusCode::CONFLICT, m)) if m.starts_with("agentenv_stale: ") && m.contains("version 2")), "{stale:?}");
        let cleared = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 2, "description": null})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(cleared.get("description").is_none() && cleared.get("version") == Some(&json!(3)), "{cleared}");
        // Bo, an admin, reaches none of hers, and lists none.
        for (name, input) in [("agentenvs.get", json!({"id": id})), ("agentenvs.update", json!({"id": id, "version": 3, "name": "x"})), ("agentenvs.delete", json!({"id": id}))] {
            let r = op(&hub, &bo, name, input).await;
            anyhow::ensure!(matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m.starts_with("agentenv_unknown: ")), "bo's {name}: {r:?}");
        }
        let theirs = op(&hub, &bo, "agentenvs.list", json!({})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(theirs == json!({"agentenvs": []}), "{theirs}");
        // An agent acting for her keeps, changes and applies none.
        for (name, input) in [("agentenvs.create", json!({"name": "a", "base": "user", "vars": []})), ("agentenvs.list", json!({}))] {
            let r = op(&hub, &agent_for("u-ada"), name, input).await;
            anyhow::ensure!(matches!(&r, Err((StatusCode::FORBIDDEN, _))), "an agent's {name}: {r:?}");
        }
        let mine = op(&hub, &ada, "agentenvs.list", json!({})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(mine.pointer("/agentenvs/0/id") == Some(&json!(id)), "{mine}");
        op(&hub, &ada, "agentenvs.delete", json!({"id": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let gone = op(&hub, &ada, "agentenvs.get", json!({"id": id})).await;
        anyhow::ensure!(matches!(&gone, Err((StatusCode::NOT_FOUND, _))), "{gone:?}");
        // Told: each change, hers, the last one gone.
        let mut told = Vec::new();
        while let Ok(t) = events.try_recv() {
            let v: Value = serde_json::from_str(&t)?;
            if v.get("type") == Some(&json!("agentenv")) {
                told.push((text_of(&v, "/owner"), text_of(&v, "/agentenv"), v.get("deleted").and_then(Value::as_bool).unwrap_or(false)));
            }
        }
        let a = "user:u-ada".to_owned();
        anyhow::ensure!(told == [(a.clone(), id.clone(), false), (a.clone(), id.clone(), false), (a.clone(), id.clone(), false), (a, id.clone(), true)], "{told:?}");
        anyhow::ensure!(hears(&ada, "user:u-ada") && !hears(&bo, "user:u-ada") && !hears(&agent_for("u-ada"), "user:u-ada"));
        Ok(())
    })
}

/// Its own tags, as its owner gives them: each trimmed, the empty dropped, once whatever their case, in order; all of
/// them replaced by a change that names them, kept by one that doesn't; one with a line break refused, saying so.
#[test]
fn an_environments_tags_are_kept_clean_in_order_and_changed_whole() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("u-ada", &["developer"]);
        let made = op(&hub, &ada, "agentenvs.create", json!({"name": "work", "base": "user", "vars": [], "tags": [" release ", "", "CI", "Release", "ci"]})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(made.get("tags") == Some(&json!(["release", "CI"])), "{made}");
        let id = text_of(&made, "/id");
        let named = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 1, "name": "work2"})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(named.get("tags") == Some(&json!(["release", "CI"])) && named.get("version") == Some(&json!(2)), "{named}");
        let retagged = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 2, "tags": ["aws"]})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(retagged.get("tags") == Some(&json!(["aws"])) && retagged.get("version") == Some(&json!(3)), "{retagged}");
        let bad = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 3, "tags": ["two\nlines"]})).await;
        anyhow::ensure!(matches!(&bad, Err((StatusCode::BAD_REQUEST, w)) if w.contains("control character")), "{bad:?}");
        Ok(())
    })
}

/// Opened for a session: each variable as its runner takes it, a secret's value read from its owner's own, marked used
/// and audited by its name alone; refused in a microVM naming its `secret` variables, and naming a secret deleted since.
#[test]
fn opened_for_a_session_its_secrets_are_read_marked_and_audited_by_name() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let scope = crate::ci::secrets::user_scope("user:u-ada");
        crate::ci::secrets::set(&hub, &scope, "ANTHROPIC_KEY", FAKE, "user:u-ada").await.map_err(|e| anyhow::anyhow!(e))?;
        let env = create(&hub, "user:u-ada", Draft { name: "work".into(), vars: serde_json::from_value(vars())?, ..Draft::default() }).await?;
        let r = resolve::open(&hub, &env, false, "user:u-ada", "a test").await?;
        anyhow::ensure!(
            r.vars == [("PATH_EXTRA".into(), Set::Machine("HOME".into())), ("MODE".into(), Set::Value("fast".into())), ("ANTHROPIC_API_KEY".into(), Set::Secret(FAKE.into())), ("NO_PROXY".into(), Set::Unset)],
            "{:?}",
            r.vars
        );
        anyhow::ensure!(r.id == env.id && r.version == 1 && !format!("{r:?}").contains(FAKE));
        let used = crate::ci::secrets::list(&hub, &scope).await?;
        anyhow::ensure!(used.iter().any(|m| m.name == "ANTHROPIC_KEY" && m.last_used_at.is_some()), "marked used");
        let rows: Vec<(String, String)> = hub.store.read(|c| Ok(c.prepare("SELECT action, detail FROM audit2 ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?)).await?;
        anyhow::ensure!(rows.iter().any(|(a, d)| a == "secret.use" && d.contains("ANTHROPIC_KEY") && d.contains(&env.id)), "{rows:?}");
        anyhow::ensure!(rows.iter().all(|(_, d)| !d.contains(FAKE)), "no value audited");
        let vm = resolve::open(&hub, &env, true, "user:u-ada", "a test").await;
        anyhow::ensure!(matches!(&vm, Err(Refusal::SecretInMicrovm(v)) if v == &["ANTHROPIC_API_KEY".to_owned()]), "{vm:?}");
        crate::ci::secrets::delete(&hub, &scope, "ANTHROPIC_KEY").await?;
        let gone = resolve::open(&hub, &env, false, "user:u-ada", "a test").await;
        anyhow::ensure!(matches!(&gone, Err(Refusal::SecretMissing(v)) if v == &["ANTHROPIC_KEY".to_owned()]), "{gone:?}");
        Ok(())
    })
}

/// A start with an environment: opened into its spec and kept as the one it runs with, so it can't be deleted while it
/// runs, and an agent it starts runs with it too; another's, a microVM given a secret, and a runner that runs none are
/// refused with nothing started.
#[test]
fn a_start_runs_with_its_persons_environment_or_is_refused() -> anyhow::Result<()> {
    use crate::harness::lineage::{Ask, Starter, start};
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let dir = tempfile::tempdir()?;
        let ada = person("u-ada", &["admin"]);
        let scope = crate::ci::secrets::user_scope("user:u-ada");
        crate::ci::secrets::set(&hub, &scope, "ANTHROPIC_KEY", FAKE, "user:u-ada").await.map_err(|e| anyhow::anyhow!(e))?;
        let env = create(&hub, "user:u-ada", Draft { name: "work".into(), vars: serde_json::from_value(vars())?, ..Draft::default() }).await?;
        let by = Starter { actor: ada.name.clone(), asker: Some(ada.clone()), ..Starter::default() };
        let ask = |agentenv: &str| Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(dir.path().to_string_lossy().into_owned()), workspace: Some("folder".into()), agentenvs: Some(vec![agentenv.into()]), ..Ask::default() };
        // Another's is none.
        let bos = create(&hub, "user:u-bo", Draft { name: "bo".into(), ..Draft::default() }).await?;
        let why = start(&hub, &by, ask(&bos.id)).await.err().map(|(_, w)| w).unwrap_or_default();
        anyhow::ensure!(why.starts_with("agentenv_unknown: "), "{why}");
        // In a microVM its secret is refused, naming the variable (a start's check of each target it's placed on).
        let why = resolve::runs_there(&hub, &env, "local", true).map_err(|r| r.said()).err().unwrap_or_default();
        anyhow::ensure!(why.starts_with("agentenv_secret_in_microvm: ") && why.contains("ANTHROPIC_API_KEY") && !why.contains(FAKE), "{why}");
        // A runner that runs none.
        hub.put_agent(crate::hub::tests::live_runner("old-box", "linux", "x86_64"));
        let old = Ask { on: Some("old-box".into()), ..ask(&env.id) };
        let why = start(&hub, &by, old).await.err().map(|(_, w)| w).unwrap_or_default();
        anyhow::ensure!(why.contains("runs no session with an environment"), "{why}");
        anyhow::ensure!(hub.harness.list().is_empty(), "nothing started");
        // Hers, on the server: started with it, kept as the one it runs with.
        let started = start(&hub, &by, ask(&env.id)).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = started.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(stack(&hub, &id) == [Link::named(&env.id)], "kept");
        anyhow::ensure!(sessions_of(&hub, &env.id) == std::slice::from_ref(&id));
        let refused = delete(&hub, "user:u-ada", &env.id).await;
        anyhow::ensure!(matches!(&refused, Err(Refusal::InUse(s)) if s.contains(&id)), "{refused:?}");
        anyhow::ensure!(refused.map(|_| ()).err().is_some_and(|r| r.said().contains(&id)), "the refusal names it");
        // An agent it starts names none: it runs with its parent's, kept for it too.
        let parent = started.first().cloned().ok_or_else(|| anyhow::anyhow!("no session"))?;
        let child = Starter { parent: Some(parent), actor: format!("agent:{id}"), opener: Some("user:u-ada".into()), ..Starter::default() };
        let kids = start(&hub, &child, Ask { agentenvs: None, ..ask("") }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let kid = kids.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(stack(&hub, &kid) == [Link::named(&env.id)], "a child runs with its parent's");
        anyhow::ensure!(sessions_of(&hub, &env.id).len() == 2);
        // Kept across a restart of the server: read again as it starts.
        let again = Held::default();
        again.load(&hub.store).await?;
        anyhow::ensure!(again.links.load().get(&id) == Some(&vec![Link::named(&env.id)]));
        // A command naming an environment is a client's, refused whoever sends it.
        let sent = hub.harness.command(&hub, &id, crate::harness::model::Command::Agentenv { agentenv: None, restart: true }).await;
        anyhow::ensure!(matches!(&sent, crate::harness::registry::Sent::Rejected { why, .. } if why.contains("agentenvs.apply")), "a client's Agentenv");
        Ok(())
    })
}

/// A stack opened as one, as its runner holds it: its ids top first joined by `+`, its names by ` + `, version 0 (one
/// alone keeps its own), the top one's base, the bottom one's variables first so the top one's win; in a microVM every
/// secret variable of the stack named.
#[test]
fn a_stack_opens_as_one_environment_the_top_winning() -> anyhow::Result<()> {
    use crate::harness::agentenv::{Base, Resolved};
    let r = |id: &str, base, vars: &[(&str, Set)]| Resolved { id: id.into(), name: id.to_uppercase(), version: 7, base, vars: vars.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect() };
    let top = r("t", Base::Clean, &[("MODE", Set::Value("top".into())), ("ONLY_TOP", Set::Unset)]);
    let mid = r("m", Base::User, &[("MODE", Set::Value("mid".into())), ("REGION", Set::Value("eu".into()))]);
    let low = r("l", Base::User, &[("ONLY_TOP", Set::Value("low".into()))]);
    let one = resolve::flatten(vec![top.clone(), mid.clone(), low.clone()]).ok_or_else(|| anyhow::anyhow!("none"))?;
    anyhow::ensure!(one.id == "t+m+l" && one.name == "T + M + L" && one.version == 0 && one.base == Base::Clean, "{one:?}");
    let names: Vec<&str> = one.vars.iter().map(|(k, _)| k.as_str()).collect();
    anyhow::ensure!(names == ["ONLY_TOP", "MODE", "REGION", "MODE", "ONLY_TOP"], "bottom first, top last: {names:?}");
    let layered = crate::harness::agentenv::layer(&one, &[]);
    let last = |k: &str| layered.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    anyhow::ensure!(last("MODE") == Some(Some("top".into())) && last("ONLY_TOP") == Some(None) && last("REGION") == Some(Some("eu".into())), "{layered:?}");
    anyhow::ensure!(resolve::flatten(vec![mid.clone()]) == Some(mid), "one alone is itself");
    anyhow::ensure!(resolve::flatten(Vec::new()).is_none(), "none: the machine's user's");
    anyhow::ensure!(stack_id(["a", "b"]) == "a+b");
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        crate::ci::secrets::set(&hub, &crate::ci::secrets::user_scope("user:u-ada"), "ANTHROPIC_KEY", FAKE, "user:u-ada").await.map_err(|e| anyhow::anyhow!(e))?;
        let plain = create(&hub, "user:u-ada", Draft { name: "plain".into(), vars: serde_json::from_value(json!([{"name": "A", "from": "value", "value": "1"}]))?, ..Draft::default() }).await?;
        let secret = create(&hub, "user:u-ada", Draft { name: "secret".into(), vars: serde_json::from_value(vars())?, ..Draft::default() }).await?;
        let both = [plain.clone(), secret.clone()];
        let opened = resolve::open_stack(&hub, &both, false, "user:u-ada", "a test").await?.ok_or_else(|| anyhow::anyhow!("none"))?;
        anyhow::ensure!(opened.id == format!("{}+{}", plain.id, secret.id) && opened.name == "plain + secret" && opened.vars.len() == 5, "{opened:?}");
        let vm = resolve::open_stack(&hub, &both, true, "user:u-ada", "a test").await;
        anyhow::ensure!(matches!(&vm, Err(Refusal::SecretInMicrovm(v)) if v == &["ANTHROPIC_API_KEY".to_owned()]), "{vm:?}");
        Ok(())
    })
}

/// A scope and `auto` kept as given (each chip once, trimmed), changed whole by an update that names them; a bad chip
/// refused with `agentenv_bad_var` (a key kiln doesn't know, a value with no harness where one is named, an empty one).
#[test]
fn a_scope_is_kept_and_checked() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("u-ada", &["developer"]);
        let made = op(&hub, &ada, "agentenvs.create", json!({"name": "w", "base": "user", "vars": [], "scope": [{"key": "os", "value": " linux "}, {"key": "os", "value": "linux"}, {"key": "permission", "value": "claude:plan"}], "auto": true})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(made.get("scope") == Some(&json!([{"key": "os", "value": "linux"}, {"key": "permission", "value": "claude:plan"}])) && made.get("auto") == Some(&json!(true)), "{made}");
        let id = text_of(&made, "/id");
        let changed = op(&hub, &ada, "agentenvs.update", json!({"id": id, "version": 1, "scope": [{"key": "arch", "value": "arm64"}], "auto": false})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(changed.get("scope") == Some(&json!([{"key": "arch", "value": "arm64"}])) && changed.get("auto") == Some(&json!(false)) && changed.get("version") == Some(&json!(2)), "{changed}");
        let plain = op(&hub, &ada, "agentenvs.create", json!({"name": "plain", "base": "user", "vars": []})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(plain.get("scope") == Some(&json!([])) && plain.get("auto") == Some(&json!(false)), "{plain}");
        for bad in [json!([{"key": "colour", "value": "red"}]), json!([{"key": "effort", "value": "high"}]), json!([{"key": "model", "value": ""}]), json!([{"key": "agent", "value": "a\u{7}"}])] {
            let r = op(&hub, &ada, "agentenvs.create", json!({"name": "bad", "base": "user", "vars": [], "scope": bad})).await;
            anyhow::ensure!(matches!(&r, Err((StatusCode::BAD_REQUEST, m)) if m.starts_with("agentenv_bad_var: ") || m.contains("scope")), "{bad}: {r:?}");
        }
        Ok(())
    })
}

/// Those applied on their own that join a session: its person's, `auto` on, scope matching, none of those given by
/// name, more chips first then by name; in a microVM one with a secret is skipped.
#[test]
fn applied_on_its_own_joins_by_scope_most_specific_first() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        crate::ci::secrets::set(&hub, &crate::ci::secrets::user_scope("user:u-ada"), "ANTHROPIC_KEY", FAKE, "user:u-ada").await.map_err(|e| anyhow::anyhow!(e))?;
        let chip = |key, value: &str| scope::Chip { key, value: value.into() };
        let auto = |name: &str, scope: Vec<scope::Chip>| Draft { name: name.into(), scope, auto: true, ..Draft::default() };
        let broad = create(&hub, "user:u-ada", auto("broad", vec![])).await?;
        let specific = create(&hub, "user:u-ada", auto("specific", vec![chip(scope::ScopeKey::Harness, "claude"), chip(scope::ScopeKey::Os, "macos")])).await?;
        let also = create(&hub, "user:u-ada", auto("also", vec![])).await?;
        let secret = create(&hub, "user:u-ada", Draft { vars: serde_json::from_value(vars())?, ..auto("secretly", vec![]) }).await?;
        create(&hub, "user:u-ada", auto("elsewhere", vec![chip(scope::ScopeKey::Os, "windows")])).await?;
        create(&hub, "user:u-ada", Draft { name: "off".into(), ..Draft::default() }).await?;
        create(&hub, "user:u-bo", auto("bos", vec![])).await?;
        let facts = scope::Facts { machine: "m".into(), os: "macos".into(), arch: "arm64".into(), harness: "claude".into(), model: "m".into(), permission: "claude:default".into(), ..scope::Facts::default() };
        let ids = |v: Vec<Agentenv>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        anyhow::ensure!(ids(autos(&hub, "user:u-ada", &facts, false, &[]).await?) == ["specific", "also", "broad", "secretly"]);
        anyhow::ensure!(ids(autos(&hub, "user:u-ada", &facts, true, &[]).await?) == ["specific", "also", "broad"], "a secret never joins a microVM on its own");
        anyhow::ensure!(ids(autos(&hub, "user:u-ada", &facts, false, std::slice::from_ref(&also.id)).await?) == ["specific", "broad", "secretly"], "one given by name isn't joined twice");
        let codex = scope::Facts { harness: "codex".into(), permission: "codex:auto".into(), ..facts };
        anyhow::ensure!(ids(autos(&hub, "user:u-ada", &codex, false, &[]).await?) == ["also", "broad", "secretly"]);
        anyhow::ensure!(broad.auto && specific.scope.len() == 2 && secret.auto);
        Ok(())
    })
}

/// A start's stack: those given by name, top first, then those applied on their own that match it, opened as one into
/// its spec (its runner holds the stack's id); a child given none runs with its parent's given by name, those applied
/// on their own matched for it again. An environment applied on its own made, or changed, joins and leaves the stacks
/// of its person's sessions (never one given by name), and one a stack holds can't be deleted.
#[test]
fn a_starts_stack_and_applied_on_its_own_joining_and_leaving() -> anyhow::Result<()> {
    use crate::harness::lineage::{Ask, Starter, start};
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let dir = tempfile::tempdir()?;
        let ada = person("u-ada", &["admin"]);
        let chip = |key, value: &str| scope::Chip { key, value: value.into() };
        let here = std::env::consts::OS;
        let named_a = create(&hub, "user:u-ada", Draft { name: "a".into(), ..Draft::default() }).await?;
        let named_b = create(&hub, "user:u-ada", Draft { name: "b".into(), ..Draft::default() }).await?;
        let mine = create(&hub, "user:u-ada", Draft { name: "mine".into(), scope: vec![chip(scope::ScopeKey::Os, here), chip(scope::ScopeKey::Harness, "claude")], auto: true, ..Draft::default() }).await?;
        let m2_only = create(&hub, "user:u-ada", Draft { name: "m2".into(), scope: vec![chip(scope::ScopeKey::Model, "m2")], auto: true, ..Draft::default() }).await?;
        let by = Starter { actor: ada.name.clone(), asker: Some(ada.clone()), ..Starter::default() };
        let ask = |envs: Vec<String>| Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(dir.path().to_string_lossy().into_owned()), workspace: Some("folder".into()), agentenvs: Some(envs), ..Ask::default() };
        let twice = start(&hub, &by, ask(vec![named_a.id.clone(), named_a.id.clone()])).await.err().map(|(_, w)| w).unwrap_or_default();
        anyhow::ensure!(twice.starts_with("agentenv_bad_var: ") && twice.contains("twice"), "{twice}");
        let started = start(&hub, &by, ask(vec![named_b.id.clone(), named_a.id.clone()])).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let parent = started.first().cloned().ok_or_else(|| anyhow::anyhow!("no session"))?;
        let id = parent.id.clone();
        let want = vec![Link::named(&named_b.id), Link::named(&named_a.id), Link { id: mine.id.clone(), auto: true }];
        anyhow::ensure!(stack(&hub, &id) == want, "{:?}", stack(&hub, &id));
        let held = parent.agentenv.clone().or_else(|| hub.harness.info(&id).and_then(|i| i.agentenv));
        anyhow::ensure!(held.as_ref().is_some_and(|b| b.id == format!("{}+{}+{}", named_b.id, named_a.id, mine.id) && b.name == "b + a + mine" && b.version == 0), "{held:?}");
        // A child given none: its parent's given by name, and those applied on their own matched for it (on model m2).
        let child = Starter { parent: Some(parent.clone()), actor: format!("agent:{id}"), opener: Some("user:u-ada".into()), ..Starter::default() };
        let kids = start(&hub, &child, Ask { model: Some("m2".into()), agentenvs: None, ..ask(Vec::new()) }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let kid = kids.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(stack(&hub, &kid) == [Link::named(&named_b.id), Link::named(&named_a.id), Link { id: mine.id.clone(), auto: true }, Link { id: m2_only.id.clone(), auto: true }], "{:?}", stack(&hub, &kid));
        // One given [] names none: those applied on their own alone.
        let bare = start(&hub, &by, ask(Vec::new())).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let bare = bare.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(stack(&hub, &bare) == [Link { id: mine.id.clone(), auto: true }], "{:?}", stack(&hub, &bare));
        // Held, it can't be deleted.
        anyhow::ensure!(matches!(delete(&hub, "user:u-ada", &mine.id).await, Err(Refusal::InUse(s)) if s.contains(&id) && s.contains(&bare)));
        // A new one applied on its own joins at once, under those given by name, among those applied on their own by its chips.
        let wide = create(&hub, "user:u-ada", Draft { name: "wide".into(), auto: true, ..Draft::default() }).await?;
        anyhow::ensure!(stack(&hub, &id) == [Link::named(&named_b.id), Link::named(&named_a.id), Link { id: mine.id.clone(), auto: true }, Link { id: wide.id.clone(), auto: true }], "{:?}", stack(&hub, &id));
        let narrow = create(&hub, "user:u-ada", Draft { name: "narrow".into(), scope: vec![chip(scope::ScopeKey::Harness, "claude"), chip(scope::ScopeKey::Os, here), chip(scope::ScopeKey::Machine, "local")], auto: true, ..Draft::default() }).await?;
        anyhow::ensure!(stack(&hub, &bare) == [Link { id: narrow.id.clone(), auto: true }, Link { id: mine.id.clone(), auto: true }, Link { id: wide.id.clone(), auto: true }], "{:?}", stack(&hub, &bare));
        // Turned off, it leaves those it joined on its own; scoped away, likewise; given by name, it stays.
        let wide = update(&hub, "user:u-ada", &wide.id, 1, Patch { auto: Some(false), ..Patch::default() }).await?;
        anyhow::ensure!(!stack(&hub, &id).iter().any(|l| l.id == wide.id) && !stack(&hub, &bare).iter().any(|l| l.id == wide.id) && wide.sessions.is_empty(), "{wide:?}");
        delete(&hub, "user:u-ada", &wide.id).await?;
        update(&hub, "user:u-ada", &named_a.id, 1, Patch { scope: Some(vec![chip(scope::ScopeKey::Os, "plan9")]), auto: Some(true), ..Patch::default() }).await?;
        anyhow::ensure!(stack(&hub, &id).first() == Some(&Link::named(&named_b.id)) && stack(&hub, &id).get(1) == Some(&Link::named(&named_a.id)), "given by name, it stays: {:?}", stack(&hub, &id));
        anyhow::ensure!(!stack(&hub, &bare).iter().any(|l| l.id == named_a.id), "a scope that doesn't match joins none");
        update(&hub, "user:u-ada", &narrow.id, 1, Patch { scope: Some(vec![chip(scope::ScopeKey::Os, "plan9")]), ..Patch::default() }).await?;
        anyhow::ensure!(stack(&hub, &bare) == [Link { id: mine.id.clone(), auto: true }], "scoped away, it left: {:?}", stack(&hub, &bare));
        // Another person's applied on their own never joins hers.
        create(&hub, "user:u-bo", Draft { name: "bos".into(), auto: true, ..Draft::default() }).await?;
        anyhow::ensure!(stack(&hub, &bare).len() == 1);
        // As its scope sees it, through its operation: what it is, its stack, and which of hers match it.
        let seen = op(&hub, &ada, "agentenvs.session", json!({"agent": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(text_of(&seen, "/facts/machine") == "local" && text_of(&seen, "/facts/os") == here && text_of(&seen, "/facts/harness") == "claude" && text_of(&seen, "/facts/permission").starts_with("claude:"), "{seen}");
        anyhow::ensure!(seen.pointer("/stack/2") == Some(&json!({"id": mine.id, "name": "mine", "version": 1, "auto": true})), "{seen}");
        let matching: Vec<String> = serde_json::from_value(seen.get("matching").cloned().unwrap_or_default())?;
        anyhow::ensure!(matching.contains(&mine.id) && matching.contains(&named_b.id) && !matching.contains(&m2_only.id) && !matching.contains(&named_a.id), "{matching:?}");
        // Kept across a restart of the server, in order, with how each got there.
        let again = Held::default();
        again.load(&hub.store).await?;
        anyhow::ensure!(again.links.load().get(&id) == Some(&stack(&hub, &id)));
        Ok(())
    })
}

/// `agentenvs.apply` takes those a session is given by name, top first, each of the caller's once: those applied on their
/// own that match it go under them, one named among them given by name from then on; `[]` leaves those alone; it answers
/// with the stack as it is now.
#[test]
fn applying_a_stack_replaces_it_whole() -> anyhow::Result<()> {
    use crate::harness::lineage::{Ask, Starter, start};
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let dir = tempfile::tempdir()?;
        let ada = person("u-ada", &["admin"]);
        let a = create(&hub, "user:u-ada", Draft { name: "a".into(), ..Draft::default() }).await?;
        let b = create(&hub, "user:u-ada", Draft { name: "b".into(), ..Draft::default() }).await?;
        let on_own = create(&hub, "user:u-ada", Draft { name: "own".into(), auto: true, ..Draft::default() }).await?;
        let bos = create(&hub, "user:u-bo", Draft { name: "bo".into(), ..Draft::default() }).await?;
        let by = Starter { actor: ada.name.clone(), asker: Some(ada.clone()), ..Starter::default() };
        let started = start(&hub, &by, Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(dir.path().to_string_lossy().into_owned()), workspace: Some("folder".into()), agentenvs: Some(vec![a.id.clone()]), ..Ask::default() }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = started.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(stack(&hub, &id) == [Link::named(&a.id), Link { id: on_own.id.clone(), auto: true }]);
        for (bad, why) in [(json!([a.id, a.id]), "twice"), (json!([" "]), "names no environment"), (json!([bos.id]), "agentenv_unknown")] {
            let r = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": bad, "restart": false})).await;
            anyhow::ensure!(matches!(&r, Err((_, m)) if m.contains(why)), "{bad}: {r:?}");
        }
        let missing = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenv": a.id})).await;
        anyhow::ensure!(missing.is_err(), "the one-environment field is gone: {missing:?}");
        let reordered = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": [b.id, a.id], "restart": false})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(
            reordered
                == json!({"agent": id, "stack": [{"id": b.id, "name": "b", "version": 1, "auto": false}, {"id": a.id, "name": "a", "version": 1, "auto": false}, {"id": on_own.id, "name": "own", "version": 1, "auto": true}], "restarted": false}),
            "{reordered}"
        );
        let applied = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": [on_own.id, b.id, a.id], "restart": false})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(
            applied
                == json!({"agent": id, "stack": [{"id": on_own.id, "name": "own", "version": 1, "auto": false}, {"id": b.id, "name": "b", "version": 1, "auto": false}, {"id": a.id, "name": "a", "version": 1, "auto": false}], "restarted": false}),
            "named, it's given by name: {applied}"
        );
        anyhow::ensure!(stack(&hub, &id) == [Link::named(&on_own.id), Link::named(&b.id), Link::named(&a.id)]);
        let none = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": [], "restart": false})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(none == json!({"agent": id, "stack": [{"id": on_own.id, "name": "own", "version": 1, "auto": true}], "restarted": false}), "those applied on their own alone: {none}");
        update(&hub, "user:u-ada", &on_own.id, 1, Patch { auto: Some(false), ..Patch::default() }).await?;
        let bare = op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": []})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(bare == json!({"agent": id, "stack": [], "restarted": true}) && !stacked(&hub, &id), "{bare}");
        Ok(())
    })
}

/// The person's decision, 2026-11-21 (docs/agents.md, "Environments"): a running agent takes every change to its
/// environments, its harness starting again on it once nothing is under way, its conversation going on. A change to the
/// variables of one it was given by name reaches its harness, and so does one applied on its own made to match it, which
/// joins its stack; neither waits for a next start nothing would bring (an `ANTHROPIC_API_KEY` set in the UI never
/// reached the running agent it was meant for). Its harness says what it started with each time it started.
#[test]
#[cfg(unix)]
fn a_running_agent_takes_every_change_to_its_environments() -> anyhow::Result<()> {
    use crate::harness::ops::end_to_end::{hub_with, recording_program, source_ready, source_until};
    use std::os::unix::fs::PermissionsExt as _;
    crate::harness::ops::end_to_end::run(async {
        let hub = hub_with(|_| {}).await?;
        let dir = tempfile::tempdir()?;
        let (recording, _) = recording_program(&dir.path().join("harness"), true)?;
        let seen = dir.path().join("seen");
        let program = dir.path().join("harness").join("seeing-claude");
        std::fs::write(&program, format!("#!/bin/sh\ncase \" $* \" in *\" --version \"*) ;; *) printf '%s/%s\\n' \"${{AGENTENV_KEY:--}}\" \"${{AGENTENV_PROBE:--}}\" >> '{}' ;; esac\nexec '{}' \"$@\"\n", seen.display(), recording.display()))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        let lines = || std::fs::read_to_string(&seen).unwrap_or_default().lines().map(str::to_owned).collect::<Vec<_>>();
        let last_is = |want: &'static str| {
            let now = lines();
            async move { anyhow::Ok(now.last().is_some_and(|l| l == want)) }
        };
        let ada = person("u-ada", &["admin"]);
        let var = |name: &str, v: &str| serde_json::from_value::<Vec<Var>>(json!([{"name": name, "from": "value", "value": v}]));
        let env = create(&hub, "user:u-ada", Draft { name: "key".into(), vars: var("AGENTENV_KEY", "one")?, ..Draft::default() }).await?;
        let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), reach: Some("off".into()), ..Default::default() };
        let id = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no session"))?;
        // Its person, as a start records whom it acts for.
        hub.store.kv_set(crate::mcp::opened_by_key(&id), "user:u-ada".to_owned()).await?;
        source_ready(&hub, &id).await?;

        // Given by name: its harness starts again with it.
        op(&hub, &ada, "agentenvs.apply", json!({"agent": id, "agentenvs": [env.id]})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        source_until(&hub, &id, "its harness started again with the environment it was given", || last_is("one/-")).await?;

        // Its variables changed: its harness starts again with the new value.
        update(&hub, "user:u-ada", &env.id, 1, Patch { vars: Some(var("AGENTENV_KEY", "two")?), ..Patch::default() }).await?;
        source_until(&hub, &id, "its harness started again with its environment's new value", || last_is("two/-")).await?;

        // One applied on its own, made to match it: it joins its stack under the one given by name, and its harness
        // starts again with both.
        let on_own = create(&hub, "user:u-ada", Draft { name: "everywhere".into(), vars: var("AGENTENV_PROBE", "joined")?, auto: true, ..Draft::default() }).await?;
        anyhow::ensure!(stack(&hub, &id) == [Link::named(&env.id), Link { id: on_own.id.clone(), auto: true }], "{:?}", stack(&hub, &id));
        source_until(&hub, &id, "its harness started again with the one applied on its own", || last_is("two/joined")).await?;
        let mut each = lines();
        each.dedup();
        anyhow::ensure!(each == ["-/-", "one/-", "two/-", "two/joined"], "its harness started with {:?}", lines());
        hub.harness.local.stop_all();
        Ok(())
    })
}
