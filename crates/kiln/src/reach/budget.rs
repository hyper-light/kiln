//! Tenant budgets in a runner's node (docs/needs.md §9.1; docs/ci.md W05-R11; LB08): the shell
//! around `kiln_ci::place::budget`, beside placement's (`super::place`). The node's owner holds
//! every run it's asked (`kiln run`, MCP `run`, `kiln.machines.run`, an agent session's start) at
//! the budgets' door before it probes anywhere: a run of a tenant with a budget is placed only once
//! its worst case is reserved from the rights this owner holds, and is otherwise held back, told
//! why, while the owner asks a peer for rights.
//!
//! - **Declared** by the person in the server's config (`[ci.budgets."<tenant>"]`), and sent with
//!   the directory (its `budgets`: the declarations and each period's split, [`for_directory`]).
//!   The server decides a period's split once, among the runners that name `place.budget` as it
//!   decides, and keeps it in its records (`budget.split`) before any runner sees it, so no two
//!   runners ever mint from different splits (the coordination Bailis et al. prove zero overspend
//!   needs, done ahead of every spend). No run's placement waits on the server.
//! - **On the wire** (docs/wire.md, "Named features"): owner to owner in placement's channel, as
//!   `{"epoch": the sender's epoch, "budget": the core's word}`: a need in `Ask::Place` on this
//!   node's link to the peer, a give in `Say::Place` on the asker's. Only to runners that named
//!   `place.budget`; an older runner reads neither and is never in a split.
//! - **Kept** (LB03's durable point, docs/reach.md §2.3) in the node's records, in a family of its
//!   own ([`FAMILY`]: each pot this owner holds, each run's reservation, each run held back with its
//!   ask and the agent session that asked), written in the same batch commit as the owner's runs
//!   (`place.own`) and before anything that came with it is said: a giver's lowering is kept
//!   before its give goes. The family sorts before `place.own`, so where a batch takes more than one
//!   transaction a reservation is kept no later than its run.
//! - **Restarted**, the rows are taken up before any word; a reservation whose run the owner didn't
//!   keep is let go (its start was never said), before the declarations come.
//! - **Not budgeted yet**: CI legs, whose owner, the server's CI engine (`super::place::CI`),
//!   admits them at runners with no tenant on the ask (W05-R11's legs-in-progress quotas stay the
//!   engine's); and agent sessions, which run until they're ended, with no timeout to reserve by.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use kiln_ci::place::answer::Answer;
use kiln_ci::place::ask::Ask;
use kiln_ci::place::budget::{Budgets, Change, Decl, Do, End, Gate, PotKey, Reserved, Row, Split, Waiting, Word};
use kiln_seams::records::{Cond, Consistency, Refusal, Tx, Write};

use super::directory::RunnerInfo;
use super::place::{Out, Placed};
use super::told::Answers;
use crate::proto::feature;

/// Budgets' rows in a node's records.
pub const FAMILY: &str = "place.budget";

/// The server's split of each period's rights (key: the tenant, NUL, the period's length and
/// index, 8 bytes each, big-endian).
pub const SPLITS: &str = "budget.split";

/// A budget word between owners, in placement's channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub epoch: u64,
    pub budget: Word,
}

/// What the server sends with the directory: the person's declarations and the splits decided.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    #[serde(default)]
    pub decls: Vec<Decl>,
    #[serde(default)]
    pub splits: Vec<Split>,
}

/// A row of [`FAMILY`], as kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "row", rename_all = "snake_case")]
enum Kept {
    Pot { pot: PotKey, kept: kiln_ci::place::budget::Kept },
    Run { run: String, reserved: Reserved },
    Held { run: String, held: Waiting, ask: Box<Ask>, #[serde(default, skip_serializing_if = "Option::is_none")] asker: Option<Answers> },
}

fn pot_key(k: &PotKey) -> Vec<u8> {
    let mut out = b"p\0".to_vec();
    out.extend_from_slice(k.tenant.as_bytes());
    out.push(0);
    out.extend_from_slice(&k.per.to_be_bytes());
    out.extend_from_slice(&k.period.to_be_bytes());
    out
}

fn run_key(prefix: &[u8], run: &str) -> Vec<u8> {
    let mut out = prefix.to_vec();
    out.extend_from_slice(run.as_bytes());
    out
}

/// The owners a split is made among, and asked for rights: the runners connected that name
/// `place.budget`.
pub fn owners(runners: &[RunnerInfo]) -> Vec<String> {
    runners.iter().filter(|r| r.connected && r.reach && feature::speaks(&r.features, feature::PLACE_BUDGET)).map(|r| r.name.clone()).collect()
}

/// Budgets in one node: its owner's rights and the asks it holds back.
pub struct Budget {
    epoch: u64,
    core: Budgets,
    /// The asks held back, and the agent session that asked each (told through told should this
    /// node start again before it's placed).
    asks: BTreeMap<String, Ask>,
    askers: BTreeMap<String, Answers>,
    /// Held runs taken up from before a restart: their asker hears through told.
    restored: BTreeSet<String>,
}

impl Budget {
    pub fn new(me: &str, epoch: u64) -> Budget {
        Budget { epoch, core: Budgets::new(me, epoch), asks: BTreeMap::new(), askers: BTreeMap::new(), restored: BTreeSet::new() }
    }

    /// The server's word with the directory (`budgets`, none from a server before them or with
    /// none declared: then nothing is held back).
    pub fn declared(&mut self, published: Option<&serde_json::Value>, runners: &[RunnerInfo], now: i64) -> (Vec<Ask>, Vec<Out>) {
        let p: Published = published.and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
        let dos = self.core.declare(p.decls, p.splits, owners(runners), now);
        self.outs(dos)
    }

    /// A run asked of this node: `None` to place it now (no budget covers it, or its worst case is
    /// reserved); else what it's told, held back or never.
    pub fn gate(&mut self, ask: &Ask, asker: Option<Answers>, now: i64) -> Option<(Vec<Ask>, Vec<Out>)> {
        // An agent session runs until it's ended, with no timeout to reserve by: not budgeted yet.
        if !self.core.any() || ask.key.as_deref().is_some_and(|k| k.starts_with("session:")) {
            return None;
        }
        let (gate, dos) = self.core.ask(&ask.run, &ask.repo, &ask.needs.request, ask.timeout, now);
        match gate {
            Gate::Free | Gate::Reserved => {
                // Placed now; what it changed for others goes with it.
                let (mut asks, outs) = self.outs(dos);
                if asks.is_empty() && outs.is_empty() {
                    return None;
                }
                asks.insert(0, ask.clone());
                Some((asks, outs))
            }
            Gate::Held => {
                self.asks.insert(ask.run.clone(), ask.clone());
                if let Some(a) = asker {
                    self.askers.insert(ask.run.clone(), a);
                }
                Some(self.outs(dos))
            }
            Gate::Never(answer) => {
                let (asks, mut outs) = self.outs(dos);
                outs.push(Out::Local { run: ask.run.clone(), placed: Placed::Answer(answer) });
                Some((asks, outs))
            }
        }
    }

    /// What the owner told a run's asker, as budgets settle by it: started, its start; ended, lost
    /// or never, its end (charged what it ran, its worst case, or nothing).
    pub fn answered(&mut self, run: &str, answer: &Answer, now: i64) -> (Vec<Ask>, Vec<Out>) {
        let end = match answer {
            Answer::Started { .. } => {
                self.core.started(run, now);
                return (Vec::new(), Vec::new());
            }
            Answer::Ended { .. } => End::Ran,
            Answer::Lost { .. } => End::Unknown,
            Answer::Never { .. } => End::NotStarted,
            Answer::Starting { .. } | Answer::Queued { .. } => return (Vec::new(), Vec::new()),
        };
        let dos = self.core.ended(run, end, now);
        self.outs(dos)
    }

    /// The owner said run `run`'s start: only its end settles it now.
    pub fn go(&mut self, run: &str) {
        self.core.go(run);
    }

    /// Its asker let run `run` go: whether it was held here (then it's gone), and what that frees.
    pub fn cancel(&mut self, run: &str, now: i64) -> (bool, (Vec<Ask>, Vec<Out>)) {
        let held = self.core.holds_back(run);
        self.asks.remove(run);
        self.askers.remove(run);
        let dos = self.core.cancel(run, now);
        (held, self.outs(dos))
    }

    /// A placement word from owner `peer`, if it's a budget's (`None`: placement's own).
    pub fn word(&mut self, peer: &str, json: &str, now: i64) -> Option<(Vec<Ask>, Vec<Out>)> {
        if !json.contains("\"budget\"") {
            return None;
        }
        let e = serde_json::from_str::<Envelope>(json).ok()?;
        let dos = self.core.heard(peer, e.budget, now);
        Some(self.outs(dos))
    }

    /// This node's link to runner `peer` went down, or came back.
    pub fn link(&mut self, peer: &str, up: bool, now: i64) -> (Vec<Ask>, Vec<Out>) {
        if !self.core.any() {
            return (Vec::new(), Vec::new());
        }
        let dos = self.core.link(peer, up, now);
        self.outs(dos)
    }

    /// The time: a period's end the runs held wait for.
    pub fn tick(&mut self, now: i64) -> (Vec<Ask>, Vec<Out>) {
        if !self.core.any() {
            return (Vec::new(), Vec::new());
        }
        let dos = self.core.tick(now);
        self.outs(dos)
    }

    pub fn timer(&self) -> Option<i64> {
        self.core.timer()
    }

    /// What changed, noted to be kept with placement's (`super::place`'s unkept rows).
    pub fn note(&mut self, unkept: &mut BTreeMap<(&'static str, Vec<u8>), Option<Vec<u8>>>) {
        for c in self.core.changes() {
            let (key, value) = match c {
                Change::Pot(pot, kept) => (pot_key(&pot), kept.and_then(|kept| serde_json::to_vec(&Kept::Pot { pot, kept }).ok())),
                Change::Run(run, reserved) => (run_key(b"r\0", &run), reserved.and_then(|reserved| serde_json::to_vec(&Kept::Run { run, reserved }).ok())),
                Change::Held(run, held) => {
                    let row = held.and_then(|held| self.asks.get(&run).map(|ask| Kept::Held { run: run.clone(), held, ask: Box::new(ask.clone()), asker: self.askers.get(&run).cloned() }));
                    (run_key(b"h\0", &run), row.and_then(|r| serde_json::to_vec(&r).ok()))
                }
            };
            unkept.insert((FAMILY, key), value);
        }
    }

    /// What this node's process before this one kept (`rows`, [`FAMILY`]'s values), taken up as it
    /// starts, before any word: a reservation whose run the owner didn't keep (`owned`) is let go,
    /// its start never said. Returns the agent sessions that asked for the runs held, by run.
    pub fn restore(&mut self, me: &str, rows: Vec<Vec<u8>>, owned: &BTreeSet<String>, now: i64) -> (BTreeMap<String, Answers>, (Vec<Ask>, Vec<Out>)) {
        let mut core = Vec::new();
        for v in rows {
            match serde_json::from_slice::<Kept>(&v) {
                Ok(Kept::Pot { pot, kept }) => core.push(Row::Pot(pot, kept)),
                Ok(Kept::Run { run, reserved }) => core.push(Row::Run(run, reserved)),
                Ok(Kept::Held { run, held, ask, asker }) => {
                    self.asks.insert(run.clone(), *ask);
                    if let Some(a) = asker {
                        self.askers.insert(run.clone(), a);
                    }
                    self.restored.insert(run.clone());
                    core.push(Row::Held(run, held));
                }
                Err(e) => tracing::warn!(error = %e, "a budget row this node kept doesn't read: left out"),
            }
        }
        self.core = Budgets::restore(me, self.epoch, core);
        let unsaid: Vec<String> = self.core.reservations().map(|(r, _)| r.clone()).filter(|r| !owned.contains(r)).collect();
        let mut dos = Vec::new();
        for r in unsaid {
            dos.extend(self.core.ended(&r, End::NotStarted, now));
        }
        (self.askers.clone(), self.outs(dos))
    }

    /// The core's acts as the node's: words on links, asks released to placement, news for the
    /// runs held.
    fn outs(&mut self, dos: Vec<Do>) -> (Vec<Ask>, Vec<Out>) {
        let (mut asks, mut outs) = (Vec::new(), Vec::new());
        for d in dos {
            match d {
                Do::Say { to, word } => {
                    let need = matches!(word, Word::Need { .. });
                    let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, budget: word }) else { continue };
                    outs.push(if need { Out::Tell { peer: to, json } } else { Out::Say { peer: to, json } });
                }
                Do::Release { run } => {
                    self.askers.remove(&run);
                    self.restored.remove(&run);
                    if let Some(a) = self.asks.remove(&run) {
                        asks.push(a);
                    }
                }
                Do::Tell { run, answer } => {
                    let never = matches!(answer, Answer::Never { .. });
                    if self.restored.contains(&run)
                        && let Some(a) = self.askers.get(&run)
                    {
                        outs.push(Out::Told { asker: a.clone(), answer: answer.clone() });
                    }
                    if never {
                        self.asks.remove(&run);
                        self.askers.remove(&run);
                        self.restored.remove(&run);
                    }
                    outs.push(Out::Local { run, placed: Placed::Answer(answer) });
                }
            }
        }
        (asks, outs)
    }
}

/// The person's budgets as the core takes them: each one that doesn't read is left out, said in
/// the log.
pub fn decls(budgets: &BTreeMap<String, crate::config::ci::Budget>) -> Vec<Decl> {
    budgets
        .iter()
        .filter_map(|(tenant, b)| match b.decl(tenant) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(%tenant, error = %e, "a budget that doesn't read: left out");
                None
            }
        })
        .collect()
}

fn split_key(d: &Decl, period: u64) -> Vec<u8> {
    let mut k = d.tenant.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(&d.per.to_be_bytes());
    k.extend_from_slice(&period.to_be_bytes());
    k
}

/// The split of `d`'s period `period`: as the server's records keep it, or decided now among
/// `owners` and kept first (only where none is: of two answers deciding at once, one is kept, and
/// both send it). `None` with no owner to split among.
async fn split_of(r: &dyn kiln_seams::Records, d: &Decl, period: u64, owners: &[String]) -> anyhow::Result<Option<Split>> {
    let key = split_key(d, period);
    let read = |row: Option<kiln_seams::records::Row>| row.and_then(|row| serde_json::from_slice::<Split>(&row.value).ok());
    if let Some(s) = read(r.get(SPLITS, &key, Consistency::Linearizable).await.map_err(|e| anyhow::anyhow!("{e}"))?) {
        return Ok(Some(s));
    }
    let Some(s) = kiln_ci::place::budget::split(d, period, owners) else { return Ok(None) };
    let tx = Tx { conds: vec![Cond::Absent { family: SPLITS.into(), key: key.clone() }], writes: vec![Write::Put { family: SPLITS.into(), key: key.clone(), value: serde_json::to_vec(&s)? }] };
    match r.commit(tx).await {
        Ok(_) => Ok(Some(s)),
        Err(Refusal::Conflict { .. }) => Ok(read(r.get(SPLITS, &key, Consistency::Linearizable).await.map_err(|e| anyhow::anyhow!("{e}"))?)),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// The directory's `budgets` (LB08): the person's declarations and the splits of each one's
/// period under way and the next, so a runner has the next before it starts. `None` with no budget
/// declared: the directory is as before.
pub async fn for_directory(cfg: &crate::config::Config, records: &dyn kiln_seams::Records, runners: &[RunnerInfo]) -> Option<serde_json::Value> {
    let decls = decls(&cfg.ci.budgets);
    if decls.is_empty() {
        return None;
    }
    let owners = owners(runners);
    let now = crate::util::now_ms();
    let mut splits = Vec::new();
    for d in &decls {
        let p = d.period(now);
        for period in [p, p.saturating_add(1)] {
            match split_of(records, d, period, &owners).await {
                Ok(Some(s)) => splits.push(s),
                Ok(None) => {}
                // Not kept, not sent: runners mint nothing from it until it is.
                Err(e) => tracing::warn!(tenant = %d.tenant, period, error = %format!("{e:#}"), "a budget's split couldn't be kept"),
            }
        }
    }
    serde_json::to_value(Published { decls, splits }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ci::Budget as Declared;

    fn runner(name: &str) -> RunnerInfo {
        RunnerInfo { name: name.into(), connected: true, reach: true, features: feature::all(), ..RunnerInfo::default() }
    }

    #[test]
    fn w05_r11_a_budget_reads_from_config_in_rights() -> anyhow::Result<()> {
        let b = Declared { per: "1d".into(), cpu: Some("2h".into()), memory: Some("1Gi*1h".into()), gpu: Some("30m".into()), require_timeout: true };
        let d = b.decl("acme/app").map_err(anyhow::Error::msg)?;
        anyhow::ensure!(d.per == 86_400_000, "{d:?}");
        anyhow::ensure!(d.limit == [Some(7_200_000 * 1000), Some(1024 * 3_600_000), None, Some(1_800_000)], "{d:?}");
        for (bad, why) in [
            (Declared { per: String::new(), ..b.clone() }, "period"),
            (Declared { cpu: None, memory: None, gpu: None, ..b.clone() }, "names no"),
            (Declared { cpu: Some("lots".into()), ..b.clone() }, "isn't a duration"),
            (Declared { memory: Some("1Gi".into()), ..b.clone() }, "size*duration"),
        ] {
            let e = bad.decl("acme/app").err().unwrap_or_default();
            anyhow::ensure!(e.contains(why), "{why}: {e}");
        }
        Ok(())
    }

    #[test]
    fn lb08_budget_words_ride_placements_channel_and_neither_reads_as_the_other() -> anyhow::Result<()> {
        let w = Word::Give { pot: PotKey { tenant: "acme".into(), per: 10, period: 1 }, total: [1, 0, 0, 0], free: [0; 4] };
        let json = serde_json::to_string(&Envelope { epoch: 2, budget: w.clone() })?;
        let back: Envelope = serde_json::from_str(&json)?;
        anyhow::ensure!(back.budget == w && back.epoch == 2);
        anyhow::ensure!(serde_json::from_str::<super::super::place::Envelope<kiln_ci::place::msg::ToRunner>>(&json).is_err(), "a placement word it isn't");
        let place = serde_json::to_string(&super::super::place::Envelope { epoch: 2, spec: None, ballot: None, members: Vec::new(), msg: kiln_ci::place::msg::ToRunner::Sync })?;
        let mut b = Budget::new("a", 1);
        anyhow::ensure!(b.word("b", &place, 0).is_none(), "a placement word goes on to placement");
        Ok(())
    }

    /// Two nodes: a run of a's held back for its budget asks b, b gives (its lowering among what's
    /// kept before its give goes), and a places the run.
    #[test]
    fn lb08_a_held_run_is_placed_once_a_peer_gives_it_rights() -> anyhow::Result<()> {
        let hour = 3_600_000u64;
        let d = Decl { tenant: "acme/app".into(), per: hour, limit: [Some(2000 * hour), None, None, None], require_timeout: true };
        let split = kiln_ci::place::budget::split(&d, 0, &["a".into(), "b".into()]).ok_or_else(|| anyhow::anyhow!("a split"))?;
        let published = serde_json::to_value(Published { decls: vec![d], splits: vec![split] })?;
        let runners = [runner("a"), runner("b")];
        let (mut a, mut b) = (Budget::new("a", 1), Budget::new("b", 1));
        let _ = a.declared(Some(&published), &runners, 0);
        let _ = b.declared(Some(&published), &runners, 0);
        let ask = Ask { run: "r1".into(), repo: "acme/app".into(), timeout: Some(hour), needs: kiln_ci::sched::needs::Needs { request: kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(2), ..Default::default() }, ..Default::default() }, ..Ask::default() };
        let (asks, outs) = a.gate(&ask, None, 0).ok_or_else(|| anyhow::anyhow!("held"))?;
        anyhow::ensure!(asks.is_empty());
        anyhow::ensure!(outs.iter().any(|o| matches!(o, Out::Local { run, placed: Placed::Answer(Answer::Queued { .. }) } if run == "r1")), "{outs:?}");
        let need = outs.iter().find_map(|o| if let Out::Tell { peer, json } = o { (peer == "b").then(|| json.clone()) } else { None }).ok_or_else(|| anyhow::anyhow!("a need to b: {outs:?}"))?;
        let (_, given) = b.word("a", &need, 1).ok_or_else(|| anyhow::anyhow!("b reads it"))?;
        let mut kept = BTreeMap::new();
        b.note(&mut kept);
        anyhow::ensure!(kept.keys().any(|(f, k)| *f == FAMILY && k.starts_with(b"p\0")), "the giver's pot is kept with its give");
        let give = given.iter().find_map(|o| if let Out::Say { peer, json } = o { (peer == "a").then(|| json.clone()) } else { None }).ok_or_else(|| anyhow::anyhow!("a give to a: {given:?}"))?;
        let (asks, _) = a.word("b", &give, 2).ok_or_else(|| anyhow::anyhow!("a reads it"))?;
        anyhow::ensure!(asks.iter().any(|x| x.run == "r1"), "placed once reserved");
        Ok(())
    }

    #[test]
    fn lb08_what_a_node_keeps_of_its_budgets_comes_back_with_its_held_asks() -> anyhow::Result<()> {
        let hour = 3_600_000u64;
        let d = Decl { tenant: "acme/app".into(), per: hour, limit: [Some(1000 * hour), None, None, None], require_timeout: true };
        let split = kiln_ci::place::budget::split(&d, 0, &["a".into()]).ok_or_else(|| anyhow::anyhow!("a split"))?;
        let published = serde_json::to_value(Published { decls: vec![d], splits: vec![split] })?;
        let mut a = Budget::new("a", 1);
        let _ = a.declared(Some(&published), &[runner("a")], 0);
        let ask = |run: &str| Ask { run: run.into(), repo: "acme/app".into(), timeout: Some(hour), needs: kiln_ci::sched::needs::Needs { request: kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(1), ..Default::default() }, ..Default::default() }, ..Ask::default() };
        anyhow::ensure!(a.gate(&ask("r1"), None, 0).is_none(), "reserved: placed at once");
        anyhow::ensure!(a.gate(&ask("r2"), None, 1).is_some_and(|(asks, _)| asks.is_empty()), "held");
        anyhow::ensure!(a.gate(&ask("r3"), None, 2).is_some_and(|(asks, _)| asks.is_empty()), "held");
        let mut kept = BTreeMap::new();
        a.note(&mut kept);
        let rows: Vec<Vec<u8>> = kept.into_values().flatten().collect();
        let mut back = Budget::new("a", 2);
        // r1's run was kept by the owner; r3 was held (its ask with it).
        let (_, (asks, _)) = back.restore("a", rows, &["r1".to_owned()].into_iter().collect(), 3);
        anyhow::ensure!(asks.is_empty() && back.core.reservation("r1").is_some() && back.core.holds_back("r2") && back.asks.contains_key("r3"));
        Ok(())
    }
}
