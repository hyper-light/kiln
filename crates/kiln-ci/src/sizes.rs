//! Sizes from history (docs/reach.md, "Sizes from history"; docs/needs.md E6): what a reach run
//! used where it ran, the key its history is kept under, what a key's runs on one platform say it
//! needs ([`Rollup`], folded in as each run is kept, never read back whole), and the size placement
//! is given for its next run ([`predict`]), its ask always the ceiling. Pure, as the rest of
//! kiln-ci, and in integers (docs/needs.md E1): the runner measures and the server keeps; this
//! decides from what they pass in.
//!
//! - **No admission of its own** (audit H2). History only predicts; the placement core is the one
//!   admission, and it queues what doesn't fit. A prediction is made only when the person declared
//!   it (`[reach] sizes = "history"`, AGENTS.md: opt-in automation).
//! - **Memory** is predicted at the most a run that wasn't killed held: memory can't be slowed, so
//!   a run fits or is killed, and the largest seen is the only figure of the runs kept that every
//!   one of them fitted under. **CPU** at their mean milli-CPUs (CPU time over wall time): CPU is
//!   shared, not held, and the mean is the rate that serves a run's measured CPU time over its
//!   measured duration. No quantile, margin or multiplier is picked.
//! - **A kill at size S is a floor** (audit H4, L4): once a run is killed for memory in its own
//!   group, memory isn't predicted from history again until a run that wasn't killed was measured
//!   above the floor; meanwhile it's the ask. So a size never goes back to one a run was killed at.
//! - **Kills are counted only in the run's own group** (its cgroup's `memory.events`). Where kills
//!   can't be seen there (macOS, Windows, a Linux run cgroup without the memory controller), a key
//!   with any such run on that platform isn't sized from history at all.

use serde::{Deserialize, Serialize};

use crate::sched::needs::{cpus, show_size};
use crate::sched::state::{Cpus, Resources};

/// How a figure was got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// The system's own count for the whole group, exited processes included: a cgroup's
    /// `cpu.stat` and `memory.peak`.
    Exact,
    /// Read from the group's processes now and then (lb/sense's per-run observations): what one
    /// held between two reads isn't seen.
    Sampled,
    /// Not measured: nothing held the run's processes, or the system keeps no count.
    #[default]
    None,
}

impl How {
    fn words(self) -> &'static str {
        match self {
            How::Exact => "counted by the system",
            How::Sampled => "sampled",
            How::None => "not measured",
        }
    }
}

/// Where a run's OOM kills were counted (audit H4: only its own group's count).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Counted {
    /// Its own cgroup's `memory.events` (`oom_kill`: its processes killed by any OOM killer, the
    /// machine's included; Linux's Documentation/admin-guide/cgroup-v2.rst).
    Group,
    /// Nowhere: macOS and Windows keep no such count for a process tree, and a Linux run cgroup
    /// without the memory controller has no `memory.events`.
    #[default]
    Nowhere,
}

/// What a run used, measured where it ran, from its start to its end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// The most memory its group held at once, in bytes (0: not measured).
    pub peak_mem: u64,
    pub mem_how: How,
    /// The CPU time all its processes used, those that ended before it included, in µs.
    pub cpu_us: u64,
    pub cpu_how: How,
    /// From its start to its end, in ms.
    pub duration_ms: u64,
    /// Its exit code; none: it ended by a signal, or never started.
    pub exit: Option<i32>,
    /// OOM kills of its processes while it ran, where [`Usage::oom_counted`] says.
    pub oom_kills: u64,
    pub oom_counted: Counted,
    /// The bytes its processes read and wrote (W05-R26): its cgroup's `io.stat` where the io
    /// controller is delegated to it (Linux), its job object's I/O counters (Windows); none where
    /// the system counts none for a process tree (macOS counts operations, not bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bytes: Option<u64>,
}

impl Usage {
    /// Whether it was killed for memory: a kill counted in its own group, yes; none counted
    /// there, no; nowhere to count, unknown.
    pub fn killed(&self) -> Option<bool> {
        match self.oom_counted {
            Counted::Group => Some(self.oom_kills > 0),
            Counted::Nowhere => None,
        }
    }

    /// Its CPUs on average, in thousandths: µs of CPU over ms of wall time is exactly that. None
    /// without either.
    pub fn milli_cpus(&self) -> Option<u64> {
        (self.duration_ms > 0 && self.cpu_how != How::None).then(|| self.cpu_us / self.duration_ms)
    }

    /// In words, for its answer: "3.1 GiB at most (counted by the system), 6 min 52 s of CPU
    /// (counted by the system, 3.168 CPUs on average) over 2 min 10 s; no OOM kill in its group".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        parts.push(match self.mem_how {
            How::None => "memory not measured".to_owned(),
            how => format!("{} at most ({})", show_size(self.peak_mem), how.words()),
        });
        if self.cpu_how != How::None {
            let cores = self.milli_cpus().map_or_else(String::new, |m| format!(", {} on average", cpus(Cpus::milli(m))));
            parts.push(format!("{} of CPU ({}{cores})", span(self.cpu_us / 1000), self.cpu_how.words()));
        }
        let mut s = format!("{} over {}", parts.join(", "), span(self.duration_ms));
        match self.killed() {
            Some(true) => s.push_str(&format!("; killed for memory ({} OOM kill{} in its group)", self.oom_kills, plural(self.oom_kills))),
            Some(false) => s.push_str("; no OOM kill in its group"),
            None => s.push_str("; OOM kills not seen here"),
        }
        s
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A span in words: `850 ms`, `12 s`, `2 min 10 s`, `1 h 4 min`.
fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => format!("{ms} ms"),
        1..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// A counter that only grows (OOM kills), read when a run started and when it ended: how much it
/// grew meanwhile, none where either read failed. A counter that went back (its cgroup made again)
/// counts from nothing.
pub fn grew(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    let (b, a) = (before?, after?);
    Some(if a >= b { a - b } else { a })
}

/// The value of `key` in a flat keyed file (`memory.events`, `cpu.stat`: `key value` lines).
pub fn keyed(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| l.split_once(' ').filter(|(k, _)| *k == key).and_then(|(_, v)| v.trim().parse().ok()))
}

/// The command as its history is keyed: `root` (the top of the asking tree) written `.` wherever it
/// stands as a whole path, and spaces collapsed, so the same command in another worktree of the
/// repository is the same command. Never the commit or the files: history is about what a command
/// is, not what it was run over.
pub fn command_key(root: Option<&str>, command: &str) -> String {
    let command = match root.map(|r| r.trim_end_matches(['/', '\\'])).filter(|r| r.len() > 1) {
        Some(r) => as_dot(command, r),
        None => command.to_owned(),
    };
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `s` with each whole `root` path in it written `.` (not `root` as the start of a longer name).
fn as_dot(s: &str, root: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(root) {
        let before = rest.get(..i).unwrap_or_default();
        let after = rest.get(i + root.len()..).unwrap_or_default();
        // A whole path: nothing of a name runs into it on either side.
        let starts = before.chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '\\')));
        let ends = after.chars().next().is_none_or(|c| c == '/' || c == '\\' || !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')));
        out.push_str(before);
        out.push_str(if starts && ends { "." } else { root });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A repository's name from its remote's URL (`https://github.com/hyper-light/kiln.git`,
/// `git@github.com:hyper-light/kiln`): `hyper-light/kiln`.
pub fn repo_of_remote(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let mut parts = s.rsplit(['/', ':']);
    let name = parts.next().filter(|n| !n.is_empty())?;
    let owner = parts.next().filter(|o| !o.is_empty() && !o.contains('@'))?;
    Some(format!("{owner}/{name}"))
}

/// One run of a key as the store keeps it: when, the memory it was given, and what it used.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    /// When it was kept (ms since the epoch, the server's clock).
    pub at_ms: i64,
    /// The memory it was placed at (0: none).
    pub sized_mem: u64,
    pub usage: Usage,
}

/// What a key's runs on one platform say, folded in one run at a time as each is kept
/// ([`Rollup::add`], audit H3): the store keeps one of these per key and platform beside the runs,
/// so a prediction reads one row, never the runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rollup {
    /// Runs kept.
    pub runs: u64,
    /// Of them, those whose OOM kills couldn't be counted in their own group.
    pub unseen: u64,
    /// The most memory a run that wasn't killed held (0: none measured).
    pub peak: u64,
    /// Runs killed for memory in their own group, and the floor they set: the most any of them
    /// was given or held (each needed more).
    pub kills: u64,
    pub floor: u64,
    /// The runs' milli-CPUs, summed, and how many were measured: their mean is the one over the
    /// other.
    pub milli_sum: u64,
    pub milli_n: u64,
    /// The oldest and newest kept (ms since the epoch).
    pub since_ms: i64,
    pub until_ms: i64,
    /// The bytes its runs read and wrote, summed, and how many were measured (W05-R26): a key's
    /// I/O volume, the mean one over the other, what an I/O-heavy ask is known by.
    pub read_sum: u64,
    pub write_sum: u64,
    pub io_n: u64,
}

impl Rollup {
    /// `run` folded in. Every part is a sum or a most, so the order runs come in doesn't matter.
    pub fn add(&mut self, run: &Kept) {
        let u = &run.usage;
        self.since_ms = if self.runs == 0 { run.at_ms } else { self.since_ms.min(run.at_ms) };
        self.until_ms = if self.runs == 0 { run.at_ms } else { self.until_ms.max(run.at_ms) };
        self.runs = self.runs.saturating_add(1);
        match u.killed() {
            Some(true) => {
                self.kills = self.kills.saturating_add(1);
                self.floor = self.floor.max(run.sized_mem).max(u.peak_mem);
            }
            Some(false) => self.peak = self.peak.max(if u.mem_how == How::None { 0 } else { u.peak_mem }),
            None => {
                self.unseen = self.unseen.saturating_add(1);
                self.peak = self.peak.max(if u.mem_how == How::None { 0 } else { u.peak_mem });
            }
        }
        if let Some(m) = u.milli_cpus() {
            self.milli_sum = self.milli_sum.saturating_add(m);
            self.milli_n = self.milli_n.saturating_add(1);
        }
        if let (Some(r), Some(w)) = (u.read_bytes, u.write_bytes) {
            self.read_sum = self.read_sum.saturating_add(r);
            self.write_sum = self.write_sum.saturating_add(w);
            self.io_n = self.io_n.saturating_add(1);
        }
    }

    /// Their mean bytes read and written (none measured: none).
    pub fn mean_io(&self) -> Option<(u64, u64)> {
        Some((self.read_sum.checked_div(self.io_n)?, self.write_sum.checked_div(self.io_n)?))
    }

    /// `runs` folded from nothing: what the store makes again for a key whose oldest runs went
    /// with the retention.
    pub fn of<'a>(runs: impl IntoIterator<Item = &'a Kept>) -> Rollup {
        let mut r = Rollup::default();
        for k in runs {
            r.add(k);
        }
        r
    }

    /// Their mean CPUs, in thousandths (none measured: none).
    pub fn mean_milli(&self) -> Option<u64> {
        self.milli_sum.checked_div(self.milli_n)
    }

    /// The floor binds: a run was killed, and none that wasn't was measured above what it was given
    /// or held.
    pub fn floor_binds(&self) -> bool {
        self.kills > 0 && self.peak <= self.floor
    }

    /// In words: "6.1 GiB at most, 3.2 CPUs on average over 14 runs".
    fn words(&self) -> String {
        let mut parts = Vec::new();
        if self.peak > 0 {
            parts.push(format!("{} at most", show_size(self.peak)));
        }
        if let Some(m) = self.mean_milli() {
            parts.push(format!("{} on average", cpus(Cpus::milli(m))));
        }
        if self.kills > 0 {
            parts.push(format!("killed for memory {} time{}, at up to {}", self.kills, plural(self.kills), show_size(self.floor)));
        }
        if parts.is_empty() {
            parts.push("nothing measured".to_owned());
        }
        format!("{} over {} run{}", parts.join(", "), self.runs, plural(self.runs))
    }
}

/// What sizes runs, as the person declared it (`[reach] sizes`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// What each run asks: kiln predicts nothing (the default).
    #[default]
    Ask,
    /// What its command used before on the platform, the ask the ceiling.
    History,
}

/// What the person declared (`[reach]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub sizes: Source,
    /// A run killed for memory below its ask, sized from history, runs once more at its ask.
    pub retry_oom: bool,
}

/// Why a run has the size it has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum By {
    /// Its ask.
    #[default]
    Ask,
    /// Its command's history on the platform, under its ask.
    History,
    /// Its ask again, after it was killed for memory at a smaller size.
    Retry,
}

/// The size a run is placed at, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Size {
    pub cpu: Cpus,
    pub mem: u64,
    pub by: By,
    /// In words, for its answer.
    pub why: String,
}

impl Size {
    /// Its ask, said as `why`.
    pub fn ask(ask: &Resources, why: String) -> Self {
        Self { cpu: ask.cpu, mem: ask.mem, by: By::Ask, why }
    }

    /// As the request placement weighs: the ask's other dimensions (disk, GPUs) as asked.
    pub fn request(&self, ask: &Resources) -> Resources {
        Resources { cpu: self.cpu, mem: self.mem, ..*ask }
    }
}

/// What a request asks, in words: "40 GiB, 12 CPUs", or "nothing".
fn ask_words(cpu: Cpus, mem: u64) -> String {
    let mut parts = Vec::new();
    if mem > 0 {
        parts.push(show_size(mem));
    }
    if !cpu.is_zero() {
        parts.push(cpus(cpu));
    }
    if parts.is_empty() { "nothing".to_owned() } else { parts.join(", ") }
}

/// The size history predicts for a run asking `ask` on a platform where its key's runs folded to
/// `rollup`: none unless the person declared sizing from history, and none for a key with no runs
/// there (placement goes by the ask as given). Otherwise the ask where kills can't be seen there or
/// a kill's floor binds, saying so; else memory at the peak and CPU at the mean, each under the
/// ask where it asks (where it asks nothing, history gives memory, which can't be slowed, and never
/// CPU, which is shared).
pub fn predict(ask: &Resources, rollup: Option<&Rollup>, policy: &Policy) -> Option<Size> {
    if policy.sizes != Source::History {
        return None;
    }
    let r = rollup.filter(|r| r.runs > 0)?;
    if r.unseen > 0 {
        return Some(Size::ask(ask, format!("by its ask: {} of its runs here ran where OOM kills can't be seen, so history can't size it ({})", r.unseen, r.words())));
    }
    let mem = if r.floor_binds() {
        if ask.mem > 0 { ask.mem } else { r.floor }
    } else if r.peak == 0 {
        ask.mem
    } else if ask.mem > 0 {
        r.peak.min(ask.mem)
    } else {
        r.peak
    };
    let cpu = match r.mean_milli() {
        Some(m) if !ask.cpu.is_zero() => Cpus::milli(m).min(ask.cpu),
        _ => ask.cpu,
    };
    if mem == ask.mem && cpu == ask.cpu {
        let why = if r.floor_binds() { "by its ask: killed for memory here, and no run since measured above that" } else { "by its ask, the ceiling" };
        return Some(Size::ask(ask, format!("{why} (history here: {})", r.words())));
    }
    let asked = if ask.mem > 0 || !ask.cpu.is_zero() { format!("asked {}, the ceiling", ask_words(ask.cpu, ask.mem)) } else { "it asked nothing".to_owned() };
    Some(Size { cpu, mem, by: By::History, why: format!("sized from history: {} ({}; {asked})", ask_words(cpu, mem), r.words()) })
}

/// The size a run killed for memory goes again at, when the person declared it: its ask, once, and
/// only when it ran sized from history below its ask in memory. Never a retry's retry.
pub fn retry(ask: &Resources, ran: &Size, killed: Option<bool>, policy: &Policy) -> Option<Size> {
    let declared = policy.retry_oom && policy.sizes == Source::History;
    (declared && killed == Some(true) && ran.by == By::History && ask.mem > ran.mem).then(|| Size {
        cpu: ask.cpu,
        mem: ask.mem,
        by: By::Retry,
        why: format!("again at its ask, {} ([reach] retry_oom): killed for memory sized from history at {}", ask_words(ask.cpu, ask.mem), show_size(ran.mem)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GIB: u64 = 1 << 30;

    fn ok(peak: u64, cpu_s: u64, secs: u64) -> Usage {
        Usage { peak_mem: peak, mem_how: How::Exact, cpu_us: cpu_s * 1_000_000, cpu_how: How::Exact, duration_ms: secs * 1000, exit: Some(0), oom_kills: 0, oom_counted: Counted::Group, read_bytes: None, write_bytes: None }
    }

    fn killed(peak: u64) -> Usage {
        Usage { exit: None, oom_kills: 1, ..ok(peak, 1, 1) }
    }

    fn kept(at: i64, sized_mem: u64, usage: Usage) -> Kept {
        Kept { at_ms: at, sized_mem, usage }
    }

    fn on() -> Policy {
        Policy { sizes: Source::History, retry_oom: false }
    }

    fn ask(cpu: u64, mem: u64) -> Resources {
        Resources { cpu: Cpus::whole(cpu), mem, ..Resources::default() }
    }

    /// W05-R26: a key's I/O volume is the mean of its runs that measured theirs; one that didn't
    /// (macOS's) doesn't pull it down, and old rows without it read as unmeasured.
    #[test]
    fn w05_r26_rollup_keeps_the_keys_io_volume() -> Result<(), serde_json::Error> {
        let io = |r: u64, w: u64| Usage { read_bytes: Some(r), write_bytes: Some(w), ..ok(GIB, 1, 1) };
        let r = Rollup::of(&[kept(1, 0, io(100, 10)), kept(2, 0, io(300, 30)), kept(3, 0, ok(GIB, 1, 1))]);
        assert_eq!(r.mean_io(), Some((200, 20)));
        assert_eq!(Rollup::of(&[kept(1, 0, ok(GIB, 1, 1))]).mean_io(), None);
        let old: Usage = serde_json::from_str(r#"{"peak_mem":1,"duration_ms":5}"#)?;
        assert_eq!((old.read_bytes, old.write_bytes), (None, None));
        Ok(())
    }

    /// H4: only a kill counted in the run's own group is its; where none can be counted, unknown.
    #[test]
    fn h4_oom_kills_count_only_in_the_runs_own_group() {
        let u = |counted, kills| Usage { oom_counted: counted, oom_kills: kills, exit: Some(1), ..Usage::default() };
        assert_eq!(u(Counted::Group, 1).killed(), Some(true));
        assert_eq!(u(Counted::Group, 0).killed(), Some(false));
        assert_eq!(u(Counted::Nowhere, 0).killed(), None);
        // The counters as the kernel writes them, before and after.
        let events = "low 0\nhigh 0\nmax 0\noom 3\noom_kill 3\noom_group_kill 0\n";
        assert_eq!(keyed(events, "oom_kill"), Some(3));
        assert_eq!(keyed("usage_usec 1089\nuser_usec 900\n", "usage_usec"), Some(1089));
        assert_eq!(keyed(events, "missing"), None);
        assert_eq!(grew(Some(1089), Some(1091)), Some(2));
        assert_eq!(grew(Some(5), Some(1)), Some(1), "a counter made again counts from nothing");
        assert_eq!(grew(None, Some(1)), None);
    }

    #[test]
    fn e6_a_runs_use_in_words() {
        let u = ok(3 * GIB + GIB / 10, 412, 130);
        assert_eq!(u.milli_cpus(), Some(3169));
        assert_eq!(u.describe(), "3.1 GiB at most (counted by the system), 6 min 52 s of CPU (counted by the system, 3.169 CPUs on average) over 2 min 10 s; no OOM kill in its group");
        assert!(killed(GIB).describe().ends_with("; killed for memory (1 OOM kill in its group)"), "{}", killed(GIB).describe());
        let none = Usage { duration_ms: 850, ..Usage::default() };
        assert_eq!(none.describe(), "memory not measured over 850 ms; OOM kills not seen here");
    }

    /// E6: the worktree's path becomes `.` only as a whole path, spaces collapse, a remote's
    /// owner/name is the repository, and two worktrees of one repository give one command.
    #[test]
    fn e6_the_command_is_keyed_as_written_with_its_tree_as_dot() {
        let root = "/Users/ada/.cache/wt-x";
        assert_eq!(command_key(Some(root), "cd /Users/ada/.cache/wt-x && cargo  test -p kiln"), "cd . && cargo test -p kiln");
        assert_eq!(command_key(Some("/home/u/wt"), "cargo test --manifest-path /home/u/wt/crates/a/Cargo.toml"), "cargo test --manifest-path ./crates/a/Cargo.toml");
        assert_eq!(command_key(Some("/home/u/wt"), "ls /home/u/wt2 /home/u/wt"), "ls /home/u/wt2 .");
        assert_eq!(command_key(Some("/home/u/wt"), "ls /x/home/u/wt"), "ls /x/home/u/wt", "only a whole path");
        assert_eq!(command_key(Some("/home/u/wt/"), "make -C /home/u/wt"), "make -C .");
        assert_eq!(command_key(None, "  cargo\ttest \n -q "), "cargo test -q");
        assert_eq!(command_key(Some("/"), "ls /"), "ls /", "the filesystem's root is never written `.`");
        assert_eq!(command_key(Some("/a/wt-1"), "cd /a/wt-1 && cargo test"), command_key(Some("/b/wt-2"), "cd /b/wt-2 && cargo test"));
        for (url, repo) in [("https://github.com/hyper-light/kiln.git", "hyper-light/kiln"), ("git@github.com:hyper-light/kiln", "hyper-light/kiln"), ("ssh://git@host:22/acme/app/", "acme/app")] {
            assert_eq!(repo_of_remote(url).as_deref(), Some(repo), "{url}");
        }
        assert_eq!(repo_of_remote("kiln"), None);
    }

    /// H2: history predicts only once the person declared it; undeclared or cold, placement has
    /// nothing from it and goes by the ask as given.
    #[test]
    fn h2_history_predicts_only_when_declared() {
        let r = Rollup::of(&[kept(1, 0, ok(6 * GIB, 300, 100))]);
        let a = ask(12, 40 * GIB);
        assert_eq!(predict(&a, Some(&r), &Policy::default()), None, "undeclared: no prediction");
        assert_eq!(predict(&a, Some(&r), &Policy { sizes: Source::Ask, retry_oom: true }), None);
        assert_eq!(predict(&a, None, &on()), None, "cold: no prediction");
        assert_eq!(predict(&a, Some(&Rollup::default()), &on()), None);
        assert!(predict(&a, Some(&r), &on()).is_some_and(|s| s.by == By::History && s.mem == 6 * GIB));
    }

    /// E6: the plan's example. The gate asks 40 GiB and 12 CPUs; its history is 6.1 GiB at most and
    /// 3.2 CPUs on average over 2 runs.
    #[test]
    fn e6_the_gate_is_sized_from_its_history_under_its_ask() {
        let a = ask(12, 40 * GIB);
        let r = Rollup::of(&[kept(1, 0, ok(6 * GIB + GIB / 10, 384, 120)), kept(2, 0, ok(5 * GIB, 384, 120))]);
        let s = predict(&a, Some(&r), &on()).unwrap_or_default();
        assert_eq!((s.mem, s.cpu, s.by), (6 * GIB + GIB / 10, Cpus::milli(3200), By::History));
        assert_eq!(s.why, "sized from history: 6.1 GiB, 3.2 CPUs (6.1 GiB at most, 3.2 CPUs on average over 2 runs; asked 40 GiB, 12 CPUs, the ceiling)");
        // Nothing asked: memory from history, never CPU.
        let free = predict(&Resources::default(), Some(&r), &on()).unwrap_or_default();
        assert_eq!((free.mem, free.cpu, free.by), (6 * GIB + GIB / 10, Cpus::ZERO, By::History));
        assert!(free.why.ends_with("it asked nothing)"), "{}", free.why);
        // History past the ask: the ask, the ceiling.
        let big = predict(&ask(0, 4 * GIB), Some(&r), &on()).unwrap_or_default();
        assert_eq!((big.mem, big.by), (4 * GIB, By::Ask));
        assert!(big.why.starts_with("by its ask, the ceiling"), "{}", big.why);
    }

    /// H4: where any run of the key on the platform ran with its kills unseen, history doesn't size
    /// it there, and says why.
    #[test]
    fn h4_where_kills_cant_be_seen_history_doesnt_size() {
        let unseen = Usage { oom_counted: Counted::Nowhere, ..ok(2 * GIB, 10, 10) };
        let r = Rollup::of(&[kept(1, 0, ok(2 * GIB, 10, 10)), kept(2, 0, unseen)]);
        let s = predict(&ask(4, 8 * GIB), Some(&r), &on()).unwrap_or_default();
        assert_eq!((s.mem, s.cpu, s.by), (8 * GIB, Cpus::whole(4), By::Ask));
        assert!(s.why.starts_with("by its ask: 1 of its runs here ran where OOM kills can't be seen"), "{}", s.why);
    }

    /// H4: a kill at size S is a floor. A run that succeeds afterwards at its ask, measured at or
    /// under S, doesn't bring the size back to S; only one measured above S does.
    #[test]
    fn h4_a_kill_is_a_floor_until_a_peak_passes_it() {
        let a = ask(0, 32 * GIB);
        let mut runs = vec![kept(1, 32 * GIB, ok(4 * GIB, 1, 1))];
        let first = predict(&a, Some(&Rollup::of(&runs)), &on()).unwrap_or_default();
        assert_eq!((first.mem, first.by), (4 * GIB, By::History));
        // Killed at 4 GiB, having held 5.
        runs.push(kept(2, 4 * GIB, killed(5 * GIB)));
        let after_kill = predict(&a, Some(&Rollup::of(&runs)), &on()).unwrap_or_default();
        assert_eq!((after_kill.mem, after_kill.by), (32 * GIB, By::Ask));
        assert!(after_kill.why.starts_with("by its ask: killed for memory here"), "{}", after_kill.why);
        // A success at its ask, measured at 5 GiB: still the floor (no oscillation back to 5).
        runs.push(kept(3, 32 * GIB, ok(5 * GIB, 1, 1)));
        assert_eq!(predict(&a, Some(&Rollup::of(&runs)), &on()).map(|s| s.mem), Some(32 * GIB));
        // One measured above the floor: its peak.
        runs.push(kept(4, 32 * GIB, ok(7 * GIB, 1, 1)));
        assert_eq!(predict(&a, Some(&Rollup::of(&runs)), &on()).map(|s| (s.mem, s.by)), Some((7 * GIB, By::History)));
        // With nothing asked, the floor itself while it binds: the least it's known to need.
        let r = Rollup::of(&[kept(1, 0, ok(GIB, 1, 1)), kept(2, GIB, killed(3 * GIB))]);
        assert_eq!(predict(&Resources::default(), Some(&r), &on()).map(|s| s.mem), Some(3 * GIB));
    }

    /// E6: a run killed for memory below its ask goes again once at its ask, only when declared.
    #[test]
    fn e6_a_run_killed_for_memory_goes_again_once_at_its_ask_when_declared() {
        let a = ask(12, 40 * GIB);
        let ran = Size { cpu: Cpus::whole(3), mem: 6 * GIB, by: By::History, why: String::new() };
        let both = Policy { sizes: Source::History, retry_oom: true };
        let again = retry(&a, &ran, Some(true), &both);
        assert_eq!(again.as_ref().map(|s| (s.mem, s.cpu, s.by)), Some((40 * GIB, Cpus::whole(12), By::Retry)));
        assert_eq!(again.as_ref().map(|s| s.why.as_str()), Some("again at its ask, 40 GiB, 12 CPUs ([reach] retry_oom): killed for memory sized from history at 6 GiB"));
        assert_eq!(retry(&a, &ran, Some(true), &on()), None);
        assert_eq!(retry(&a, &ran, Some(true), &Policy { sizes: Source::Ask, retry_oom: true }), None);
        assert_eq!(retry(&a, &ran, Some(false), &both), None);
        assert_eq!(retry(&a, &ran, None, &both), None, "a kill not seen is never retried");
        assert_eq!(retry(&a, &Size { by: By::Ask, mem: 40 * GIB, ..ran.clone() }, Some(true), &both), None);
        if let Some(r) = &again {
            assert_eq!(retry(&a, r, Some(true), &both), None, "a retry is never retried");
        }
        assert_eq!(retry(&Resources::default(), &ran, Some(true), &both), None, "nothing asked: no ceiling to go to");
    }

    fn arb_usage() -> impl Strategy<Value = Usage> {
        (0u64..64 * GIB, 0u64..10_000_000_000, 1u64..10_000_000, prop::option::of(0i32..256), 0u64..3, any::<bool>(), any::<bool>()).prop_map(|(peak, cpu, dur, exit, kills, seen, measured)| Usage {
            peak_mem: peak,
            mem_how: if measured { How::Exact } else { How::None },
            cpu_us: cpu,
            cpu_how: How::Exact,
            duration_ms: dur,
            exit,
            oom_kills: kills,
            oom_counted: if seen { Counted::Group } else { Counted::Nowhere },
            read_bytes: None,
            write_bytes: None,
        })
    }

    proptest! {
        /// H3: the rollup kept as runs come in, in any order, is the one folded from them all.
        #[test]
        fn h3_the_rollup_folded_on_insert_is_the_batch_one(runs in prop::collection::vec((0i64..1_000_000, 0u64..64 * GIB, arb_usage()), 0..16), seed in any::<u64>()) {
            let kept: Vec<Kept> = runs.into_iter().map(|(at, sized, usage)| Kept { at_ms: at, sized_mem: sized, usage }).collect();
            let batch = Rollup::of(&kept);
            let mut shuffled = kept.clone();
            let n = shuffled.len().max(1);
            shuffled.rotate_left(usize::try_from(seed).unwrap_or(0) % n);
            shuffled.reverse();
            let mut incremental = Rollup::default();
            for k in &shuffled {
                incremental.add(k);
            }
            prop_assert_eq!(&incremental, &batch);
            prop_assert_eq!(batch.runs, kept.len() as u64);
        }

        /// E6: for any ask and history the prediction never passes the ask in a dimension asked,
        /// never adds CPU, and is the ask wherever kills went unseen.
        #[test]
        fn e6_the_ask_is_the_ceiling(
            cpu in prop::sample::select(vec![0u64, 500, 1000, 4000, 12000]),
            mem in prop::sample::select(vec![0, GIB / 2, 2 * GIB, 40 * GIB]),
            runs in prop::collection::vec((0i64..1_000_000, 0u64..64 * GIB, arb_usage()), 0..12),
        ) {
            let a = Resources { cpu: Cpus::milli(cpu), mem, ..Resources::default() };
            let kept: Vec<Kept> = runs.into_iter().map(|(at, sized, usage)| Kept { at_ms: at, sized_mem: sized, usage }).collect();
            let r = Rollup::of(&kept);
            let Some(s) = predict(&a, Some(&r), &on()) else { prop_assert!(kept.is_empty()); return Ok(()) };
            if mem > 0 { prop_assert!(s.mem <= mem, "{s:?}"); }
            prop_assert!(s.cpu <= a.cpu, "CPU is never added: {s:?}");
            if r.unseen > 0 { prop_assert_eq!((s.mem, s.cpu, s.by), (mem, a.cpu, By::Ask)); }
            prop_assert!(s.by != By::Retry);
        }

        /// L4: sizes never oscillate between killed and succeeding. A command that needs `need`
        /// bytes, asked at `ask` >= need, run again and again with its size from history: a run
        /// given less than it needs is killed (holding any amount up to `need` when it is), one given
        /// enough succeeds measured anywhere up to `need` (exactly, or a reading under it). After a
        /// kill at size S no later run is ever given S or less, so the kills are strictly rising and
        /// end; and with exact readings no run is ever killed.
        #[test]
        fn l4_sizes_never_go_back_to_a_size_that_was_killed(
            need in GIB..32 * GIB,
            over in 0u64..32 * GIB,
            readings in prop::collection::vec(0u64..=1000, 1..40),
            exact in any::<bool>(),
        ) {
            let a = Resources { mem: need.saturating_add(over), ..Resources::default() };
            let mut r = Rollup::default();
            let mut killed_at: Vec<u64> = Vec::new();
            for (i, permille) in readings.iter().enumerate() {
                let size = predict(&a, Some(&r), &on()).map_or(a.mem, |s| s.mem);
                prop_assert!(size <= a.mem);
                for s in &killed_at {
                    prop_assert!(size > *s, "run {i} given {size}, at or under {s}, where a run was killed: {r:?}");
                }
                let held = if exact { need } else { need / 1000 * permille };
                let usage = if size < need { killed(held.min(size.max(held))) } else { ok(held, 1, 1) };
                if size < need {
                    prop_assert!(!exact, "with exact readings no run is killed: run {i} at {size} of {need}");
                    killed_at.push(size);
                }
                r.add(&Kept { at_ms: i64::try_from(i).unwrap_or(0), sized_mem: size, usage });
            }
            // Kills strictly rise.
            prop_assert!(killed_at.windows(2).all(|w| w.first() < w.get(1)), "{killed_at:?}");
        }
    }
}
