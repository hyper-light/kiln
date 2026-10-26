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
}

/// How hard a machine is pressed, by its last sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Strain {
    /// Cores it isn't using: by its busy share or by its load average, whichever leaves fewer
    /// (work waiting on disk or memory counts in its load, not in its busy share).
    pub idle: f32,
    /// Memory it has free.
    pub free: u64,
    /// Nearly out of memory (under [`LOW_MEMORY`] or a twentieth of it free) or loaded past twice
    /// its cores: work goes to it only when no other machine will do.
    pub overloaded: bool,
}

/// Free memory under which a machine is overloaded, whatever its size.
pub const LOW_MEMORY: u64 = 2 << 30;
/// What being overloaded costs a machine's score: more than any idle cores and memory make up.
pub const OVERLOADED: f32 = 1.0e4;

/// A machine's [`Strain`] from its sample (zeros: one that hasn't said, never overloaded).
pub fn strain(cpus: u32, cpu_pct: f32, load1: f32, mem_total: u64, mem_used: u64) -> Strain {
    let c = cpus as f32;
    let by_busy = (c * (1.0 - (cpu_pct / 100.0).clamp(0.0, 1.0))).max(0.0);
    let by_load = (c - load1.max(0.0)).max(0.0);
    let free = mem_total.saturating_sub(mem_used);
    let low_memory = mem_total > 0 && (free < LOW_MEMORY || free < mem_total / 20);
    Strain { idle: by_busy.min(by_load), free, overloaded: low_memory || (cpus > 0 && load1 > 2.0 * c) }
}

/// How much room a machine has for more work: its idle cores count most, free memory next, the
/// share of its CI slots taken against it, and being overloaded most of all. Higher is better.
pub fn room(s: Strain, busy: u32, slots: u32) -> f32 {
    let ci = if slots > 0 { busy as f32 / slots as f32 } else { 0.0 };
    let free_gb = s.free as f32 / (1024.0 * 1024.0 * 1024.0);
    s.idle * 4.0 + free_gb.min(64.0) - ci * 8.0 - if s.overloaded { OVERLOADED } else { 0.0 }
}

impl RunnerInfo {
    /// How hard it is pressed, by its last sample.
    pub fn strain(&self) -> Strain {
        strain(self.cpus, self.cpu_pct, self.load1, self.mem_total, self.mem_used)
    }

    /// Cores it is not using, by its last sample (its busy share or its load, whichever leaves fewer).
    pub fn idle_cpus(&self) -> f32 {
        self.strain().idle
    }

    pub fn mem_free(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_used)
    }

    /// `linux/x86_64`.
    pub fn platform(&self) -> String {
        format!("{}/{}", self.os, self.arch)
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
            }
        }
        Ok(w)
    }

    /// Why `r` does not suit, or None when it does (whether it is reachable aside).
    pub fn mismatch(&self, r: &RunnerInfo) -> Option<String> {
        if let Some(n) = &self.name
            && n != &r.name
            && n != &r.hostname
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
        if (ask.cpu > 0.0 && r.cpus == 0) || (ask.mem > 0 && r.mem_total == 0) {
            return Some("it hasn't said how busy it is yet".into());
        }
        let idle = r.idle_cpus().floor();
        if f64::from(idle) < ask.cpu {
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
    needs::parse(needs)?;
    Ok([on.trim(), needs.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(","))
}

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

/// Free memory below which the asking machine gives way to the idlest other one, even for work it
/// suits: what it has left is for what already runs on it.
const ROOM: u64 = 4 << 30;
/// What suiting and having room counts for the asking machine: more than any other's idle cores and
/// memory can.
const HOME: f32 = 1.0e6;

/// A runner that suits, ranked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub runner: RunnerInfo,
    /// Higher is better.
    pub score: f32,
    /// Why, in a line: "linux/x86_64 · 14 of 16 cores idle · 52 GB free".
    pub why: String,
}

/// Every runner that suits `want` and can be reached from `me`, best first, and why each other one
/// was passed over. `me` comes first whenever it suits and has memory to spare ([`ROOM`], beyond
/// what the work asks): work goes to another machine only for what this one isn't or hasn't
/// (another platform, a GPU, more cores or memory than it has). Choices leave out runners' facts.
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
        // Idle cores count most, free memory next; CI work on it and a tunnelled path count against
        // it, and being overloaded (nearly out of memory, or loaded past twice its cores) most of
        // all: such a machine gets work only when no other suits. This machine itself comes first
        // while it has room and isn't overloaded (nothing crosses the network, and another
        // machine's resources stay its own), and wins a tie when it hasn't.
        let mut score = room(s, r.busy, r.slots);
        if r.via == "tunnel" {
            score -= 2.0;
        }
        if r.name == me {
            score += if !s.overloaded && (r.mem_total == 0 || r.mem_free() >= ROOM.saturating_add(want.needs.request.mem)) { HOME } else { 0.5 };
        }
        let asked = needs::request_words(&want.needs.request);
        let why = format!(
            "{} · {:.0} of {} cores idle · {} free{}{}{}{}",
            r.platform(),
            s.idle,
            r.cpus,
            human_bytes(r.mem_free()),
            if asked.is_empty() { String::new() } else { format!(" · room for {}", asked.join(", ")) },
            if r.busy > 0 { format!(" · {} CI jobs", r.busy) } else { String::new() },
            if s.overloaded { format!(" · overloaded (load {:.0})", r.load1) } else { String::new() },
            if r.name == me { " · this machine" } else { "" }
        );
        ok.push(Choice { runner: RunnerInfo { facts: None, ..r.clone() }, score, why });
    }
    ok.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.runner.name.cmp(&b.runner.name)));
    (ok, passed)
}

/// The best runner for `want`, or an error saying why there is none.
pub fn pick(runners: &[RunnerInfo], want: &Want, me: &str) -> anyhow::Result<Choice> {
    let (ok, passed) = rank(runners, want, me);
    if let Some(best) = ok.into_iter().next() {
        return Ok(best);
    }
    if passed.is_empty() {
        anyhow::bail!("no runners are known yet");
    }
    let detail: Vec<String> = passed.iter().map(|(n, w)| format!("{n}: {w}")).collect();
    anyhow::bail!("no runner suits {}: {}", want.describe(), detail.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(name: &str, os: &str, arch: &str, cpus: u32, cpu_pct: f32, mem_free_gb: u64) -> RunnerInfo {
        RunnerInfo {
            name: name.into(),
            hostname: format!("{name}.lan"),
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

    /// A machine nearly out of memory, or loaded past twice its cores, gets work only when none
    /// other suits, however many cores its busy share leaves idle; overloaded, the asking machine
    /// gives way too. Its load counts against its idle cores.
    #[test]
    fn an_overloaded_machine_gets_work_only_when_none_other_suits() -> anyhow::Result<()> {
        let want = Want::parse("linux")?;
        // A machine as fedora was: a third of its cores busy by its share, load 126, 1 GB free.
        let mut thrashing = runner("fedora", "linux", "x86_64", 32, 36.0, 1);
        thrashing.load1 = 126.0;
        // Every core busy, but room.
        let busy = runner("adacachy", "linux", "x86_64", 32, 100.0, 60);
        let (ok, _) = rank(&[thrashing.clone(), busy.clone()], &want, "mac");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("adacachy"), "{ok:?}");
        assert!(ok.iter().any(|c| c.runner.name == "fedora" && c.why.contains("overloaded")), "{ok:?}");
        // The only one that suits: it still runs there, saying so.
        assert_eq!(pick(std::slice::from_ref(&thrashing), &want, "mac")?.runner.name, "fedora");
        // The asking machine overloaded: it gives way.
        let (ok, _) = rank(&[thrashing, busy], &want, "fedora");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("adacachy"), "{ok:?}");
        // A load past its cores leaves none idle, whatever its busy share says.
        let mut loaded = runner("b", "linux", "x86_64", 8, 10.0, 32);
        loaded.load1 = 12.0;
        assert!(loaded.idle_cpus().abs() < f32::EPSILON, "{}", loaded.idle_cpus());
        Ok(())
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
        for (bad, why) in [("plan9/amd64", "plan9/amd64: no such OS"), ("mem>=lots", "mem>=lots: not a size"), ("memm=4g", "memm=4g: no such need"), ("tol:cargo", "tol:cargo: no such need")] {
            let e = Want::parse(bad).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(e.starts_with(why), "{bad}: {e}");
        }
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
        // This machine comes first only with room beyond what's asked.
        let (ok, _) = rank(&fleet, &Want::parse("mem=12g")?, "small");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("small"), "16 GB free: 12 asked and 4 to spare");
        let (ok, _) = rank(&fleet, &Want::parse("mem=13g")?, "small");
        assert_eq!(ok.first().map(|c| c.runner.name.as_str()), Some("big"), "16 GB free leaves under 4 to spare");
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

    /// The gate asks for what it needs (AGENTS.md, ci/gate.sh) and goes to a runner that has it, by
    /// no name: the one with 12 cores idle and 40 GiB free.
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
    fn this_machine_comes_first_when_it_suits_and_has_room() -> anyhow::Result<()> {
        // Busier and smaller than the other, and still first: it suits and has memory to spare.
        let runners = vec![runner("a", "linux", "x86_64", 32, 0.0, 48), runner("b", "linux", "x86_64", 8, 90.0, 16)];
        assert_eq!(pick(&runners, &Want::default(), "b")?.runner.name, "b");
        let first = pick(&runners, &Want::parse("linux/amd64")?, "b")?;
        assert!(first.runner.name == "b" && first.why.ends_with(" · this machine"), "{first:?}");
        // What it isn't or hasn't goes elsewhere: another platform, more cores, more memory.
        let mac = vec![runner("mac", "macos", "aarch64", 12, 0.0, 32), runner("a", "linux", "x86_64", 32, 0.0, 48)];
        assert_eq!(pick(&mac, &Want::parse("linux/amd64")?, "mac")?.runner.name, "a");
        assert_eq!(pick(&runners, &Want::parse("cpus>=16")?, "b")?.runner.name, "a");
        assert_eq!(pick(&runners, &Want::parse("mem>=32g")?, "b")?.runner.name, "a");
        // Short of memory: the idlest; only a tie is still its.
        let short = vec![runner("a", "linux", "x86_64", 32, 0.0, 48), runner("b", "linux", "x86_64", 8, 0.0, 1)];
        assert_eq!(pick(&short, &Want::default(), "b")?.runner.name, "a");
        let even = vec![runner("a", "linux", "x86_64", 8, 0.0, 1), runner("b", "linux", "x86_64", 8, 0.0, 1)];
        assert_eq!(pick(&even, &Want::default(), "b")?.runner.name, "b");
        assert_eq!(pick(&even, &Want::default(), "c")?.runner.name, "a");
        Ok(())
    }
}
