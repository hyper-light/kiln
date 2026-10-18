//! kiln's built-in actions (docs/ci.md W07-R10): `uses: kiln/<name>@v1`, run by the runner itself
//! (no Node, nothing to fetch), in either dialect, since `kiln/*` resolves to these before any
//! remote repository. Their metadata is served as `action.yml` would be, by
//! `GET /api/ci/actions/kiln`.

use serde_json::{Value, json};

/// An input of a built-in.
pub struct Input {
    pub name: &'static str,
    pub description: &'static str,
    pub default: Option<&'static str>,
    pub required: bool,
}

/// A built-in action.
pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
    pub inputs: &'static [Input],
    pub outputs: &'static [(&'static str, &'static str)],
}

const fn input(name: &'static str, description: &'static str, default: Option<&'static str>) -> Input {
    Input { name, description, default, required: false }
}

/// The built-ins this runner has.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "checkout",
        description: "Check out the run's commit (or another ref) from the server's mirror: shallow by default, sparse and partial when asked.",
        inputs: &[
            input("path", "Where, below the workspace.", Some(".")),
            input("ref", "A commit, branch or tag; the run's commit by default.", None),
            input("fetch-depth", "Commits of history; 0 for all of it.", Some("1")),
            input("sparse-checkout", "Patterns (one a line) to check out only those paths.", None),
            input("sparse-checkout-cone-mode", "Read the patterns as directories (git's cone mode).", Some("true")),
            input("filter", "A partial clone filter, such as `blob:none`.", None),
        ],
        outputs: &[("ref", "The ref checked out."), ("commit", "The commit checked out.")],
    },
    Builtin {
        name: "test-results",
        description: "Send test reports to kiln, which keeps and parses them: results on the run, failures annotated where they are, quarantine and flakiness judged (W16).",
        inputs: &[
            Input { name: "path", description: "The reports: patterns, one a line, from the working directory.", default: None, required: true },
            input("format", "junit, tap, go-test-json, libtest-json, trx, xunit, mocha-json or ctrf; found from each file when left out.", None),
            input("fail-on-failure", "Fail the step when a test failed (quarantined ones, under `quarantine: enforce`, excepted).", Some("true")),
        ],
        outputs: &[("failed", "How many tests failed."), ("passed", "How many passed.")],
    },
];

/// `kiln/<name>@<version>`: the built-in it names, when it is one (`v1`, or any version
/// starting `v1`).
pub fn builtin(uses: &str) -> Option<&'static Builtin> {
    let rest = uses.strip_prefix("kiln/")?;
    let (name, version) = rest.split_once('@')?;
    if !(version == "v1" || version.starts_with("v1.")) {
        return None;
    }
    BUILTINS.iter().find(|b| b.name == name)
}

/// Whether `uses` is kiln's own namespace (`kiln/<name>@…`), built in or not: never fetched from
/// a remote repository.
pub fn is_kiln(uses: &str) -> bool {
    uses.strip_prefix("kiln/").is_some_and(|r| !r.contains('/') && r.contains('@'))
}

/// The built-ins as `action.yml` reads, for the catalog and the editor (W07-R12).
pub fn catalog() -> Value {
    Value::Array(
        BUILTINS
            .iter()
            .map(|b| {
                let inputs: serde_json::Map<String, Value> = b.inputs.iter().map(|i| (i.name.to_owned(), json!({ "description": i.description, "required": i.required, "default": i.default }))).collect();
                let outputs: serde_json::Map<String, Value> = b.outputs.iter().map(|(n, d)| ((*n).to_owned(), json!({ "description": d }))).collect();
                json!({ "uses": format!("kiln/{}@v1", b.name), "name": b.name, "description": b.description, "inputs": inputs, "outputs": outputs, "runs": { "using": "kiln" } })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w07_r10_names() {
        assert_eq!(builtin("kiln/checkout@v1").map(|b| b.name), Some("checkout"));
        assert_eq!(builtin("kiln/test-results@v1.2").map(|b| b.name), Some("test-results"));
        assert!(builtin("kiln/checkout@v2").is_none() && builtin("kiln/nope@v1").is_none() && builtin("kilnx/checkout@v1").is_none());
        assert!(is_kiln("kiln/nope@v1") && !is_kiln("kiln/repo/path@v1") && !is_kiln("actions/checkout@v4"));
        assert!(catalog().as_array().is_some_and(|a| a.len() == BUILTINS.len()));
    }
}
