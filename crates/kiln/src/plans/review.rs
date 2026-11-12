//! A person's answer to a plan its agent's goal stack waits on (docs/plans.md, "Reviewing a plan"):
//! a plan made from a goal and said ready for them to look at first (`plans.ready` with `then:
//! review`) holds the stack until they answer, in the agent's Plan tab, for the version they looked
//! at: approve (the stack resumes, its steps go to the agent), reject (its steps come off the stack,
//! the goal is skipped, the agent is told, and the stack waits for them), or revise (their words go
//! to the agent as a part of the goal, the plan goes back to planning, and once its agent says it's
//! ready again it comes back to them, changed). Each answer is kept, the last with the plan.
//! Running it (`plans.run`) is approving it ([`run_approves`]): it goes the one way, as the goal's
//! parts. A plan proposed in plan mode isn't answered here: its harness asks, and the `approve`
//! command answers it.
use super::{Error, Plan, R};
use crate::hub::Hub;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// W21-R02: one read revision of the plan and the research its review presents.
#[derive(Debug, Serialize)]
pub struct Context {
    pub plan: Plan,
    pub research: Option<crate::research::View>,
    pub cohort: Cohort,
}

#[derive(Debug, Serialize)]
pub struct Cohort {
    pub plan_version: i64,
    pub research_id: Option<String>,
    pub research_updated_at: Option<i64>,
}

fn context_in(c: &rusqlite::Connection, id: &str) -> anyhow::Result<Option<(Plan, Option<crate::research::Research>)>> {
    let tx = crate::store::tx(c)?;
    let Some(mut plan) = super::get_in(c, id)? else { return Ok(None) };
    plan.reviewed = last_in(c, id)?;
    let research = plan.research.as_deref().map(|id| crate::research::load(c, id)).transpose()?.flatten();
    tx.commit()?;
    Ok(Some((plan, research)))
}

/// Read-only: approval still checks the current held goal stack and expected plan version.
pub async fn context(hub: &Hub, id: &str) -> R<Context> {
    let id = id.to_owned();
    let (plan, research) = hub.store.call(move |c| context_in(c, &id)).await?.ok_or(Error::Missing)?;
    let cohort = Cohort { plan_version: plan.version, research_id: plan.research.clone(), research_updated_at: research.as_ref().map(|r| r.updated_at) };
    Ok(Context { plan, research: research.as_ref().map(|r| crate::research::view(r, false)), cohort })
}

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
/// until it's answered. In the order they came, with no lock (`crate::turn`; goal 5, plan 01a1a59c2aae, its top goal).
static ANSWERING: crate::turn::Gate = crate::turn::Gate::one();

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
    let _one = ANSWERING.turn().await;
    let p = super::get(hub, id).await?;
    let goal = p
        .source
        .as_ref()
        .and_then(|s| s.goal.clone())
        .ok_or_else(|| Error::Bad("only a plan made from a goal waits on a review here: one an agent proposed in plan mode is answered as its harness asks".into()))?;
    let stack_now = || async { crate::goals::get(hub, &goal.agent).await.map_err(goals_error) };
    if let Some(k) = key.as_deref()
        && answered_under(hub, &p.id, k).await?
    {
        return Ok((super::get(hub, id).await?, stack_now().await?));
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

/// Whether plan `id`'s review was answered under `key` already.
async fn answered_under(hub: &Hub, id: &str, key: &str) -> R<bool> {
    let (pid, k) = (id.to_owned(), key.to_owned());
    Ok(hub.store.call(move |c| Ok(c.query_row("SELECT 1 FROM plan_reviews WHERE plan_id = ?1 AND key = ?2", params![pid, k], |_| Ok(())).optional()?.is_some())).await?)
}

/// Whether running plan `id` (`plans.run`) approves it instead: its agent's goals wait on a person's
/// review of it, or a run under `key` approved it already (asked again, its answer lost). A plan
/// made from a goal goes one way once it's ready, its steps to its agent as the goal's parts; run
/// a second way beside them, each step would be done twice.
pub async fn run_approves(hub: &Hub, id: &str, key: Option<&str>) -> R<bool> {
    let p = super::get(hub, id).await?;
    let Some(goal) = p.source.as_ref().and_then(|s| s.goal.as_ref()) else {
        return Ok(false);
    };
    if let Some(k) = key
        && answered_under(hub, &p.id, k).await?
    {
        return Ok(true);
    }
    Ok(crate::goals::get(hub, &goal.agent).await.map_err(goals_error)?.held_for.as_deref() == Some(p.id.as_str()))
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


#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn w21_r02_review_context_keeps_plan_version_last_answer_and_latest_linked_research() -> anyhow::Result<()> {
        let store = crate::store::Store::in_memory_inline()?;
        store.now(|c| {
        c.execute("INSERT INTO plans (id, title, body, tags, source, version, created_at, updated_at, items, phase) VALUES ('p', 'A plan', '- a', '[]', NULL, 3, 1, 2, '[]', 'ready')", [])?;
        assert!(context_in(c, "missing")?.is_none());
        let Some((without, research)) = context_in(c, "p")? else { anyhow::bail!("plan missing") };
        assert_eq!(without.version, 3);
        assert!(research.is_none());
        c.execute("INSERT INTO research (id, question, agent, by, plan_id, sources, findings, skipped, created_at, updated_at) VALUES ('r-old', 'Older', NULL, 'admin', 'p', '[]', '[]', '{}', 1, 1), ('r-new', 'Current', NULL, 'admin', 'p', '[]', '[]', '{}', 2, 4)", [])?;
        c.execute("INSERT INTO plan_reviews (plan_id, version, answer, words, goal, by, at, key) VALUES ('p', 2, 'revise', 'Tighten it', 'g', 'admin', 3, 'k')", [])?;
        let Some((plan, research)) = context_in(c, "p")? else { anyhow::bail!("plan missing") };
        assert_eq!(plan.version, 3);
        assert_eq!(plan.research.as_deref(), Some("r-new"));
        assert_eq!(plan.reviewed.as_ref().map(|reviewed| reviewed.version), Some(2));
        assert_eq!(research.as_ref().map(|r| (r.id.as_str(), r.plan.as_deref(), r.updated_at)), Some(("r-new", Some("p"), 4)));
        Ok(())
        })
    }
}
