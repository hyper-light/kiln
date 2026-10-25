//! Fingerprints (docs/ci.md W24-R01, R03, R05): what a unit of work (a run, a leg, a step, a
//! tool's action, a test) depends on, hashed canonically, so the same inputs always give the same
//! key and a change to any of them gives another.
//!
//! - A **weak key** holds what is known before the unit runs: its definition and the secrets it
//!   receives (by name), the environment the rules keep ([`relevant_env`]), its platform and
//!   toolchains, its declared inputs by git tree id, its upstream units' output digests and its
//!   trust domain. Upstream units enter by what they produced, never by their keys: one that
//!   reruns and produces the same bytes leaves everything after it reusable (R03).
//! - A **strong key** adds what the unit was seen to read (R04): each path's digest, or that it
//!   was absent.
//!
//! The encoding is SHA-256 over tagged, length-prefixed fields after [`VERSION`] and the key's and
//! the unit's kinds, so no two different units encode alike, granularities never collide, and a
//! version bump invalidates every key.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Bumped when what a key covers changes: every key made before then stops matching.
pub const VERSION: &str = "kiln/work/v1";

/// The granularity of a unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Run,
    Leg,
    Step,
    Action,
    Test,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Run => "run",
            Kind::Leg => "leg",
            Kind::Step => "step",
            Kind::Action => "action",
            Kind::Test => "test",
        }
    }
}

/// What a unit runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub libc: String,
    pub executor: String,
    /// The job container's image digest, when it runs in one.
    pub image: Option<String>,
    /// Each toolchain's identity: `rustc` to a hash of `rustc -vV` and its sysroot, and so on.
    pub toolchains: BTreeMap<String, String>,
}

/// What a unit depends on before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub kind: Kind,
    /// The job or step after evaluation; a secret appears in it by name only.
    pub definition: String,
    /// The secrets it receives, by name: their values never enter a key (R05).
    pub secrets: BTreeSet<String>,
    /// The environment the rules keep ([`relevant_env`]).
    pub env: BTreeMap<String, String>,
    pub platform: Platform,
    /// Declared inputs: each path's or glob's git tree (or blob) id.
    pub inputs: BTreeMap<String, String>,
    /// Upstream units' outputs, by name, as content digests (R03).
    pub upstream: BTreeMap<String, String>,
    /// Its trust domain: `trusted`, `protected`, `untrusted` or `person:<id>` (R21).
    pub trust: String,
}

/// What a unit was seen to read at one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Read {
    /// A file, by its content's digest.
    File(String),
    /// A folder it listed, by its listing's digest.
    Dir(String),
    /// A path it looked for and didn't find.
    Absent,
}

/// What a unit read, path by path (R04).
pub type PathSet = BTreeMap<String, Read>;

/// A fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; 32]);

impl Key {
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

/// The canonical encoding: each field tagged, every string and collection length-prefixed.
struct Enc(Sha256);

impl Enc {
    fn new(domain: &str, kind: &str) -> Self {
        let mut e = Enc(Sha256::new());
        e.str(VERSION);
        e.str(domain);
        e.str(kind);
        e
    }

    fn tag(&mut self, t: u8) {
        self.0.update([t]);
    }

    fn len(&mut self, n: usize) {
        self.0.update((n as u64).to_be_bytes());
    }

    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.update(s.as_bytes());
    }

    fn opt(&mut self, o: Option<&str>) {
        match o {
            None => self.tag(0),
            Some(s) => {
                self.tag(1);
                self.str(s);
            }
        }
    }

    fn map(&mut self, tag: u8, m: &BTreeMap<String, String>) {
        self.tag(tag);
        self.len(m.len());
        for (k, v) in m {
            self.str(k);
            self.str(v);
        }
    }

    fn set(&mut self, tag: u8, s: &BTreeSet<String>) {
        self.tag(tag);
        self.len(s.len());
        for v in s {
            self.str(v);
        }
    }

    fn finish(self) -> Key {
        Key(self.0.finalize().into())
    }
}

/// A unit's weak key: what is known before it runs.
pub fn weak(u: &Unit) -> Key {
    let mut e = Enc::new("weak", u.kind.tag());
    e.tag(1);
    e.str(&u.definition);
    e.set(2, &u.secrets);
    e.map(3, &u.env);
    let p = &u.platform;
    e.tag(4);
    e.str(&p.os);
    e.str(&p.arch);
    e.str(&p.libc);
    e.str(&p.executor);
    e.opt(p.image.as_deref());
    e.map(5, &p.toolchains);
    e.map(6, &u.inputs);
    e.map(7, &u.upstream);
    e.tag(8);
    e.str(&u.trust);
    e.finish()
}

/// A unit's strong key: its weak key and what it was seen to read.
pub fn strong(weak: &Key, read: &PathSet) -> Key {
    let mut e = Enc::new("strong", "");
    e.0.update(weak.0);
    e.len(read.len());
    for (path, r) in read {
        e.str(path);
        match r {
            Read::File(d) => {
                e.tag(1);
                e.str(d);
            }
            Read::Dir(d) => {
                e.tag(2);
                e.str(d);
            }
            Read::Absent => e.tag(3),
        }
    }
    e.finish()
}

/// Whether a variable is one kiln sets per run (its ids, attempts, tokens, paths), which a key
/// keeps only when the unit names it or reads it (R05).
pub fn per_run(name: &str) -> bool {
    ["KILN_", "GITHUB_", "RUNNER_", "ACTIONS_"].iter().any(|p| name.starts_with(p)) || name == "CI"
}

/// The environment a key keeps (R01, R05): each variable the unit declares (`env:`, `memo.env`),
/// a toolchain is known to read (`CARGO_*` names a prefix) or the unit was seen reading; and of
/// kiln's per-run variables only those its text names or it was seen reading.
pub fn relevant_env(all: &BTreeMap<String, String>, declared: &BTreeSet<String>, toolchain: &[&str], text: &str, read: &BTreeSet<String>) -> BTreeMap<String, String> {
    let known = |k: &str| toolchain.iter().any(|t| t.strip_suffix('*').map_or(*t == k, |prefix| k.starts_with(prefix)));
    all.iter()
        .filter(|(k, _)| {
            let seen = read.contains(*k);
            if per_run(k) { seen || text.contains(k.as_str()) } else { seen || declared.contains(*k) || known(k) }
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_./=:-]{0,12}"
    }

    fn map() -> impl Strategy<Value = BTreeMap<String, String>> {
        proptest::collection::btree_map("[A-Z_]{1,8}", "[a-z0-9]{0,8}", 0..5)
    }

    fn kind() -> impl Strategy<Value = Kind> {
        prop_oneof![Just(Kind::Run), Just(Kind::Leg), Just(Kind::Step), Just(Kind::Action), Just(Kind::Test)]
    }

    fn unit() -> impl Strategy<Value = Unit> {
        let platform = (text(), text(), text(), text(), proptest::option::of(text()), map()).prop_map(|(os, arch, libc, executor, image, toolchains)| Platform { os, arch, libc, executor, image, toolchains });
        (kind(), text(), proptest::collection::btree_set("[A-Z_]{1,8}", 0..4), map(), platform, map(), map(), text())
            .prop_map(|(kind, definition, secrets, env, platform, inputs, upstream, trust)| Unit { kind, definition, secrets, env, platform, inputs, upstream, trust })
    }

    /// Inserts a key no generated map has (`~` is outside their alphabet), changes a value, or
    /// removes an entry: the map always ends different.
    fn edit_map(m: &mut BTreeMap<String, String>, salt: u8) {
        let first = m.keys().next().cloned();
        match (salt % 3, first) {
            (1, Some(k)) => {
                if let Some(v) = m.get_mut(&k) {
                    v.push('~');
                }
            }
            (2, Some(k)) => {
                m.remove(&k);
            }
            _ => {
                m.insert(format!("~{}", m.len()), String::new());
            }
        }
    }

    const COMPONENTS: usize = 13;

    /// One component of `u` changed, whichever `which` names.
    fn edit(u: &mut Unit, which: usize, salt: u8) {
        let c = char::from(b'a' + salt % 26);
        match which {
            0 => {
                u.kind = match u.kind {
                    Kind::Run => Kind::Leg,
                    Kind::Leg => Kind::Step,
                    Kind::Step => Kind::Action,
                    Kind::Action => Kind::Test,
                    Kind::Test => Kind::Run,
                }
            }
            1 => u.definition.push(c),
            2 => match (salt % 2, u.secrets.iter().next().cloned()) {
                (1, Some(s)) => {
                    u.secrets.remove(&s);
                }
                _ => {
                    u.secrets.insert(format!("~{}", u.secrets.len()));
                }
            },
            3 => edit_map(&mut u.env, salt),
            4 => u.platform.os.push(c),
            5 => u.platform.arch.push(c),
            6 => u.platform.libc.push(c),
            7 => u.platform.executor.push(c),
            8 => {
                u.platform.image = match u.platform.image.take() {
                    None => Some(String::new()),
                    Some(i) if salt.is_multiple_of(2) => Some(format!("{i}{c}")),
                    Some(_) => None,
                }
            }
            9 => edit_map(&mut u.platform.toolchains, salt),
            10 => edit_map(&mut u.inputs, salt),
            11 => edit_map(&mut u.upstream, salt),
            _ => u.trust.push(c),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100_000))]

        /// W24-AC01: every random single-component edit changes the key.
        #[test]
        fn w24_ac01_every_component_edit_changes_the_key(u in unit(), which in 0..COMPONENTS, salt in any::<u8>()) {
            let mut edited = u.clone();
            edit(&mut edited, which, salt);
            prop_assert_ne!(&u, &edited);
            prop_assert_ne!(weak(&u), weak(&edited));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        /// W24-AC01's other half: what isn't a component never changes the key. The same unit
        /// built again, or carried through JSON, keys alike; so does its environment with
        /// variables kiln sets per run that it neither names nor reads, or others it neither
        /// declares nor reads, added or changed.
        #[test]
        fn w24_ac01_edits_outside_the_components_never_change_it(u in unit(), noise in "[a-z0-9]{1,8}") {
            prop_assert_eq!(weak(&u), weak(&u.clone()));
            let json = serde_json::to_string(&u).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let back: Unit = serde_json::from_str(&json).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(weak(&u), weak(&back));
            let declared: BTreeSet<String> = u.env.keys().cloned().collect();
            let mut all = u.env.clone();
            let kept = relevant_env(&all, &declared, &[], &u.definition, &BTreeSet::new());
            all.insert("KILN_RUN_ID".into(), noise.clone());
            all.insert("GITHUB_RUN_ATTEMPT".into(), noise.clone());
            all.insert("UNDECLARED_VALUE".into(), noise);
            let after = relevant_env(&all, &declared, &[], &u.definition, &BTreeSet::new());
            prop_assert_eq!(&kept, &after);
            prop_assert_eq!(weak(&Unit { env: kept, ..u.clone() }), weak(&Unit { env: after, ..u }));
        }

        /// The same entry under another component, or split differently between a name and
        /// its value, keys differently: the encoding is unambiguous.
        #[test]
        fn w24_r01_the_encoding_is_unambiguous(u in unit(), k in "[A-Z_]{1,6}", v in "[a-z0-9]{1,6}") {
            let mut a = u.clone();
            let mut b = u.clone();
            a.env.clear();
            b.env.clear();
            a.inputs.clear();
            b.inputs.clear();
            a.env.insert(k.clone(), v.clone());
            b.inputs.insert(k.clone(), v.clone());
            prop_assert_ne!(weak(&a), weak(&b));
            let mut c = a.clone();
            c.env.clear();
            c.env.insert(format!("{k}{v}"), String::new());
            prop_assert_ne!(weak(&a), weak(&c));
        }
    }

    #[test]
    fn w24_r04_strong_keys_add_what_was_read() {
        let u = Unit { kind: Kind::Step, definition: "cargo test".into(), secrets: BTreeSet::new(), env: BTreeMap::new(), platform: Platform::default(), inputs: BTreeMap::new(), upstream: BTreeMap::new(), trust: "trusted".into() };
        let w = weak(&u);
        let none = strong(&w, &PathSet::new());
        assert_ne!(w, none, "a strong key is its own domain");
        let read = |path: &str, r: Read| PathSet::from([(path.to_owned(), r)]);
        let a = strong(&w, &read("src/lib.rs", Read::File("d1".into())));
        assert_ne!(a, none);
        assert_ne!(a, strong(&w, &read("src/lib.rs", Read::File("d2".into()))), "a changed file");
        assert_ne!(a, strong(&w, &read("src/lib.rs", Read::Dir("d1".into()))), "a folder, not a file");
        assert_ne!(strong(&w, &read("src/lib.rs", Read::Absent)), none, "a path looked for and absent");
        assert_eq!(a, strong(&w, &read("src/lib.rs", Read::File("d1".into()))));
        assert_eq!(w.hex().len(), 64);
    }

    #[test]
    fn w24_r05_secrets_and_per_run_values_stay_out_unless_used() {
        let all = BTreeMap::from([
            ("KILN_RUN_ID".to_owned(), "r1".to_owned()),
            ("GITHUB_SHA".to_owned(), "abc".to_owned()),
            ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()),
            ("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
            ("MODE".to_owned(), "fast".to_owned()),
        ]);
        let declared = BTreeSet::from(["MODE".to_owned()]);
        let kept = relevant_env(&all, &declared, &["CARGO_*", "RUSTFLAGS"], "cargo build", &BTreeSet::new());
        assert_eq!(kept.keys().map(String::as_str).collect::<Vec<_>>(), ["CARGO_INCREMENTAL", "MODE", "RUSTFLAGS"]);
        // Named in the text, or seen read, a per-run variable counts.
        let named = relevant_env(&all, &declared, &[], "echo $GITHUB_SHA", &BTreeSet::from(["PATH".to_owned()]));
        assert_eq!(named.keys().map(String::as_str).collect::<Vec<_>>(), ["GITHUB_SHA", "MODE", "PATH"]);
    }
}
