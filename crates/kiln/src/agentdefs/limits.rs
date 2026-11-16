//! An agent definition's time limit held to the sessions it starts (kiln plan 01a1dd2e8618733a87f2793aab1322a8
//! P5.4): a session still open when its definition's `limits.seconds` have passed since it started from it is
//! ended for good, the limit named where its end is read (the audit trail's actor, and its definition's
//! sessions: `agentdefs.sessions`' `ended`). Its deadline comes from what kiln.db keeps of it, when it started
//! from its definition and that definition's settings at the version it started from, so a server that
//! restarts holds the sessions it finds to theirs. One task owns the deadlines ([`run`]): it ends those due,
//! then sleeps until the soonest of the rest, woken early by a session started from a definition since
//! ([`armed`]). A definition's cost and token limits are not held here: they wait on a session's usage as it
//! changes, which no event of kiln's says yet.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use rusqlite::params;

use crate::hub::Hub;

/// A session started from a definition since the owner last looked ([`armed`]).
static RECORDED: LazyLock<crate::rt::Changed> = LazyLock::new(crate::rt::Changed::new);

/// Tells [`run`] a session started from a definition (`super::sessions::record`), whose deadline may be the
/// soonest.
pub fn armed() {
    RECORDED.notify();
}

/// What the key kiln keeps why a limit ended a session under starts with ([`ended_key`]).
pub const ENDED: &str = "agentdefs.ended:";

/// Where kiln keeps why a limit ended a session, which its definition's sessions say.
pub fn ended_key(session: &str) -> String {
    format!("{ENDED}{session}")
}

/// A session kiln still has, started from a definition whose version then set a time limit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Limited {
    session: String,
    name: String,
    seconds: u64,
    /// When it started from its definition, Unix ms.
    at: i64,
}

impl Limited {
    /// When its time is up, Unix ms.
    fn due(&self) -> i64 {
        self.at
            .saturating_add(i64::try_from(self.seconds.saturating_mul(1000)).unwrap_or(i64::MAX))
    }

    /// Why it ended, in words.
    fn why(&self) -> String {
        format!(
            "past its time limit of {} s (agent definition {})",
            self.seconds, self.name
        )
    }

    /// Who ended it, as the audit trail names them: a principal's name, which takes no " of " or " for "
    /// (those split one acting for another, `Store::audit`).
    fn actor(&self) -> String {
        format!(
            "kiln (time limit: {} s, agent definition {})",
            self.seconds, self.name
        )
    }
}

/// Every session kiln still has whose definition, at the version it started from, set a time limit, and
/// that no limit has ended yet.
async fn limited(hub: &Hub) -> anyhow::Result<Vec<Limited>> {
    let rows: Vec<(String, String, String, i64)> = hub
        .store
        .read(|c| {
            let mut st = c.prepare(
                "SELECT s.session_id, d.name, v.settings, s.at FROM agent_def_sessions s \
                 JOIN agent_def_versions v ON v.agent_def_id = s.agent_def_id AND v.version = s.version \
                 JOIN agent_defs d ON d.id = s.agent_def_id",
            )?;
            let rows = st.query_map(params![], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut out = Vec::new();
    for (session, name, settings, at) in rows {
        let Some(seconds) = serde_json::from_str::<super::Settings>(&settings)
            .ok()
            .and_then(|s| s.limits.seconds)
        else {
            continue;
        };
        if hub.harness.info(&session).is_none()
            || hub.store.kv_get(ended_key(&session)).await?.is_some()
        {
            continue;
        }
        out.push(Limited {
            session,
            name,
            seconds,
            at,
        });
    }
    Ok(out)
}

/// Ends `l` for good, saying why: kept first, so its definition's sessions say it whatever the end finds.
async fn end(hub: &Hub, l: &Limited) {
    let why = l.why();
    if let Err(e) = hub.store.kv_set(ended_key(&l.session), why.clone()).await {
        tracing::warn!(session = %l.session, error = %format!("{e:#}"), "why a limit ended a session wasn't kept");
    }
    if !crate::harness::ops::end(hub, &l.session, &l.actor()).await {
        tracing::warn!(session = %l.session, "a session past its definition's time limit couldn't be ended: kiln no longer has it");
    }
}

/// Ends every session past its deadline: the soonest deadline of the rest, Unix ms, if any.
async fn due_now(hub: &Hub) -> anyhow::Result<Option<i64>> {
    let now = hub.clock.unix_ms();
    let mut soonest: Option<i64> = None;
    for l in limited(hub).await? {
        let due = l.due();
        if due <= now {
            end(hub, &l).await;
        } else {
            soonest = Some(soonest.map_or(due, |s| s.min(due)));
        }
    }
    Ok(soonest)
}

/// Holds sessions to their definitions' time limits until the server shuts down.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        // Listened for before the look, so a session recorded during it wakes the wait after it.
        let recorded = RECORDED.listen();
        let soonest = due_now(&hub).await?;
        let until = async {
            match soonest {
                Some(due) => {
                    hub.clock
                        .sleep(Duration::from_millis(
                            u64::try_from(due.saturating_sub(hub.clock.unix_ms())).unwrap_or(0),
                        ))
                        .await
                }
                None => std::future::pending::<()>().await,
            }
        };
        let woken = futures::future::select(std::pin::pin!(recorded), std::pin::pin!(until));
        if let futures::future::Either::Right(_) =
            futures::future::select(std::pin::pin!(hub.shutdown.cancelled()), woken).await
        {
            continue;
        }
        return Ok(());
    }
}

#[cfg(test)]
mod tests;
