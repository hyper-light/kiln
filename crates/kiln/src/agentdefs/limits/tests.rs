//! An agent definition's limits held to the sessions it starts (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.4's tests
//! "a session past its time limit", "a session past its token limit" and "a session past its cost limit"; a token or
//! cost limit refused for a harness that doesn't say that of its sessions, and a session on one ended as it starts).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::authz::{Caller, Grant, Rights, gate::Kind};
use crate::harness::core::Update;
use crate::harness::model::{State, Usage};
use crate::harness::registry::Location;
use crate::harness::session::Info;
use crate::hub::Hub;
use crate::ops::{Cx, Fail};

/// A person, `user:<name>`, an admin here (they may start agents on the server).
fn person(name: &str) -> anyhow::Result<Caller> {
    let admin = Grant::parse("admin").map_err(anyhow::Error::msg)?;
    let mut caller = Caller::with(
        Kind::User,
        &format!("user:{name}"),
        Some(Rights {
            grants: vec![admin],
            ..Default::default()
        }),
        None,
    );
    caller.user = Some(name.to_owned());
    Ok(caller)
}

fn failed((code, why): Fail) -> anyhow::Error {
    anyhow::anyhow!("{code}: {why}")
}

async fn op(hub: &Arc<Hub>, caller: &Caller, name: &str, input: Value) -> anyhow::Result<Value> {
    crate::ops::call(
        Cx {
            hub: Arc::clone(hub),
            caller: caller.clone(),
        },
        name,
        input,
    )
    .await
    .map_err(failed)
}

/// Operation `name` refused, as it answers: its status and words.
async fn refused(
    hub: &Arc<Hub>,
    caller: &Caller,
    name: &str,
    input: Value,
) -> anyhow::Result<Fail> {
    match crate::ops::call(
        Cx {
            hub: Arc::clone(hub),
            caller: caller.clone(),
        },
        name,
        input,
    )
    .await
    {
        Err(fail) => Ok(fail),
        Ok(v) => anyhow::bail!("{name} took it: {v}"),
    }
}

/// Agent definition `name` of `caller`'s, of `harness`, with `limits`: its id.
async fn defined(
    hub: &Arc<Hub>,
    caller: &Caller,
    name: &str,
    harness: &str,
    limits: Value,
) -> anyhow::Result<String> {
    let d = op(
        hub,
        caller,
        "agentdefs.create",
        json!({"name": name, "harness": harness, "limits": limits}),
    )
    .await?;
    d.get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{d}"))
}

/// Where the sessions of the spending tests run: a runner kiln hears them from, no process of the server's.
fn runner() -> Location {
    Location::Machine("box".into())
}

/// Session `id`, running harness `kind`, as its runner announces it, started from definition `def` (its version 1) for
/// Ada.
async fn started(hub: &Arc<Hub>, id: &str, kind: &str, def: &str) {
    let info = Info {
        id: id.into(),
        kind: kind.into(),
        ..Default::default()
    };
    hub.harness
        .ingest(
            &runner(),
            id,
            Update::Open {
                info: Box::new(info),
                since: None,
            },
        )
        .await;
    let def = super::super::sessions::Definition {
        id: def.to_owned(),
        version: 1,
    };
    super::super::sessions::record(hub, id, &def, Some("user:ada"), "agent:box").await;
}

/// Session `id`'s state as its runner says it at a turn's end: what it has spent, `input` and `output` tokens and
/// `usd` dollars.
async fn spent(hub: &Arc<Hub>, id: &str, input: u64, output: u64, usd: f64) {
    let state = State {
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            cost_usd: usd,
            turns: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    hub.harness
        .ingest(
            &runner(),
            id,
            Update::State {
                state: Box::new(state),
            },
        )
        .await;
}

/// Who the audit trail says closed session `id` (`agent.close`'s principals), now.
async fn closers(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Vec<String>> {
    let id = id.to_owned();
    hub.store
        .read(move |c| {
            Ok(c.prepare(
                "SELECT principal FROM audit2 WHERE detail = ?1 AND action = 'agent.close'",
            )?
            .query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?)
        })
        .await
}

/// [`closers`] once someone has closed it, or a minute is up.
async fn closed_by(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Vec<String>> {
    let began = Instant::now();
    loop {
        let by = closers(hub, id).await?;
        if !by.is_empty() || began.elapsed() >= Duration::from_secs(60) {
            return Ok(by);
        }
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
}

/// Why a limit ended session `id`, definition `def`'s one session, as `agentdefs.sessions` lists it: none where none
/// did.
async fn ended(
    hub: &Arc<Hub>,
    caller: &Caller,
    def: &str,
    id: &str,
) -> anyhow::Result<Option<String>> {
    let listed = op(hub, caller, "agentdefs.sessions", json!({"id": def})).await?;
    anyhow::ensure!(
        listed.pointer("/sessions/0/id") == Some(&json!(id)),
        "{listed}"
    );
    Ok(listed
        .pointer("/sessions/0/ended")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// P5.4's test "a session past its time limit": Ada's agent `quick` has a time limit of 1 s, `slow` one of an
/// hour, `free` none; a session started from each is held to its own. `quick`'s is ended once its second is
/// up, its end naming the limit in its definition's sessions (`ended`) and in the audit trail (who ended it);
/// the other two run on. Taking `hold`'s end out leaves `quick`'s session open past its limit.
#[test]
fn p5_4_a_session_past_its_time_limit_is_ended_with_the_limit_named() -> anyhow::Result<()> {
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
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = super::run(h).await;
        });
        hub.harness
            .set_test_program("/nonexistent/kiln-test-no-harness");
        let ada = person("ada")?;
        let folder = tempfile::tempdir()?;
        let mut sessions = Vec::new();
        for (name, limits) in [
            ("quick", json!({"seconds": 1})),
            ("slow", json!({"seconds": 3600})),
            ("free", json!({})),
        ] {
            let d = op(
                &hub,
                &ada,
                "agentdefs.create",
                json!({"name": name, "harness": "claude", "limits": limits}),
            )
            .await?;
            let def = super::super::sessions::Definition {
                id: d
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                version: 1,
            };
            let spec = crate::harness::session::Spec {
                kind: "claude".into(),
                cwd: Some(folder.path().to_string_lossy().into_owned()),
                name: Some(name.into()),
                ..Default::default()
            };
            let id = hub
                .harness
                .open(&hub, "local", spec, 1)
                .await?
                .into_iter()
                .next()
                .map(|i| i.id)
                .ok_or_else(|| anyhow::anyhow!("no session"))?;
            super::super::sessions::record(&hub, &id, &def, Some("user:ada"), "local").await;
            sessions.push((def.id, id));
        }
        let [(quick_def, quick), (_, slow), (_, free)] = sessions.as_slice() else {
            anyhow::bail!("{sessions:?}")
        };
        let began = Instant::now();
        while hub.harness.info(quick).is_some() {
            anyhow::ensure!(
                began.elapsed() < Duration::from_secs(60),
                "quick's session is open a minute past its 1 s limit"
            );
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        anyhow::ensure!(
            hub.harness.info(slow).is_some() && hub.harness.info(free).is_some(),
            "only the session past its limit ends"
        );
        let listed = op(&hub, &ada, "agentdefs.sessions", json!({"id": quick_def})).await?;
        let ended = listed
            .pointer("/sessions/0/ended")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        anyhow::ensure!(
            ended == "past its time limit of 1 s (agent definition quick)"
                && listed.pointer("/sessions/0/state") == Some(&json!("gone")),
            "{listed}"
        );
        // The end is audited once its close returns (harness::ops::end): read the trail until it
        // says so, as the close was waited for above.
        let want = (
            "kiln (time limit: 1 s, agent definition quick)".to_owned(),
            "agent.close".to_owned(),
        );
        let began = Instant::now();
        let audited = loop {
            let quick_id = quick.clone();
            let audited: Vec<(String, String)> = hub
                .store
                .read(move |c| {
                    Ok(
                        c.prepare("SELECT principal, action FROM audit2 WHERE detail = ?1")?
                            .query_map([quick_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                })
                .await?;
            if audited.contains(&want) || began.elapsed() >= Duration::from_secs(60) {
                break audited;
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        };
        anyhow::ensure!(audited.contains(&want), "{audited:?}");
        Ok(())
    })
}

/// P5.4: kiln tells the limits' owner of a session whose spending changed (its tokens, its cost) as the state that says
/// it lands, at a turn's end or sooner where its harness says it during one, and of no session whose state changed while
/// its spending didn't. Taking the registry's telling out leaves what a session spent after its start unseen, and this
/// test fails.
#[test]
fn p5_4_a_session_that_spends_more_is_told_to_its_limits() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let info = Info {
            id: "s".into(),
            kind: "claude".into(),
            ..Default::default()
        };
        hub.harness
            .ingest(
                &runner(),
                "s",
                Update::Open {
                    info: Box::new(info),
                    since: None,
                },
            )
            .await;
        let told = &hub.harness.limits;
        anyhow::ensure!(told.take().is_empty(), "an Open says nothing it spent");
        spent(&hub, "s", 100, 20, 0.01).await;
        anyhow::ensure!(told.take() == ["s"], "what it first spent");
        spent(&hub, "s", 100, 20, 0.01).await;
        anyhow::ensure!(told.take().is_empty(), "the same again");
        spent(&hub, "s", 100, 25, 0.01).await;
        anyhow::ensure!(told.take() == ["s"], "more tokens");
        spent(&hub, "s", 100, 25, 0.02).await;
        anyhow::ensure!(told.take() == ["s"], "more cost");
        Ok(())
    })
}

/// P5.4's test "a session past its token limit": Ada's agent `few` has a limit of 1,000 tokens, `many` one of a
/// million, `free` none. A session started from each says, as its turn ends, that it took 700 tokens in and 400 out:
/// `few`'s is ended, its end naming the limit in its definition's sessions (`ended`) and in the audit trail (who ended
/// it); the other two, which spent first, run on.
#[test]
fn p5_4_a_session_past_its_token_limit_is_ended_with_the_limit_named() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = super::run(h).await;
        });
        let ada = person("ada")?;
        let mut sessions = Vec::new();
        for (name, limits) in [
            ("many", json!({"tokens": 1_000_000})),
            ("free", json!({})),
            ("few", json!({"tokens": 1000})),
        ] {
            let def = defined(&hub, &ada, name, "claude", limits).await?;
            let id = format!("{name}-session");
            started(&hub, &id, "claude", &def).await;
            spent(&hub, &id, 700, 400, 0.0).await;
            sessions.push((def, id));
        }
        let [(many_def, many), (free_def, free), (few_def, few)] = sessions.as_slice() else {
            anyhow::bail!("{sessions:?}")
        };
        let by = closed_by(&hub, few).await?;
        anyhow::ensure!(
            by == ["kiln (token limit: 1000 tokens, agent definition few)"],
            "{by:?}"
        );
        let why = ended(&hub, &ada, few_def, few).await?;
        anyhow::ensure!(
            why.as_deref() == Some("past its token limit of 1000 tokens (agent definition few)"),
            "{why:?}"
        );
        for (def, id) in [(many_def, many), (free_def, free)] {
            anyhow::ensure!(
                ended(&hub, &ada, def, id).await?.is_none() && closers(&hub, id).await?.is_empty(),
                "only the session past its limit ends: {id}'s was"
            );
        }
        Ok(())
    })
}

/// P5.4's test "a session past its cost limit": Ada's agent `cheap` (Claude Code) has a limit of 50 cents, `dear` one
/// of $10,000. A session started from each says, as its turn ends, that it has spent $0.75: `cheap`'s is ended, its end
/// naming the limit in its definition's sessions and in the audit trail, and `dear`'s runs on. `away`, limited to 50
/// cents too, runs Codex all the same (a start may choose its harness over its definition's: `super::super::uses`),
/// which says no cost: its session is ended as it starts, saying its limit can't be held.
#[test]
fn p5_4_a_session_past_its_cost_limit_is_ended_with_the_limit_named() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = super::run(h).await;
        });
        let ada = person("ada")?;
        let dear = defined(&hub, &ada, "dear", "claude", json!({"cents": 1_000_000})).await?;
        let away = defined(&hub, &ada, "away", "claude", json!({"cents": 50})).await?;
        let cheap = defined(&hub, &ada, "cheap", "claude", json!({"cents": 50})).await?;
        started(&hub, "dear-session", "claude", &dear).await;
        spent(&hub, "dear-session", 1000, 100, 0.75).await;
        started(&hub, "away-session", "codex", &away).await;
        started(&hub, "cheap-session", "claude", &cheap).await;
        spent(&hub, "cheap-session", 1000, 100, 0.75).await;
        let by = closed_by(&hub, "cheap-session").await?;
        anyhow::ensure!(
            by == ["kiln (cost limit: 50 cents, agent definition cheap)"],
            "{by:?}"
        );
        let why = ended(&hub, &ada, &cheap, "cheap-session").await?;
        anyhow::ensure!(
            why.as_deref() == Some("past its cost limit of 50 cents (agent definition cheap)"),
            "{why:?}"
        );
        let by = closed_by(&hub, "away-session").await?;
        anyhow::ensure!(
            by == ["kiln (cost limit: 50 cents, agent definition away)"],
            "{by:?}"
        );
        let why = ended(&hub, &ada, &away, "away-session").await?;
        anyhow::ensure!(
            why.as_deref()
                == Some(
                    "its cost limit of 50 cents can't be held: Codex doesn't say what its sessions cost (agent definition away)"
                ),
            "{why:?}"
        );
        anyhow::ensure!(
            ended(&hub, &ada, &dear, "dear-session").await?.is_none()
                && closers(&hub, "dear-session").await?.is_empty(),
            "only the sessions past their limits end: dear's was"
        );
        Ok(())
    })
}

/// P5.4: a cost limit is held only to a harness that says what its sessions cost. Codex says tokens alone, so a
/// definition of it that sets `cents` is refused as it's saved (422, the reason said; the check says it first), as is a
/// change that moves a Claude Code agent with a cost limit to Codex; Codex's token limit, and Claude Code's cost limit,
/// are kept.
#[test]
fn p5_4_a_cost_limit_is_refused_for_a_harness_that_says_no_cost() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let ada = person("ada")?;
        let why = "Codex doesn't say what its sessions cost, so kiln can't hold a limit in `cents` to it: limit its `tokens` instead";
        let refusal = (
            http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("kiln can't run this agent: {why}"),
        );
        let said = refused(
            &hub,
            &ada,
            "agentdefs.create",
            json!({"name": "spender", "harness": "codex", "limits": {"cents": 50}}),
        )
        .await?;
        anyhow::ensure!(said == refusal, "{said:?}");
        let checked = op(
            &hub,
            &ada,
            "agentdefs.check",
            json!({"harness": "codex", "limits": {"cents": 50}}),
        )
        .await?;
        anyhow::ensure!(
            checked == json!({"ok": false, "problems": [why]}),
            "{checked}"
        );
        defined(&hub, &ada, "counter", "codex", json!({"tokens": 1000})).await?;
        let spender = defined(&hub, &ada, "spender", "claude", json!({"cents": 50})).await?;
        let said = refused(
            &hub,
            &ada,
            "agentdefs.update",
            json!({"id": spender, "harness": "codex"}),
        )
        .await?;
        anyhow::ensure!(said == refusal, "{said:?}");
        Ok(())
    })
}

/// P5.4: a token limit is held only to a harness that says how many tokens its sessions use. Claude Code, Codex,
/// Antigravity (its ACP server says each turn's) and Pi do; any other ACP agent's tokens are its own to give or leave
/// out, so a definition of any of the six that sets `tokens` is refused as it's saved (422, the reason said; the check says it first), as is a change that
/// moves a Claude Code agent with a token limit to one. A cost limit refused for an ACP agent is told to limit its
/// time instead, as its tokens would be refused too; its time limit is kept. Making `Kind::says_tokens` true for ACP,
/// or taking `check`'s token refusal out, fails this test.
#[test]
fn p5_4_a_token_limit_is_refused_for_a_harness_that_says_no_tokens() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let ada = person("ada")?;
        for (harness, name) in [
            ("devin", "Devin"),
            ("gemini", "Gemini CLI"),
            ("opencode", "opencode"),
            ("goose", "Goose"),
            ("amp", "Amp"),
            ("cursor", "Cursor CLI"),
        ] {
            let why = format!(
                "{name} doesn't say how many tokens its sessions use, so kiln can't hold a limit in `tokens` to it: limit its `seconds` instead"
            );
            let checked = op(
                &hub,
                &ada,
                "agentdefs.check",
                json!({"harness": harness, "limits": {"tokens": 1000}}),
            )
            .await?;
            anyhow::ensure!(
                checked == json!({"ok": false, "problems": [why]}),
                "{harness}: {checked}"
            );
        }
        for harness in ["claude", "codex", "antigravity", "pi"] {
            let name = format!("{harness}-counter");
            defined(&hub, &ada, &name, harness, json!({"tokens": 1000})).await?;
        }
        let why = "Gemini CLI doesn't say how many tokens its sessions use, so kiln can't hold a limit in `tokens` to it: limit its `seconds` instead";
        let refusal = (
            http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("kiln can't run this agent: {why}"),
        );
        let said = refused(
            &hub,
            &ada,
            "agentdefs.create",
            json!({"name": "reader", "harness": "gemini", "limits": {"tokens": 1000}}),
        )
        .await?;
        anyhow::ensure!(said == refusal, "{said:?}");
        let reader = defined(&hub, &ada, "reader", "claude", json!({"tokens": 1000})).await?;
        let said = refused(
            &hub,
            &ada,
            "agentdefs.update",
            json!({"id": reader, "harness": "gemini"}),
        )
        .await?;
        anyhow::ensure!(said == refusal, "{said:?}");
        let checked = op(
            &hub,
            &ada,
            "agentdefs.check",
            json!({"harness": "gemini", "limits": {"cents": 50}}),
        )
        .await?;
        anyhow::ensure!(
            checked
                == json!({"ok": false, "problems": [
                    "Gemini CLI doesn't say what its sessions cost, so kiln can't hold a limit in `cents` to it: limit its `seconds` instead"
                ]}),
            "{checked}"
        );
        defined(&hub, &ada, "timed", "gemini", json!({"seconds": 600})).await?;
        Ok(())
    })
}

/// P5.4: a session held to a token limit while it runs a harness that says no tokens is ended as it starts. Ada's agent
/// `away` (Claude Code) is limited to 1,000 tokens, and its session runs Gemini CLI all the same (a start may choose its
/// harness over its definition's: `super::super::uses`), an ACP agent whose tokens are its own to give or leave out:
/// it's ended before it says it spent anything, its end saying the limit can't be held, in its definition's sessions
/// and in the audit trail. `home`, limited alike, runs Claude Code and runs on. Taking `past`'s `UnseenTokens` out
/// leaves `away`'s session open, and this test fails.
#[test]
fn p5_4_a_session_on_a_harness_that_says_no_tokens_is_ended_as_it_starts() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = super::run(h).await;
        });
        let ada = person("ada")?;
        let home = defined(&hub, &ada, "home", "claude", json!({"tokens": 1000})).await?;
        let away = defined(&hub, &ada, "away", "claude", json!({"tokens": 1000})).await?;
        started(&hub, "home-session", "claude", &home).await;
        started(&hub, "away-session", "gemini", &away).await;
        let by = closed_by(&hub, "away-session").await?;
        anyhow::ensure!(
            by == ["kiln (token limit: 1000 tokens, agent definition away)"],
            "{by:?}"
        );
        let why = ended(&hub, &ada, &away, "away-session").await?;
        anyhow::ensure!(
            why.as_deref()
                == Some(
                    "its token limit of 1000 tokens can't be held: Gemini CLI doesn't say how many tokens its sessions use (agent definition away)"
                ),
            "{why:?}"
        );
        anyhow::ensure!(
            ended(&hub, &ada, &home, "home-session").await?.is_none()
                && closers(&hub, "home-session").await?.is_empty(),
            "only the session whose limit can't be held ends: home's was"
        );
        Ok(())
    })
}
