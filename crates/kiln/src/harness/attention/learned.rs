//! Mission Control's learned order on the server (attention plan 01a199e1b177752e84dad5dc990e6a15, steps 2 and 5).
//! Each person's uses, choices and finished visits are read from the store once and then as they come, and learned by
//! `kiln_ci::attention`'s ensemble one choice at a time (one recorded after later ones were learned teaches the fits but
//! isn't scored: forecast from the future). An arrival draws its two orders from it; the person's own actions move only
//! the rows they acted on, scored again with the visit's own draw. The ensemble is kept in the store, so a server that
//! starts again goes on from it.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock};

use kiln_ci::attention::{Candidate, Choice, Ensemble, Evidence, Family, FitReport, Model, Ranked, Use, UseIndex, activation_of, rank_candidates};
use kiln_ci::attention_examination::{Counts, Report as Examination};
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::hub::Hub;

/// One person's learned state, as far as the store has been read.
#[derive(Default)]
pub struct Learned {
    /// Its saved ensemble was read (or there was none): a server that starts again goes on from it.
    loaded: bool,
    index: UseIndex,
    uses_read: i64,
    ensemble: Ensemble,
    choices_read: i64,
    last_choice: Option<(i64, i64)>,
    counts: Counts,
    visits_read: Option<(i64, String)>,
    report: Option<FitReport>,
    examination: Option<Examination>,
}

type Shared = Arc<futures::lock::Mutex<Learned>>;
static PEOPLE: LazyLock<Mutex<BTreeMap<String, Shared>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn state(person: &str) -> Shared {
    Arc::clone(PEOPLE.lock().entry(person.to_owned()).or_default())
}

/// What's kept of a person's learned state between runs: the ensemble and how far into their choices it learned. Uses
/// and finished visits are read again (an index of uses and the examination's counts are quick to make).
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    ensemble: Ensemble,
    choices_read: i64,
    last_choice: Option<(i64, i64)>,
}

fn load(c: &Connection, person: &str) -> anyhow::Result<Option<Saved>> {
    use rusqlite::OptionalExtension as _;
    let value: Option<String> = c.query_row("SELECT value FROM attention_learned WHERE principal=?1", [person], |r| r.get(0)).optional()?;
    // A state this build can't read is learned again, from the start.
    Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
}

fn save(c: &Connection, person: &str, saved: &str) -> anyhow::Result<()> {
    c.execute("INSERT INTO attention_learned(principal,value) VALUES(?1,?2) ON CONFLICT(principal) DO UPDATE SET value=excluded.value", params![person, saved])?;
    Ok(())
}

/// What the store has that the state hasn't read.
struct Fresh {
    uses: Vec<(i64, Use)>,
    choices: Vec<(i64, Choice)>,
    visits: Vec<(i64, String, kiln_ci::attention_examination::Exposure)>,
}

fn new_uses(c: &Connection, person: &str, after: i64) -> anyhow::Result<Vec<(i64, Use)>> {
    let mut q = c.prepare("SELECT rowid,agent,kind,at FROM attention_uses WHERE rowid>?1 AND (principal=?2 OR principal='') ORDER BY rowid")?;
    let rows: Vec<(i64, String, String, i64)> = q.query_map(params![after, person], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
    rows.into_iter().map(|(id, agent, kind, at)| Ok((id, Use { agent, kind: serde_json::from_value(Value::String(kind))?, at }))).collect()
}

fn new_choices(c: &Connection, person: &str, after: i64) -> anyhow::Result<Vec<(i64, Choice)>> {
    let mut q = c.prepare("SELECT rowid,at,chosen,candidates,propensity,correction,evidence FROM attention_choices WHERE principal=?1 AND rowid>?2 ORDER BY at,rowid")?;
    type Row = (i64, i64, String, String, Option<f64>, Option<String>, Option<String>);
    let rows: Vec<Row> = q.query_map(params![person, after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, at, chosen, candidates, propensity, correction, evidence)| {
            let evidence = evidence.map(|e| serde_json::from_value(Value::String(e))).transpose()?.unwrap_or_default();
            Ok((id, Choice { at, chosen, candidates: serde_json::from_str(&candidates)?, propensity, correction: correction.map(|j| serde_json::from_str(&j)).transpose()?, evidence }))
        })
        .collect()
}

/// Read what's new and learn it, one choice at a time. A choice recorded after later ones were learned teaches the fits
/// where it lands, but isn't scored (see [`Ensemble::learn_late_choice`]).
pub async fn refresh(hub: &Hub, person: &str) -> anyhow::Result<Shared> {
    let shared = state(person);
    let mut guard = shared.lock().await;
    if !guard.loaded {
        let p = person.to_owned();
        if let Some(saved) = hub.store.call(move |c| load(c, &p)).await? {
            guard.ensemble = saved.ensemble;
            guard.choices_read = saved.choices_read;
            guard.last_choice = saved.last_choice;
        }
        guard.loaded = true;
    }
    {
        let (uses_after, choices_after, visits_after) = (guard.uses_read, guard.choices_read, guard.visits_read.clone());
        let p = person.to_owned();
        let fresh = hub
            .store
            .call(move |c| Ok(Fresh { uses: new_uses(c, &p, uses_after)?, choices: new_choices(c, &p, choices_after)?, visits: super::examination::finished_since(c, &p, visits_after.as_ref())? }))
            .await?;
        if fresh.uses.is_empty() && fresh.choices.is_empty() && fresh.visits.is_empty() {
            drop(guard);
            return Ok(shared);
        }
        let learned_any = !fresh.choices.is_empty();
        let taken = std::mem::take(&mut *guard);
        let now = crate::util::now_ms();
        let (learned, saved) = crate::rt::blocking(move || {
            let mut learned = taken;
            learned.apply(fresh, now);
            let saved = learned_any
                .then(|| serde_json::to_string(&Saved { ensemble: learned.ensemble.clone(), choices_read: learned.choices_read, last_choice: learned.last_choice }).ok())
                .flatten();
            (learned, saved)
        })
        .await?;
        *guard = learned;
        if let Some(saved) = saved {
            let p = person.to_owned();
            hub.store.call(move |c| save(c, &p, &saved)).await?;
        }
    }
    drop(guard);
    Ok(shared)
}

impl Learned {
    fn apply(&mut self, fresh: Fresh, now: i64) {
        for (id, u) in &fresh.uses {
            self.index.add(u);
            self.uses_read = self.uses_read.max(*id);
        }
        for (id, choice) in &fresh.choices {
            if self.last_choice.is_some_and(|(at, _)| choice.at < at) {
                self.ensemble.learn_late_choice(&self.index, choice);
            } else {
                self.ensemble.observe_choice(&self.index, choice);
                self.last_choice = Some((choice.at, *id));
            }
            self.choices_read = self.choices_read.max(*id);
            self.report = None;
        }
        for (at, key, exposure) in &fresh.visits {
            self.counts.add(exposure, now);
            self.visits_read = Some((*at, key.clone()));
            self.examination = None;
        }
    }

    pub fn report(&mut self) -> FitReport {
        if self.report.is_none() {
            self.report = Some(self.ensemble.report());
        }
        self.report.clone().unwrap_or_else(|| self.ensemble.report())
    }

    /// The position-bias fit from every finished visit: what a pick from the list is weighted by.
    pub fn examination(&mut self) -> Examination {
        if self.examination.is_none() {
            self.examination = Some(self.counts.report());
        }
        self.examination.clone().unwrap_or_else(|| self.counts.report())
    }

    pub fn candidate(&self, agent: &str, at: i64) -> Candidate {
        self.index.candidate(agent, at)
    }

    /// Whether the person used `agent` themselves after `since`.
    fn acted_since(&self, agent: &str, since: i64, now: i64) -> bool {
        let then = self.index.candidate(agent, since.saturating_add(1));
        let latest = |c: &Candidate| c.traces.iter().filter(|t| t.kind.human()).filter_map(|t| t.recent.first().copied()).max();
        latest(&self.index.candidate(agent, now.saturating_add(1))) > latest(&then)
    }
}

/// One standard normal from two independent uniforms on (0, 1) (Box–Muller).
fn normal(u: f64, v: f64) -> f64 {
    (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
}

fn normals(n: usize) -> Vec<f64> {
    use rand::distr::{Distribution as _, Open01};
    let mut rng = rand::rng();
    (0..n).map(|_| normal(Open01.sample(&mut rng), Open01.sample(&mut rng))).collect()
}

/// What an arrival shows: two orders, each from its own Thompson draw of the learned family when it beats every simpler
/// order and the person hasn't opted out; the person's most recent interaction otherwise.
pub struct Arrival {
    pub report: FitReport,
    pub examination: Examination,
    pub model_a: Option<Model>,
    pub model_b: Option<Model>,
    pub ranks_a: Vec<Ranked>,
    pub ranks_b: Vec<Ranked>,
}

pub async fn arrive(shared: Shared, candidates: Vec<String>, learned: bool, now: i64) -> anyhow::Result<Arrival> {
    let mut guard = shared.lock().await;
    let taken = std::mem::take(&mut *guard);
    let (state, arrival) = crate::rt::blocking(move || {
        let mut state = taken;
        let report = state.report();
        let examination = state.examination();
        let family = if learned { state.ensemble.learned() } else { None };
        let draw = |f: Family, z: Vec<f64>| state.ensemble.draw(f, &z).ok().flatten();
        let (model_a, model_b) = match family {
            Some(f) => (draw(f, normals(state.ensemble.dimension(f))), draw(f, normals(state.ensemble.dimension(f)))),
            None => (None, None),
        };
        let read: Vec<Candidate> = candidates.iter().map(|id| state.candidate(id, now)).collect();
        let ranks_a = rank_candidates(model_a.as_ref(), &read, now);
        let ranks_b = rank_candidates(model_b.as_ref(), &read, now);
        (state, Arrival { report, examination, model_a, model_b, ranks_a, ranks_b })
    })
    .await?;
    *guard = state;
    Ok(arrival)
}

/// The visit's order after the person's own actions since it began: the rows they acted on, scored again with the
/// visit's own draw at `now` (with no draw, to the top, as their latest interaction); every other row where the visit
/// put it.
pub async fn after_own_actions(shared: Shared, ranks: Vec<Ranked>, model: Option<Model>, since: i64, now: i64) -> anyhow::Result<Vec<Ranked>> {
    let guard = shared.lock().await;
    let acted: HashSet<String> = ranks.iter().filter(|r| guard.acted_since(&r.agent, since, now)).map(|r| r.agent.clone()).collect();
    if acted.is_empty() {
        return Ok(ranks);
    }
    let Some(model) = model else {
        let (mut moved, rest): (Vec<Ranked>, Vec<Ranked>) = ranks.into_iter().partition(|r| acted.contains(&r.agent));
        let latest = |id: &str| guard.candidate(id, now.saturating_add(1)).traces.iter().filter(|t| t.kind.human()).filter_map(|t| t.recent.first().copied()).max();
        moved.sort_by_key(|r| std::cmp::Reverse(latest(&r.agent)));
        for r in &mut moved {
            r.reason = "Your latest interaction".into();
        }
        moved.extend(rest);
        return Ok(moved);
    };
    let mut rescored: Vec<Ranked> = ranks
        .into_iter()
        .map(|mut r| {
            if acted.contains(&r.agent) {
                let c = guard.candidate(&r.agent, now);
                r.score = activation_of(&model, &c, now);
                r.reason = format!("{} · your action just now", r.reason.split(" · your action just now").next().unwrap_or_default());
            }
            r
        })
        .collect();
    // A stable sort: rows not acted on keep the visit's order among themselves.
    rescored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(rescored)
}

/// Where each chat would have sat at `at` by what the person's history has taught (the learned family's point
/// estimate when it beats the simpler orders, else their most recent interaction), among the chats of the last choice
/// recorded at or before then; and where that choice's chat sat.
pub async fn replay(hub: &Hub, shared: Shared, person: &str, at: i64, now_rows: Vec<String>) -> anyhow::Result<Value> {
    let p = person.to_owned();
    let last: Option<(i64, String, String)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row("SELECT at,chosen,candidates FROM attention_choices WHERE principal=?1 AND at<=?2 AND COALESCE(evidence,'pick')='pick' ORDER BY at DESC,rowid DESC LIMIT 1", params![p, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)
        })
        .await?;
    let mut guard = shared.lock().await;
    let report = guard.report();
    let model = guard.ensemble.learned().and_then(|f| guard.ensemble.point(f));
    let (rows, picked) = match &last {
        Some((when, chosen, candidates)) => (serde_json::from_str::<Vec<String>>(candidates)?, Some((*when, chosen.clone()))),
        None => (now_rows, None),
    };
    let read: Vec<Candidate> = rows.iter().map(|id| guard.candidate(id, at)).collect();
    let order = rank_candidates(model.as_ref(), &read, at);
    let sat = picked.as_ref().and_then(|(when, chosen)| {
        let then: Vec<Candidate> = rows.iter().map(|id| guard.candidate(id, *when)).collect();
        rank_candidates(model.as_ref(), &then, *when).iter().position(|r| &r.agent == chosen).map(|p| json!({"agent": chosen, "at": when, "position": p + 1, "of": then.len()}))
    });
    drop(guard);
    Ok(json!({
        "at": at,
        "order": order,
        "model": report,
        "learned": model.is_some(),
        "pick": sat,
        "limitation": "Each chat's uses are as recorded before that moment, and its activation reads them by Petrov's approximation (its most recent use exactly, the rest as spread evenly back to its first). Choices recorded before the attention tables have no candidate sets, so history from then teaches activation, not choices."
    }))
}

/// Record a pin, park or archive as evidence of the person's attention: a best (pin) or worst (park, archive) choice
/// among the chats in their list then.
pub fn record_evidence(c: &Connection, person: &str, agent: &str, evidence: Evidence, key: &str, candidates: &[String], at: i64) -> anyhow::Result<bool> {
    if evidence == Evidence::Pick || !candidates.iter().any(|id| id == agent) || candidates.len() < 2 {
        return Ok(false);
    }
    let name = serde_json::to_value(evidence)?.as_str().map(str::to_owned).unwrap_or_default();
    Ok(c.execute(
        "INSERT OR IGNORE INTO attention_choices(principal,key,at,chosen,candidates,source,evidence) VALUES(?1,?2,?3,?4,?5,'evidence',?6)",
        params![person, format!("evidence:{key}"), at, agent, serde_json::to_string(candidates)?, name],
    )? > 0)
}

/// Forget a person's state (their history is the store's; it's read again when next needed).
#[cfg(test)]
pub fn forget(person: &str) {
    PEOPLE.lock().remove(person);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::attention::UseKind;

    fn shared(uses: &[Use]) -> Shared {
        Arc::new(futures::lock::Mutex::new(Learned { index: UseIndex::new(uses), loaded: true, ..Learned::default() }))
    }

    fn rows(ids: &[&str], scores: &[Option<f64>]) -> Vec<Ranked> {
        ids.iter().zip(scores).map(|(id, score)| Ranked { agent: (*id).to_owned(), score: *score, reason: "before".into() }).collect()
    }

    fn model() -> Model {
        Model { decay: 0.5, weights: [(UseKind::Message, 1.0), (UseKind::Turn, 1.0)].into_iter().collect(), range_weight: None, scales: BTreeMap::new(), family: Family::ActR }
    }

    /// The plan's step 5 test: an agent finishes while the person is reading the list; its place changes only on the next
    /// arrival or return, while the person's own action moves the row they acted on at once, and only it.
    #[test]
    fn mc_attention_within_a_visit_only_the_persons_own_action_moves_a_row() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let visit = 1_000;
            let before = [Use { agent: "a".into(), kind: UseKind::Message, at: 900 }, Use { agent: "b".into(), kind: UseKind::Message, at: 800 }, Use { agent: "c".into(), kind: UseKind::Message, at: 700 }];
            let ranks = rows(&["a", "b", "c"], &[Some(-2.0), Some(-3.0), Some(-4.0)]);
            // An agent's turn during the visit moves nothing.
            let mut uses = before.to_vec();
            uses.push(Use { agent: "c".into(), kind: UseKind::Turn, at: 1_500 });
            let kept = after_own_actions(shared(&uses), ranks.clone(), Some(model()), visit, 2_000).await?;
            anyhow::ensure!(kept == ranks, "{kept:?}");
            // The person's message to c moves c, and only c, by the visit's own draw.
            uses.push(Use { agent: "c".into(), kind: UseKind::Message, at: 1_900 });
            let moved = after_own_actions(shared(&uses), ranks.clone(), Some(model()), visit, 2_000).await?;
            let order: Vec<&str> = moved.iter().map(|r| r.agent.as_str()).collect();
            anyhow::ensure!(order == ["c", "a", "b"], "{order:?}");
            anyhow::ensure!(moved.iter().filter(|r| r.agent != "c").zip(ranks.iter().filter(|r| r.agent != "c")).all(|(a, b)| a == b), "the others keep their visit's scores and order");
            // Without a draw, the person's latest interaction goes first.
            let fallback = after_own_actions(shared(&uses), rows(&["a", "b", "c"], &[None, None, None]), None, visit, 2_000).await?;
            anyhow::ensure!(fallback.first().map(|r| r.agent.as_str()) == Some("c"));
            Ok(())
        })
    }

    fn record_choice(c: &Connection, key: &str, at: i64, chosen: &str, candidates: &[&str]) -> anyhow::Result<()> {
        c.execute("INSERT INTO attention_choices(principal,key,at,chosen,candidates,source) VALUES('owner',?1,?2,?3,?4,'command')", params![key, at, chosen, serde_json::to_string(candidates)?])?;
        Ok(())
    }

    /// Learned as choices come, a late one once where it lands, and kept between runs.
    #[test]
    fn mc_attention_learning_reads_on_learns_a_late_choice_once_and_survives_a_restart() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            forget("owner");
            hub.store
                .call(|c| {
                    for (agent, at) in [("a", 10), ("b", 20), ("c", 30)] {
                        super::super::store::record(c, "owner", agent, "message", &format!("m{at}"), at)?;
                    }
                    record_choice(c, "k1", 40, "a", &["a", "b", "c"])?;
                    record_choice(c, "k2", 50, "b", &["a", "b", "c"])?;
                    Ok(())
                })
                .await?;
            let observed = |s: &Shared| async move { s.lock().await.report().observations };
            let state = refresh(&hub, "owner").await?;
            anyhow::ensure!(observed(&state).await == 2);
            // One more, then one from before the last learned: learned once, the last learned still the last.
            hub.store.call(|c| record_choice(c, "k3", 60, "c", &["a", "b", "c"])).await?;
            anyhow::ensure!(observed(&refresh(&hub, "owner").await?).await == 3);
            hub.store.call(|c| record_choice(c, "k0", 35, "c", &["a", "b", "c"])).await?;
            let again = refresh(&hub, "owner").await?;
            anyhow::ensure!(observed(&again).await == 4 && again.lock().await.last_choice.is_some_and(|(at, _)| at == 60), "a late choice is learned where it lands, once");
            // A server that starts again goes on from what was kept.
            forget("owner");
            let restarted = refresh(&hub, "owner").await?;
            anyhow::ensure!(observed(&restarted).await == 4, "the saved ensemble, not learned again from nothing");
            Ok(())
        })
    }

    #[test]
    fn mc_attention_evidence_is_a_choice_among_the_list_and_never_a_pick() -> anyhow::Result<()> {
        let c = Connection::open_in_memory()?;
        c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;
        c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;
        c.execute_batch(include_str!("../../store/migrations/20261108-0101_w21_attention_examination.sql"))?;
        let list = ["a".to_owned(), "b".to_owned()];
        anyhow::ensure!(record_evidence(&c, "owner", "a", Evidence::Park, "k", &list, 5)?);
        anyhow::ensure!(!record_evidence(&c, "owner", "a", Evidence::Park, "k", &list, 6)?, "once per action");
        anyhow::ensure!(!record_evidence(&c, "owner", "z", Evidence::Pin, "k2", &list, 7)?, "not among the list");
        anyhow::ensure!(!record_evidence(&c, "owner", "a", Evidence::Pick, "k3", &list, 8)?, "a pick is recorded where it happens");
        let choices = super::super::store::choices(&c, "owner")?;
        anyhow::ensure!(choices.len() == 1 && choices.first().is_some_and(|c| c.evidence == Evidence::Park));
        Ok(())
    }
}
