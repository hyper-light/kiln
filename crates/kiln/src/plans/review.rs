//! A person's answer to a plan its agent's goal stack waits on (docs/plans.md, "Reviewing a plan"):
//! a plan made from a goal and said ready for them to look at first (`plans.ready` with `then:
//! review`) holds the stack until they answer, in the agent's Plan tab, for the version they looked
//! at: approve (the stack resumes, its steps go to the agent), reject (its steps come off the stack,
//! the goal is skipped, the agent is told, and the stack waits for them), or revise (their words go
//! to the agent as a part of the goal, the plan goes back to planning, and once its agent says it's
//! ready again it comes back to them, changed). Each answer is kept, the last with the plan.
//! A plan proposed in plan mode isn't answered here: its harness asks, and the `approve` command
//! answers it.
use super::{Error, Plan, R};
use crate::hub::Hub;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// What a person answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Approve,
    Reject,
    Revise,
}

impl Answer {
    fn word(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Revise => "revise",
        }
    }

    fn of(word: &str) -> Option<Self> {
        match word {
            "approve" => Some(Self::Approve),
            "reject" => Some(Self::Reject),
            "revise" => Some(Self::Revise),
            _ => None,
        }
    }
}

/// The last answer a person gave a plan's review, as `plans.get` has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewed {
    pub answer: Answer,
    /// Revise: what to change, in their words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<String>,
    /// The version they answered.
    pub version: i64,
    pub by: String,
    /// Unix seconds.
    pub at: i64,
}

/// An answer as it's asked: for `version` (none: as the plan is now, Approve from a list), with its
/// idempotency key.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    pub answer: Option<Answer>,
    pub words: Option<String>,
    pub version: Option<i64>,
    pub key: Option<String>,
}

/// Words sent back with a plan, at the most.
const WORDS: usize = 8000;

/// One answer at a time: what's checked (the stack held for the plan, its version, the key) holds
/// until it's answered.
static ANSWERING: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// Plan `id`'s last answered review.
pub async fn last(hub: &Hub, id: &str) -> R<Option<Reviewed>> {
    let id = id.to_owned();
    Ok(hub.store.call(move |c| Ok(last_in(c, &id)?)).await?)
}

pub(super) fn last_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Reviewed>> {
    c.query_row("SELECT answer, words, version, by, at FROM plan_reviews WHERE plan_id = ?1 ORDER BY id DESC LIMIT 1", params![id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?))
    })
    .optional()
    .map(|o| o.and_then(|(a, words, version, by, at)| Answer::of(&a).map(|answer| Reviewed { answer, words, version, by, at })))
}

fn goals_error(e: crate::goals::Error) -> Error {
    match e {
        crate::goals::Error::NoAgent => Error::Bad("the agent whose goals wait on this plan has ended".into()),
        crate::goals::Error::Bad(m) => Error::Bad(m),
        crate::goals::Error::Other(e) => Error::Other(e),
    }
}

/// Answers plan `id`'s review as `asked`, by `who` (as goals say it: `by`): the plan as it is now and
/// its agent's stack. Asked again under its key (its answer lost), it's answered as things stand and
/// does nothing more (CP-T3.2). Refused (409) when nothing waits on its review now, or it changed
/// since the version answered.
pub async fn answer(hub: &Hub, id: &str, asked: Asked, who: &str, by: &crate::goals::By) -> R<(Plan, crate::goals::Stack)> {
    let answer = asked.answer.ok_or_else(|| Error::Bad("`answer` is approve, reject or revise".into()))?;
    let words = asked.words.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(str::to_owned);
    if answer == Answer::Revise && words.is_none() {
        return Err(Error::Bad("say what should change (`words`)".into()));
    }
    if words.as_ref().is_some_and(|w| w.chars().count() > WORDS) {
        return Err(Error::Bad(format!("what should change is at most {WORDS} characters")));
    }
    let key = asked.key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned);
    let _one = ANSWERING.lock().await;
    let p = super::get(hub, id).await?;
    let goal = p
        .source
        .as_ref()
        .and_then(|s| s.goal.clone())
        .ok_or_else(|| Error::Bad("only a plan made from a goal waits on a review here: one an agent proposed in plan mode is answered as its harness asks".into()))?;
    let stack_now = || async { crate::goals::get(hub, &goal.agent).await.map_err(goals_error) };
    if let Some(k) = key.clone() {
        let (pid, k2) = (p.id.clone(), k);
        let seen = hub.store.call(move |c| Ok(c.query_row("SELECT 1 FROM plan_reviews WHERE plan_id = ?1 AND key = ?2", params![pid, k2], |_| Ok(())).optional()?.is_some())).await?;
        if seen {
            return Ok((super::get(hub, id).await?, stack_now().await?));
        }
    }
    let stack = stack_now().await?;
    if stack.held_for.as_deref() != Some(p.id.as_str()) {
        return Err(Error::Refused(http::StatusCode::CONFLICT, "nothing waits on a review of this plan now: its goals went on, or another answer came first".into()));
    }
    if let Some(v) = asked.version.filter(|v| *v != p.version) {
        return Err(Error::Refused(http::StatusCode::CONFLICT, format!("the plan changed since you looked (version {v}): version {} is the one to review", p.version)));
    }
    let made = key.as_deref().map(|k| crate::util::derive_id(&["plans.review", &p.id, k]));
    match answer {
        Answer::Approve => crate::goals::control_as(hub, &goal.agent, crate::goals::Action::Start, by, crate::goals::Pausing::default()).await.map_err(goals_error)?,
        Answer::Reject => crate::goals::answer_plan(hub, &goal.agent, &goal.id, &p, None, None, by).await.map_err(goals_error)?,
        Answer::Revise => {
            // Its planning begins again: once its agent says it's ready, it comes back here (`plans.ready`).
            let (pid, now) = (p.id.clone(), crate::util::now_unix());
            hub.store.call(move |c| Ok(c.execute("UPDATE plans SET phase = 'planning', updated_at = ?2 WHERE id = ?1", params![pid, now]).map(|_| ())?)).await?;
            crate::goals::answer_plan(hub, &goal.agent, &goal.id, &p, Some((words.as_deref().unwrap_or_default(), p.research.as_deref())), made.clone(), by).await.map_err(goals_error)?
        }
    };
    let (pid, version, gid, who_s, now, w, k) = (p.id.clone(), p.version, goal.id.clone(), who.to_owned(), crate::util::now_unix(), words.clone(), key);
    let word = answer.word();
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO plan_reviews (plan_id, version, answer, words, goal, by, at, key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![pid, version, word, w, gid, who_s, now, k])?;
            Ok(())
        })
        .await?;
    // Rejected, its agent is told: the goal it planned won't be pursued, and its stack waits.
    if answer == Answer::Reject {
        tell(hub, &goal.agent, &p, made.as_deref()).await;
    }
    Ok((super::get(hub, id).await?, stack_now().await?))
}

/// Agent `agent` told its plan `p` was rejected (a message, queued behind a turn under way; `id`: the
/// answer's own, so asked again it's one message).
async fn tell(hub: &Hub, agent: &str, p: &Plan, id: Option<&str>) {
    let text = format!(
        "The person rejected your plan \"{}\" (version {}): its steps won't run, and the goal it was made from is skipped. Your goal stack waits, paused, until they resume it: don't start on anything for it meanwhile.",
        p.title, p.version
    );
    let id = id.map_or_else(crate::util::new_id, |i| format!("{i}-told"));
    let c = crate::harness::model::Command::Send { id, text, mode: crate::harness::model::SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
    let _sent = hub.harness.command(hub, agent, c).await;
}
