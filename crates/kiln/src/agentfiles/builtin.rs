//! The Agentfiles kiln ships, one per harness it runs in a Shards microVM (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, "Every kiln harness in Shards microVMs"): each embedded in kiln's
//! one binary, listed read-only beside a person's own, and copied to make one theirs. Each harness's
//! own step adds its Agentfile here (`include_str!` of `.kiln/images/harness-<id>/`, where kiln keeps
//! image definitions, W13-R11).

use serde::Serialize;

/// An Agentfile kiln ships.
pub struct Builtin {
    /// Its name, an image's (as a person's Agentfile's is: [`super::name_problem`]).
    pub name: &'static str,
    /// The harness it runs (`crate::harness::kinds::Kind`'s id: `claude`, `codex` ...).
    pub harness: &'static str,
    pub text: &'static str,
    /// Its pin.json: each platform's files of the harness directory, by URL, SHA-256 and size, and
    /// how its pin.sh checked them.
    pub pin: &'static str,
    /// The small files of its build context beside the Agentfile (its harness.json), by name.
    pub files: &'static [(&'static str, &'static str)],
}

/// Every one, in the order they're listed.
pub const ALL: &[Builtin] = &[
    // Antigravity's CLI, agy, in its headless stream-json mode (kiln plan
    // 01a1cd8e862e7169a074db3d5fa33f3d, step 26).
    Builtin {
        name: "harness-antigravity",
        harness: "antigravity",
        text: include_str!("../../../../.kiln/images/harness-antigravity/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-antigravity/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-antigravity/harness.json"))],
    },
];

/// One as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileBuiltin"))]
pub struct View {
    pub name: String,
    pub harness: String,
    pub text: String,
}

/// Every one, as the list shows them.
pub fn views() -> Vec<View> {
    ALL.iter().map(|b| View { name: b.name.to_owned(), harness: b.harness.to_owned(), text: b.text.to_owned() }).collect()
}

#[cfg(test)]
mod antigravity_tests;

#[cfg(test)]
mod tests {
    /// Each is what a person's Agentfile must be (a name an image may have, a text Shards may read), and
    /// no two share a name.
    #[test]
    fn every_builtin_is_a_valid_agentfile_once() {
        let mut names = std::collections::BTreeSet::new();
        for b in super::ALL {
            assert_eq!(super::super::name_problem(b.name), None, "{}", b.name);
            assert_eq!(super::super::text_problem(b.text), None, "{}", b.name);
            assert!(crate::harness::kinds::Kind::get(b.harness).is_some(), "{}: no harness {}", b.name, b.harness);
            assert!(names.insert(b.name), "{} twice", b.name);
        }
    }
}
