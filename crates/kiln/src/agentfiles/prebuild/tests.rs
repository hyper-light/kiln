//! P4.5 (kiln plan 01a1dd2e6a0b76bbba478c532781feed): a save asks each of its person's microvm runners to build its
//! Agentfile, their words move each build forward only, the page reads where each is, and a runner's build is the one a
//! session there makes, so the session that starts after it builds nothing.

use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};

use super::*;
use crate::authz::Caller;

/// An Agentfile of Claude Code's (`HARNESS claude`), as kiln keeps them.
const TEXT: &str = "FROM ubuntu:24.04\nHARNESS claude FROM ./harness\nAGENT reviewer FROM ./agent\nATTACH reviewer FOR claude\nCMD [\"claude\"]\n";

async fn op(hub: &Arc<Hub>, caller: &Caller, name: &str, input: Value) -> anyhow::Result<Value> {
    crate::ops::call(
        crate::ops::Cx {
            hub: Arc::clone(hub),
            caller: caller.clone(),
        },
        name,
        input,
    )
    .await
    .map_err(|(code, why)| anyhow::anyhow!("{name}: {code} {why}"))
}

/// A runner of a test's hub whose facts say it holds microVMs (its engine `microvm`, nothing set) or can't (`host`),
/// connected or away.
fn runner(name: &str, microvm: bool, connected: bool) -> crate::hub::AgentLive {
    use kiln_ci::facts::{Isolation, RunnerFacts};
    let isolation = if microvm {
        Isolation {
            available: true,
            hypervisor: Some("KVM".into()),
            why_not: None,
            engine: None,
        }
    } else {
        Isolation {
            available: false,
            hypervisor: None,
            why_not: Some("no hypervisor it can use".into()),
            engine: None,
        }
    };
    let mut facts = RunnerFacts {
        isolation: Some(isolation),
        ..RunnerFacts::default()
    };
    crate::harness::engine::overlay(&mut facts, crate::harness::engine::Asked::default());
    crate::hub::AgentLive {
        facts: Some(Box::new(facts)),
        connected,
        ..crate::hub::tests::live_runner(name, "linux", "x86_64")
    }
}

/// Agentfile `id`'s rows: runner, harness, version and state, by runner and harness.
async fn rows(hub: &Hub, id: &str) -> anyhow::Result<Vec<(String, String, i64, String)>> {
    let i = id.to_owned();
    hub.store
        .read(move |c| {
            let mut st = c.prepare("SELECT runner, harness, version, state FROM prebuilds WHERE agentfile_id = ?1 ORDER BY runner, harness")?;
            let rows = st.query_map(params![i], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

fn row(runner: &str, harness: &str, version: i64, state: &str) -> (String, String, i64, String) {
    (
        runner.to_owned(),
        harness.to_owned(),
        version,
        state.to_owned(),
    )
}

/// Runner `runner`'s word on its build of Agentfile `id` at `version` for Claude Code.
async fn word(hub: &Hub, runner: &str, id: &str, version: i64, how: How) -> anyhow::Result<()> {
    heard(
        hub,
        runner,
        &serde_json::to_string(&Said {
            agentfile: id.to_owned(),
            version,
            harness: "claude".to_owned(),
            how,
        })?,
    )
    .await;
    Ok(())
}

/// What the events `events` hold now tell of Agentfile `id`: how many times its version (`agentfile`), and how many its
/// builds (`agentfile_builds`).
fn told(
    events: &mut crate::memory::lanes::Reader<Arc<str>>,
    id: &str,
) -> anyhow::Result<(usize, usize)> {
    let (mut versions, mut builds) = (0, 0);
    while let Ok(t) = events.try_recv() {
        let v: Value = serde_json::from_str(&t)?;
        if v.get("id") != Some(&json!(id)) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("agentfile") => versions += 1,
            Some("agentfile_builds") => builds += 1,
            _ => {}
        }
    }
    Ok((versions, builds))
}

/// An Agentfile is built for each of kiln's harnesses its sessions run: those it declares, and its agent definitions',
/// each once; a name that isn't one of kiln's harnesses, none.
#[test]
fn p4_5_an_agentfile_is_built_for_each_of_kilns_harnesses_its_sessions_run() {
    assert_eq!(harnesses(TEXT, &[]), ["claude"]);
    let mut both = harnesses(
        TEXT,
        &["codex".to_owned(), "claude".to_owned(), "nobody".to_owned()],
    );
    both.sort_unstable();
    assert_eq!(both, ["claude", "codex"]);
    assert!(
        harnesses("FROM scratch\n", &[]).is_empty(),
        "no session runs one that declares none"
    );
}

/// A word moves a build forward only: queued to building, either to its end; nothing moves an ended one but a save.
#[test]
fn p4_5_a_builds_state_moves_forward_only() {
    let may = |from: State, to: State| to.follows().contains(&from);
    assert!(
        may(State::Queued, State::Building)
            && may(State::Queued, State::Done)
            && may(State::Building, State::Failed)
    );
    for end in [State::Done, State::Failed] {
        for to in [State::Queued, State::Building, State::Done, State::Failed] {
            assert!(!may(end, to), "{end:?} to {to:?}");
        }
    }
    for s in [State::Queued, State::Building, State::Done, State::Failed] {
        assert_eq!(State::of(s.as_str()), Some(s));
    }
    assert_eq!(State::of("cancelled"), None);
}

/// Saved, an Agentfile is queued on each of its person's microvm runners (one away too, none whose engine is host), for
/// each harness its sessions run; its runners' words move each forward only, and a word on a version saved over since
/// moves nothing; the page reads each runner's state, its image, and Shards' words where it failed; deleted, its builds
/// go with it.
#[test]
fn p4_5_a_save_queues_its_build_on_each_microvm_runner_and_their_words_move_it()
-> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        hub.put_agent(runner("mac", true, true));
        hub.put_agent(runner("away", true, false));
        hub.put_agent(runner("host", false, true));
        let owner = Caller::owner();
        let mut events = hub.subscribe_events()?;
        let made = op(
            &hub,
            &owner,
            "agentfiles.create",
            json!({"name": "reviewer", "text": TEXT}),
        )
        .await?;
        let id = made
            .pointer("/agentfile/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got == [
                row("away", "claude", 1, "queued"),
                row("mac", "claude", 1, "queued")
            ],
            "{got:?}"
        );
        // Each change told once, each as its own kind: its version by the save, its builds as their rows were written
        // (a save that writes none tells none: agentfiles::tests::step30_versions_restore_and_delete).
        let said = told(&mut events, &id)?;
        anyhow::ensure!(said == (1, 1), "(versions, builds) told: {said:?}");

        word(&hub, "mac", &id, 1, How::Building).await?;
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got.get(1) == Some(&row("mac", "claude", 1, "building")),
            "{got:?}"
        );
        let image = format!("agentfile-{id}@sha256:{}", "0".repeat(64));
        word(
            &hub,
            "mac",
            &id,
            1,
            How::Done {
                image: image.clone(),
            },
        )
        .await?;
        // Another build of it ending after, a version saved over, a runner it wasn't asked of: nothing moves.
        word(&hub, "mac", &id, 1, How::Building).await?;
        word(
            &hub,
            "mac",
            &id,
            0,
            How::Failed {
                why: "an older version's".into(),
            },
        )
        .await?;
        word(
            &hub,
            "host",
            &id,
            1,
            How::Done {
                image: image.clone(),
            },
        )
        .await?;
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got == [
                row("away", "claude", 1, "queued"),
                row("mac", "claude", 1, "done")
            ],
            "{got:?}"
        );
        // The two words that moved a row told its builds, once each; the three that moved none, nothing.
        let said = told(&mut events, &id)?;
        anyhow::ensure!(said == (0, 2), "(versions, builds) told: {said:?}");
        let read = op(&hub, &owner, "agentfiles.builds", json!({"id": id})).await?;
        anyhow::ensure!(
            read.pointer("/builds/0/state") == Some(&json!("queued"))
                && read.pointer("/builds/1/image") == Some(&json!(image)),
            "{read}"
        );

        // A new version is asked again; a late word on the one before moves nothing.
        op(
            &hub,
            &owner,
            "agentfiles.update",
            json!({"id": id, "text": format!("{TEXT}LABEL v=2\n")}),
        )
        .await?;
        word(
            &hub,
            "mac",
            &id,
            1,
            How::Done {
                image: image.clone(),
            },
        )
        .await?;
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got == [
                row("away", "claude", 2, "queued"),
                row("mac", "claude", 2, "queued")
            ],
            "{got:?}"
        );

        // An agent definition of another harness naming it: built for that harness too.
        op(
            &hub,
            &owner,
            "agentdefs.create",
            json!({"name": "coder", "harness": "codex", "agentfile": format!("agentfile:{id}")}),
        )
        .await?;
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got == [
                row("away", "claude", 2, "queued"),
                row("away", "codex", 2, "queued"),
                row("mac", "claude", 2, "queued"),
                row("mac", "codex", 2, "queued")
            ],
            "{got:?}"
        );

        // A build Shards refuses: the page says its words.
        const SAID: &str = "Shards didn't build it: ERROR: failed to solve: process \"/bin/sh -c exit 3\" did not complete successfully: exit code: 3";
        word(&hub, "mac", &id, 2, How::Failed { why: SAID.into() }).await?;
        let read = op(&hub, &owner, "agentfiles.builds", json!({"id": id})).await?;
        let mac = read
            .get("builds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|b| {
                b.get("runner") == Some(&json!("mac")) && b.get("harness") == Some(&json!("claude"))
            })
            .cloned()
            .unwrap_or_default();
        anyhow::ensure!(
            mac.get("state") == Some(&json!("failed"))
                && mac.get("why") == Some(&json!(SAID))
                && mac.get("image").is_none(),
            "{read}"
        );

        op(&hub, &owner, "agentfiles.delete", json!({"id": id})).await?;
        anyhow::ensure!(rows(&hub, &id).await?.is_empty(), "its builds went with it");
        Ok(())
    })
}

/// A runner's build of an Agentfile for a harness is the one a session of that harness there makes (one tag in its
/// store), so once it's done the session's start finds its image and builds nothing (`built` none, its start's
/// `built_us`); a machine whose kiln holds no microVMs says why.
#[test]
fn p4_5_a_runners_build_is_a_sessions_own_so_a_start_after_it_builds_none() -> anyhow::Result<()> {
    use crate::harness::session::{Isolation, Spec};
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let id = crate::util::new_id();
        let ask = Ask {
            agentfile: id.clone(),
            version: 1,
            harness: "claude".into(),
            text: TEXT.into(),
            secrets: Vec::new(),
        };
        let spec = Spec {
            isolation: Some(Isolation {
                image: Some(format!("{}{id}", crate::sandbox::isolated::KEPT)),
                ..Isolation::microvm()
            }),
            agentfile: Some(TEXT.into()),
            ..Spec::default()
        };
        let kind = crate::harness::kinds::Kind::get("claude")
            .ok_or_else(|| anyhow::anyhow!("no Claude Code"))?;
        let session =
            crate::sandbox::isolated::source_of(kind, &spec).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            crate::sandbox::image::tag_of(source(&ask))? == crate::sandbox::image::tag_of(session)?,
            "one tag for both"
        );

        // Its build done: its machine's store holds it.
        let (images, pinned) = crate::sandbox::image::tests::holding(dir.path(), source(&ask))?;
        let vms = crate::sandbox::Microvms::new(
            Arc::new(crate::sandbox::fake::FakeVms::with(
                &pinned,
                "claude",
                dir.path(),
            )) as Arc<dyn kiln_seams::Sandbox>,
            images,
        );
        anyhow::ensure!(
            build(Some(&vms), &ask).await == Ok(pinned.clone()),
            "found in the store"
        );
        let started = vms.images.pinned(session, &[]).await?;
        anyhow::ensure!(
            started
                == crate::sandbox::image::Pinned {
                    image: pinned,
                    built: None
                },
            "the start after it builds nothing: {started:?}"
        );

        anyhow::ensure!(build(None, &ask).await == Err(crate::sandbox::NO_MICROVM.to_owned()));
        Ok(())
    })
}

/// The wire's words both ways read back as written; a secret's value is never in what a build's ask says of itself.
#[test]
fn p4_5_the_asks_and_words_read_back_and_an_ask_never_shows_a_secret() -> anyhow::Result<()> {
    let ask = Ask {
        agentfile: "a".repeat(32),
        version: 3,
        harness: "claude".into(),
        text: TEXT.into(),
        secrets: vec![BuildSecret {
            id: "GIT_AUTH_TOKEN".into(),
            value: "ghp_p45_never_shown".into(),
            given: crate::secrets::mine::Given::Secret,
        }],
    };
    anyhow::ensure!(serde_json::from_str::<Ask>(&serde_json::to_string(&ask)?)? == ask);
    anyhow::ensure!(
        !format!("{ask:?}").contains("ghp_p45_never_shown"),
        "{ask:?}"
    );
    for how in [
        How::Building,
        How::Done {
            image: "agentfile-x@sha256:00".into(),
        },
        How::Failed {
            why: "Shards' words".into(),
        },
    ] {
        let said = Said {
            agentfile: "a".repeat(32),
            version: 3,
            harness: "claude".into(),
            how,
        };
        anyhow::ensure!(serde_json::from_str::<Said>(&serde_json::to_string(&said)?)? == said);
    }
    Ok(())
}

/// A person, `user:<id>`, holding `developer` on the server.
fn developer(id: &str) -> Caller {
    use crate::authz::gate::{Kind, Rights};
    let rights = Rights {
        grants: ["developer"]
            .iter()
            .filter_map(|g| crate::authz::Grant::parse(g).ok())
            .collect(),
        custom: std::collections::HashMap::new(),
    };
    let mut c = Caller::with(Kind::User, &format!("user:{id}"), Some(rights), None);
    c.user = Some(id.to_owned());
    c
}

/// `caller`'s ask to build Agentfile `id` again on `runner` (`agentfiles.rebuild`; for `harness` alone, where named): its
/// answer, or the status and words it was refused with.
async fn again(
    hub: &Arc<Hub>,
    caller: &Caller,
    id: &str,
    runner: &str,
    harness: Option<&str>,
) -> Result<Value, (http::StatusCode, String)> {
    let mut input = json!({"id": id, "runner": runner});
    if let (Some(h), Some(o)) = (harness, input.as_object_mut()) {
        o.insert("harness".into(), json!(h));
    }
    crate::ops::call(
        crate::ops::Cx {
            hub: Arc::clone(hub),
            caller: caller.clone(),
        },
        "agentfiles.rebuild",
        input,
    )
    .await
}

/// Runner `runner`'s build for Claude Code in an answer listing builds (`{builds: [...]}`); null where there's none.
fn build_on(answer: &Value, runner: &str) -> Value {
    answer
        .get("builds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|b| {
            b.get("runner") == Some(&json!(runner)) && b.get("harness") == Some(&json!("claude"))
        })
        .cloned()
        .unwrap_or_default()
}

/// A runner's builds of an Agentfile asked again (`agentfiles.rebuild`) by whoever reaches it as they read its builds
/// (another person's is as none): one that failed, or one done, is queued again at its version now, and its runner's words
/// move it forward as they move a save's; one on a runner away waits, queued, until it's back. Refused, nothing written,
/// where the runner can't build it (none of that name, its engine host, its kiln saying it builds nothing ahead), where no
/// session of it runs the harness named, or where its build there is underway.
#[test]
fn p4_5_a_runners_build_is_asked_again_and_refused_where_it_cant_be() -> anyhow::Result<()> {
    use http::StatusCode;
    anyhow::ensure!(
        [State::Done, State::Failed].iter().all(|s| s.ended())
            && ![State::Queued, State::Building].iter().any(|s| s.ended()),
        "only a build at its end is asked again"
    );
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        // Its newest process said what it speaks, without building ahead: its kiln is older.
        crate::hub::tests::join_runner(&hub, 1, "older", 0);
        hub.instances
            .update(|i| i.set_features(1, vec![feature::PLACE.to_owned()]));
        for (name, microvm, connected) in [
            ("older", true, true),
            ("mac", true, true),
            ("away", true, false),
            ("host", false, true),
        ] {
            hub.put_agent(runner(name, microvm, connected));
        }
        let owner = Caller::owner();
        let made = op(
            &hub,
            &owner,
            "agentfiles.create",
            json!({"name": "reviewer", "text": TEXT}),
        )
        .await?;
        let id = made
            .pointer("/agentfile/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got == [
                row("away", "claude", 1, "queued"),
                row("mac", "claude", 1, "queued"),
                row("older", "claude", 1, "queued")
            ],
            "{got:?}"
        );

        // Underway, queued and then building, until its runner ends it.
        let r = again(&hub, &owner, &id, "mac", None).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::CONFLICT, m)) if m.contains("for claude is queued already")),
            "{r:?}"
        );
        word(&hub, "mac", &id, 1, How::Building).await?;
        let r = again(&hub, &owner, &id, "mac", Some("claude")).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::CONFLICT, m)) if m.contains("for claude is building already")),
            "{r:?}"
        );

        // Failed: queued again at its version, and its runner's words move it as they move a save's.
        word(
            &hub,
            "mac",
            &id,
            1,
            How::Failed {
                why: "Shards didn't build it: exit code: 3".into(),
            },
        )
        .await?;
        let asked = again(&hub, &owner, &id, "mac", None)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mac = build_on(&asked, "mac");
        anyhow::ensure!(
            mac.get("state") == Some(&json!("queued"))
                && mac.get("version") == Some(&json!(1))
                && mac.get("why").is_none(),
            "{asked}"
        );
        word(&hub, "mac", &id, 1, How::Building).await?;
        let image = format!("agentfile-{id}@sha256:{}", "0".repeat(64));
        word(
            &hub,
            "mac",
            &id,
            1,
            How::Done {
                image: image.clone(),
            },
        )
        .await?;
        let got = rows(&hub, &id).await?;
        anyhow::ensure!(
            got.get(1) == Some(&row("mac", "claude", 1, "done")),
            "{got:?}"
        );

        // Done: queued again too (its runner finds its image in its store at once).
        let asked = again(&hub, &owner, &id, "mac", Some("claude"))
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mac = build_on(&asked, "mac");
        anyhow::ensure!(
            mac.get("state") == Some(&json!("queued")) && mac.get("image").is_none(),
            "{asked}"
        );

        // Away: queued again, asked as it comes back.
        word(
            &hub,
            "away",
            &id,
            1,
            How::Failed {
                why: "Shards didn't build it".into(),
            },
        )
        .await?;
        let asked = again(&hub, &owner, &id, "away", None)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(
            build_on(&asked, "away").get("state") == Some(&json!("queued")),
            "{asked}"
        );

        // Refused, nothing written.
        let before = rows(&hub, &id).await?;
        let r = again(&hub, &owner, &id, "nowhere", None).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m.starts_with("no runner nowhere builds Agentfiles")),
            "{r:?}"
        );
        let r = again(&hub, &owner, &id, "host", None).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::CONFLICT, m)) if m.starts_with("host builds no Agentfile")),
            "{r:?}"
        );
        let r = again(&hub, &owner, &id, "older", None).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::CONFLICT, m)) if m.starts_with("older's kiln builds no Agentfile ahead")),
            "{r:?}"
        );
        let r = again(&hub, &owner, &id, "mac", Some("codex")).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m.starts_with("none of its sessions runs codex")),
            "{r:?}"
        );
        let plain = op(&hub, &owner, "agentfiles.create", json!({"name": "plain", "text": "FROM ubuntu:24.04\nAGENT main FROM ./agent\nCMD [\"claude\"]\n"})).await?;
        let plain = plain
            .pointer("/agentfile/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let r = again(&hub, &owner, &plain, "mac", None).await;
        anyhow::ensure!(
            matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m.starts_with("none of its sessions runs one of kiln's harnesses")),
            "{r:?}"
        );
        // Another person reaches its builds as none, to read them or to ask them again.
        let bo = developer("u-bo");
        let read = crate::ops::call(
            crate::ops::Cx {
                hub: Arc::clone(&hub),
                caller: bo.clone(),
            },
            "agentfiles.builds",
            json!({"id": id}),
        )
        .await;
        let asked = again(&hub, &bo, &id, "mac", None).await;
        for (name, r) in [("agentfiles.builds", read), ("agentfiles.rebuild", asked)] {
            anyhow::ensure!(
                matches!(&r, Err((StatusCode::NOT_FOUND, m)) if m == "no such Agentfile"),
                "bo's {name}: {r:?}"
            );
        }
        let after = rows(&hub, &id).await?;
        anyhow::ensure!(after == before, "{before:?} became {after:?}");
        Ok(())
    })
}
