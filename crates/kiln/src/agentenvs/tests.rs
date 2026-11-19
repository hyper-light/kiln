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
                for table in ["agentenvs", "agentenv_sessions"] {
                    out.push(c.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?.query_map([table], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?);
                }
                Ok(out)
            })
            .await?;
        anyhow::ensure!(
            columns == [vec!["id", "owner", "name", "description", "base", "vars", "tags", "version", "created_at", "updated_at"], vec!["session", "agentenv_id", "applied_at", "applied_by"]],
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
        let ask = |agentenv: &str| Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(dir.path().to_string_lossy().into_owned()), workspace: Some("folder".into()), agentenv: Some(agentenv.into()), ..Ask::default() };
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
        anyhow::ensure!(linked(&hub, &id).as_deref() == Some(env.id.as_str()), "kept");
        anyhow::ensure!(sessions_of(&hub, &env.id) == std::slice::from_ref(&id));
        let refused = delete(&hub, "user:u-ada", &env.id).await;
        anyhow::ensure!(matches!(&refused, Err(Refusal::InUse(s)) if s.contains(&id)), "{refused:?}");
        anyhow::ensure!(refused.map(|_| ()).err().is_some_and(|r| r.said().contains(&id)), "the refusal names it");
        // An agent it starts names none: it runs with its parent's, kept for it too.
        let parent = started.first().cloned().ok_or_else(|| anyhow::anyhow!("no session"))?;
        let child = Starter { parent: Some(parent), actor: format!("agent:{id}"), opener: Some("user:u-ada".into()), ..Starter::default() };
        let kids = start(&hub, &child, Ask { agentenv: None, ..ask("") }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let kid = kids.first().map(|i| i.id.clone()).unwrap_or_default();
        anyhow::ensure!(linked(&hub, &kid).as_deref() == Some(env.id.as_str()), "a child runs with its parent's");
        anyhow::ensure!(sessions_of(&hub, &env.id).len() == 2);
        // Kept across a restart of the server: read again as it starts.
        let again = Held::default();
        again.load(&hub.store).await?;
        anyhow::ensure!(again.links.load().get(&id) == Some(&env.id));
        // A command naming an environment is a client's, refused whoever sends it.
        let sent = hub.harness.command(&hub, &id, crate::harness::model::Command::Agentenv { agentenv: None, restart: true }).await;
        anyhow::ensure!(matches!(&sent, crate::harness::registry::Sent::Rejected { why, .. } if why.contains("agentenvs.apply")), "a client's Agentenv");
        Ok(())
    })
}
