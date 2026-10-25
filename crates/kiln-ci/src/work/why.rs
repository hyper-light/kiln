//! Reports and why (docs/ci.md W24-R28, AC09): what each unit of work did (ran, was reused, or
//! ran warm) and, for one that ran, why: the fingerprint components (W24-R01) that differ from
//! the nearest earlier run's, each in a line that reads the same every time (`Cargo.lock
//! a1→b2`, `rustc 1.91→1.92`). Pure: the caller passes the units and what it knows of the run.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::key::{Kind, PathSet, Read, Unit};

/// One component of a fingerprint, as a why-diff names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "component", rename_all = "kebab-case")]
pub enum Component {
    /// The unit's kind: a run, a leg, a step, an action or a test.
    Kind,
    /// Its definition: the job or step after evaluation.
    Definition,
    /// A secret it receives, by name.
    Secret { name: String },
    /// A variable of the environment its key keeps.
    Env { name: String },
    /// A field of its platform: `os`, `arch`, `libc`, `executor` or `image`.
    Platform { field: String },
    /// A toolchain's identity.
    Toolchain { name: String },
    /// A declared input's tree (or blob) id, by its path or glob.
    Input { path: String },
    /// An upstream unit's output digest, by name.
    Upstream { name: String },
    /// Its trust domain.
    Trust,
    /// A path it was seen to read: its strong key (W24-R04).
    Read { path: String },
}

/// One component that differs, with its values before and after (`None`: it had none there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    #[serde(flatten)]
    pub component: Component,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The change in one line, the same for the same change every time.
    pub line: String,
}

/// The characters a value keeps in a line, beyond which it's cut.
const LONG: usize = 40;

/// The hex digits of a digest: hex alone (a git object id), or after `algorithm:`
/// (`sha256:…`), 16 digits or more.
fn hex_of(v: &str) -> Option<(&str, &str)> {
    let (algorithm, hex) = match v.split_once(':') {
        Some((a, h)) if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric()) => (a, h),
        _ => ("", v),
    };
    (hex.len() >= 16 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some((algorithm, hex))
}

/// A value as a line shows it. `absent` where there's none. A digest is cut to its first 8
/// digits, or as many more as tell it from `other`'s; text is put on one line, between backquotes
/// when it holds spaces or nothing, and cut at [`LONG`] characters.
fn shown(v: Option<&str>, other: Option<&str>, absent: &str) -> String {
    let Some(v) = v else { return absent.to_owned() };
    if let Some((algorithm, hex)) = hex_of(v) {
        let same = other.and_then(hex_of).map_or(0, |(_, o)| hex.chars().zip(o.chars()).take_while(|(a, b)| a == b).count());
        let digits: String = hex.chars().take((same + 1).max(8)).collect();
        return if algorithm.is_empty() { digits } else { format!("{algorithm}:{digits}") };
    }
    let mut one = String::new();
    for c in v.chars() {
        if c.is_control() {
            one.extend(c.escape_default());
        } else {
            one.push(c);
        }
    }
    let cut: String = one.chars().take(LONG).collect();
    let cut = if one.chars().count() > LONG { format!("{cut}…") } else { cut };
    if cut.is_empty() || v.chars().any(char::is_whitespace) { format!("`{cut}`") } else { cut }
}

fn change(component: Component, before: Option<&str>, after: Option<&str>, label: &str, absent: &str) -> Change {
    let line = format!("{label} {}→{}", shown(before, after, absent), shown(after, before, absent));
    Change { component, before: before.map(str::to_owned), after: after.map(str::to_owned), line }
}

/// The entries of two maps that differ, in key order.
fn maps(out: &mut Vec<Change>, before: &BTreeMap<String, String>, after: &BTreeMap<String, String>, component: fn(String) -> Component, label: &dyn Fn(&str) -> String, absent: &str) {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for k in keys {
        let (b, a) = (before.get(k).map(String::as_str), after.get(k).map(String::as_str));
        if b != a {
            out.push(change(component(k.clone()), b, a, &label(k), absent));
        }
    }
}

fn kind(k: Kind) -> &'static str {
    match k {
        Kind::Run => "run",
        Kind::Leg => "leg",
        Kind::Step => "step",
        Kind::Action => "action",
        Kind::Test => "test",
    }
}

/// A definition's change: the first line that differs, with its number.
fn definition(before: &str, after: &str) -> Change {
    let (b, a): (Vec<&str>, Vec<&str>) = (before.split('\n').collect(), after.split('\n').collect());
    let at = (0..b.len().max(a.len())).find(|i| b.get(*i) != a.get(*i)).unwrap_or(0);
    let quoted = |l: Option<&&str>, o: Option<&&str>| match l {
        Some(l) => {
            let s = shown(Some(*l), o.copied(), "(none)");
            if s.starts_with('`') { s } else { format!("`{s}`") }
        }
        None => "(none)".to_owned(),
    };
    let line = format!("definition, line {}: {}→{}", at + 1, quoted(b.get(at), a.get(at)), quoted(a.get(at), b.get(at)));
    Change { component: Component::Definition, before: Some(before.to_owned()), after: Some(after.to_owned()), line }
}

/// What differs between two units' fingerprints (W24-R28), component by component in the order
/// keys encode them (W24-R01): kind, definition, secrets, environment, platform, toolchains,
/// inputs, upstream outputs, trust; within each, by name. Empty when they'd key alike.
pub fn diff(before: &Unit, after: &Unit) -> Vec<Change> {
    let mut out = Vec::new();
    if before.kind != after.kind {
        out.push(change(Component::Kind, Some(kind(before.kind)), Some(kind(after.kind)), "kind", "(none)"));
    }
    if before.definition != after.definition {
        out.push(definition(&before.definition, &after.definition));
    }
    for name in before.secrets.symmetric_difference(&after.secrets) {
        let added = after.secrets.contains(name);
        let line = format!("secret {name} {}", if added { "added" } else { "removed" });
        let (b, a) = if added { (None, Some(name.clone())) } else { (Some(name.clone()), None) };
        out.push(Change { component: Component::Secret { name: name.clone() }, before: b, after: a, line });
    }
    maps(&mut out, &before.env, &after.env, |name| Component::Env { name }, &|k| format!("env {k}"), "(unset)");
    let (p, q) = (&before.platform, &after.platform);
    for (field, b, a) in [("os", Some(&p.os), Some(&q.os)), ("arch", Some(&p.arch), Some(&q.arch)), ("libc", Some(&p.libc), Some(&q.libc)), ("executor", Some(&p.executor), Some(&q.executor)), ("image", p.image.as_ref(), q.image.as_ref())] {
        if b != a {
            out.push(change(Component::Platform { field: field.to_owned() }, b.map(String::as_str), a.map(String::as_str), field, "(none)"));
        }
    }
    maps(&mut out, &p.toolchains, &q.toolchains, |name| Component::Toolchain { name }, &|k| k.to_owned(), "(none)");
    maps(&mut out, &before.inputs, &after.inputs, |path| Component::Input { path }, &|k| k.to_owned(), "(none)");
    maps(&mut out, &before.upstream, &after.upstream, |name| Component::Upstream { name }, &|k| format!("upstream {k}"), "(none)");
    if before.trust != after.trust {
        out.push(change(Component::Trust, Some(&before.trust), Some(&after.trust), "trust", "(none)"));
    }
    out
}

fn read(r: &Read) -> String {
    match r {
        Read::File(d) => d.clone(),
        Read::Dir(d) => format!("dir:{d}"),
        Read::Absent => "(absent)".to_owned(),
    }
}

/// What differs between two units' reads, path by path (their strong keys, W24-R04): a file or
/// folder that changed, one found where none was, one no longer read.
pub fn diff_reads(before: &PathSet, after: &PathSet) -> Vec<Change> {
    let as_text = |s: &PathSet| -> BTreeMap<String, String> { s.iter().map(|(p, r)| (p.clone(), read(r))).collect() };
    let mut out = Vec::new();
    maps(&mut out, &as_text(before), &as_text(after), |path| Component::Read { path }, &|k| format!("read {k}"), "(not read)");
    out
}

/// The nearest of `earlier` to `unit`, for "why did this rebuild?" (W24-R28): the one fewest
/// components apart (the first of those: callers list the newest first), and what differs.
pub fn nearest<'a, T>(unit: &Unit, earlier: &'a [(T, Unit)]) -> Option<(&'a T, Vec<Change>)> {
    earlier.iter().map(|(t, u)| (t, diff(u, unit))).min_by_key(|(_, d)| d.len())
}

/// How a unit came by its result (W24-R28).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "kebab-case")]
pub enum How {
    /// It ran, with nothing kept to start from.
    Ran,
    /// It took an earlier result over: run `run` (its number, `number`), leg `leg`, which
    /// `runner` ran.
    Reused {
        run: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leg: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runner: Option<String>,
    },
    /// It ran warm in a kept workspace (W24-R11): which one, how many commits its commit was
    /// behind and files changed between the two, and the units its tools rebuilt (cargo's
    /// `fresh: false`), where counted.
    Warm {
        workspace: String,
        behind: u32,
        files: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rebuilt: Option<u32>,
    },
}

/// What a unit says of itself (W24-R28): how it came by its result, its tools' cache hits, the
/// time it saved against a cold run, and, for one that ran, why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    #[serde(flatten)]
    pub how: How,
    /// Its tools' cache hits (W24-R08), where counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<u64>,
    /// Seconds saved against a cold run, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_s: Option<f64>,
    /// Why it ran: what differs from the nearest earlier run's fingerprint ([`diff`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<Change>,
}

/// Seconds as a person reads them: `40 s`, `14 m 3 s`, `2 h 5 m`.
fn span(s: f64) -> String {
    let s = if s.is_finite() { s.max(0.0).round() as u64 } else { 0 };
    match (s / 3_600, s % 3_600 / 60, s % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, 0) => format!("{m} m"),
        (0, m, s) => format!("{m} m {s} s"),
        (h, 0, _) => format!("{h} h"),
        (h, m, _) => format!("{h} h {m} m"),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Report {
    /// The report in a sentence: `reused from run #12 on fedora, 14 m 3 s saved`; `warm in
    /// workspace w3, 2 commits behind, 5 files changed, 3 units rebuilt, 40 s saved`; `ran:
    /// Cargo.lock a1→b2; rustc 1.91→1.92`.
    pub fn summary(&self) -> String {
        let mut parts = vec![match &self.how {
            How::Ran if self.why.is_empty() => "ran".to_owned(),
            How::Ran => format!("ran: {}", self.why.iter().map(|c| c.line.as_str()).collect::<Vec<_>>().join("; ")),
            How::Reused { run, number, runner, .. } => {
                let from = number.map_or_else(|| format!("run {run}"), |n| format!("run #{n}"));
                runner.as_ref().map_or_else(|| format!("reused from {from}"), |r| format!("reused from {from} on {r}"))
            }
            How::Warm { workspace, behind, files, rebuilt } => {
                let mut s = format!("warm in workspace {workspace}, {} behind, {} changed", plural(u64::from(*behind), "commit", "commits"), plural(u64::from(*files), "file", "files"));
                if let Some(n) = rebuilt {
                    s.push_str(&format!(", {} rebuilt", plural(u64::from(*n), "unit", "units")));
                }
                s
            }
        }];
        if let Some(h) = self.hits {
            parts.push(plural(h, "cache hit", "cache hits"));
        }
        if let Some(s) = self.saved_s.filter(|s| *s > 0.0) {
            parts.push(format!("{} saved", span(s)));
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::key::{Platform, weak};
    use proptest::prelude::*;

    fn base() -> Unit {
        Unit {
            kind: Kind::Leg,
            definition: "steps:\n  - run: cargo test\n".into(),
            secrets: BTreeSet::from(["REGISTRY_TOKEN".to_owned()]),
            env: BTreeMap::from([("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())]),
            platform: Platform {
                os: "linux".into(),
                arch: "x86_64".into(),
                libc: "gnu".into(),
                executor: "host".into(),
                image: None,
                toolchains: BTreeMap::from([("rustc".to_owned(), "1.91".to_owned())]),
            },
            inputs: BTreeMap::from([("Cargo.lock".to_owned(), "a1".to_owned()), ("crates/**".to_owned(), "c1".to_owned())]),
            upstream: BTreeMap::from([("build/target".to_owned(), "sha256:00ff00ff00ff00ff00ff00ff00ff00ff".to_owned())]),
            trust: "trusted".into(),
        }
    }

    fn named(c: &Change) -> (&Component, &str) {
        (&c.component, c.line.as_str())
    }

    /// A map's entry set, or gone with `None`.
    fn set(m: &mut BTreeMap<String, String>, k: &str, v: Option<&str>) {
        match v {
            Some(v) => {
                m.insert(k.to_owned(), v.to_owned());
            }
            None => {
                m.remove(k);
            }
        }
    }

    /// W24-AC09: a table of edits, each to one component; the why-diff names exactly that
    /// component, in a line that says what it was and is, and backwards the same, swapped.
    #[test]
    fn w24_r28_ac09_the_diff_names_exactly_the_changed_component() {
        let comp = |c: &str, name: &str| match c {
            "env" => Component::Env { name: name.into() },
            "secret" => Component::Secret { name: name.into() },
            "platform" => Component::Platform { field: name.into() },
            "toolchain" => Component::Toolchain { name: name.into() },
            "input" => Component::Input { path: name.into() },
            "upstream" => Component::Upstream { name: name.into() },
            "kind" => Component::Kind,
            "trust" => Component::Trust,
            _ => Component::Definition,
        };
        type Edit = fn(&mut Unit);
        let table: Vec<(Edit, Component, &str)> = vec![
            (|u: &mut Unit| set(&mut u.inputs, "Cargo.lock", Some("b2")), comp("input", "Cargo.lock"), "Cargo.lock a1→b2"),
            (|u: &mut Unit| set(&mut u.platform.toolchains, "rustc", Some("1.92")), comp("toolchain", "rustc"), "rustc 1.91→1.92"),
            (|u: &mut Unit| set(&mut u.platform.toolchains, "cargo", Some("1.92")), comp("toolchain", "cargo"), "cargo (none)→1.92"),
            (|u: &mut Unit| set(&mut u.inputs, "src/**", Some("d4")), comp("input", "src/**"), "src/** (none)→d4"),
            (|u: &mut Unit| set(&mut u.inputs, "crates/**", None), comp("input", "crates/**"), "crates/** c1→(none)"),
            (|u: &mut Unit| u.definition = "steps:\n  - run: cargo test --all\n".into(), comp("definition", ""), "definition, line 2: `  - run: cargo test`→`  - run: cargo test --all`"),
            (|u: &mut Unit| u.definition.push_str("  - run: cargo doc\n"), comp("definition", ""), "definition, line 3: ``→`  - run: cargo doc`"),
            (|u: &mut Unit| set(&mut u.env, "RUSTFLAGS", Some("-Dwarnings -Copt-level=3")), comp("env", "RUSTFLAGS"), "env RUSTFLAGS -Dwarnings→`-Dwarnings -Copt-level=3`"),
            (|u: &mut Unit| set(&mut u.env, "CARGO_INCREMENTAL", Some("0")), comp("env", "CARGO_INCREMENTAL"), "env CARGO_INCREMENTAL (unset)→0"),
            (|u: &mut Unit| set(&mut u.env, "RUSTFLAGS", None), comp("env", "RUSTFLAGS"), "env RUSTFLAGS -Dwarnings→(unset)"),
            (
                |u: &mut Unit| {
                    u.secrets.insert("DEPLOY_KEY".into());
                },
                comp("secret", "DEPLOY_KEY"),
                "secret DEPLOY_KEY added",
            ),
            (
                |u: &mut Unit| {
                    u.secrets.remove("REGISTRY_TOKEN");
                },
                comp("secret", "REGISTRY_TOKEN"),
                "secret REGISTRY_TOKEN removed",
            ),
            (|u: &mut Unit| u.platform.os = "macos".into(), comp("platform", "os"), "os linux→macos"),
            (|u: &mut Unit| u.platform.arch = "aarch64".into(), comp("platform", "arch"), "arch x86_64→aarch64"),
            (|u: &mut Unit| u.platform.libc = "musl".into(), comp("platform", "libc"), "libc gnu→musl"),
            (|u: &mut Unit| u.platform.executor = "docker".into(), comp("platform", "executor"), "executor host→docker"),
            (|u: &mut Unit| u.platform.image = Some("sha256:5a5a5a5a5a5a5a5a5a5a".into()), comp("platform", "image"), "image (none)→sha256:5a5a5a5a"),
            (|u: &mut Unit| set(&mut u.upstream, "build/target", Some("sha256:00ff00ff00ff11ff00ff00ff00ff00ff")), comp("upstream", "build/target"), "upstream build/target sha256:00ff00ff00ff0→sha256:00ff00ff00ff1"),
            (|u: &mut Unit| u.trust = "untrusted".into(), comp("trust", ""), "trust trusted→untrusted"),
            (|u: &mut Unit| u.kind = Kind::Step, comp("kind", ""), "kind leg→step"),
        ];
        let before = base();
        assert!(diff(&before, &before).is_empty(), "nothing changed, nothing named");
        for (edit, component, line) in table {
            let mut after = base();
            edit(&mut after);
            assert_ne!(weak(&before), weak(&after), "{line}: the edit changes the key");
            let d = diff(&before, &after);
            assert_eq!(d.iter().map(named).collect::<Vec<_>>(), vec![(&component, line)], "{line}");
            let back = diff(&after, &before);
            assert_eq!(back.iter().map(|c| (&c.component, c.before.as_ref(), c.after.as_ref())).collect::<Vec<_>>(), d.iter().map(|c| (&c.component, c.after.as_ref(), c.before.as_ref())).collect::<Vec<_>>(), "{line}: backwards, swapped");
        }
    }

    /// Several edits at once: each named once, in the order keys encode them.
    #[test]
    fn w24_r28_changes_come_in_encoding_order() {
        let mut after = base();
        after.trust = "person:ada".into();
        after.inputs.insert("Cargo.lock".into(), "b2".into());
        after.env.insert("A_FIRST".into(), "1".into());
        after.platform.toolchains.insert("rustc".into(), "1.92".into());
        let lines: Vec<String> = diff(&base(), &after).into_iter().map(|c| c.line).collect();
        assert_eq!(lines, ["env A_FIRST (unset)→1", "rustc 1.91→1.92", "Cargo.lock a1→b2", "trust trusted→person:ada"]);
    }

    /// Lines read the same every time: digests cut to 8 digits, or as many as tell the two
    /// apart; text on one line, cut, between backquotes when it holds spaces.
    #[test]
    fn w24_r28_lines_are_stable_and_short() {
        let a = "a".repeat(30) + "0123456789";
        let b = "a".repeat(30) + "9876543210";
        assert_eq!(shown(Some(&a), Some(&b), "-"), format!("{}0", "a".repeat(30)), "as many digits as tell them apart");
        assert_eq!(shown(Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904"), Some("8a90be4f11c49ad9b4bb0d8c9a8e63f1fb0f2b4e"), "-"), "4b825dc6");
        assert_eq!(shown(Some("sha256:4b825dc642cb6eb9a060e54bf8d69288"), None, "-"), "sha256:4b825dc6");
        assert_eq!(shown(Some("1.92"), None, "-"), "1.92", "short ids and versions as they are");
        assert_eq!(shown(Some("line one\nline two"), None, "-"), "`line one\\nline two`");
        assert_eq!(shown(Some(""), None, "-"), "``");
        assert_eq!(shown(None, Some("x"), "(unset)"), "(unset)");
        let long = "x".repeat(60);
        assert_eq!(shown(Some(&long), None, "-"), format!("{}…", "x".repeat(LONG)));
        // The same pair, the same line, every time.
        let mut after = base();
        after.inputs.insert("Cargo.lock".into(), b.clone());
        let mut before = base();
        before.inputs.insert("Cargo.lock".into(), a.clone());
        assert_eq!(diff(&before, &after), diff(&before, &after));
        assert_eq!(diff(&before, &after).first().map(|c| c.line.clone()), Some(format!("Cargo.lock {}0→{}9", "a".repeat(30), "a".repeat(30))));
    }

    /// The strong key's half: what a unit read, path by path.
    #[test]
    fn w24_r28_reads_that_changed() {
        let before = PathSet::from([("src/lib.rs".to_owned(), Read::File("d1".into())), ("build.rs".to_owned(), Read::Absent), ("src".to_owned(), Read::Dir("l1".into()))]);
        let mut after = before.clone();
        after.insert("src/lib.rs".into(), Read::File("d2".into()));
        after.insert("build.rs".into(), Read::File("d3".into()));
        after.remove("src");
        let lines: Vec<String> = diff_reads(&before, &after).into_iter().map(|c| c.line).collect();
        assert_eq!(lines, ["read build.rs (absent)→d3", "read src dir:l1→(not read)", "read src/lib.rs d1→d2"]);
        assert!(diff_reads(&before, &before).is_empty());
    }

    /// "Why did this rebuild?": the nearest earlier run is the one fewest components away.
    #[test]
    fn w24_r28_the_nearest_earlier_run() {
        let now = base();
        let mut far = base();
        far.platform.os = "windows".into();
        far.trust = "untrusted".into();
        let mut near = base();
        near.inputs.insert("Cargo.lock".into(), "a0".into());
        let earlier = vec![("run-9", far), ("run-8", near.clone()), ("run-7", near)];
        let (from, why) = nearest(&now, &earlier).unwrap_or((&"none", Vec::new()));
        assert_eq!((*from, why.iter().map(|c| c.line.as_str()).collect::<Vec<_>>()), ("run-8", vec!["Cargo.lock a0→a1"]), "the newest of the nearest");
        assert!(nearest::<&str>(&now, &[]).is_none());
    }

    /// A report as JSON, and in a sentence, for each way a unit comes by its result.
    #[test]
    fn w24_r28_reports() -> Result<(), serde_json::Error> {
        let reused = Report { how: How::Reused { run: "r1".into(), number: Some(12), leg: Some("l1".into()), runner: Some("fedora".into()) }, hits: None, saved_s: Some(843.0), why: Vec::new() };
        assert_eq!(serde_json::to_value(&reused)?, serde_json::json!({ "how": "reused", "run": "r1", "number": 12, "leg": "l1", "runner": "fedora", "saved_s": 843.0 }));
        assert_eq!(reused.summary(), "reused from run #12 on fedora, 14 m 3 s saved");
        let warm = Report { how: How::Warm { workspace: "rust/0".into(), behind: 2, files: 5, rebuilt: Some(3) }, hits: Some(1), saved_s: Some(40.0), why: Vec::new() };
        assert_eq!(serde_json::to_value(&warm)?, serde_json::json!({ "how": "warm", "workspace": "rust/0", "behind": 2, "files": 5, "rebuilt": 3, "hits": 1, "saved_s": 40.0 }));
        assert_eq!(warm.summary(), "warm in workspace rust/0, 2 commits behind, 5 files changed, 3 units rebuilt, 1 cache hit, 40 s saved");
        let mut after = base();
        after.inputs.insert("Cargo.lock".into(), "b2".into());
        after.platform.toolchains.insert("rustc".into(), "1.92".into());
        let ran = Report { how: How::Ran, hits: None, saved_s: None, why: diff(&base(), &after) };
        assert_eq!(ran.summary(), "ran: rustc 1.91→1.92; Cargo.lock a1→b2");
        let json = serde_json::to_value(&ran)?;
        assert_eq!(json.pointer("/why/1"), Some(&serde_json::json!({ "component": "input", "path": "Cargo.lock", "before": "a1", "after": "b2", "line": "Cargo.lock a1→b2" })));
        for r in [reused, warm, ran] {
            let back: Report = serde_json::from_value(serde_json::to_value(&r)?)?;
            assert_eq!(back, r, "round trip");
        }
        assert_eq!(Report { how: How::Ran, hits: None, saved_s: None, why: Vec::new() }.summary(), "ran");
        assert_eq!((span(59.6), span(3_600.0), span(7_500.0), span(f64::NAN)), ("1 m".to_owned(), "1 h".to_owned(), "2 h 5 m".to_owned(), "0 s".to_owned()));
        Ok(())
    }

    fn text() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_./=:-]{0,12}"
    }

    fn map() -> impl Strategy<Value = BTreeMap<String, String>> {
        proptest::collection::btree_map("[A-Z_]{1,8}", "[a-z0-9]{0,8}", 0..5)
    }

    fn unit() -> impl Strategy<Value = Unit> {
        let kind = prop_oneof![Just(Kind::Run), Just(Kind::Leg), Just(Kind::Step), Just(Kind::Action), Just(Kind::Test)];
        let platform = (text(), text(), text(), text(), proptest::option::of(text()), map()).prop_map(|(os, arch, libc, executor, image, toolchains)| Platform { os, arch, libc, executor, image, toolchains });
        (kind, "[a-z \n]{0,20}", proptest::collection::btree_set("[A-Z_]{1,8}", 0..4), map(), platform, map(), map(), text())
            .prop_map(|(kind, definition, secrets, env, platform, inputs, upstream, trust)| Unit { kind, definition, secrets, env, platform, inputs, upstream, trust })
    }

    /// One entry of a map changed (a new key, a changed value, or one gone); its key.
    fn edit_map(m: &mut BTreeMap<String, String>, salt: u8) -> String {
        match (salt % 3, m.keys().next().cloned()) {
            (1, Some(k)) => {
                if let Some(v) = m.get_mut(&k) {
                    v.push('~');
                }
                k
            }
            (2, Some(k)) => {
                m.remove(&k);
                k
            }
            _ => {
                let k = format!("~{}", m.len());
                m.insert(k.clone(), String::new());
                k
            }
        }
    }

    /// One component of `u` changed, as `which` names it; the component the diff should name.
    fn edit(u: &mut Unit, which: usize, salt: u8) -> Component {
        let c = char::from(b'a' + salt % 26);
        match which {
            0 => {
                u.kind = if u.kind == Kind::Test { Kind::Run } else { Kind::Test };
                Component::Kind
            }
            1 => {
                u.definition.push(c);
                Component::Definition
            }
            2 => {
                let name = match u.secrets.iter().next().cloned() {
                    Some(s) if !salt.is_multiple_of(2) => {
                        u.secrets.remove(&s);
                        s
                    }
                    _ => {
                        let s = format!("~{}", u.secrets.len());
                        u.secrets.insert(s.clone());
                        s
                    }
                };
                Component::Secret { name }
            }
            3 => Component::Env { name: edit_map(&mut u.env, salt) },
            4 => {
                u.platform.os.push(c);
                Component::Platform { field: "os".into() }
            }
            5 => {
                u.platform.arch.push(c);
                Component::Platform { field: "arch".into() }
            }
            6 => {
                u.platform.libc.push(c);
                Component::Platform { field: "libc".into() }
            }
            7 => {
                u.platform.executor.push(c);
                Component::Platform { field: "executor".into() }
            }
            8 => {
                u.platform.image = match u.platform.image.take() {
                    None => Some(String::new()),
                    Some(i) if salt.is_multiple_of(2) => Some(format!("{i}{c}")),
                    Some(_) => None,
                };
                Component::Platform { field: "image".into() }
            }
            9 => Component::Toolchain { name: edit_map(&mut u.platform.toolchains, salt) },
            10 => Component::Input { path: edit_map(&mut u.inputs, salt) },
            11 => Component::Upstream { name: edit_map(&mut u.upstream, salt) },
            _ => {
                u.trust.push(c);
                Component::Trust
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        /// W24-AC09 at random: any single-component edit is named, alone; a unit against itself,
        /// or carried through JSON, names nothing; and a diff's lines come out the same twice.
        #[test]
        fn w24_r28_ac09_any_single_edit_is_named_alone(u in unit(), which in 0..13usize, salt in any::<u8>()) {
            let mut after = u.clone();
            let component = edit(&mut after, which, salt);
            let d = diff(&u, &after);
            prop_assert_eq!(d.iter().map(|c| &c.component).collect::<Vec<_>>(), vec![&component]);
            prop_assert!(d.iter().all(|c| !c.line.is_empty() && !c.line.contains('\n')));
            prop_assert_eq!(&d, &diff(&u, &after));
            prop_assert!(diff(&u, &u).is_empty());
            let json = serde_json::to_string(&u).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let back: Unit = serde_json::from_str(&json).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(diff(&u, &back).is_empty());
        }
    }
}
