//! Agentfiles kept, versioned and reached only by their owner (and those who reach everyone's), through
//! the operation table as every surface calls it (`crate::ops::call`: the route table, the input's type,
//! the audit). What Shards says of a text is `check`'s tests'; here the server's `shards`, if any, checks
//! the valid texts these keep.

use std::collections::HashMap;

use http::StatusCode;
use serde_json::{Value, json};

use kiln_seams::blobs::{Put, Refusal, SimBlobs, SimLimits};

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

/// Where `text` is kept.
fn key_of(text: &str) -> String {
    key(&crate::util::sha256_hex(text.as_bytes()))
}

/// A person, as the module's own functions take one.
fn who(person: &str) -> Who {
    Who { person: person.into(), all: false }
}

/// Records only: an Agentfile and each version name their text by its digest, and hold none.
#[test]
fn step30_the_migration_makes_its_tables() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let store = crate::store::Store::in_memory()?;
        let (tables, unique, columns): (Vec<String>, i64, Vec<Vec<String>>) = store
            .read(|c| {
                let t = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'agentfile%' ORDER BY name")?.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                let u = c.query_row("SELECT COUNT(*) FROM pragma_index_list('agentfiles') WHERE \"unique\" = 1 AND origin = 'u'", [], |r| r.get(0))?;
                let mut cols = Vec::new();
                for table in ["agentfiles", "agentfile_versions"] {
                    cols.push(c.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?.query_map([table], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?);
                }
                Ok((t, u, cols))
            })
            .await?;
        anyhow::ensure!(tables == ["agentfile_versions", "agentfiles"], "{tables:?}");
        anyhow::ensure!(unique == 1, "a name once per owner");
        anyhow::ensure!(
            columns == [vec!["id", "owner", "name", "version", "digest", "created_at", "updated_at"], vec!["agentfile_id", "version", "digest", "at", "by"]],
            "{columns:?}"
        );
        // A second Agentfile of one name and owner is refused by the table itself.
        let dup = store
            .call(|c| {
                c.execute("INSERT INTO agentfiles (id, owner, name, digest, created_at, updated_at) VALUES ('a', 'user:u1', 'x', 'd', 0, 0)", [])?;
                c.execute("INSERT INTO agentfiles (id, owner, name, digest, created_at, updated_at) VALUES ('b', 'user:u2', 'x', 'd', 0, 0)", [])?;
                Ok(c.execute("INSERT INTO agentfiles (id, owner, name, digest, created_at, updated_at) VALUES ('c', 'user:u1', 'x', 'd', 0, 0)", []).is_err())
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

/// A text is bytes, kept in kiln's store of bytes under its content's SHA-256: once, however many
/// Agentfiles and versions have it, whoever's. kiln.db names it by its digest, and each read gives it
/// back whole. A name taken keeps nothing.
#[test]
fn step30_texts_are_kept_once_in_the_store_of_bytes() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let sim = SimBlobs::new(crate::rt::clock(), SimLimits::default());
        let (ada, bo) = (who("user:u-ada"), who("user:u-bo"));
        let mine = create(&hub, &sim, &ada, "claude", CLAUDE, "user:u-ada").await?;
        create(&hub, &sim, &bo, "claude", CLAUDE, "user:u-bo").await?;
        let id = mine.agentfile.id.clone();
        update(&hub, &sim, &ada, &id, Patch { name: None, text: Some(CLAUDE_2.into()) }, "user:u-ada").await?;
        let back = restore(&hub, &sim, &ada, &id, 1, "user:u-ada").await?;
        anyhow::ensure!(back.agentfile.version == 3 && back.agentfile.text == CLAUDE, "{back:?}");
        let taken = create(&hub, &sim, &ada, "claude", "FROM scratch\n", "user:u-ada").await;
        anyhow::ensure!(matches!(&taken, Err(Error::Conflict(_))), "{taken:?}");
        // Two texts, each once, at its content's name.
        let mut keys: Vec<String> = sim.list("agentfiles/", None, 100).await?.objects.into_iter().map(|o| o.key).collect();
        keys.sort();
        let mut want = vec![key_of(CLAUDE), key_of(CLAUDE_2)];
        want.sort();
        anyhow::ensure!(keys == want, "{keys:?}");
        // kiln.db names them: Ada's three versions and Bo's one.
        let digests: Vec<String> = hub.store.read(|c| Ok(c.prepare("SELECT digest FROM agentfile_versions")?.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?)).await?;
        anyhow::ensure!(digests.len() == 4 && digests.iter().all(|d| want.contains(&key(d))), "{digests:?}");
        // Read back whole: the Agentfile and each of its versions.
        let got = get(&hub, &sim, &ada, &id).await?;
        anyhow::ensure!(got.text == CLAUDE && got.version == 3, "{got:?}");
        let vs: Vec<(i64, String)> = versions(&hub, &sim, &ada, &id).await?.into_iter().map(|v| (v.version, v.text)).collect();
        anyhow::ensure!(vs == [(3, CLAUDE.to_owned()), (2, CLAUDE_2.to_owned()), (1, CLAUDE.to_owned())], "{vs:?}");
        Ok(())
    })
}

/// A text read back is held to its digest: bytes the store gives other than the name says, or none, are
/// the store's failure, never shown as the Agentfile's text.
#[test]
fn step30_a_text_read_back_is_held_to_its_digest() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let sim = SimBlobs::new(crate::rt::clock(), SimLimits::default());
        let ada = who("user:u-ada");
        let id = create(&hub, &sim, &ada, "claude", CLAUDE, "user:u-ada").await?.agentfile.id;
        sim.put(&key_of(CLAUDE), CLAUDE_2.as_bytes().to_vec(), Put::default()).await?;
        let r = get(&hub, &sim, &ada, &id).await;
        anyhow::ensure!(matches!(&r, Err(Error::Store(m)) if m.contains("other bytes")), "{r:?}");
        let r = versions(&hub, &sim, &ada, &id).await;
        anyhow::ensure!(matches!(&r, Err(Error::Store(_))), "{r:?}");
        sim.delete(&key_of(CLAUDE)).await?;
        let r = get(&hub, &sim, &ada, &id).await;
        anyhow::ensure!(matches!(&r, Err(Error::Store(m)) if m.contains("couldn't give back")), "{r:?}");
        Ok(())
    })
}

/// The bytes first, then the record: a store that can't keep a text leaves no record naming it, the
/// caller told why, and an Agentfile it can't keep a new text for stays as it was. A text kept already
/// needs no room.
#[test]
fn step30_a_text_the_store_cant_keep_writes_no_record() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let sim = SimBlobs::new(crate::rt::clock(), SimLimits { objects: 1, ..SimLimits::default() });
        let ada = who("user:u-ada");
        let id = create(&hub, &sim, &ada, "claude", CLAUDE, "user:u-ada").await?.agentfile.id;
        let r = create(&hub, &sim, &ada, "codex", CLAUDE_2, "user:u-ada").await;
        anyhow::ensure!(matches!(&r, Err(Error::Store(m)) if m.contains("couldn't keep")), "{r:?}");
        let r = update(&hub, &sim, &ada, &id, Patch { name: None, text: Some(CLAUDE_2.into()) }, "user:u-ada").await;
        anyhow::ensure!(matches!(&r, Err(Error::Store(_))), "{r:?}");
        let rows: (i64, i64) = hub.store.read(|c| Ok((c.query_row("SELECT COUNT(*) FROM agentfiles", [], |r| r.get(0))?, c.query_row("SELECT COUNT(*) FROM agentfile_versions", [], |r| r.get(0))?))).await?;
        anyhow::ensure!(rows == (1, 1), "{rows:?} Agentfiles and versions");
        let still = get(&hub, &sim, &ada, &id).await?;
        anyhow::ensure!(still.version == 1 && still.text == CLAUDE, "{still:?}");
        let again = create(&hub, &sim, &ada, "claude-too", CLAUDE, "user:u-ada").await?;
        anyhow::ensure!(again.agentfile.version == 1 && again.agentfile.text == CLAUDE, "{again:?}");
        Ok(())
    })
}

/// A delete drops the records and lets go of the texts no record names any more; a text another
/// person's Agentfile has stays until theirs goes too.
#[test]
fn step30_a_delete_lets_go_of_texts_no_record_names() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let sim = SimBlobs::new(crate::rt::clock(), SimLimits::default());
        let (ada, bo) = (who("user:u-ada"), who("user:u-bo"));
        let mine = create(&hub, &sim, &ada, "claude", CLAUDE, "user:u-ada").await?.agentfile.id;
        let theirs = create(&hub, &sim, &bo, "claude", CLAUDE, "user:u-bo").await?.agentfile.id;
        update(&hub, &sim, &ada, &mine, Patch { name: None, text: Some(CLAUDE_2.into()) }, "user:u-ada").await?;
        let gone = delete(&hub, &sim, &ada, &mine).await?;
        anyhow::ensure!(gone.name == "claude" && gone.owner == "user:u-ada" && gone.version == 2, "{gone:?}");
        anyhow::ensure!(matches!(sim.head(&key_of(CLAUDE_2)).await, Err(Refusal::NotFound(_))), "Ada's second text stayed");
        anyhow::ensure!(sim.head(&key_of(CLAUDE)).await.is_ok(), "Bo's text went with Ada's");
        delete(&hub, &sim, &bo, &theirs).await?;
        anyhow::ensure!(matches!(sim.head(&key_of(CLAUDE)).await, Err(Refusal::NotFound(_))), "the last text named stayed");
        Ok(())
    })
}
