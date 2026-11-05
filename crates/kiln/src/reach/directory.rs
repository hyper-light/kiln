//! Which runners can be reached, and which one suits a piece of work (`docs/reach.md` §2).
//!
//! The server knows every runner: what it is (OS, architecture, labels, cores, memory), how busy it
//! is (its latest sample, its CI slots) and its reach identity. A runner asks it for this list and
//! picks from it itself, so a choice never waits on the server once the list is in hand.

use kiln_ci::facts::RunnerFacts;
use kiln_ci::sched::needs::{self, Needs};
use kiln_ci::sched::routing::{self, Constraint, Lit, Op};
use serde::{Deserialize, Serialize};

/// A runner as reach sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunnerInfo {
    pub name: String,
    #[serde(default)]
    pub hostname: String,
    /// The name people see, for a runner kiln knows by its machine id (docs/machines.md): `--on`
    /// finds it by this as well as by its name and host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `linux`, `macos`, `windows` (Rust's names).
    pub os: String,
    /// `x86_64`, `aarch64` (Rust's names).
    pub arch: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub connected: bool,
    /// It takes reach links: it announced an identity and accepts other runners.
    #[serde(default)]
    pub reach: bool,
    /// Its reach identity's fingerprint ([`super::e2e::fingerprint_of_spki`]).
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// The features it speaks on reach links, as it announced them (docs/wire.md, "Named
    /// features"): none from a runner or a server before them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default)]
    pub cpus: u32,
    /// Busy share of all cores, 0–100, at the last sample.
    #[serde(default)]
    pub cpu_pct: f32,
    #[serde(default)]
    pub load1: f32,
    #[serde(default)]
    pub mem_total: u64,
    #[serde(default)]
    pub mem_used: u64,
    /// CI slots, and how many are taken.
    #[serde(default)]
    pub slots: u32,
    #[serde(default)]
    pub busy: u32,
    #[serde(default)]
    pub draining: bool,
    /// The harnesses it has installed (`claude`, `codex`, ...).
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// How it reaches the server: `direct` (UDP) or `tunnel` (HTTP).
    #[serde(default)]
    pub via: String,
    /// What it last reported of itself (docs/ci.md W13-R01), which a need's constraints are checked
    /// against. None from an older server, or before it has said.
    #[serde(default)]
    pub facts: Option<Box<RunnerFacts>>,
    /// When the server received its latest sample, by the server's clock (ms): which sample work
    /// placed since is counted against ([`rank`] counts it until a newer one shows it). None from
    /// an older server, or before it has sampled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at: Option<i64>,
}

/// How hard a machine is pressed, by its last sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Strain {
    /// CPUs it isn't using: its CPUs times the share of their time they were idle over its last
    /// sample. Its load average plays no part: Linux counts tasks waiting on disk in it (its I/O
    /// pressure counts on its own, [`fullness`]), and Windows has none.
    pub idle: f32,
    /// Memory it has free.
    pub free: u64,
    /// How full it is with nothing more on it ([`fullness`]); none before it has sampled.
    pub full: Option<f64>,
}

/// A machine's room for more work as a score, higher the more: the share of it free by
/// [`fullness`] (none said: below every machine that has). What callers that rank by a score take;
/// no weight, threshold or CI slot count goes into it (W05: CI work shows in its busy share).
pub fn room(s: Strain, _busy: u32, _slots: u32) -> f32 {
    s.full.map_or(f32::NEG_INFINITY, |f| (1.0 - f) as f32)
}

/// How full a machine would be with `request` on it, by its sample: the CI engine's measure
/// ([`kiln_ci::sched::state::Headroom::fullness`]), the larger of its CPUs' and its memory's share
/// in use, the shares of time its tasks all stalled on memory and on I/O on top. Lower is more
/// room; one that hasn't said its CPUs and memory is unknown (`None`), and goes after every one
/// that has.
pub fn fullness(sample: &kiln_ci::sched::state::Sample, request: &kiln_ci::sched::state::Resources) -> Option<f64> {
    (sample.cpus > 0 && sample.mem_total > 0).then(|| kiln_ci::sched::state::Headroom::of(sample).fullness(request))
}

/// Which of two machines' fullness ([`fullness`]) has more room: the lower first, unknown last.
pub fn emptier(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Work this process placed on each runner ([`pick`]), and the sample of the runner's it was
/// placed by ([`RunnerInfo::sampled_at`]): counted in use until a newer sample shows it, as the CI
/// engine counts what it placed since a runner's sample (docs/needs.md). The sample's stamp only
/// tells one sample from the next: no clock is compared with another.
static PLACED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, (Option<i64>, kiln_ci::sched::state::Resources)>>> = std::sync::LazyLock::new(Default::default);

/// What this process placed on `r` since its latest sample. Nothing for a runner whose samples
/// carry no stamp (from an older server): when a newer one shows the work can't be told.
fn placed_since(r: &RunnerInfo) -> kiln_ci::sched::state::Resources {
    if r.sampled_at.is_none() {
        return kiln_ci::sched::state::Resources::default();
    }
    PLACED.lock().get(&r.name).filter(|(at, _)| *at == r.sampled_at).map(|(_, q)| *q).unwrap_or_default()
}

/// `request` placed on `r` now: counted with what was placed since its latest sample.
fn note_placed(r: &RunnerInfo, request: &kiln_ci::sched::state::Resources) {
    if r.sampled_at.is_none() {
        return;
    }
    let mut g = PLACED.lock();
    let e = g.entry(r.name.clone()).or_insert((r.sampled_at, kiln_ci::sched::state::Resources::default()));
    if e.0 != r.sampled_at {
        *e = (r.sampled_at, kiln_ci::sched::state::Resources::default());
    }
    e.1 = plus(&e.1, request);
}

/// Two requests' CPUs and memory together.
fn plus(a: &kiln_ci::sched::state::Resources, b: &kiln_ci::sched::state::Resources) -> kiln_ci::sched::state::Resources {
    kiln_ci::sched::state::Resources { cpu: a.cpu.saturating_add(b.cpu), mem: a.mem.saturating_add(b.mem), ..*a }
}

impl RunnerInfo {
    /// The name people see: its label, else its name. What sentences say of it; what's looked up
    /// by, its name.
    pub fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// How hard it is pressed, by its last sample.
    pub fn strain(&self) -> Strain {
        let c = self.cpus as f32;
        Strain { idle: (c * (1.0 - (self.cpu_pct / 100.0).clamp(0.0, 1.0))).max(0.0), free: self.mem_free(), full: self.fullness(&kiln_ci::sched::state::Resources::default()) }
    }

    /// Cores it is not using, by its last sample: its CPUs times their idle share.
    pub fn idle_cpus(&self) -> f32 {
        self.strain().idle
    }

    /// How full `request` would leave it, by its last sample ([`fullness`]), what this process
    /// placed on it since counted in use; `None` before it has said its CPUs and memory.
    pub fn fullness(&self, request: &kiln_ci::sched::state::Resources) -> Option<f64> {
        fullness(&self.sample(), &plus(request, &placed_since(self)))
    }

    pub fn mem_free(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_used)
    }

    /// `linux/x86_64`.
    pub fn platform(&self) -> String {
        format!("{}/{}", self.os, self.arch)
    }

    /// Its latest sample as the CI engine weighs a runner's ([`kiln_ci::sched::state::Sample`]),
    /// its pressure from its facts as `crate::ci::dispatch::sample_of` takes it: Linux's memory
    /// and I/O stalls, and memory macOS calls critical counted all in use.
    pub fn sample(&self) -> kiln_ci::sched::state::Sample {
        let work = self.facts.as_ref().and_then(|f| f.work.as_ref());
        let depleted = work.and_then(|w| w.memory_level.as_deref()) == Some("critical");
        kiln_ci::sched::state::Sample {
            cpus: self.cpus,
            busy: f64::from(self.cpu_pct.clamp(0.0, 100.0)) / 100.0,
            load1: f64::from(self.load1),
            mem_total: self.mem_total,
            mem_used: if depleted { self.mem_total } else { self.mem_used },
            io_full: work.and_then(|w| w.io_full),
            mem_full: work.and_then(|w| w.mem_full),
        }
    }

    /// Its facts as a need's constraints read them: what it last reported, with the platform, cores
    /// and memory of its latest sample over them (fresher than the report, and there before one).
    pub fn facts_now(&self) -> RunnerFacts {
        let mut f = self.facts.as_deref().cloned().unwrap_or_default();
        if !self.os.is_empty() {
            f.os.clone_from(&self.os);
        }
        if !self.arch.is_empty() {
            f.arch.clone_from(&self.arch);
        }
        if self.cpus > 0 {
            f.cpu.threads = self.cpus;
        }
        if self.mem_total > 0 {
            f.mem.total = self.mem_total;
            f.mem.free = self.mem_free();
        }
        f
    }
}

/// What a piece of work needs of a runner: one by name, or what it must be and have, in the needs
/// spelling (`kiln_ci::sched::needs`, docs/runners.md) that CI's jobs use too.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Want {
    /// Exactly this runner.
    #[serde(default)]
    pub name: Option<String>,
    /// What it must be and have: constraints on its facts and labels, and the room asked for, idle
    /// and free on it now (reach holds nothing for the work).
    #[serde(default)]
    pub needs: Needs,
    /// A harness it must have installed.
    #[serde(default)]
    pub harness: Option<String>,
    /// Runners not to pick.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Want {
    /// Parse what people write: a runner's name, or what it needs as the needs spelling has it
    /// (`linux/amd64`, `macos`, `+gpu`, `cpus=8`, `mem=32g`, `tool:cargo`, `cpus>=16`, `mem>=32g`,
    /// `exclusive`, …), and `harness=codex` or `-name` (not that one). Words are separated by commas
    /// or spaces; a plain word that is no need (no OS, architecture or `exclusive`) is a runner's name.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut w = Self::default();
        for word in spec.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(h) = word.strip_prefix("harness=") {
                w.harness = Some(h.to_owned());
            } else if let Some(n) = word.strip_prefix('-') {
                w.exclude.push(n.to_owned());
            } else if names_a_runner(word) {
                w.name = Some(word.to_owned());
            } else {
                w.needs.add(word).map_err(|why| anyhow::anyhow!("{word}: {why}"))?;
                if w.needs.together.is_some() {
                    anyhow::bail!("{word}: {}", GANGS_LATER);
                }
            }
        }
        Ok(w)
    }

    /// Why `r` does not suit, or None when it does (whether it is reachable aside).
    pub fn mismatch(&self, r: &RunnerInfo) -> Option<String> {
        if let Some(n) = &self.name
            && n != &r.name
            && n != &r.hostname
            && r.label.as_deref() != Some(n.as_str())
        {
            return Some(format!("not {n}"));
        }
        if self.exclude.iter().any(|x| x == &r.name) {
            return Some("excluded".into());
        }
        let f = r.facts_now();
        for src in &self.needs.requires {
            match Constraint::parse(src) {
                Ok(c) if c.eval(&f, &r.labels) => {}
                Ok(c) => return Some(unmet(src, &c, &f, r.facts.is_some())),
                Err(e) => return Some(e),
            }
        }
        if let Some(h) = &self.harness
            && !r.harnesses.contains(h)
        {
            return Some(format!("no {h} installed"));
        }
        self.short_of(r, &f)
    }

    /// What `r` hasn't room for of what the work asks (idle cores, free memory and disk, GPUs, the
    /// runner to itself), or None.
    fn short_of(&self, r: &RunnerInfo, f: &RunnerFacts) -> Option<String> {
        let ask = &self.needs.request;
        if (!ask.cpu.is_zero() && r.cpus == 0) || (ask.mem > 0 && r.mem_total == 0) {
            return Some("it hasn't said how busy it is yet".into());
        }
        let idle = r.idle_cpus().floor();
        // Its idle cores, measured, against what's asked, in thousandths.
        if kiln_ci::sched::state::Cpus::of_f64(f64::from(idle)).unwrap_or_default() < ask.cpu {
            return Some(format!("{idle:.0} of {} cores idle, under {}", r.cpus, needs::cpus(ask.cpu)));
        }
        if r.mem_free() < ask.mem {
            return Some(format!("{} free, under {}", human_bytes(r.mem_free()), human_bytes(ask.mem)));
        }
        if (ask.disk > 0 || ask.gpu > 0) && r.facts.is_none() {
            return Some(NO_FACTS.into());
        }
        if f.disk.free < ask.disk {
            return Some(format!("{} of disk free, under {}", human_bytes(f.disk.free), human_bytes(ask.disk)));
        }
        let gpus = u32::try_from(f.gpus.len()).unwrap_or(u32::MAX);
        if gpus < ask.gpu {
            return Some(format!("{}, under {}", needs::gpus(gpus), needs::gpus(ask.gpu)));
        }
        if self.needs.exclusive && r.busy > 0 {
            return Some(format!("{} CI job{} on it", r.busy, if r.busy == 1 { "" } else { "s" }));
        }
        None
    }

    /// In words, for messages: "adacachy", "8 CPUs, os == linux, with codex", "any runner".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = &self.name {
            parts.push(n.clone());
        }
        if self.needs != Needs::default() {
            parts.push(self.needs.describe());
        }
        if let Some(h) = &self.harness {
            parts.push(format!("with {h}"));
        }
        if parts.is_empty() { "any runner".into() } else { parts.join(", ") }
    }
}

/// A word of [`Want::parse`] that names a runner: no operator in it, and no OS, architecture or
/// `exclusive` (the only plain words that are needs).
fn names_a_runner(w: &str) -> bool {
    !w.contains(|c: char| "=<>!:/+".contains(c)) && needs::os_name(w).is_none() && needs::arch_name(w).is_none() && !w.eq_ignore_ascii_case("exclusive")
}

/// Why a runner that has sent no facts of its own doesn't suit a need that reads them.
const NO_FACTS: &str = "it hasn't reported its facts (an older runner, or one just started)";

/// Why a runner with facts `f` doesn't meet constraint `c` (written `src`), in words: "macos is not
/// linux", "no label gpu", "no cargo on its PATH", "8 cores, under 16". `reported`: it has sent facts
/// of its own (else only its platform, cores and memory are known).
fn unmet(src: &str, c: &Constraint, f: &RunnerFacts, reported: bool) -> String {
    let sampled = c.facts().iter().all(|x| matches!(x.as_str(), "os" | "arch" | "cpu.threads" | "mem.total" | "mem.free"));
    match c {
        Constraint::Cmp { fact, op: Op::Eq, value } if fact == "os" || fact == "arch" => format!("{} is not {}", routing::fact(f, fact).show(), value.show()),
        Constraint::Cmp { fact, op: Op::Ge, value: Lit::Num(n) } if fact == "cpu.threads" => format!("{} cores, under {n}", f.cpu.threads),
        Constraint::Cmp { fact, op: Op::Ge, value: Lit::Num(n) } if fact == "mem.free" => format!("{} free, under {}", human_bytes(f.mem.free), human_bytes(*n as u64)),
        Constraint::Label(l) => format!("no label {l}"),
        _ if !reported && !sampled => NO_FACTS.into(),
        Constraint::Has { fact, item } if fact == "tools" => format!("no {item} on its PATH"),
        Constraint::Has { fact, item } if fact == "executors" => format!("no {item} executor"),
        Constraint::Has { fact, item } if fact == "cpu.flags" => format!("no CPU flag {item}"),
        Constraint::Has { fact, item } if fact == "binfmt" => format!("can't run {item} binaries"),
        _ => format!("not {src} ({})", c.facts().iter().map(|x| format!("{x} = {}", routing::fact(f, x).show())).collect::<Vec<_>>().join(", ")),
    }
}

/// What `kiln run` and MCP's `run` ask their runner for: the runner `on` names (or what it must be,
/// as before needs) and the needs spelling `needs`, as one spelling. `needs` is checked here (no
/// runner names in it): an error names its bad word. A runner older than the spelling reads a word it
/// doesn't know as a runner's name, so it refuses the run rather than running it where it doesn't suit.
pub fn target(on: &str, needs: &str) -> Result<String, String> {
    if let Some(g) = needs::parse(needs)?.together {
        return Err(format!("together:{}/{}: {GANGS_LATER}", g.id, g.size));
    }
    Ok([on.trim(), needs.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(","))
}

/// Why a command can't be one of a gang yet (W05-R40): starting together is the engine's to
/// decide, and commands go through it with docs/needs.md E5. Refused rather than run alone.
const GANGS_LATER: &str = "commands can't start together yet: gangs of commands come with commands on the engine (docs/needs.md E5)";

/// Where a command `kiln run --needs` (or MCP's `run` with `needs`) runs finds its needs, as they
/// were written: `ci/gate.sh` holds itself to them.
pub const NEEDS_ENV: &str = "KILN_NEEDS";

/// A command's environment `env` with `needs` in [`NEEDS_ENV`] (none asked: as it was).
pub fn with_needs(mut env: Vec<(String, String)>, needs: &str) -> Vec<(String, String)> {
    let needs = needs.trim();
    if !needs.is_empty() {
        env.retain(|(k, _)| k != NEEDS_ENV);
        env.push((NEEDS_ENV.to_owned(), needs.to_owned()));
    }
    env
}

pub fn human_bytes(n: u64) -> String {
    let gib = n as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 { format!("{gib:.0} GB") } else { format!("{:.0} MB", n as f64 / (1024.0 * 1024.0)) }
}

/// A runner that suits, ranked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub runner: RunnerInfo,
    /// The share of it the work would leave free, by its last sample: 1 less its [`fullness`] with
    /// the work on it. Higher is more room; none for a runner that hasn't said its CPUs and memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Why, in a line: "linux/x86_64 · 14 of 16 cores idle · 52 GB free".
    pub why: String,
}

/// Every runner that suits `want` and can be reached from `me`, best first, and why each other one
/// was passed over. Each is weighed by the one measure the CI engine places by ([`fullness`],
/// W05): how full the work would leave it, the larger of its CPUs' and its memory's share in use,
/// its memory and I/O stalls on top, and what this process placed on it since its last sample
/// counted in use. The emptiest goes first; one that hasn't sampled goes after every one that
/// has. No constant favours a machine: the asking one is weighed like any other and comes first
/// only on a tie, where nothing then has to cross the network. Choices leave out runners' facts.
pub fn rank(runners: &[RunnerInfo], want: &Want, me: &str) -> (Vec<Choice>, Vec<(String, String)>) {
    let mut ok = Vec::new();
    let mut passed = Vec::new();
    for r in runners {
        let why_not = want.mismatch(r).or_else(|| {
            if !r.connected {
                Some("offline".into())
            } else if !r.reach && r.name != me {
                Some("does not take reach links (an older runner, or it turned them off)".into())
            } else if r.draining {
                Some("draining".into())
            } else {
                None
            }
        });
        if let Some(w) = why_not {
            passed.push((r.name.clone(), w));
            continue;
        }
        let s = r.strain();
        let full = r.fullness(&want.needs.request);
        let asked = needs::request_words(&want.needs.request);
        let sample = r.sample();
        let stalled = [("memory", sample.mem_full), ("I/O", sample.io_full)].into_iter().filter_map(|(what, p)| p.filter(|p| *p > 0.0).map(|p| format!(" · {what} pressure {p:.0}%"))).collect::<String>();
        let why = format!(
            "{} · {:.0} of {} cores idle · {} free{}{}{}{}",
            r.platform(),
            s.idle,
            r.cpus,
            human_bytes(r.mem_free()),
            if asked.is_empty() { String::new() } else { format!(" · room for {}", asked.join(", ")) },
            if r.busy > 0 { format!(" · {} CI jobs", r.busy) } else { String::new() },
            stalled,
            if r.name == me { " · this machine" } else { "" }
        );
        ok.push((full, Choice { runner: RunnerInfo { facts: None, ..r.clone() }, score: full.map(|f| (1.0 - f) as f32), why }));
    }
    ok.sort_by(|(fa, a), (fb, b)| emptier(*fa, *fb).then_with(|| (b.runner.name == me).cmp(&(a.runner.name == me))).then_with(|| a.runner.name.cmp(&b.runner.name)));
    (ok.into_iter().map(|(_, c)| c).collect(), passed)
}

/// The best runner for `want`, or an error saying why there is none.
pub fn pick(runners: &[RunnerInfo], want: &Want, me: &str) -> anyhow::Result<Choice> {
    let (ok, passed) = rank(runners, want, me);
    if let Some(best) = ok.into_iter().next() {
        // Counted on it until its next sample shows it (docs/needs.md).
        note_placed(&best.runner, &want.needs.request);
        return Ok(best);
    }
    if passed.is_empty() {
        anyhow::bail!("no runners are known yet");
    }
    let shown = |n: &str| runners.iter().find(|r| r.name == n).map_or_else(|| n.to_owned(), |r| r.shown().to_owned());
    let detail: Vec<String> = passed.iter().map(|(n, w)| format!("{}: {w}", shown(n))).collect();
    anyhow::bail!("no runner suits {}: {}", want.describe(), detail.join("; "))
}

/// Which runners said they hold which objects of their caches, by repository (docs/ci.md
/// W24-R27): what a runner asks for an object it lacks. A runner's word only: what a peer sends is
/// checked by its digest.
static HOLDERS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<(String, String), std::collections::BTreeSet<String>>>> = std::sync::LazyLock::new(Default::default);

/// The most objects the directory keeps holders for: past it, it starts over.
const HOLDERS_MAX: usize = 1 << 20;

/// `runner` holds `digest` for `repo`.
pub fn holds(runner: &str, repo: &str, digest: &str) {
    if runner.is_empty() || repo.is_empty() || digest.is_empty() {
        return;
    }
    let mut h = HOLDERS.lock();
    if h.len() >= HOLDERS_MAX {
        h.clear();
    }
    h.entry((repo.to_owned(), digest.to_owned())).or_default().insert(runner.to_owned());
}

/// The runners besides `asker` that said they hold `digest` for `repo`.
pub fn holders(repo: &str, digest: &str, asker: &str) -> Vec<String> {
    HOLDERS.lock().get(&(repo.to_owned(), digest.to_owned())).map(|s| s.iter().filter(|r| *r != asker).cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W24-R27: the directory says which runners hold an object for a repository, never the
    /// one asking, and nothing for a runner, repository or object not named.
    #[test]
    fn w24_r27_the_directory_of_holders() {
        holds("linux", "acme/app", "sha256:d1");
        holds("mac", "acme/app", "sha256:d1");
        holds("win", "acme/else", "sha256:d1");
        holds("", "acme/app", "sha256:d2");
        assert_eq!(holders("acme/app", "sha256:d1", "mac"), vec!["linux".to_owned()]);
        assert_eq!(holders("acme/else", "sha256:d1", "mac"), vec!["win".to_owned()]);
        assert!(holders("acme/app", "sha256:d2", "mac").is_empty() && holders("acme/app", "sha256:d3", "mac").is_empty());
    }

    fn runner(name: &str, os: &str, arch: &str, cpus: u32, cpu_pct: f32, mem_free_gb: u64) -> RunnerInfo {
        RunnerInfo {
            name: name.into(),
            hostname: format!("{name}.lan"),
            label: None,
            os: os.into(),
            arch: arch.into(),
            connected: true,
            reach: true,
            fingerprint: Some("f".into()),
            cpus,
            cpu_pct,
            mem_total: 64 << 30,
            mem_used: (64 - mem_free_gb) << 30,
            slots: 4,
            via: "direct".into(),
            ..RunnerInfo::default()
        }
    }

    /// A machine's idle cores are its CPUs times their idle share: its load average takes none of
    /// them (Linux counts tasks waiting on disk in it, and Windows has none), and no load sets a
    /// machine aside. What it waits on disk and memory counts on its own, its stalls on top of how
    /// full it is. Nearly out of memory, a machine goes after one with room by the same measure,
    /// and still takes the work when it's the only one that suits.
    #[test]
    fn w05_a_machines_load_average_takes_none_of_its_cores() -> anyhow::Result<()> {
        // adacachy on 2026-11-01: 32 cores 43% busy, load 38 from tasks waiting on disk.
        let mut on_disk = runner("adacachy", "linux", "x86_64", 32, 43.0, 47);
        on_disk.load1 = 38.0;
        assert!((on_disk.idle_cpus() - 18.24).abs() < 1e-3, "{}", on_disk.idle_cpus());
        let c = pick(std::slice::from_ref(&on_disk), &Want::parse("cpus=12,linux")?, "mac")?;
        assert!(c.why.starts_with("linux/x86_64 · 18 of 32 cores idle"), "{}", c.why);
        let mut loaded = runner("b", "linux", "x86_64", 8, 10.0, 32);
        loaded.load1 = 40.0;
        assert!((loaded.idle_cpus() - 7.2).abs() < 1e-3, "{}", loaded.idle_cpus());
        // Stalled on disk or on memory, the same machine goes after one that isn't, saying so.
        let calm = RunnerInfo { name: "calm".into(), ..on_disk.clone() };
        let work = |io: Option<f64>, mem: Option<f64>| Some(Box::new(RunnerFacts { work: Some(kiln_ci::facts::WorkFacts { io_full: io, mem_full: mem, memory_level: None }), ..RunnerFacts::default() }));
        let stalled = RunnerInfo { facts: work(Some(30.0), None), ..on_disk.clone() };
        let (ok, _) = rank(&[stalled, calm.clone()], &Want::parse("linux")?, "mac");
        assert_eq!(ok.iter().map(|c| c.runner.name.as_str()).collect::<Vec<_>>(), ["calm", "adacachy"], "{ok:?}");
        assert!(ok.last().is_some_and(|c| c.why.contains(" · I/O pressure 30%")), "{ok:?}");
        let thrashing = RunnerInfo { name: "thrash".into(), facts: work(None, Some(12.0)), ..on_disk.clone() };
        let (ok, _) = rank(&[thrashing, calm.clone()], &Want::parse("linux")?, "mac");
        assert_eq!(ok.iter().map(|c| c.runner.name.as_str()).collect::<Vec<_>>(), ["calm", "thrash"], "{ok:?}");
        // macOS at critical memory: all its memory counts in use.
        let critical = RunnerInfo { name: "mac-critical".into(), facts: Some(Box::new(RunnerFacts { work: Some(kiln_ci::facts::WorkFacts { memory_level: Some("critical".into()), ..Default::default() }), ..RunnerFacts::default() })), ..calm.clone() };
        assert!(critical.fullness(&kiln_ci::sched::state::Resources::default()).is_some_and(|f| f >= 1.0), "{:?}", critical.sample());
        // Nearly out of memory: after one with room; the only one that suits, it takes the work.
        let short = runner("fedora", "linux", "x86_64", 32, 36.0, 1);
        let roomy = runner("roomy", "linux", "x86_64", 32, 60.0, 40);
        let (ok, _) = rank(&[short.clone(), roomy], &Want::parse("linux")?, "mac");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("roomy"), "{ok:?}");
        assert_eq!(pick(std::slice::from_ref(&short), &Want::parse("linux")?, "mac")?.runner.name, "fedora");
        Ok(())
    }

    /// docs/needs.md: work placed on a runner counts in use there until a newer sample shows it, as
    /// the CI engine counts what it placed: two asks in a row before either machine samples again
    /// go one to each, not both to the one that looked emptier.
    #[test]
    fn w05_work_placed_since_a_sample_counts_until_a_newer_one() -> anyhow::Result<()> {
        let at = |name: &str, cpu_pct: f32| RunnerInfo { sampled_at: Some(1_000), ..runner(name, "linux", "x86_64", 16, cpu_pct, 48) };
        let fleet = [at("w05-placed-a", 10.0), at("w05-placed-b", 20.0)];
        let want = Want::parse("cpus=8,linux")?;
        assert_eq!(pick(&fleet, &want, "elsewhere")?.runner.name, "w05-placed-a");
        assert_eq!(pick(&fleet, &want, "elsewhere")?.runner.name, "w05-placed-b", "a holds the first 8 CPUs until it samples again");
        // A newer sample of a's shows what runs there (here, nothing more): counted no longer.
        let resampled = [RunnerInfo { sampled_at: Some(2_000), ..at("w05-placed-a", 10.0) }, at("w05-placed-b", 20.0)];
        let (ok, _) = rank(&resampled, &want, "elsewhere");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("w05-placed-a"), "{ok:?}");
        // A runner whose samples carry no stamp (an older server) isn't counted: when a newer
        // sample shows the work can't be told.
        let unstamped = [runner("w05-placed-c", "linux", "x86_64", 16, 10.0, 48), runner("w05-placed-d", "linux", "x86_64", 16, 20.0, 48)];
        assert_eq!(pick(&unstamped, &want, "elsewhere")?.runner.name, "w05-placed-c");
        assert_eq!(pick(&unstamped, &want, "elsewhere")?.runner.name, "w05-placed-c");
        Ok(())
    }

    /// A score for callers that rank by one (`room`): the share free, unknown below every known.
    #[test]
    fn w05_room_is_the_share_free() {
        let r = runner("r", "linux", "x86_64", 10, 40.0, 48);
        assert!((room(r.strain(), 3, 4) - 0.6).abs() < 1e-6, "{}", room(r.strain(), 3, 4));
        assert_eq!(room(Strain::default(), 0, 0), f32::NEG_INFINITY);
    }

    #[test]
    fn people_write_platforms_many_ways() -> anyhow::Result<()> {
        let w = Want::parse("linux/amd64")?;
        assert_eq!(w.needs.requires, ["os == linux", "arch == x86_64"]);
        let w = Want::parse("Darwin, arm64 +gpu cpus>=8 mem>=16g harness=codex -mac2")?;
        assert_eq!(w.needs.requires, ["os == macos", "arch == aarch64", "label gpu", "cpu.threads >= 8", "mem.free >= 16Gi"]);
        assert_eq!((w.harness.as_deref(), w.exclude.as_slice(), w.name.as_deref()), (Some("codex"), ["mac2".to_owned()].as_slice(), None));
        assert_eq!(Want::parse("adacachy")?.name.as_deref(), Some("adacachy"));
        assert_eq!(Want::parse("adas-macbook-pro-2.local,mem=8g")?.name.as_deref(), Some("adas-macbook-pro-2.local"));
        // `--on` reads the needs spelling as `--needs` does: the same needs.
        let both = Want::parse("cpus=8,mem=32g,linux/amd64,+gpu,tool:cargo,exclusive")?;
        assert_eq!(both.needs, needs::parse("cpus=8,mem=32g,linux/amd64,+gpu,tool:cargo,exclusive").map_err(anyhow::Error::msg)?);
        assert!(both.name.is_none());
        for (bad, why) in [("plan9/amd64", "plan9/amd64: no such OS"), ("mem>=lots", "mem>=lots: not a size"), ("memm=4g", "memm=4g: no such need"), ("tol:cargo", "tol:cargo: no such need"), ("together:g/3", "together:g/3: commands can't start together yet")] {
            let e = Want::parse(bad).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(e.starts_with(why), "{bad}: {e}");
        }
        // A gang of commands waits for commands on the engine (W05-R40, docs/needs.md E5): refused,
        // not run alone.
        assert!(target("", "cpus=2,together:g/3").is_err_and(|e| e.starts_with("together:g/3: commands can't start together yet")));
        Ok(())
    }

    /// A runner of a fleet for `kiln run --needs`: its facts as it reported them.
    fn reported(name: &str, cpus: u32, mem_free_gb: u64, facts: impl FnOnce(&mut RunnerFacts)) -> RunnerInfo {
        let mut f = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        f.cpu.threads = cpus;
        f.disk.free = 20 << 30;
        facts(&mut f);
        RunnerInfo { facts: Some(Box::new(f)), ..runner(name, "linux", "x86_64", cpus, 0.0, mem_free_gb) }
    }

    /// `kiln run --needs` (and MCP's `run` with `needs`) picks the runner that is and has what each
    /// kind of need asks, saying why the others don't suit: room asked for (cores idle, memory and
    /// disk free, GPUs), constraints (platform, labels, tools, executors, CPU flags, binfmt, any
    /// fact; cores in all and memory free as `--on` took them) and the runner to itself.
    #[test]
    fn kiln_run_needs_pick_by_each_kind_of_need() -> anyhow::Result<()> {
        let small = reported("small", 8, 16, |f| {
            f.tools.insert("git".into(), "2.47".into());
        });
        let big = reported("big", 32, 60, |f| {
            f.tools.insert("cargo".into(), "1.92".into());
            f.executors.insert("docker".into(), kiln_ci::facts::Executor::default());
            f.cpu.flags = vec!["avx2".into()];
            f.binfmt = vec!["linux/arm64".into()];
            f.disk.free = 500 << 30;
            f.custom.insert("rack".into(), "a3".into());
        });
        let mut gpu = reported("gpu", 16, 32, |f| f.gpus = vec![kiln_ci::facts::Gpu::default()]);
        gpu.labels = vec!["gpu".into()];
        gpu.busy = 2;
        let mac = RunnerInfo { facts: None, ..runner("mac", "macos", "aarch64", 12, 0.0, 24) };
        let fleet = [small, big, gpu, mac];
        let cases: &[(&str, &str, (&str, &str))] = &[
            // Room asked for: idle and free now.
            ("cpus=24", "big", ("gpu", "16 of 16 cores idle, under 24 CPUs")),
            ("mem=40g", "big", ("small", "16 GB free, under 40 GB")),
            ("disk=100g", "big", ("small", "20 GB of disk free, under 100 GB")),
            ("gpus=1", "gpu", ("big", "no GPU, under 1 GPU")),
            // Constraints on what it is and has.
            ("macos", "mac", ("big", "linux is not macos")),
            ("+gpu", "gpu", ("big", "no label gpu")),
            ("tool:cargo", "big", ("small", "no cargo on its PATH")),
            ("executor:docker", "big", ("small", "no docker executor")),
            ("flag:avx2", "big", ("small", "no CPU flag avx2")),
            ("binfmt:linux/arm64", "big", ("small", "can't run linux/arm64 binaries")),
            ("custom.rack==a3", "big", ("small", "not custom.rack == a3 (custom.rack = (unknown))")),
            ("cpus>=24", "big", ("small", "8 cores, under 24")),
            ("mem>=40g", "big", ("small", "16 GB free, under 40 GB")),
            // One that hasn't reported facts meets only what its sample shows.
            ("arm64,tool:cargo", "", ("mac", "it hasn't reported its facts (an older runner, or one just started)")),
            // The runner to itself: none with CI work on it.
            ("exclusive,+gpu", "", ("gpu", "2 CI jobs on it")),
            // The headline: a Linux x86-64 runner with 8 cores idle and 32 GiB free.
            ("cpus=8,mem=32g,linux/amd64", "big", ("small", "16 GB free, under 32 GB")),
        ];
        for (spelling, picked, (other, why)) in cases {
            let want = Want::parse(spelling)?;
            let (ok, passed) = rank(&fleet, &want, "elsewhere");
            assert_eq!(ok.first().map_or("", |c| c.runner.name.as_str()), *picked, "{spelling}: {ok:?} {passed:?}");
            assert!(passed.iter().any(|(n, w)| n == other && w == why), "{spelling}: {passed:?}");
            assert!(ok.iter().all(|c| c.runner.facts.is_none()), "a choice leaves its facts out");
        }
        // It says which and why, and the room it has for what's asked.
        let c = pick(&fleet, &Want::parse("cpus=8,mem=32g,linux/amd64")?, "elsewhere")?;
        assert_eq!(c.why, "linux/x86_64 · 32 of 32 cores idle · 60 GB free · room for 8 CPUs, 32 GiB of memory");
        let none = pick(&fleet, &Want::parse("exclusive,+gpu")?, "elsewhere").err().map(|e| e.to_string()).unwrap_or_default();
        assert!(none.starts_with("no runner suits label gpu, the runner whole: ") && none.contains("gpu: 2 CI jobs on it"), "{none}");
        // This machine is weighed like any other: though it suits, the one the work leaves emptier
        // goes first.
        let (ok, _) = rank(&fleet, &Want::parse("mem=12g")?, "small");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("big"), "{ok:?}");
        Ok(())
    }

    #[test]
    fn kiln_run_puts_on_and_needs_in_one_spelling() {
        assert_eq!(target("", "cpus=8,mem=32g,linux/amd64").as_deref(), Ok("cpus=8,mem=32g,linux/amd64"));
        assert_eq!(target("adacachy", "mem=32g").as_deref(), Ok("adacachy,mem=32g"));
        assert_eq!(target(" linux/amd64 ", "").as_deref(), Ok("linux/amd64"));
        assert_eq!(target("", "").as_deref(), Ok(""));
        // `--needs` is needs alone: a runner's name there is a mistake, said as such.
        assert_eq!(target("", "adacachy"), Err("adacachy: no such need".to_owned()));
        assert_eq!(target("adacachy", "memm=4g"), Err("memm=4g: no such need".to_owned()));
    }

    /// The command finds what it was asked for in KILN_NEEDS, `--needs` over a `--env` of the same.
    #[test]
    fn kiln_run_gives_the_command_its_needs() {
        let mine = vec![("A".to_owned(), "1".to_owned()), (NEEDS_ENV.to_owned(), "cpus=1".to_owned())];
        assert_eq!(with_needs(mine.clone(), " cpus=12,mem=40g,linux/amd64 "), vec![("A".to_owned(), "1".to_owned()), (NEEDS_ENV.to_owned(), "cpus=12,mem=40g,linux/amd64".to_owned())]);
        assert_eq!(with_needs(mine.clone(), ""), mine);
    }

    /// A gate's run asks for what it needs (AGENTS.md, ci/gate.sh) and goes to a runner that has it,
    /// by no name: asked 12 CPUs and 40 GiB, the one with 12 cores idle and 40 GiB free.
    #[test]
    fn kiln_run_puts_the_gate_on_a_runner_with_room_for_it() -> anyhow::Result<()> {
        let fleet = [
            runner("mac", "macos", "aarch64", 24, 0.0, 60),
            runner("small", "linux", "x86_64", 8, 0.0, 48),
            runner("busy", "linux", "x86_64", 32, 75.0, 60),
            runner("tight", "linux", "x86_64", 32, 0.0, 30),
            runner("roomy", "linux", "x86_64", 32, 10.0, 56),
        ];
        let want = Want::parse("cpus=12,mem=40g,linux/amd64")?;
        let (ok, passed) = rank(&fleet, &want, "small");
        assert_eq!(ok.iter().map(|c| c.runner.name.as_str()).collect::<Vec<_>>(), ["roomy"], "{passed:?}");
        for (n, why) in [("mac", "macos is not linux"), ("small", "8 of 8 cores idle, under 12 CPUs"), ("busy", "8 of 32 cores idle, under 12 CPUs"), ("tight", "30 GB free, under 40 GB")] {
            assert!(passed.iter().any(|(m, w)| m == n && w == why), "{n}: {passed:?}");
        }
        Ok(())
    }

    #[test]
    fn the_idlest_matching_runner_wins_and_the_rest_say_why() -> anyhow::Result<()> {
        let mut busy = runner("busy", "linux", "x86_64", 32, 90.0, 8);
        busy.busy = 4;
        let mut tunnelled = runner("far", "linux", "x86_64", 16, 10.0, 40);
        tunnelled.via = "tunnel".into();
        let mut old = runner("old", "linux", "x86_64", 64, 0.0, 60);
        old.reach = false;
        let runners = vec![
            runner("mac", "macos", "aarch64", 12, 5.0, 20),
            busy,
            runner("idle", "linux", "x86_64", 16, 5.0, 48),
            tunnelled,
            old,
        ];
        let want = Want::parse("linux/amd64")?;
        let (ok, passed) = rank(&runners, &want, "mac");
        let names: Vec<&str> = ok.iter().map(|c| c.runner.name.as_str()).collect();
        assert_eq!(names, vec!["idle", "far", "busy"]);
        assert!(ok.first().is_some_and(|c| c.why.starts_with("linux/x86_64 · 15 of 16 cores idle")));
        assert!(passed.iter().any(|(n, w)| n == "mac" && w == "macos is not linux"));
        assert!(passed.iter().any(|(n, w)| n == "old" && w.contains("reach")));
        assert_eq!(pick(&runners, &want, "mac")?.runner.name, "idle");
        let none = pick(&runners, &Want::parse("windows")?, "mac");
        assert!(none.is_err_and(|e| e.to_string().contains("no runner suits os == windows: mac: macos is not windows")));
        Ok(())
    }

    #[test]
    fn on_finds_a_runner_known_by_its_machine_id_by_the_name_people_see() -> anyhow::Result<()> {
        let mut laptop = runner("5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b", "macos", "aarch64", 12, 5.0, 20);
        laptop.label = Some("adas-macbook-pro-3.local".into());
        let runners = vec![laptop, runner("adas-macbook-pro.local", "macos", "aarch64", 12, 5.0, 20)];
        // By its label, its id or its host name; another machine's name finds that one.
        for on in ["adas-macbook-pro-3.local", "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b", "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b.lan"] {
            assert_eq!(pick(&runners, &Want::parse(on)?, "elsewhere")?.runner.name, "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b", "{on}");
        }
        assert_eq!(pick(&runners, &Want::parse("adas-macbook-pro.local")?, "elsewhere")?.runner.name, "adas-macbook-pro.local");
        Ok(())
    }

    /// The asking machine is weighed like any other, by how full the work would leave it: no
    /// constant puts it first. It comes first on a tie, where nothing then has to cross the network;
    /// what it isn't or hasn't goes elsewhere. A choice's score is the share the work would leave
    /// free.
    #[test]
    fn w05_this_machine_is_weighed_like_any_other() -> anyhow::Result<()> {
        // Busier and smaller than the other: the other goes first, though this one suits.
        let runners = vec![runner("a", "linux", "x86_64", 32, 0.0, 48), runner("b", "linux", "x86_64", 8, 90.0, 16)];
        assert_eq!(pick(&runners, &Want::default(), "b")?.runner.name, "a");
        let (ok, _) = rank(&runners, &Want::parse("linux/amd64")?, "b");
        assert_eq!(ok.iter().map(|c| c.runner.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(ok.last().is_some_and(|c| c.why.ends_with(" · this machine")), "{ok:?}");
        // The emptier, it goes first, and its score says the share the work leaves free.
        let roomier_here = vec![runner("a", "linux", "x86_64", 32, 70.0, 48), runner("b", "linux", "x86_64", 8, 10.0, 60)];
        let c = pick(&roomier_here, &Want::parse("cpus=4")?, "b")?;
        assert!(c.runner.name == "b" && c.score.is_some_and(|s| (s - 0.4).abs() < 1e-6), "{c:?}");
        // What it isn't or hasn't goes elsewhere: another platform, more cores, more memory.
        let mac = vec![runner("mac", "macos", "aarch64", 12, 0.0, 32), runner("a", "linux", "x86_64", 32, 0.0, 48)];
        assert_eq!(pick(&mac, &Want::parse("linux/amd64")?, "mac")?.runner.name, "a");
        assert_eq!(pick(&runners, &Want::parse("cpus>=16")?, "b")?.runner.name, "a");
        assert_eq!(pick(&runners, &Want::parse("mem>=32g")?, "b")?.runner.name, "a");
        // Even: this machine; else by name. One that hasn't sampled goes after every one that has.
        let even = vec![runner("a", "linux", "x86_64", 8, 0.0, 1), runner("b", "linux", "x86_64", 8, 0.0, 1)];
        assert_eq!(pick(&even, &Want::default(), "b")?.runner.name, "b");
        assert_eq!(pick(&even, &Want::default(), "c")?.runner.name, "a");
        let quiet = RunnerInfo { cpus: 0, mem_total: 0, mem_used: 0, ..runner("quiet", "linux", "x86_64", 0, 0.0, 0) };
        let (ok, _) = rank(&[quiet, runner("full", "linux", "x86_64", 8, 99.0, 1)], &Want::parse("linux")?, "quiet");
        assert_eq!(ok.iter().map(|c| (c.runner.name.as_str(), c.score.is_some())).collect::<Vec<_>>(), [("full", true), ("quiet", false)]);
        Ok(())
    }
}
