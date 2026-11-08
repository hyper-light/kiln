//! Sizes from history between a run's asker, the runner that does it, and the server (docs/reach.md,
//! "Sizes from history"; docs/needs.md E6; audit H1 to H10).
//!
//! - **The key** a run's history is kept under: its repository and its command as keyed
//!   (`kiln_ci::sizes::command_key`). The repository is the asking folder's tree, whether it's
//!   synced or not (audit H8): its `origin` as `owner/name`, else that tree on this machine
//!   (`local:<runner>:<path>`); a run with `--no-sync` in a folder of the other machine's is keyed
//!   by that folder (`remote:<path>`). A run with no folder isn't kept. The platform it ran on is
//!   the server's to say, from its record of the runner.
//! - **The asker never waits on the server** (audit H3): it places by the prediction it has for
//!   the key ([`cached`], from the server's answer to an earlier run), or by the ask as given when it
//!   has none, and asks the server again in the background for the next run. The server answers
//!   from one rolled-up row per platform, folded in as each run is kept ([`add`]), never the runs.
//! - **No admission here** (audit H2): history only predicts, and only once the person declared it
//!   (`[reach] sizes = "history"`); placement admits, and the placement core queues.
//! - **The doer** measures the run (`super::measure`) and tells the server what it used (ReachAsk
//!   `measured`), which keeps it under the run's key as that runner's, by the connection it came on.
//!   Only the runner's own code says `measured`: its local socket refuses it from an agent (audit
//!   H9, [`for_agent`]).
//! - **Older runners** (audit H1): a run goes with what its asker knows ([`super::wire::Ask::RunWith`])
//!   only to a runner whose directory entry names [`FEATURE`]; to any other it goes as
//!   `Ask::Run`, as before. Nothing new goes over the local socket: what a run used reaches its
//!   client as a note (`Ev::Note`), which every client reads.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use kiln_ci::sched::state::Resources;
use kiln_ci::sizes::{Kept, Policy, Rollup, Size, Usage};
use parking_lot::Mutex;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::directory::{self, Choice, RunnerInfo, Want};
use super::local::{Ev, Req, RunReq};
use super::node::{Handle, Says};
use super::wire::RunSpec;

/// What a runner names in its reach announcement (`Announce.features`, docs/wire.md "Named
/// features") when it takes [`super::wire::Ask::RunWith`] and says [`super::wire::Say::Used`].
/// Registered as `proto::feature::SIZES`, in `proto::feature::ALL`.
pub const FEATURE: &str = crate::proto::feature::SIZES;

/// How the note begins that says a run goes again at its ask (`[reach] retry_oom`): what came
/// before it was the try that was killed (`super::reuse` keeps only what comes after, audit H6).
pub const AGAIN: &str = "again:";

/// What this build names in its reach announcement.
pub fn features() -> Vec<String> {
    vec![FEATURE.to_owned()]
}

/// Whether `r` says it takes a run with what its asker knows.
pub fn takes(r: &RunnerInfo) -> bool {
    r.features.iter().any(|f| f == FEATURE)
}

/// A run's history key: its repository and its command, as keyed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Key {
    pub repo: String,
    pub command: String,
}

impl Key {
    /// The key of `req` asked on runner `me`; none for a run with no folder. Blocks (it asks git).
    pub fn of(me: &str, req: &RunReq) -> Option<Key> {
        if !req.sync
            && let Some(there) = req.remote_cwd.as_deref().filter(|c| !c.trim().is_empty())
        {
            return Some(Key { repo: format!("remote:{there}"), command: kiln_ci::sizes::command_key(None, &req.command) });
        }
        let cwd = req.cwd.as_deref().filter(|c| !c.trim().is_empty())?;
        let (root, _) = super::sync::find_root(Path::new(cwd));
        let canonical = std::fs::canonicalize(&root).ok();
        let origin = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["remote", "get-url", "origin"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok());
        let place = canonical.as_ref().unwrap_or(&root);
        let repo = origin.as_deref().and_then(kiln_ci::sizes::repo_of_remote).unwrap_or_else(|| format!("local:{me}:{}", place.display()));
        // The tree's top as the command may write it: as given, and as the system resolves it.
        let mut command = req.command.clone();
        for r in [Some(root.clone()), canonical].into_iter().flatten() {
            command = kiln_ci::sizes::command_key(Some(&r.to_string_lossy()), &command);
        }
        Some(Key { repo, command: kiln_ci::sizes::command_key(None, &command) })
    }

    /// Its hash: what the store keeps its runs under.
    pub fn hash(&self) -> String {
        crate::util::sha256_hex(format!("{}\0{}", self.repo, self.command).as_bytes())
    }
}

/// What a run's asker tells the runner doing it (`Ask::RunWith`'s JSON).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct With {
    /// Its history key; none: what it used isn't kept.
    pub key: Option<Key>,
    /// What it asked.
    pub ask: Resources,
    /// The size history gave it, when it did (none: placed by its ask as given).
    pub size: Option<Size>,
}

/// What the server says a key's runs say on each platform, what the person declared, and how long
/// the server keeps runs (ReachAsk `sizes`' answer).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sizes {
    pub policy: Policy,
    /// By platform (`linux/x86_64`).
    pub platforms: BTreeMap<String, Rollup>,
    /// How long the server keeps a run (`[retention] days`, in ms): an answer older than that says
    /// nothing the server still has.
    pub kept_ms: u64,
}

impl Sizes {
    /// The size history predicts on `platform` for a run asking `ask` (none: by the ask as given).
    pub fn predict(&self, platform: &str, ask: &Resources) -> Option<Size> {
        kiln_ci::sizes::predict(ask, self.platforms.get(platform), &self.policy)
    }
}

// ---------------------------------------------------------------- the asker's own record (H3)

/// The server's latest answer for a key, and when it came.
struct Known {
    sizes: Sizes,
    at: Instant,
}

fn known() -> &'static Mutex<HashMap<String, Known>> {
    static KNOWN: OnceLock<Mutex<HashMap<String, Known>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// What the server last said of the key hashed `hash`, if it has said.
pub fn cached(hash: &str) -> Option<Sizes> {
    known().lock().get(hash).map(|k| k.sizes.clone())
}

/// The server's answer for `hash`, kept; answers older than the server keeps runs go as it comes.
fn learn(hash: String, sizes: Sizes) {
    let now = Instant::now();
    let mut k = known().lock();
    k.retain(|_, v| u64::try_from(now.duration_since(v.at).as_millis()).unwrap_or(u64::MAX) < v.sizes.kept_ms);
    k.insert(hash, Known { sizes, at: now });
}

/// Ask the server what `key`'s runs say, in the background: what it answers places the next run.
/// A server from before refuses the question; nothing is kept then.
fn refresh(handle: &Handle, key: &Key) {
    let Ok(q) = serde_json::to_string(&super::Question::Sizes { repo: key.repo.clone(), command: key.command.clone() }) else { return };
    let (handle, hash) = (handle.clone(), key.hash());
    crate::rt::spawn(async move {
        let Ok(answer) = handle.query(q).await else { return };
        let Ok(v) = serde_json::from_str::<Value>(&answer) else { return };
        if let Some(e) = v.get("error") {
            tracing::debug!(error = %e, "reach: the server couldn't say what this command's history says; its runs go by their ask");
            return;
        }
        if let Ok(s) = serde_json::from_value::<Sizes>(v) {
            learn(hash, s);
        }
    });
}

/// The best runner for `want`, each platform's runners weighed at the size `size_of` gives the work
/// there (none: its ask as given), with that size; a size says why in the choice's why. With no size
/// anywhere, or no runner with room for one, it's [`directory::pick`] by the ask as given.
pub fn pick_sized(runners: &[RunnerInfo], want: &Want, me: &str, size_of: impl Fn(&str) -> Option<Size>) -> anyhow::Result<(Choice, Option<Size>)> {
    let mut by: BTreeMap<String, (Option<Size>, Vec<RunnerInfo>)> = BTreeMap::new();
    for r in runners {
        let p = r.platform();
        by.entry(p.clone()).or_insert_with(|| (size_of(&p), Vec::new())).1.push(r.clone());
    }
    if by.values().all(|(s, _)| s.is_none()) {
        return directory::pick(runners, want, me).map(|c| (c, None));
    }
    let mut ok: Vec<(Choice, Option<Size>)> = Vec::new();
    for (size, group) in by.into_values() {
        let w = match &size {
            Some(s) => Want { needs: kiln_ci::sched::needs::Needs { request: s.request(&want.needs.request), ..want.needs.clone() }, ..want.clone() },
            None => want.clone(),
        };
        for mut c in directory::rank(&group, &w, me).0 {
            if let Some(s) = &size {
                c.why = format!("{} · {}", c.why, s.why);
            }
            ok.push((c, size.clone()));
        }
    }
    // More room first, a runner that hasn't said its size last, as lb/sense's ranking has it.
    let room = |a: Option<f32>, b: Option<f32>| match (a, b) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    };
    ok.sort_by(|(a, _), (b, _)| room(a.score, b.score).then_with(|| a.runner.name.cmp(&b.runner.name)));
    match ok.into_iter().next() {
        Some(best) => Ok(best),
        None => directory::pick(runners, want, me).map(|c| (c, None)),
    }
}

// ---------------------------------------------------------------- one try at a run, and again

/// A run, try by try: its size, what it used, and whether it goes again (`[reach] retry_oom`).
#[derive(Clone, Default)]
pub struct Try(Arc<Mutex<Attempt>>);

#[derive(Default)]
struct Attempt {
    /// The size it goes again at, after a try killed for memory.
    again: Option<Size>,
    with: With,
    policy: Policy,
    used: Option<Usage>,
    retry: Option<Size>,
}

/// Run `req`, start to end, once more at its ask where it was killed for memory sized from history
/// below its ask and the person declared `[reach] retry_oom` (audit H10: never a detached run or
/// one fed input, [`retries`]).
pub async fn run(handle: &Handle, me: &str, req: RunReq, ev: &async_channel::Sender<Ev>, ctl: async_channel::Receiver<Req>) -> anyhow::Result<()> {
    let mut again = None;
    loop {
        let t = Try(Arc::new(Mutex::new(Attempt { again: again.take(), ..Attempt::default() })));
        super::local::run_try(handle, me, req.clone(), ev, ctl.clone(), &t).await?;
        let Some(size) = t.0.lock().retry.take() else { return Ok(()) };
        let _ = ev.send(Ev::Note(format!("{AGAIN} {}", size.why))).await;
        again = Some(size);
    }
}

/// Whether a run killed for memory may go again (audit H10). Not a detached one: its client went,
/// and its end is followed with nobody to run it again for, or to tell (opt-in automation says what
/// it did). Not one fed input: what it read is gone, and holding all of it to feed again would hold
/// without bound.
pub fn retries(req: &RunReq) -> bool {
    !req.detach && !req.stdin
}

impl Try {
    /// The runner for `req`, weighed at the size history predicts on each platform, or the size of
    /// a try again. Never waits on the server (audit H3): the prediction is the one this runner
    /// has, and the server is asked again for the next run.
    pub async fn pick(&self, handle: &Handle, me: &str, req: &RunReq) -> anyhow::Result<Choice> {
        let runners = handle.directory().await?;
        let want = Want::parse(&req.target)?;
        let ask = want.needs.request;
        let key = {
            let (me, req) = (me.to_owned(), req.clone());
            crate::rt::blocking(move || Key::of(&me, &req)).await.ok().flatten()
        };
        let sizes = key.as_ref().and_then(|k| cached(&k.hash()));
        if let Some(k) = &key {
            refresh(handle, k);
        }
        let again = self.0.lock().again.clone();
        let (choice, size) = pick_sized(&runners, &want, me, |platform| again.clone().or_else(|| sizes.as_ref().and_then(|s| s.predict(platform, &ask))))?;
        let mut a = self.0.lock();
        a.policy = sizes.map(|s| s.policy).unwrap_or_default();
        a.with = With { key, ask, size };
        Ok(choice)
    }

    /// Start `spec` on `runner`: with what this runner knows of it where `runner` takes that (its
    /// directory entry names [`FEATURE`], audit H1), else as before.
    pub async fn start(&self, handle: &Handle, runner: &RunnerInfo, spec: RunSpec, says: Says, custody: Vec<String>) -> anyhow::Result<u64> {
        let with = self.0.lock().with.clone();
        match serde_json::to_string(&with).ok().filter(|_| takes(runner) && with.key.is_some()) {
            Some(w) => handle.run_sized(&runner.name, spec, w, says, custody).await,
            None => handle.run_with(&runner.name, spec, says, custody).await,
        }
    }

    /// What the runner doing it said it used (`Say::Used`'s JSON).
    pub fn used(&self, json: &str) {
        self.0.lock().used = serde_json::from_str(json).ok();
    }

    /// It ended on this runner, having used `used`: kept under its key (in the background), said,
    /// then its exit, unless it goes again.
    pub async fn exit_here(&self, handle: &Handle, ev: &async_channel::Sender<Ev>, req: &RunReq, code: Option<i32>, error: Option<String>, used: Option<&Usage>) -> anyhow::Result<()> {
        self.0.lock().used = used.cloned();
        self.record(handle, used);
        self.exit(ev, req, false, code, error, 0).await
    }

    /// It ended (`stopped`: its client went or stopped it): what it used said, then its exit,
    /// unless it goes again.
    pub async fn exit(&self, ev: &async_channel::Sender<Ev>, req: &RunReq, stopped: bool, code: Option<i32>, error: Option<String>, lost: u64) -> anyhow::Result<()> {
        let (note, again) = {
            let mut a = self.0.lock();
            let killed = a.used.as_ref().and_then(Usage::killed);
            let retry = (!stopped && retries(req)).then(|| a.with.size.as_ref().and_then(|ran| kiln_ci::sizes::retry(&a.with.ask, ran, killed, &a.policy))).flatten();
            a.retry = retry;
            (a.used.as_ref().map(|u| format!("used {}", u.describe())), a.retry.is_some())
        };
        if let Some(n) = note {
            let _ = ev.send(Ev::Note(n)).await;
        }
        if !again {
            let _ = ev.send(Ev::Exit { code, error, lost }).await;
        }
        Ok(())
    }

    /// Tell the server what a run on this runner used (in the background), when it has a key.
    pub fn record(&self, handle: &Handle, used: Option<&Usage>) {
        let json = {
            let a = self.0.lock();
            match (&a.with.key, used) {
                (Some(key), Some(u)) => measured(key, &a.with, u),
                _ => return,
            }
        };
        let handle = handle.clone();
        crate::rt::spawn(async move {
            if let Err(e) = handle.query(json).await {
                tracing::debug!(error = %format!("{e:#}"), "reach: what a run used couldn't be kept");
            }
        });
    }
}

// ---------------------------------------------------------------- the runner doing it

/// What a run asked with `Ask::RunWith` carries on the runner doing it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doing {
    with: With,
    /// What it used, once it ended (`Say::Used`'s JSON).
    used: Option<String>,
}

/// What came with a run (`Ask::RunWith`'s `with`); none when it doesn't read (it runs unkept).
pub fn doing(with: &str) -> Option<Doing> {
    serde_json::from_str(with).ok().map(|with| Doing { with, used: None })
}

impl Doing {
    /// It ended having used `used`: kept to say to its asker, and the question that tells the
    /// server (none without a key, or nothing measured).
    pub fn ended(&mut self, used: Option<&Usage>) -> Option<String> {
        let u = used?;
        self.used = serde_json::to_string(u).ok();
        Some(measured(self.with.key.as_ref()?, &self.with, u))
    }

    /// What it used, to say before its exit.
    pub fn used(&self) -> Option<String> {
        self.used.clone()
    }
}

/// The question that tells the server what a run of `key` used (ReachAsk `measured`).
fn measured(key: &Key, with: &With, used: &Usage) -> String {
    let q = super::Question::Measured { repo: key.repo.clone(), command: key.command.clone(), ask: with.ask, size: with.size.clone().map(Box::new), used: Box::new(used.clone()) };
    serde_json::to_string(&q).unwrap_or_default()
}

// ---------------------------------------------------------------- an agent's questions (H9)

/// An agent's question for the server, through this runner's socket (`Req::Ask`): asked, unless
/// it's one only the runner's own code says (`measured`: what a run used), which is refused here.
pub async fn for_agent(handle: &Handle, json: String) -> anyhow::Result<String> {
    if let Some(why) = refused_from_agent(&json) {
        anyhow::bail!("{why}");
    }
    handle.query(json).await
}

/// Why an agent may not ask `json` of the server, when it may not: what a run used is the runner's
/// to say, as it measured it. Read both ways the server could (the question it parses, and the last
/// `op` a reader that keeps the last of a repeated name takes).
fn refused_from_agent(json: &str) -> Option<&'static str> {
    let parsed = matches!(serde_json::from_str::<super::Question>(json), Ok(super::Question::Measured { .. }));
    let named = serde_json::from_str::<Value>(json).ok().and_then(|v| v.get("op").and_then(Value::as_str).map(str::to_owned)).is_some_and(|op| op == "measured");
    (parsed || named).then_some("what a run used is its runner's to tell kiln, as it measured it, never an agent's (docs/reach.md, \"Sizes from history\")")
}

// ---------------------------------------------------------------- the server's side

/// How long the server keeps a run's use: the runs' own retention (`[retention] days`).
pub fn kept_ms(days: u32) -> u64 {
    u64::from(days).saturating_mul(86_400_000)
}

/// ReachAsk `sizes`: what the runs kept under (`repo`, `command`) say on each platform (one row
/// each), the person's `[reach]` policy, and how long runs are kept.
pub async fn answer(hub: &Arc<crate::hub::Hub>, repo: &str, command: &str) -> Value {
    let key = Key { repo: repo.to_owned(), command: command.to_owned() };
    match rollups(&hub.store, key.hash()).await {
        Ok(platforms) => json!(Sizes { policy: hub.cfg.reach.policy(), platforms, kept_ms: kept_ms(hub.cfg.retention.days) }),
        Err(e) => json!({"error": format!("what runs used couldn't be read: {e:#}")}),
    }
}

/// ReachAsk `measured` from runner `runner` (the connection it came on, never what it says): kept
/// under its key with the platform the server knows the runner by.
pub async fn keep(hub: &Arc<crate::hub::Hub>, runner: &str, repo: String, command: String, ask: Resources, size: Option<Size>, used: Usage) -> Value {
    let Some(platform) = hub.agents.lock().get(runner).filter(|a| !a.ephemeral).map(|a| format!("{}/{}", a.os, a.arch)) else {
        return json!({"error": format!("{runner} isn't a runner that does reach runs")});
    };
    let key = Key { repo, command };
    let row = Row { key: key.hash(), repo: key.repo, command: key.command, platform, runner: runner.to_owned(), ask, size, used, at_ms: crate::util::now_ms() };
    match add(&hub.store, row).await {
        Ok(()) => json!({}),
        Err(e) => json!({"error": format!("what the run used couldn't be kept: {e:#}")}),
    }
}

/// One run's use as the store keeps it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// [`Key::hash`].
    pub key: String,
    pub repo: String,
    pub command: String,
    pub platform: String,
    pub runner: String,
    pub ask: Resources,
    pub size: Option<Size>,
    pub used: Usage,
    /// When it was kept (ms since the epoch).
    pub at_ms: i64,
}

impl Row {
    fn kept(&self) -> Kept {
        Kept { at_ms: self.at_ms, sized_mem: self.size.as_ref().map_or(self.ask.mem, |s| s.mem), usage: self.used.clone() }
    }
}

/// `row` kept, and folded into its key's rollup on its platform, in one transaction: one row read
/// and one written beside the insert, whatever the key's runs (audit H3).
pub async fn add(store: &crate::store::Store, row: Row) -> anyhow::Result<()> {
    store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            tx.execute(
                "INSERT INTO run_sizes (key, repo, command, platform, runner, ask, size, used, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![row.key, row.repo, row.command, row.platform, row.runner, to_json(&row.ask), row.size.as_ref().map(to_json), to_json(&row.used), row.at_ms],
            )?;
            let mut r = rollup_of(&tx, &row.key, &row.platform)?.unwrap_or_default();
            r.add(&row.kept());
            put_rollup(&tx, &row.key, &row.platform, &r)?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The rollups kept for key hash `key`, by platform.
pub async fn rollups(store: &crate::store::Store, key: String) -> anyhow::Result<BTreeMap<String, Rollup>> {
    store
        .call(move |c| {
            let mut st = c.prepare("SELECT platform, rollup FROM run_size_rollups WHERE key = ?1")?;
            let rows = st.query_map([key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().filter_map(|(p, j)| serde_json::from_str(&j).ok().map(|r| (p, r))).collect())
        })
        .await
}

/// Runs older than `days` go with the runs' prune (`[retention] days`), and the rollups of the keys
/// they were under are folded again from what's left of those keys alone.
pub async fn prune(store: &crate::store::Store, days: u32) -> anyhow::Result<u64> {
    let cutoff = crate::util::now_ms().saturating_sub(i64::try_from(kept_ms(days)).unwrap_or(i64::MAX));
    store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let touched: Vec<(String, String)> = {
                let mut st = tx.prepare("SELECT DISTINCT key, platform FROM run_sizes WHERE at < ?1")?;
                st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
            };
            let gone = tx.execute("DELETE FROM run_sizes WHERE at < ?1", [cutoff])?;
            for (key, platform) in &touched {
                let left: Vec<Kept> = {
                    let mut st = tx.prepare("SELECT ask, size, used, at FROM run_sizes WHERE key = ?1 AND platform = ?2")?;
                    st.query_map([key, platform], |r| {
                        let ask: Resources = serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or_default();
                        let size: Option<Size> = r.get::<_, Option<String>>(1)?.and_then(|s| serde_json::from_str(&s).ok());
                        let used: Usage = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
                        Ok(Row { ask, size, used, at_ms: r.get(3)?, ..Row::default() }.kept())
                    })?
                    .collect::<Result<_, _>>()?
                };
                if left.is_empty() {
                    tx.execute("DELETE FROM run_size_rollups WHERE key = ?1 AND platform = ?2", [key, platform])?;
                } else {
                    put_rollup(&tx, key, platform, &Rollup::of(&left))?;
                }
            }
            tx.commit()?;
            Ok(u64::try_from(gone).unwrap_or(u64::MAX))
        })
        .await
}

fn rollup_of(c: &rusqlite::Connection, key: &str, platform: &str) -> rusqlite::Result<Option<Rollup>> {
    let j: Option<String> = c.query_row("SELECT rollup FROM run_size_rollups WHERE key = ?1 AND platform = ?2", [key, platform], |r| r.get(0)).optional()?;
    Ok(j.and_then(|j| serde_json::from_str(&j).ok()))
}

fn put_rollup(c: &rusqlite::Connection, key: &str, platform: &str, r: &Rollup) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO run_size_rollups (key, platform, rollup) VALUES (?1, ?2, ?3) ON CONFLICT (key, platform) DO UPDATE SET rollup = excluded.rollup",
        rusqlite::params![key, platform, serde_json::to_string(r).unwrap_or_default()],
    )?;
    Ok(())
}

/// A value as the JSON the store keeps it as.
fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::sizes::{By, Counted, How, Source};

    const GIB: u64 = 1 << 30;

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().is_ok_and(|o| o.status.success())
    }

    /// E6: two worktrees of one repository give one key; the repository is origin's owner/name.
    #[test]
    fn e6_two_worktrees_of_one_repository_give_one_key() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let (a, b) = (base.path().join("wt-a"), base.path().join("wt-b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d)?;
            if !git(d, &["init", "-q"]) {
                eprintln!("skipped: no git here");
                return Ok(());
            }
            git(d, &["remote", "add", "origin", "git@github.com:hyper-light/kiln.git"]);
        }
        let req = |d: &Path, c: String| RunReq { cwd: Some(d.display().to_string()), command: c, sync: true, ..RunReq::default() };
        let ka = Key::of("mac", &req(&a, format!("cd {} && cargo  test -p kiln", a.display()))).ok_or_else(|| anyhow::anyhow!("no key"))?;
        let kb = Key::of("mac", &req(&b, format!("cd {} && cargo test -p kiln", b.display()))).ok_or_else(|| anyhow::anyhow!("no key"))?;
        anyhow::ensure!(ka == kb && ka.hash() == kb.hash(), "{ka:?} {kb:?}");
        anyhow::ensure!(ka.repo == "hyper-light/kiln" && ka.command == "cd . && cargo test -p kiln", "{ka:?}");
        anyhow::ensure!(Key::of("mac", &RunReq { command: "uptime".into(), ..RunReq::default() }).is_none(), "a run with no folder isn't kept");
        Ok(())
    }

    /// H8: with sync off, the same command asked from two repositories is two keys: the asking
    /// folder's tree keys it whether it's synced or not, and a folder on the other machine keys a
    /// run there.
    #[test]
    fn h8_with_sync_off_one_command_in_two_repositories_is_two_keys() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let (a, b, c) = (base.path().join("app"), base.path().join("lib"), base.path().join("plain"));
        for (d, origin) in [(&a, Some("git@github.com:acme/app.git")), (&b, Some("https://github.com/acme/lib")), (&c, None)] {
            std::fs::create_dir_all(d)?;
            if !git(d, &["init", "-q"]) {
                eprintln!("skipped: no git here");
                return Ok(());
            }
            if let Some(o) = origin {
                git(d, &["remote", "add", "origin", o]);
            }
        }
        let unsynced = |d: &Path| RunReq { cwd: Some(d.display().to_string()), command: "cargo test".into(), sync: false, ..RunReq::default() };
        let keys: Vec<Key> = [&a, &b, &c].iter().filter_map(|d| Key::of("mac", &unsynced(d))).collect();
        anyhow::ensure!(keys.len() == 3, "{keys:?}");
        let repos: Vec<&str> = keys.iter().map(|k| k.repo.as_str()).collect();
        anyhow::ensure!(repos.first() == Some(&"acme/app") && repos.get(1) == Some(&"acme/lib"), "{repos:?}");
        anyhow::ensure!(repos.get(2).is_some_and(|r| r.starts_with("local:mac:") && r.ends_with("plain")), "{repos:?}");
        let hashes: std::collections::BTreeSet<String> = keys.iter().map(Key::hash).collect();
        anyhow::ensure!(hashes.len() == 3, "one command in three repositories shares a history: {keys:?}");
        // In a folder of the other machine's, copying nothing: that folder keys it.
        let there = Key::of("mac", &RunReq { remote_cwd: Some("/srv/app".into()), ..unsynced(&a) }).ok_or_else(|| anyhow::anyhow!("no key"))?;
        anyhow::ensure!(there.repo == "remote:/srv/app" && there.hash() != keys.first().map(Key::hash).unwrap_or_default(), "{there:?}");
        Ok(())
    }

    #[test]
    fn e6_what_comes_with_a_run_reads_back() -> anyhow::Result<()> {
        let with = With {
            key: Some(Key { repo: "hyper-light/kiln".into(), command: "ci/gate.sh".into() }),
            ask: Resources { cpu: kiln_ci::sched::state::Cpus::whole(12), mem: 40 * GIB, ..Resources::default() },
            size: Some(Size { cpu: kiln_ci::sched::state::Cpus::whole(3), mem: 6 * GIB, by: By::History, why: "w".into() }),
        };
        let back: With = serde_json::from_str(&serde_json::to_string(&with)?)?;
        anyhow::ensure!(back == with);
        // From a newer asker, with more than this build knows: what it knows is read.
        let newer: With = serde_json::from_str(r#"{"key": {"repo": "r", "command": "c", "branch": "x"}, "ask": {"cpu": 1, "mem": 2, "disk": 0, "gpu": 0}, "size": {"mem": 2, "by": "ask"}, "later": true}"#)?;
        anyhow::ensure!(newer.key.map(|k| k.command) == Some("c".into()) && newer.size.map(|s| s.mem) == Some(2));
        Ok(())
    }

    /// H9: an agent can't tell the server what a run used through its runner's socket, however it
    /// spells it; every other question goes through.
    #[test]
    fn h9_an_agent_cant_say_what_a_run_used() {
        let forged = [
            r#"{"op": "measured", "repo": "acme/app", "command": "make", "used": {"peak_mem": 1}}"#,
            r#"{"op":"measured"}"#,
            r#"{"op": "call", "op": "measured", "used": {}}"#,
            r#"{"used": {"peak_mem": 1}, "op": "measured", "session": "s"}"#,
        ];
        for j in forged {
            assert!(refused_from_agent(j).is_some(), "{j}");
        }
        for j in [r#"{"op": "call", "name": "runners", "input": {}}"#, r#"{"op": "sizes", "repo": "r", "command": "c"}"#, r#"{"op": "directory"}"#, "not json"] {
            assert!(refused_from_agent(j).is_none(), "{j}");
        }
    }

    /// H10: a detached run, or one fed input, never goes again; any other may.
    #[test]
    fn h10_retry_oom_never_applies_to_a_detached_run_or_one_fed_input() {
        let req = RunReq { command: "make".into(), ..RunReq::default() };
        assert!(retries(&req));
        assert!(!retries(&RunReq { detach: true, ..req.clone() }));
        assert!(!retries(&RunReq { stdin: true, ..req }));
    }

    /// H1: what reaches the local socket for a run is what runners from before read (no request of
    /// its own: `Req` gained none, its run requests are where they were), and what a run used goes
    /// back as a note, which every client since notes reads.
    #[test]
    fn h1_a_runs_local_requests_and_its_use_are_what_older_runners_and_clients_read() -> anyhow::Result<()> {
        let first = |b: Vec<u8>| b.first().copied();
        let req = RunReq { command: "true".into(), ..RunReq::default() };
        anyhow::ensure!(first(postcard::to_allocvec(&Req::Run(Box::new(req.clone())))?) == Some(2));
        anyhow::ensure!(first(postcard::to_allocvec(&Req::RunReuse(Box::new(req)))?) == Some(9));
        anyhow::ensure!(first(postcard::to_allocvec(&Ev::Note("used 1 GiB at most".into()))?) == Some(11));
        Ok(())
    }

    fn row(key: &str, platform: &str, at: i64, peak: u64) -> Row {
        let used = Usage { peak_mem: peak, mem_how: How::Exact, cpu_us: 2_000_000, cpu_how: How::Exact, duration_ms: 1000, exit: Some(0), oom_kills: 0, oom_counted: Counted::Group };
        Row { key: key.into(), repo: "acme/app".into(), command: "make".into(), platform: platform.into(), runner: "linux".into(), ask: Resources { mem: 8 * GIB, ..Resources::default() }, size: None, used, at_ms: at }
    }

    /// H3: each run kept folds into its key's rollup on its platform as it's kept, so what the
    /// server answers is one row per platform; the prune folds what's left of the keys it touched.
    #[test]
    fn h3_runs_fold_into_a_rollup_as_theyre_kept_and_the_prune_folds_whats_left() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let s = crate::store::Store::in_memory()?;
            let now = crate::util::now_ms();
            let old = now - i64::try_from(kept_ms(31)).unwrap_or(0);
            add(&s, row("k", "linux/x86_64", old, 9 * GIB)).await?;
            add(&s, row("k", "linux/x86_64", now, 2 * GIB)).await?;
            add(&s, row("k", "macos/aarch64", now, 3 * GIB)).await?;
            add(&s, row("other", "linux/x86_64", old, GIB)).await?;
            let r = rollups(&s, "k".into()).await?;
            let linux = r.get("linux/x86_64").cloned().unwrap_or_default();
            anyhow::ensure!(r.len() == 2 && linux.runs == 2 && linux.peak == 9 * GIB && linux.mean_milli() == Some(2000), "{r:?}");
            anyhow::ensure!(r.get("macos/aarch64").is_some_and(|m| m.runs == 1 && m.peak == 3 * GIB), "{r:?}");
            // 30 days kept: the 31-day-old runs go, and the rollups say only what's left.
            anyhow::ensure!(prune(&s, 30).await? == 2);
            let r = rollups(&s, "k".into()).await?;
            anyhow::ensure!(r.get("linux/x86_64").is_some_and(|l| l.runs == 1 && l.peak == 2 * GIB), "{r:?}");
            anyhow::ensure!(rollups(&s, "other".into()).await?.is_empty(), "a key with nothing left has no rollup");
            Ok(())
        })
    }

    fn runner(name: &str, os: &str, mem_gb: u64) -> RunnerInfo {
        RunnerInfo { name: name.into(), os: os.into(), arch: "x86_64".into(), connected: true, reach: true, cpus: 16, mem_total: mem_gb << 30, mem_used: 0, ..RunnerInfo::default() }
    }

    /// H2, H3: with nothing predicted the pick is by the ask as given; a prediction on a platform
    /// weighs its runners at that size (so a smaller machine there suits), says so, and leaves the
    /// other platforms' at the ask.
    #[test]
    fn h2_a_prediction_sizes_its_platforms_runners_and_nothing_else() -> anyhow::Result<()> {
        let runners = [runner("small", "linux", 8), runner("win", "windows", 64)];
        let want = Want::parse("mem=32g")?;
        let (c, size) = pick_sized(&runners, &want, "elsewhere", |_| None)?;
        anyhow::ensure!(c.runner.name == "win" && size.is_none(), "{c:?}");
        let sizes = Sizes { policy: Policy { sizes: Source::History, retry_oom: false }, platforms: BTreeMap::from([("linux/x86_64".to_owned(), Rollup::of(&[row("k", "linux/x86_64", 1, 4 * GIB).kept()]))]), kept_ms: kept_ms(30) };
        let ask = want.needs.request;
        let (c, size) = pick_sized(&runners, &want, "elsewhere", |p| sizes.predict(p, &ask))?;
        anyhow::ensure!((c.runner.os == "linux") == size.as_ref().is_some_and(|s| s.mem == 4 * GIB && s.by == By::History), "the size goes with its platform's runner: {c:?} {size:?}");
        let small = pick_sized(&[runner("small", "linux", 8)], &want, "elsewhere", |p| sizes.predict(p, &ask))?;
        anyhow::ensure!(small.0.why.contains("sized from history: 4 GiB"), "{small:?}");
        // Undeclared: the same history predicts nothing, and an 8 GiB machine doesn't suit 32 GiB.
        let off = Sizes { policy: Policy::default(), ..sizes };
        anyhow::ensure!(pick_sized(&[runner("small", "linux", 8)], &want, "elsewhere", |p| off.predict(p, &ask)).is_err());
        Ok(())
    }

    /// H3: what the server said of a key is kept for the next run, and answers older than the
    /// server keeps runs go as new ones come.
    #[test]
    fn h3_the_servers_answer_is_kept_for_the_next_run() {
        let k = format!("test-{}", uuid::Uuid::now_v7().simple());
        assert_eq!(cached(&k), None);
        let s = Sizes { kept_ms: kept_ms(30), ..Sizes::default() };
        learn(k.clone(), s.clone());
        assert_eq!(cached(&k), Some(s));
        let gone = format!("test-{}", uuid::Uuid::now_v7().simple());
        learn(gone.clone(), Sizes { kept_ms: 0, ..Sizes::default() });
        learn(format!("test-{}", uuid::Uuid::now_v7().simple()), Sizes::default());
        assert_eq!(cached(&gone), None, "an answer the server's retention passed went");
        assert!(cached(&k).is_some());
    }
}
