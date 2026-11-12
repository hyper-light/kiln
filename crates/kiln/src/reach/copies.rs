//! The server's side of keeping the copies of runners' trees other runners hold (docs/reach.md
//! §5.1): the rules people set for every runner, one runner, a repository's copies and an agent's
//! (in kiln's store, under `reach/keep/`), what each runner last said it holds and let go (kept
//! beside the server's data, so a runner that's away still shows), asking a runner to let go now,
//! and what the System page, a repository and an agent's settings show of it. What goes and why is
//! the runners' to decide, with [`super::keep`]; what the server shows of it ahead is the same
//! function weighing what the runner last said.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::keep::{self, Answer, Look, Report, Rules, Set, TreeCopy};
use crate::hub::Hub;

/// Where the rules are kept in kiln's store.
const PREFIX: &str = "reach/keep/";

/// Whose rules: every runner's, one runner's, a repository's copies' or an agent's.
#[derive(Debug, Clone, Copy)]
pub enum Whose<'a> {
    System,
    Runner(&'a str),
    Repo(&'a str),
    Agent(&'a str),
}

impl Whose<'_> {
    fn key(&self) -> String {
        match self {
            Self::System => format!("{PREFIX}system"),
            Self::Runner(n) => format!("{PREFIX}runner/{n}"),
            Self::Repo(k) => format!("{PREFIX}repo/{k}"),
            Self::Agent(a) => format!("{PREFIX}agent/{a}"),
        }
    }

    /// As the audit log says it.
    pub fn said(&self) -> String {
        match self {
            Self::System => "every runner".into(),
            Self::Runner(n) => format!("runner {n}"),
            Self::Repo(k) => format!("repository {k}"),
            Self::Agent(a) => format!("agent {a}"),
        }
    }
}

/// Every scope's rules, as people set them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct All {
    pub system: Rules,
    pub runners: BTreeMap<String, Rules>,
    pub repos: BTreeMap<String, Rules>,
    pub agents: BTreeMap<String, Rules>,
}

impl All {
    /// What runner `name` weighs its copies with: every runner's rules, its own, every
    /// repository's, and those of the agents its copies were used by.
    pub fn set_for(&self, name: &str, copies: &[TreeCopy]) -> Set {
        let agents: BTreeSet<&str> = copies.iter().flat_map(|c| c.agents.iter().map(String::as_str)).collect();
        Set {
            system: self.system.clone(),
            runner: self.runners.get(name).cloned().unwrap_or_default(),
            repos: self.repos.clone(),
            agents: self.agents.iter().filter(|(a, _)| agents.contains(a.as_str())).map(|(a, r)| (a.clone(), r.clone())).collect(),
        }
    }

    fn put(&mut self, whose: Whose<'_>, rules: Option<Rules>) {
        let place = |m: &mut BTreeMap<String, Rules>, k: &str, r: Option<Rules>| match r {
            Some(r) => {
                m.insert(k.to_owned(), r);
            }
            None => {
                m.remove(k);
            }
        };
        match whose {
            Whose::System => self.system = rules.unwrap_or_default(),
            Whose::Runner(n) => place(&mut self.runners, n, rules),
            Whose::Repo(k) => place(&mut self.repos, k, rules),
            Whose::Agent(a) => place(&mut self.agents, a, rules),
        }
    }
}

/// What a runner last said, and when the server heard it (unix seconds). Shared: what's weighed off the shards, and what
/// a save writes there, hold it as it is ([`Copies::heard`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Heard {
    report: Arc<Report>,
    at: i64,
}

/// Whether reports `a` and `b` say the same but for what changes all the time (a runner's clock, the room on its disk):
/// every other field compared, so a field added to a report is weighed here or said not to be.
fn same_but_clock(a: &Report, b: &Report) -> bool {
    let Report { at: _, base, disk: _, copies, gone, done, error } = a;
    *base == b.base && *copies == b.copies && *gone == b.gone && *done == b.done && *error == b.error
}

/// A copy as a view shows it: what the rules say of it now, and where it is.
#[derive(Serialize)]
struct Shown<'a> {
    #[serde(flatten)]
    copy: &'a TreeCopy,
    kept: bool,
    why: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner: Option<&'a str>,
}

pub struct Copies {
    file: PathBuf,
    /// The rules, read from the store once and kept as they change.
    rules: Mutex<Option<All>>,
    heard: Mutex<BTreeMap<String, Heard>>,
    /// Each runner's last ask to let go now.
    asked: Mutex<BTreeMap<String, u64>>,
}

impl Copies {
    /// What runners said before, kept in `file`.
    pub fn open(file: PathBuf) -> Self {
        let heard = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { file, rules: Mutex::new(None), heard: Mutex::new(heard), asked: Mutex::new(BTreeMap::new()) }
    }

    /// Every scope's rules.
    pub async fn all(&self, hub: &Hub) -> All {
        if let Some(all) = self.rules.lock().clone() {
            return all;
        }
        let mut all = All::default();
        for (k, v) in hub.store.kv_prefix(PREFIX.to_owned()).await.unwrap_or_default() {
            let Ok(rules) = serde_json::from_str::<Rules>(&v) else { continue };
            match k.strip_prefix(PREFIX).and_then(|rest| rest.split_once('/').or(Some((rest, "")))) {
                Some(("system", "")) => all.system = rules,
                Some(("runner", n)) if !n.is_empty() => all.put(Whose::Runner(n), Some(rules)),
                Some(("repo", r)) if !r.is_empty() => all.put(Whose::Repo(r), Some(rules)),
                Some(("agent", a)) if !a.is_empty() => all.put(Whose::Agent(a), Some(rules)),
                _ => {}
            }
        }
        *self.rules.lock() = Some(all.clone());
        all
    }

    /// Set `whose` rules (none, or none set: the scope above's apply).
    pub async fn set(&self, hub: &Hub, whose: Whose<'_>, rules: Option<Rules>) -> anyhow::Result<()> {
        let rules = rules.filter(|r| !r.is_empty());
        if let Some(r) = &rules {
            r.check().map_err(anyhow::Error::msg)?;
            hub.store.kv_set(whose.key(), serde_json::to_string(r)?).await?;
        } else {
            hub.store.kv_delete(whose.key()).await?;
        }
        let mut all = self.all(hub).await;
        all.put(whose, rules);
        *self.rules.lock() = Some(all);
        // Each runner hears what its rules are now, at once (goal 9).
        self.tell(hub, None).await;
        Ok(())
    }

    /// Tell `runners` (none: every runner kiln knows here) their rules and last ask as their next
    /// report's answer would ([`Copies::heard`]): their keepers look at once, not at their next
    /// report (`ServerMsg::Copies`, goal 9). Only to runner processes that named
    /// [`crate::proto::feature::KEEP`]; the rest have them at their next report, as before.
    pub async fn tell(&self, hub: &Hub, runners: Option<&[String]>) {
        let names: Vec<String> = match runners {
            Some(r) => r.to_vec(),
            None => self.runners(hub).into_keys().collect(),
        };
        let all = self.all(hub).await;
        let speaking = hub.instances.load().speaking(crate::proto::feature::KEEP);
        for name in names {
            let peers: Vec<crate::instances::PeerId> = hub.instances.load().of(&name).into_iter().filter(|p| speaking.contains(p)).collect();
            if peers.is_empty() {
                continue;
            }
            let report = self.heard.lock().get(&name).map(|h| Arc::clone(&h.report)).unwrap_or_default();
            let answer = Answer { set: all.set_for(&name, &report.copies), asked: self.asked.lock().get(&name).copied().unwrap_or(0) };
            let Ok(json) = serde_json::to_string(&answer) else { continue };
            let msg = crate::proto::ServerMsg::Copies { json };
            for p in peers {
                hub.send_peer(p, &msg).await;
            }
        }
    }

    /// An agent closed for good: its rules go.
    pub async fn forget_agent(&self, hub: &Hub, id: &str) {
        if self.all(hub).await.agents.contains_key(id)
            && let Err(e) = self.set(hub, Whose::Agent(id), None).await
        {
            tracing::warn!(target: "kiln.reach", agent = %id, error = %format!("{e:#}"), "an agent's rules for its copies couldn't be let go");
        }
    }

    /// Runner `runner`'s report of its copies (none: it sent none): kept, and answered with the rules it's to weigh them
    /// with and the last ask to let go now, as the answer's JSON. It's weighed on the compute pool (the rules for its
    /// copies, whether it says anything new, the answer written), and the report it replaces let go of there: a runner
    /// holds many copies (goal 9, plan 01a1a87b2cec step 3: read on the hub's shard from a JSON tree of it, copied whole to
    /// be read and twice more to be compared, a report held the shard 0.55 ms on the CPU in probe 19's Linux rounds).
    pub async fn heard(&self, hub: &Hub, runner: &str, report: Option<Report>) -> String {
        let Some(report) = report else {
            return json!({ "error": "a report of copies was expected" }).to_string();
        };
        let all = self.all(hub).await;
        let before = self.heard.lock().get(runner).map(|h| Arc::clone(&h.report));
        let asked = self.asked.lock().get(runner).copied().unwrap_or(0);
        let name = runner.to_owned();
        let weighed = crate::rt::compute(move || {
            let answer = serde_json::to_string(&Answer { set: all.set_for(&name, &report.copies), asked }).unwrap_or_default();
            // What changes all the time (its clock, the room on its disk) is kept, but no change.
            let same = before.is_some_and(|b| same_but_clock(&b, &report));
            (Arc::new(report), same, answer)
        })
        .await;
        let Ok((report, same, answer)) = weighed else {
            return json!({ "error": "the report of copies couldn't be weighed" }).to_string();
        };
        let replaced = self.heard.lock().insert(runner.to_owned(), Heard { report, at: crate::util::now_unix() });
        let _ = crate::rt::compute(move || drop(replaced)).await;
        if !same {
            self.save().await;
            hub.emit(&crate::wire::events::ServerEvent::Keep);
        }
        answer
    }

    /// Ask `runner` (or every runner kiln knows) to let go now of what its rules don't keep: the
    /// runners asked.
    pub fn ask(&self, hub: &Hub, runner: Option<&str>) -> Vec<String> {
        let known = self.runners(hub);
        let names: Vec<String> = known.into_keys().filter(|n| runner.is_none_or(|r| r == n)).collect();
        let token = u64::try_from(crate::util::now_ms()).unwrap_or(1).max(1);
        let mut asked = self.asked.lock();
        for n in &names {
            asked.insert(n.clone(), token);
        }
        names
    }

    /// Every runner kiln knows here, and whether it's connected: those that joined (job pods
    /// aside), and those that said what they hold.
    fn runners(&self, hub: &Hub) -> BTreeMap<String, bool> {
        let mut out: BTreeMap<String, bool> = hub.agents.load().values().filter(|a| !a.ephemeral).map(|a| (a.name.clone(), a.connected)).collect();
        for n in self.heard.lock().keys() {
            out.entry(n.clone()).or_insert(false);
        }
        out
    }

    /// The System page's: every runner's rules and each repository's, and every runner with its
    /// own, its disk, its copies (each with what the rules say of it now), what it let go, when a
    /// schedule next comes round, and what letting go now would let go.
    pub async fn view(&self, hub: &Hub) -> Value {
        let all = self.all(hub).await;
        let now = crate::util::now_unix();
        let heard = self.heard.lock().clone();
        let asked = self.asked.lock().clone();
        let mut runners = self.runners(hub);
        for n in all.runners.keys() {
            runners.entry(n.clone()).or_insert(false);
        }
        let shown: Vec<Value> = runners
            .iter()
            .map(|(name, connected)| {
                let h = heard.get(name);
                let empty = Report::default();
                let report = h.map_or(&empty, |h| &*h.report);
                let set = all.set_for(name, &report.copies);
                let look = Look { now, since: now, disk: report.disk, asked: false, copies: &report.copies };
                let verdicts = keep::verdicts(&set, &look);
                let would = keep::plan(&set, &Look { asked: true, ..look });
                let bytes: u64 = would.iter().filter_map(|s| report.copies.iter().find(|c| c.from == s.from && c.key == s.key).and_then(|c| c.bytes)).sum();
                let copies: Vec<Shown> = report.copies.iter().zip(&verdicts).map(|(c, v)| Shown { copy: c, kept: v.kept, why: &v.why, runner: None }).collect();
                json!({
                    "name": name,
                    "connected": connected,
                    "rules": all.runners.get(name).cloned().unwrap_or_default(),
                    "heard": h.map(|h| h.at),
                    "base": h.map(|h| h.report.base.clone()),
                    "disk": report.disk,
                    "copies": copies,
                    "gone": report.gone,
                    "next": keep::next(&set, &report.copies, report.disk, now),
                    "asked": asked.get(name).copied().unwrap_or(0),
                    "done": report.done,
                    "now": { "copies": would.len(), "bytes": bytes },
                    "error": report.error,
                })
            })
            .collect();
        json!({ "now": now, "rules": all.system, "repos": all.repos, "runners": shown })
    }

    /// An agent's settings': its own rules, and its copies on every runner with what the rules say
    /// of each now.
    pub async fn agent(&self, hub: &Hub, id: &str) -> Value {
        let all = self.all(hub).await;
        let now = crate::util::now_unix();
        let heard = self.heard.lock().clone();
        let mut copies = Vec::new();
        for (runner, h) in &heard {
            let set = all.set_for(runner, &h.report.copies);
            let verdicts = keep::verdicts(&set, &Look { now, since: now, disk: h.report.disk, asked: false, copies: &h.report.copies });
            for (c, v) in h.report.copies.iter().zip(&verdicts).filter(|(c, _)| c.agent() == Some(id)) {
                copies.push(serde_json::to_value(Shown { copy: c, kept: v.kept, why: &v.why, runner: Some(runner) }).unwrap_or_default());
            }
        }
        json!({ "rules": all.agents.get(id).cloned().unwrap_or_default(), "copies": copies })
    }

    /// What every runner last said, written to its file: written out with the file's write, off the shards (what each said
    /// is shared, not copied, to get there).
    async fn save(&self) {
        let heard = self.heard.lock().clone();
        let file = self.file.clone();
        if let Ok(Err(e)) = crate::rt::blocking(move || crate::sync::group::save_atomic(&file, &serde_json::to_vec(&heard).unwrap_or_default())).await {
            tracing::warn!(target: "kiln.reach", error = %e, "what runners said of their copies couldn't be saved");
        }
    }
}

/// The rules a request's body says: an object of [`keep::FIELDS`], each within what kiln takes.
pub fn rules_of(body: &[u8]) -> Result<Rules, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("the body isn't rules: {e}"))?;
    let Value::Object(fields) = &v else { return Err("the body isn't rules: an object of them is".into()) };
    if let Some(k) = fields.keys().find(|k| !keep::FIELDS.contains(&k.as_str())) {
        return Err(format!("`{k}` isn't a rule; the rules are {}", keep::FIELDS.join(", ")));
    }
    let rules: Rules = serde_json::from_value(v).map_err(|e| format!("the body isn't rules: {e}"))?;
    rules.check()?;
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_weighs_every_runners_rules_its_own_and_its_agents() {
        let all = All {
            system: Rules { newest: Some(5), ..Rules::default() },
            runners: BTreeMap::from([("fedora".into(), Rules { hours: Some(24), ..Rules::default() })]),
            repos: BTreeMap::from([("github.com/a/b".into(), Rules { newest: Some(1), ..Rules::default() })]),
            agents: BTreeMap::from([("a1".into(), Rules { newest: Some(9), ..Rules::default() }), ("a2".into(), Rules::default())]),
        };
        let copies = vec![TreeCopy { agents: vec!["a1".into()], ..TreeCopy::default() }];
        let set = all.set_for("fedora", &copies);
        assert_eq!(set.runner.hours, Some(24));
        assert_eq!(set.system.newest, Some(5));
        assert!(set.repos.contains_key("github.com/a/b"));
        assert_eq!(set.agents.keys().collect::<Vec<_>>(), ["a1"], "only the agents its copies were used by");
        assert_eq!(all.set_for("mac", &[]).runner, Rules::default());
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a runner's report of its copies comes read with its question (no JSON tree of it to
    /// read again on the hub's shard), and is weighed on the compute pool: the answer it's given and whether it says
    /// anything new, the report it replaces let go of there too. One that changes only its clock and disk is kept as said,
    /// and is no change.
    #[test]
    fn g9_s3_a_report_of_copies_is_read_with_its_question_and_weighed_off_the_shard() -> anyhow::Result<()> {
        let q: crate::reach::Question = serde_json::from_str(r#"{"op": "keep", "report": {"at": 5, "base": "/c", "copies": [{"from": "mac", "key": "k1", "agents": ["a1"]}]}}"#)?;
        let crate::reach::Question::Keep { report: Some(report) } = q else { anyhow::bail!("a keep question with its report read: {q:?}") };
        anyhow::ensure!(report.copies.first().is_some_and(|c| c.key == "k1" && c.agents == ["a1"]), "{report:?}");
        anyhow::ensure!(same_but_clock(&report, &Report { at: 9, disk: Some(keep::Disk { total: 10, free: 4 }), ..report.clone() }), "its clock and disk are no change");
        anyhow::ensure!(!same_but_clock(&report, &Report { done: 3, ..report.clone() }), "what it carried out is");
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            let asked = crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get);
            let answer: Value = serde_json::from_str(&hub.copies.heard(&hub, "fedora", Some(report.clone())).await)?;
            anyhow::ensure!(crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get) > asked, "weighed on the compute pool");
            anyhow::ensure!(answer.get("asked") == Some(&json!(0)) && answer.get("set").is_some_and(Value::is_object), "{answer}");
            let later = Report { at: 9, ..report.clone() };
            hub.copies.heard(&hub, "fedora", Some(later)).await;
            anyhow::ensure!(hub.copies.heard.lock().get("fedora").is_some_and(|h| h.report.at == 9), "kept as said");
            let none: Value = serde_json::from_str(&hub.copies.heard(&hub, "fedora", None).await)?;
            anyhow::ensure!(none.get("error").is_some(), "{none}");
            Ok(())
        })
    }

    #[test]
    fn rules_in_a_body_are_checked_field_by_field() {
        assert_eq!(rules_of(br#"{"newest": 3, "schedule": "0 3 * * *"}"#).ok().and_then(|r| r.newest), Some(3));
        assert!(rules_of(b"{}").is_ok_and(|r| r.is_empty()));
        assert!(rules_of(br#"{"newset": 3}"#).is_err_and(|e| e.contains("`newset` isn't a rule")));
        assert!(rules_of(br#"{"schedule": "daily"}"#).is_err());
        assert!(rules_of(br#"[1]"#).is_err());
        assert!(rules_of(br#"{"free_percent": "ten"}"#).is_err());
    }
}
