//! An agent definition's limits held to the sessions it starts (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.4): a
//! session still open past its time (`limits.seconds` since it started from its definition), its tokens
//! (`limits.tokens`, in and out) or its cost (`limits.cents`) is ended for good, the limit named where its end is read
//! (the audit trail's actor, and its definition's sessions: `agentdefs.sessions`' `ended`). Each limit is its
//! definition's at the version the session started from, as kiln.db keeps it, so a server that restarts holds the
//! sessions it finds to theirs.
//!
//! What a session has spent is what kiln's copy of its state says (`Usage`, as an attachment's tokens a day count it,
//! `crate::ci::attach`): its harness says it at each turn's end, and sooner where it can (Codex's token counts, Pi's
//! replies, an ACP agent's cost). The registry tells the limits' owner of each session whose spending changed
//! ([`Told`]), so none runs a whole turn past its limit unseen. A cost limit is held only to a harness that says what
//! its sessions cost (`Kind::says_cost`): one set for another is refused as it's saved (`super::check`), and a session
//! that runs another all the same (one its start chose over its definition's, `super::uses`) is ended at once, as its
//! cost would go unseen.
//!
//! One task owns the limits ([`run`]): it looks at every session as it starts, then at each it's told of (one started
//! from a definition, its deadline maybe the soonest, or one that spent more), and at every one again as the soonest
//! deadline comes, sleeping in between.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::params;

use crate::harness::kinds::Kind;
use crate::harness::model::Usage;
use crate::hub::Hub;

/// The sessions that may be past one of their definitions' limits now, each once, and a wait for the next: told by
/// whoever learns of one (the registry, as what a session spent changes; `super::sessions::record`, as one starts from
/// a definition), taken by [`run`] alone. One per server, its registry's (`Registry::limits`).
#[derive(Default)]
pub struct Told {
    sessions: Mutex<HashSet<String>>,
    changed: crate::rt::Changed,
}

impl Told {
    /// Session `id` may be past one of its definition's limits now.
    pub fn tell(&self, id: &str) {
        self.sessions.lock().insert(id.to_owned());
        self.changed.notify();
    }

    /// The sessions told of since the last take.
    fn take(&self) -> Vec<String> {
        self.sessions.lock().drain().collect()
    }
}

/// What the key kiln keeps why a limit ended a session under starts with ([`ended_key`]).
pub const ENDED: &str = "agentdefs.ended:";

/// Where kiln keeps why a limit ended a session, which its definition's sessions say.
pub fn ended_key(session: &str) -> String {
    format!("{ENDED}{session}")
}

/// The limit of its definition's a session is past.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Past {
    /// Its time, in seconds.
    Seconds(u64),
    /// Its tokens, in and out.
    Tokens(u64),
    /// Its cost, in US cents.
    Cents(u64),
    /// Its cost, in US cents, while it runs `harness` (that harness's name), which doesn't say what its sessions cost:
    /// never held.
    Unseen { cents: u64, harness: String },
}

impl Past {
    /// Why it ended a session started from definition `name`, in words.
    fn why(&self, name: &str) -> String {
        match self {
            Past::Seconds(n) => format!("past its time limit of {n} s (agent definition {name})"),
            Past::Tokens(n) => {
                format!("past its token limit of {n} tokens (agent definition {name})")
            }
            Past::Cents(n) => format!("past its cost limit of {n} cents (agent definition {name})"),
            Past::Unseen { cents, harness } => format!(
                "its cost limit of {cents} cents can't be held: {harness} doesn't say what its sessions cost (agent definition {name})"
            ),
        }
    }

    /// Who ended it, as the audit trail names them: a principal's name, which takes no " of " or " for " (those split
    /// one acting for another, `Store::audit`).
    fn actor(&self, name: &str) -> String {
        match self {
            Past::Seconds(n) => format!("kiln (time limit: {n} s, agent definition {name})"),
            Past::Tokens(n) => format!("kiln (token limit: {n} tokens, agent definition {name})"),
            Past::Cents(n) | Past::Unseen { cents: n, .. } => {
                format!("kiln (cost limit: {n} cents, agent definition {name})")
            }
        }
    }
}

/// A session started from a definition whose version then set a limit, that no limit has ended yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    session: String,
    name: String,
    limits: super::Limits,
    /// When it started from its definition, Unix ms.
    at: i64,
}

impl Held {
    /// When its time is up, Unix ms, where it has a time limit.
    fn due(&self) -> Option<i64> {
        let seconds = self.limits.seconds?;
        Some(
            self.at
                .saturating_add(i64::try_from(seconds.saturating_mul(1000)).unwrap_or(i64::MAX)),
        )
    }

    /// The limit it's past at `now`, running harness `kind` (its id) and having spent `used`: its time first, then its
    /// tokens, then its cost. A cost limit while it runs a harness that doesn't say what its sessions cost can't be
    /// held, so it's past at once.
    fn past(&self, now: i64, kind: &str, used: &Usage) -> Option<Past> {
        if let (Some(n), Some(due)) = (self.limits.seconds, self.due())
            && due <= now
        {
            return Some(Past::Seconds(n));
        }
        if let Some(n) = self.limits.tokens
            && used.tokens() >= n
        {
            return Some(Past::Tokens(n));
        }
        let n = self.limits.cents?;
        match Kind::get(kind) {
            // In dollars, as its harness says it.
            Some(k) if k.says_cost() => {
                (used.cost_usd >= n as f64 / 100.0).then_some(Past::Cents(n))
            }
            k => Some(Past::Unseen {
                cents: n,
                harness: k.map_or_else(|| kind.to_owned(), |k| k.name.to_owned()),
            }),
        }
    }
}

/// The sessions (of `only` alone, where it names them) started from a definition whose version then set a limit, and
/// that no limit has ended yet.
async fn held(hub: &Hub, only: Option<Vec<String>>) -> anyhow::Result<Vec<Held>> {
    let only = only.map(|ids| serde_json::to_string(&ids)).transpose()?;
    let rows: Vec<(String, String, String, i64)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT s.session_id, d.name, v.settings, s.at FROM agent_def_sessions s \
                 JOIN agent_def_versions v ON v.agent_def_id = s.agent_def_id AND v.version = s.version \
                 JOIN agent_defs d ON d.id = s.agent_def_id \
                 WHERE (?1 IS NULL OR s.session_id IN (SELECT value FROM json_each(?1))) \
                 AND NOT EXISTS (SELECT 1 FROM kv WHERE k = ?2 || s.session_id)",
            )?;
            let rows = st.query_map(params![only, ENDED], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(session, name, settings, at)| {
            let limits = serde_json::from_str::<super::Settings>(&settings)
                .ok()?
                .limits;
            (limits != super::Limits::default()).then_some(Held {
                session,
                name,
                limits,
                at,
            })
        })
        .collect())
}

/// Ends `h` for good, past `past`, saying why: kept first, so its definition's sessions say it whatever the end finds.
async fn end(hub: &Hub, h: &Held, past: &Past) {
    if let Err(e) = hub
        .store
        .kv_set(ended_key(&h.session), past.why(&h.name))
        .await
    {
        tracing::warn!(session = %h.session, error = %format!("{e:#}"), "why a limit ended a session wasn't kept");
    }
    if !crate::harness::ops::end(hub, &h.session, &past.actor(&h.name)).await {
        tracing::warn!(session = %h.session, limit = ?past, "a session past its definition's limit couldn't be ended: kiln no longer has it");
    }
}

/// Ends each of `held` kiln still has that's past one of its limits now: the soonest deadline of the rest, Unix ms, if
/// any.
async fn hold(hub: &Hub, held: Vec<Held>) -> Option<i64> {
    let now = hub.clock.unix_ms();
    let mut soonest = None;
    for h in held {
        let Some(info) = hub.harness.info_shared(&h.session) else {
            continue;
        };
        let used = hub
            .harness
            .state(&h.session)
            .map(|s| s.usage)
            .unwrap_or_default();
        match h.past(now, &info.kind, &used) {
            Some(past) => end(hub, &h, &past).await,
            None => soonest = earliest(soonest, h.due()),
        }
    }
    soonest
}

/// The sooner of `a` and `b`, where either is.
fn earliest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    a.into_iter().chain(b).min()
}

/// Holds sessions to their definitions' limits until the server shuts down.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let told = &hub.harness.limits;
    let mut every = true;
    let mut soonest: Option<i64> = None;
    loop {
        // Listened for before the look, so a session told of during it wakes the wait after it.
        let changed = told.changed.listen();
        let these = told.take();
        if every {
            soonest = hold(&hub, held(&hub, None).await?).await;
        } else if !these.is_empty() {
            soonest = earliest(soonest, hold(&hub, held(&hub, Some(these)).await?).await);
        }
        let wait = soonest.map(|due| {
            Duration::from_millis(
                u64::try_from(due.saturating_sub(hub.clock.unix_ms())).unwrap_or(0),
            )
        });
        let until = async {
            match wait {
                Some(d) => hub.clock.sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        let woken = futures::future::select(std::pin::pin!(changed), std::pin::pin!(until));
        match futures::future::select(std::pin::pin!(hub.shutdown.cancelled()), woken).await {
            futures::future::Either::Left(_) => return Ok(()),
            // Told of sessions: those alone are looked at. The soonest deadline come: every one is, as the next is found.
            futures::future::Either::Right((woken, _)) => {
                every = matches!(woken, futures::future::Either::Right(_));
            }
        }
    }
}

#[cfg(test)]
mod tests;
