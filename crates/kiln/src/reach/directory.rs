//! Which runners can be reached, and which one suits a piece of work (`docs/reach.md` §2).
//!
//! The server knows every runner: what it is (OS, architecture, labels, cores, memory), how busy it
//! is (its latest sample, its CI slots) and its reach identity. A runner asks it for this list and
//! picks from it itself, so a choice never waits on the server once the list is in hand.

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
}

/// The names people and tools use for an OS, as Rust names it.
pub fn os_name(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "linux" | "gnu" => "linux",
        "macos" | "mac" | "darwin" | "osx" | "apple" => "macos",
        "windows" | "win" | "win32" | "win64" => "windows",
        "freebsd" => "freebsd",
        _ => return None,
    })
}

/// The names people and tools use for an architecture, as Rust names it.
pub fn arch_name(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" | "x86-64" | "intel" => "x86_64",
        "aarch64" | "arm64" | "armv8" | "apple-silicon" => "aarch64",
        "x86" | "i386" | "i686" => "x86",
        "arm" | "armv7" | "armhf" => "arm",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

/// What a piece of work needs of a runner.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Want {
    /// Exactly this runner.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    /// Labels it must carry.
    #[serde(default)]
    pub labels: Vec<String>,
    /// A harness it must have installed.
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub min_cpus: Option<u32>,
    /// Free memory it must have, bytes.
    #[serde(default)]
    pub min_mem_free: Option<u64>,
    /// Runners not to pick.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Want {
    /// Parse what people write: `linux/amd64`, `macos`, `arm64`, `linux,+gpu`, `cpus>=16`,
    /// `mem>=32g`, `harness=codex`, or a runner's name. Words are separated by commas or spaces;
    /// one that is none of these is a runner name.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut w = Self::default();
        for word in spec.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((os, arch)) = word.split_once('/') {
                w.os = Some(os_name(os).ok_or_else(|| anyhow::anyhow!("unknown OS {os:?}"))?.to_owned());
                w.arch = Some(arch_name(arch).ok_or_else(|| anyhow::anyhow!("unknown architecture {arch:?}"))?.to_owned());
            } else if let Some(os) = os_name(word) {
                w.os = Some(os.to_owned());
            } else if let Some(arch) = arch_name(word) {
                w.arch = Some(arch.to_owned());
            } else if let Some(label) = word.strip_prefix('+').or_else(|| word.strip_prefix("label=")) {
                w.labels.push(label.to_owned());
            } else if let Some(h) = word.strip_prefix("harness=") {
                w.harness = Some(h.to_owned());
            } else if let Some(n) = word.strip_prefix("cpus>=") {
                w.min_cpus = Some(n.parse().map_err(|_| anyhow::anyhow!("bad core count in {word:?}"))?);
            } else if let Some(m) = word.strip_prefix("mem>=") {
                w.min_mem_free = Some(bytes(m).ok_or_else(|| anyhow::anyhow!("bad memory size in {word:?}"))?);
            } else if let Some(n) = word.strip_prefix('-') {
                w.exclude.push(n.to_owned());
            } else {
                w.name = Some(word.to_owned());
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
        if let Some(os) = &self.os
            && os != &r.os
        {
            return Some(format!("{} is not {os}", r.os));
        }
        if let Some(arch) = &self.arch
            && arch != &r.arch
        {
            return Some(format!("{} is not {arch}", r.arch));
        }
        if let Some(l) = self.labels.iter().find(|l| !r.labels.contains(l)) {
            return Some(format!("no label {l}"));
        }
        if let Some(h) = &self.harness
            && !r.harnesses.contains(h)
        {
            return Some(format!("no {h} installed"));
        }
        if let Some(c) = self.min_cpus
            && r.cpus < c
        {
            return Some(format!("{} cores, under {c}", r.cpus));
        }
        if let Some(m) = self.min_mem_free
            && r.mem_free() < m
        {
            return Some(format!("{} free, under {}", human_bytes(r.mem_free()), human_bytes(m)));
        }
        None
    }

    /// In words, for messages: "linux/x86_64 with gpu".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = &self.name {
            parts.push(n.clone());
        }
        match (&self.os, &self.arch) {
            (Some(o), Some(a)) => parts.push(format!("{o}/{a}")),
            (Some(o), None) => parts.push(o.clone()),
            (None, Some(a)) => parts.push(a.clone()),
            (None, None) => {}
        }
        if !self.labels.is_empty() {
            parts.push(format!("with {}", self.labels.join(", ")));
        }
        if let Some(h) = &self.harness {
            parts.push(format!("with {h}"));
        }
        if parts.is_empty() { "any runner".into() } else { parts.join(" ") }
    }
}

/// `32g`, `512m`, `1.5t`, plain bytes.
fn bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (num, unit) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => s.split_at(i),
        None => (s, ""),
    };
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = n * mult;
    (v.is_finite() && v >= 0.0).then_some(v as u64)
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
/// was passed over. `me` comes first whenever it suits and has memory to spare ([`ROOM`]): work goes
/// to another machine only for what this one isn't or hasn't (another platform, a GPU, more cores
/// or memory than it has).
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
            score += if !s.overloaded && (r.mem_total == 0 || r.mem_free() >= ROOM) { HOME } else { 0.5 };
        }
        let why = format!(
            "{} · {:.0} of {} cores idle · {} free{}{}{}",
            r.platform(),
            s.idle,
            r.cpus,
            human_bytes(r.mem_free()),
            if r.busy > 0 { format!(" · {} CI jobs", r.busy) } else { String::new() },
            if s.overloaded { format!(" · overloaded (load {:.0})", r.load1) } else { String::new() },
            if r.name == me { " · this machine" } else { "" }
        );
        ok.push(Choice { runner: r.clone(), score, why });
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
        assert_eq!((w.os.as_deref(), w.arch.as_deref()), (Some("linux"), Some("x86_64")));
        let w = Want::parse("Darwin, arm64 +gpu cpus>=8 mem>=16g harness=codex -mac2")?;
        assert_eq!((w.os.as_deref(), w.arch.as_deref()), (Some("macos"), Some("aarch64")));
        assert_eq!(w.labels, vec!["gpu".to_owned()]);
        assert_eq!((w.min_cpus, w.min_mem_free, w.harness.as_deref()), (Some(8), Some(16 << 30), Some("codex")));
        assert_eq!(w.exclude, vec!["mac2".to_owned()]);
        assert_eq!(Want::parse("adacachy")?.name.as_deref(), Some("adacachy"));
        assert!(Want::parse("plan9/amd64").is_err());
        assert!(Want::parse("mem>=lots").is_err());
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
        assert!(none.is_err_and(|e| e.to_string().contains("no runner suits windows")));
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
