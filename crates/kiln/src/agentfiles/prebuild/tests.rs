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
