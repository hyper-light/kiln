//! Agentfiles kept, versioned and reached only by their owner (and those who reach everyone's), through
//! the operation table as every surface calls it (`crate::ops::call`: the route table, the input's type,
//! the audit). What Shards says of a text is `check`'s tests'; here the server's `shards`, if any, checks
//! the valid texts these keep.

use std::collections::HashMap;

use http::StatusCode;
use serde_json::{Value, json};

use super::*;
use crate::authz::gate::{Kind, Rights};
use crate::authz::{Caller, Grant};

/// A person, `user:<id>`, holding `grants` on the server.
fn person(id: &str, grants: &[&str]) -> Caller {
    let rights = Rights { grants: grants.iter().filter_map(|g| Grant::parse(g).ok()).collect(), custom: HashMap::new() };
    let mut c = Caller::with(Kind::User, &format!("user:{id}"), Some(rights), None);
    c.user = Some(id.to_owned());
    c
}

/// An agent session acting for person `id`, as kiln's MCP server makes one (`mcp::session_caller`).
fn agent_for(id: &str, grants: &[&str]) -> Caller {
    let mut c = person(id, grants);
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

const CLAUDE: &str = "FROM ubuntu:24.04\nAGENT main FROM ./agent\nCMD [\"claude\"]\n";
const CLAUDE_2: &str = "FROM ubuntu:24.04\nAGENT main FROM ./agent\nAGENT helper FROM ./helper\nCMD [\"claude\"]\n";

#[test]
fn step30_the_migration_makes_its_tables() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let store = crate::store::Store::in_memory()?;
        let (tables, unique): (Vec<String>, i64) = store
            .read(|c| {
                let t = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'agentfile%' ORDER BY name")?.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                let u = c.query_row("SELECT COUNT(*) FROM pragma_index_list('agentfiles') WHERE \"unique\" = 1 AND origin = 'u'", [], |r| r.get(0))?;
                Ok((t, u))
            })
            .await?;
        anyhow::ensure!(tables == ["agentfile_versions", "agentfiles"], "{tables:?}");
        anyhow::ensure!(unique == 1, "a name once per owner");
        // A second Agentfile of one name and owner is refused by the table itself.
        let dup = store
            .call(|c| {
                c.execute("INSERT INTO agentfiles (id, owner, name, text, created_at, updated_at) VALUES ('a', 'user:u1', 'x', 't', 0, 0)", [])?;
                c.execute("INSERT INTO agentfiles (id, owner, name, text, created_at, updated_at) VALUES ('b', 'user:u2', 'x', 't', 0, 0)", [])?;
                Ok(c.execute("INSERT INTO agentfiles (id, owner, name, text, created_at, updated_at) VALUES ('c', 'user:u1', 'x', 't', 0, 0)", []).is_err())
            })
            .await?;
        anyhow::ensure!(dup, "the same owner and name went in twice");
        Ok(())
    })
}

#[test]
fn step30_names_and_texts() {
    assert_eq!(name_problem("claude-rust"), None);
    assert_eq!(name_problem("a.b_c"), None);
    for bad in ["", "Claude", "-x", "x-", "a/b", ".hidden", "with space", &"x".repeat(256)] {
        assert!(name_problem(bad).is_some(), "{bad:?}");
    }
    assert!(text_problem(" \n").is_some());
    assert!(text_problem(&"#".repeat(MOST + 1)).is_some());
    assert_eq!(text_problem(CLAUDE), None);
}

/// Each person keeps their own: one name once among theirs (another person may use it), listed, read
/// and changed by them alone; another's is none, through every operation.
#[test]
fn step30_a_person_reaches_only_their_own() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let (ada, bo) = (person("u-ada", &["developer"]), person("u-bo", &["developer"]));
        let mine = op(&hub, &ada, "agentfiles.create", json!({"name": "claude", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = text_of(&mine, "/agentfile/id");
        anyhow::ensure!(text_of(&mine, "/agentfile/owner") == "user:u-ada" && mine.pointer("/agentfile/version") == Some(&json!(1)), "{mine}");
        anyhow::ensure!(mine.pointer("/check/checked").is_some(), "what Shards said comes with it: {mine}");
        // Another person may use the name; its owner can't twice.
        let theirs = op(&hub, &bo, "agentfiles.create", json!({"name": "claude", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let bo_id = text_of(&theirs, "/agentfile/id");
        let again = op(&hub, &ada, "agentfiles.create", json!({"name": "claude", "text": CLAUDE_2})).await;
        anyhow::ensure!(matches!(&again, Err((StatusCode::CONFLICT, m)) if m.contains("claude")), "{again:?}");
        // Each lists their own.
        for (who, want) in [(&ada, &id), (&bo, &bo_id)] {
            let l = op(&hub, who, "agentfiles.list", json!({})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let ids: Vec<String> = l.get("agentfiles").and_then(Value::as_array).into_iter().flatten().map(|a| text_of(a, "/id")).collect();
            anyhow::ensure!(ids == [want.clone()], "{} lists {ids:?}", who.name);
            anyhow::ensure!(l.get("builtin").is_some_and(Value::is_array) && l.get("project").is_none() && l.get("reaches_all") == Some(&json!(false)), "{l}");
        }
        // Bo reaches none of Ada's: each is as no Agentfile.
        for (name, input) in [
            ("agentfiles.get", json!({"id": id})),
            ("agentfiles.versions", json!({"id": id})),
            ("agentfiles.update", json!({"id": id, "text": CLAUDE_2})),
            ("agentfiles.restore", json!({"id": id, "version": 1})),
            ("agentfiles.delete", json!({"id": id})),
        ] {
            let r = op(&hub, &bo, name, input).await;
            anyhow::ensure!(matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m == "no such Agentfile"), "bo's {name}: {r:?}");
        }
        let all = op(&hub, &bo, "agentfiles.list", json!({"all": true})).await;
        anyhow::ensure!(matches!(&all, Err((StatusCode::FORBIDDEN, m)) if m.contains(MANAGE)), "{all:?}");
        // Ada's is as it was.
        let got = op(&hub, &ada, "agentfiles.get", json!({"id": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(text_of(&got, "/text") == CLAUDE && got.get("version") == Some(&json!(1)), "{got}");
        Ok(())
    })
}

/// A new text is a new version; a new name alone is none; a restore is a new version with the old text;
/// a delete takes its versions too. Each change is audited once, and told on the events sockets.
#[test]
fn step30_versions_restore_and_delete() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("u-ada", &["developer"]);
        let mut events = hub.subscribe_events()?;
        let made = op(&hub, &ada, "agentfiles.create", json!({"name": "claude", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = text_of(&made, "/agentfile/id");
        let v2 = op(&hub, &ada, "agentfiles.update", json!({"id": id, "text": CLAUDE_2})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(v2.pointer("/agentfile/version") == Some(&json!(2)), "{v2}");
        let renamed = op(&hub, &ada, "agentfiles.update", json!({"id": id, "name": "claude-code"})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(renamed.pointer("/agentfile/version") == Some(&json!(2)) && text_of(&renamed, "/agentfile/name") == "claude-code", "{renamed}");
        let bad = op(&hub, &ada, "agentfiles.update", json!({"id": id, "name": "Not An Image"})).await;
        anyhow::ensure!(matches!(&bad, Err((StatusCode::BAD_REQUEST, _))), "{bad:?}");
        let v3 = op(&hub, &ada, "agentfiles.restore", json!({"id": id, "version": 1})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(v3.pointer("/agentfile/version") == Some(&json!(3)) && text_of(&v3, "/agentfile/text") == CLAUDE, "{v3}");
        let none = op(&hub, &ada, "agentfiles.restore", json!({"id": id, "version": 9})).await;
        anyhow::ensure!(matches!(&none, Err((StatusCode::BAD_REQUEST, m)) if m.contains("no version 9")), "{none:?}");
        let vs = op(&hub, &ada, "agentfiles.versions", json!({"id": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let listed: Vec<(i64, String)> = vs.get("versions").and_then(Value::as_array).into_iter().flatten().map(|v| (v.get("version").and_then(Value::as_i64).unwrap_or(0), text_of(v, "/text"))).collect();
        anyhow::ensure!(listed == [(3, CLAUDE.to_owned()), (2, CLAUDE_2.to_owned()), (1, CLAUDE.to_owned())], "{listed:?}");
        anyhow::ensure!(vs.pointer("/versions/0/by") == Some(&json!("user:u-ada")), "{vs}");
        op(&hub, &ada, "agentfiles.delete", json!({"id": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let gone = op(&hub, &ada, "agentfiles.get", json!({"id": id})).await;
        anyhow::ensure!(matches!(&gone, Err((StatusCode::NOT_FOUND, _))), "{gone:?}");
        let left: i64 = hub.store.read(move |c| Ok(c.query_row("SELECT COUNT(*) FROM agentfile_versions WHERE agentfile_id = ?1", [&id], |r| r.get(0))?)).await?;
        anyhow::ensure!(left == 0, "its versions stay: {left}");
        // Told: each change, Ada's, the last one gone.
        let mut told = Vec::new();
        while let Ok(t) = events.try_recv() {
            let v: Value = serde_json::from_str(&t)?;
            if v.get("type") == Some(&json!("agentfile")) {
                told.push((text_of(&v, "/owner"), v.get("version").and_then(Value::as_i64).unwrap_or(0), v.get("deleted").and_then(Value::as_bool).unwrap_or(false)));
            }
        }
        let a = "user:u-ada".to_owned();
        anyhow::ensure!(told == [(a.clone(), 1, false), (a.clone(), 2, false), (a.clone(), 2, false), (a.clone(), 3, false), (a, 3, true)], "{told:?}");
        // Audited once each, as its route is, saying what it did.
        let rows: Vec<(String, String)> =
            hub.store.read(|c| Ok(c.prepare("SELECT action, detail FROM audit2 WHERE detail LIKE 'agentfile.%' ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?)).await?;
        let said: Vec<(&str, &str)> = rows.iter().map(|(a, d)| (a.as_str(), d.split(':').next().unwrap_or_default())).collect();
        anyhow::ensure!(
            said == [
                ("POST /api/agentfiles", "agentfile.create"),
                ("PATCH /api/agentfiles/**", "agentfile.update"),
                ("PATCH /api/agentfiles/**", "agentfile.update"),
                ("POST /api/agentfiles/**", "agentfile.restore"),
                ("DELETE /api/agentfiles/**", "agentfile.delete")
            ],
            "{rows:?}"
        );
        Ok(())
    })
}

/// The server's owner and an admin (`agentfiles.manage`) reach everyone's; an agent reaches only those
/// of the person it acts for, whoever that is; a viewer, who runs no agents, none; a job none.
#[test]
fn step30_who_reaches_everyones() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let (ada, bo, admin) = (person("u-ada", &["developer"]), person("u-bo", &["developer"]), person("u-root", &["admin"]));
        let a = op(&hub, &ada, "agentfiles.create", json!({"name": "claude", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        op(&hub, &bo, "agentfiles.create", json!({"name": "codex", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let id = text_of(&a, "/agentfile/id");
        for who in [&Caller::owner(), &admin] {
            let l = op(&hub, who, "agentfiles.list", json!({"all": true})).await.map_err(|e| anyhow::anyhow!("{} {e:?}", who.name))?;
            anyhow::ensure!(l.get("reaches_all") == Some(&json!(true)), "{}: {l}", who.name);
            let owners: Vec<String> = l.get("agentfiles").and_then(Value::as_array).into_iter().flatten().map(|a| text_of(a, "/owner")).collect();
            anyhow::ensure!(owners.len() == 2 && owners.contains(&"user:u-ada".to_owned()) && owners.contains(&"user:u-bo".to_owned()), "{}: {owners:?}", who.name);
            // Its own list is its own.
            let own = op(&hub, who, "agentfiles.list", json!({})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(own.get("agentfiles").and_then(Value::as_array).is_some_and(Vec::is_empty), "{}: {own}", who.name);
        }
        // The admin changes Ada's; it stays hers, the version saying who.
        let changed = op(&hub, &admin, "agentfiles.update", json!({"id": id, "text": CLAUDE_2})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(text_of(&changed, "/agentfile/owner") == "user:u-ada", "{changed}");
        let vs = op(&hub, &ada, "agentfiles.versions", json!({"id": id})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(vs.pointer("/versions/0/by") == Some(&json!("user:u-root")), "{vs}");
        // An agent for Ada keeps and reaches hers, and no one else's, though the person it acts for were an admin.
        let agent = agent_for("u-ada", &["developer"]);
        let made = op(&hub, &agent, "agentfiles.create", json!({"name": "pi", "text": CLAUDE})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(text_of(&made, "/agentfile/owner") == "user:u-ada", "{made}");
        let l = op(&hub, &agent, "agentfiles.list", json!({})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(l.get("agentfiles").and_then(Value::as_array).is_some_and(|a| a.len() == 2), "{l}");
        let root_agent = agent_for("u-root", &["admin"]);
        let all = op(&hub, &root_agent, "agentfiles.list", json!({"all": true})).await;
        anyhow::ensure!(matches!(&all, Err((StatusCode::FORBIDDEN, _))), "an agent reaches its person's alone: {all:?}");
        // A viewer is refused by the route table, before anything is read.
        let vera = person("u-vera", &["viewer"]);
        let r = op(&hub, &vera, "agentfiles.list", json!({})).await;
        anyhow::ensure!(matches!(&r, Err((StatusCode::FORBIDDEN, m)) if m.contains("agents.run")), "{r:?}");
        // Who hears an Agentfile's events: its owner, and whoever reaches everyone's.
        anyhow::ensure!(hears(&ada, "user:u-ada") && !hears(&bo, "user:u-ada") && hears(&admin, "user:u-ada") && hears(&Caller::owner(), "user:u-bo"));
        anyhow::ensure!(!hears(&root_agent, "user:u-bo") && hears(&agent, "user:u-ada"));
        Ok(())
    })
}

/// A project's Agentfiles are listed only for those who read its runs; one they may not is as none.
#[test]
fn step30_a_project_is_listed_for_its_readers_alone() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let repo = |id: &str, name: &str| crate::store::Repo {
            id: id.into(),
            name: name.into(),
            clone_url: format!("https://example.invalid/{name}"),
            branches: vec![],
            poll_seconds: 60,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "upstream".into(),
            default_branch: None,
            mirror: None,
        };
        hub.store.upsert_repo(repo("r1", "acme/app")).await?;
        hub.store.upsert_repo(repo("r2", "acme/other")).await?;
        // Agents on the server, runs read in acme/app alone.
        let ada = person("u-ada", &["agents.run", "viewer@repo:acme/app"]);
        for asked in ["acme/other", "r2", "acme/none"] {
            let r = op(&hub, &ada, "agentfiles.list", json!({"repo": asked})).await;
            anyhow::ensure!(matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m == &format!("no repository {asked}")), "{asked}: {r:?}");
        }
        // Without a project, the list is the person's own.
        let l = op(&hub, &ada, "agentfiles.list", json!({"repo": " "})).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(l.get("project").is_none(), "{l}");
        Ok(())
    })
}

#[test]
fn step30_ls_tree_entries() {
    assert_eq!(tree_entry("100644 blob 3b18e512dba79e4c8300dd08aeb37f8e728b8dad      12\t.kiln/images/claude/Agentfile"), Some(("3b18e512dba79e4c8300dd08aeb37f8e728b8dad", Some(12), ".kiln/images/claude/Agentfile")));
    assert_eq!(tree_entry("160000 commit 3b18e512dba79e4c8300dd08aeb37f8e728b8dad       -\t.kiln/images/sub"), None);
    assert_eq!(tree_entry(""), None);
}
