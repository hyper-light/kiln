//! An agent definition's time limit held to the sessions it starts (kiln plan 01a1dd2e8618733a87f2793aab1322a8
//! P5.4's test "a session past its time limit").

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::authz::{Caller, Grant, Rights, gate::Kind};
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

/// P5.4's test "a session past its time limit": Ada's agent `quick` has a time limit of 1 s, `slow` one of an
/// hour, `free` none; a session started from each is held to its own. `quick`'s is ended once its second is
/// up, its end naming the limit in its definition's sessions (`ended`) and in the audit trail (who ended it);
/// the other two run on. Taking `due_now`'s end out leaves `quick`'s session open past its limit.
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
        anyhow::ensure!(
            audited.contains(&(
                "kiln (time limit: 1 s, agent definition quick)".to_owned(),
                "agent.close".to_owned()
            )),
            "{audited:?}"
        );
        Ok(())
    })
}
