//! New (docs/ci.md W29-R08): a new workflow's or action's files, by its kind, its dialect and its
//! name, from where it starts: blank, recommended for the repository (by what the files at its
//! head say it is), one of GitHub's starter workflows, or a copy of a pipeline. Pure: the server
//! passes in the repository's paths, its default branch, and what a copy copies.

use serde::Serialize;

/// The seven kinds New makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Workflow,
    Reusable,
    Composite,
    JavaScript,
    Docker,
    Firecracker,
    Shards,
}

impl Kind {
    /// A kind by the name the API and the read model use (`kiln_ci::pipelines`).
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "workflow" => Kind::Workflow,
            "reusable" => Kind::Reusable,
            "composite" => Kind::Composite,
            "javascript" => Kind::JavaScript,
            "docker" => Kind::Docker,
            "firecracker" => Kind::Firecracker,
            "shards" => Kind::Shards,
            _ => return None,
        })
    }

    pub fn is_action(self) -> bool {
        !matches!(self, Kind::Workflow | Kind::Reusable)
    }
}

/// A workflow's dialect: GitHub's, or kiln's (`.kiln/`) in YAML or TypeScript (W29-R27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Gha,
    Kiln,
    TypeScript,
}

/// Where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start<'a> {
    Blank,
    /// What the repository's files say it is ([`detect`]).
    Recommended,
    /// One of GitHub's starter workflows, by its language ([`STARTERS`]).
    Starter(&'a str),
    /// A pipeline's text, copied with its name changed.
    Copy(&'a str),
}

/// What a repository is, by the files at its head: what Recommended starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    Rust,
    Node,
    Go,
    Python,
    Other,
}

/// A file New makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct File {
    pub path: String,
    pub text: String,
}

/// GitHub's starter workflows New offers (actions/starter-workflows at
/// e3c451d60f119b71caebf13c98ac45da6e15b4b7, MIT: F-03, tests/fixtures/gha/SOURCES.md), by the
/// language each is for.
pub const STARTERS: &[(&str, &str)] = &[
    ("rust", include_str!("../tests/fixtures/gha/starter-workflows/ci_rust.yml")),
    ("node", include_str!("../tests/fixtures/gha/starter-workflows/ci_node.js.yml")),
    ("go", include_str!("../tests/fixtures/gha/starter-workflows/ci_go.yml")),
    ("python", include_str!("../tests/fixtures/gha/starter-workflows/ci_python-app.yml")),
    ("blank", include_str!("../tests/fixtures/gha/starter-workflows/ci_blank.yml")),
];

/// What a repository is, by its paths (the first a file at its root says, else Other).
pub fn detect(paths: &[&str]) -> Stack {
    let has = |name: &str| paths.contains(&name);
    if has("Cargo.toml") {
        Stack::Rust
    } else if has("package.json") {
        Stack::Node
    } else if has("go.mod") {
        Stack::Go
    } else if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        Stack::Python
    } else {
        Stack::Other
    }
}

/// A name New takes: letters, digits, `-` and `_`, 64 at the most.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("`{name}` isn't a name New takes: letters, digits, `-` and `_`, up to 64"));
    }
    Ok(())
}

/// Where its main file goes: a workflow's file, or an action's `action.yml` (W29-R08's paths).
pub fn path_of(kind: Kind, dialect: Dialect, name: &str) -> String {
    match (kind.is_action(), dialect) {
        (true, _) => format!(".github/actions/{name}/action.yml"),
        (false, Dialect::Gha) => format!(".github/workflows/{name}.yml"),
        (false, Dialect::Kiln) => format!(".kiln/{name}.yaml"),
        (false, Dialect::TypeScript) => format!(".kiln/{name}.ts"),
    }
}

/// The commands a workflow's steps run for what the repository is, after its checkout.
fn commands_for(stack: Stack) -> &'static [&'static str] {
    match stack {
        Stack::Rust => &["cargo build --locked", "cargo test --locked"],
        Stack::Node => &["npm ci", "npm test"],
        Stack::Go => &["go build ./...", "go test ./..."],
        Stack::Python => &["pip install -r requirements.txt", "python -m pytest"],
        Stack::Other => &["echo \"it runs\""],
    }
}

/// The steps a workflow starts with for what the repository is.
fn steps_for(stack: Stack, kiln: bool) -> String {
    let checkout = if kiln { "      - uses: kiln/checkout@v1\n" } else { "      - uses: actions/checkout@v4\n" };
    let rest: String = commands_for(stack).iter().map(|c| format!("      - run: {c}\n")).collect();
    format!("{checkout}{rest}")
}

/// A kiln workflow written in TypeScript (W29-R27): the YAML one's triggers, job and steps, made by
/// `@kiln/sdk`'s calls.
fn program(name: &str, stack: Stack, branch: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let steps: Vec<String> = std::iter::once("step({ uses: \"kiln/checkout@v1\" })".to_owned()).chain(commands_for(stack).iter().map(|c| format!("step({{ run: {} }})", q(c)))).collect();
    format!(
        "// {name}: a kiln workflow in TypeScript (docs/ci.md W29-R27). Pipelines shows the YAML it makes.\nimport {{ workflow, job, step }} from \"@kiln/sdk\";\n\nexport default workflow({{\n  name: {},\n  on: {{ push: {{ branches: [{}] }} }},\n  jobs: {{\n    {}: job({{\n      \"runs-on\": [\"baremetal\"],\n      steps: [\n        {},\n      ],\n    }}),\n  }},\n}});\n",
        q(name),
        q(branch),
        q(name),
        steps.join(",\n        ")
    )
}

/// A workflow's or reusable workflow's YAML.
fn workflow(kind: Kind, dialect: Dialect, name: &str, stack: Stack, branch: &str) -> String {
    let kiln = dialect == Dialect::Kiln;
    let on = match kind {
        Kind::Reusable => "on:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n        default: main\n".to_owned(),
        _ if kiln => format!("on:\n  push:\n    branches: [{branch}]\n"),
        _ => format!("on:\n  push:\n    branches: [{branch}]\n  workflow_dispatch:\n"),
    };
    let runs_on = if kiln { "[baremetal]" } else { "ubuntu-latest" };
    format!("name: {name}\n{on}jobs:\n  {name}:\n    runs-on: {runs_on}\n    steps:\n{}", steps_for(stack, kiln))
}

/// An action's files: its `action.yml`, and what its kind runs.
fn action(kind: Kind, name: &str) -> Vec<File> {
    let dir = format!(".github/actions/{name}");
    let at = |file: &str, text: String| File { path: format!("{dir}/{file}"), text };
    let head = format!("name: {name}\ndescription: What {name} does\ninputs:\n  target:\n    description: what it works on\n    required: false\n    default: \".\"\n");
    match kind {
        Kind::Composite => vec![at("action.yml", format!("{head}runs:\n  using: composite\n  steps:\n    - run: echo \"{name} on ${{{{ inputs.target }}}}\"\n      shell: bash\n"))],
        Kind::JavaScript => vec![
            at("action.yml", format!("{head}runs:\n  using: node24\n  main: dist/index.js\n")),
            at("src/index.ts", format!("// {name}: built to dist/index.js (npm run build), which action.yml runs.\nconst target = process.env[\"INPUT_TARGET\"] ?? \".\";\nconsole.log(`{name}: ${{target}}`);\n")),
            at("package.json", format!("{{\n  \"name\": \"{name}\",\n  \"private\": true,\n  \"type\": \"module\",\n  \"scripts\": {{ \"build\": \"esbuild src/index.ts --bundle --platform=node --outfile=dist/index.js\" }},\n  \"devDependencies\": {{ \"esbuild\": \"^0.24.0\" }}\n}}\n")),
        ],
        Kind::Docker => vec![
            at("action.yml", format!("{head}runs:\n  using: docker\n  image: Dockerfile\n  args: [\"${{{{ inputs.target }}}}\"]\n")),
            at("Dockerfile", "FROM alpine:3.20\nCOPY entrypoint.sh /entrypoint.sh\n# A host's API commits files without their mode: made runnable here.\nRUN chmod +x /entrypoint.sh\nENTRYPOINT [\"/entrypoint.sh\"]\n".to_owned()),
            at("entrypoint.sh", format!("#!/bin/sh\nset -eu\necho \"{name}: $1\"\n")),
        ],
        // kiln's own (W29-R29, R30): a microVM per call, booted from the image its Dockerfile builds.
        _ => {
            let using = if kind == Kind::Firecracker { "firecracker" } else { "shards" };
            vec![
                at("action.yml", format!("{head}runs:\n  using: {using}\n  image: Dockerfile\n  resources: {{ cpus: 2, memory: 4Gi }}\n  egress: []\n  args: [\"${{{{ inputs.target }}}}\"]\n")),
                at("Dockerfile", "FROM alpine:3.20\nCOPY run.sh /run.sh\n# A host's API commits files without their mode: made runnable here.\nRUN chmod +x /run.sh\nENTRYPOINT [\"/run.sh\"]\n".to_owned()),
                at("run.sh", format!("#!/bin/sh\nset -eu\necho \"{name}: $1\"\n")),
            ]
        }
    }
}

/// A copy of a pipeline's text, its `name:` its own.
fn copied(text: &str, name: &str) -> String {
    let mut named = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if !named && l.starts_with("name:") {
                named = true;
                format!("name: {name}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !named {
        out.insert(0, format!("name: {name}"));
    }
    format!("{}\n", out.join("\n"))
}

/// New's files: the main one first (a workflow's file, an action's `action.yml`), then the rest
/// its kind runs. `paths` are the repository's at its head, `branch` its default branch.
pub fn scaffold(kind: Kind, dialect: Dialect, name: &str, start: Start<'_>, paths: &[&str], branch: &str) -> Result<Vec<File>, String> {
    check_name(name)?;
    let path = path_of(kind, dialect, name);
    if dialect == Dialect::TypeScript && !kind.is_action() {
        return match (kind, start) {
            (Kind::Reusable, _) => Err("a reusable workflow is YAML, the file its callers read: make it in kiln's or GitHub's dialect".to_owned()),
            (_, Start::Copy(_) | Start::Starter(_)) => Err("a copy or a starter is YAML: start a TypeScript workflow blank or recommended".to_owned()),
            (_, Start::Recommended) => Ok(vec![File { path, text: program(name, detect(paths), branch) }]),
            (_, Start::Blank) => Ok(vec![File { path, text: program(name, Stack::Other, branch) }]),
        };
    }
    match start {
        Start::Copy(text) => Ok(vec![File { path, text: copied(text, name) }]),
        _ if kind.is_action() => Ok(action(kind, name)),
        Start::Starter(lang) => {
            if dialect == Dialect::Kiln || kind == Kind::Reusable {
                return Err("GitHub's starter workflows are GitHub's dialect, run on push: start a kiln workflow or a reusable one blank or recommended".to_owned());
            }
            let text = STARTERS.iter().find(|(l, _)| *l == lang).map(|(_, t)| *t).ok_or_else(|| format!("no starter for `{lang}`: {}", STARTERS.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", ")))?;
            Ok(vec![File { path, text: copied(&text.replace("$default-branch", branch), name) }])
        }
        Start::Recommended => Ok(vec![File { path, text: workflow(kind, dialect, name, detect(paths), branch) }]),
        Start::Blank => Ok(vec![File { path, text: workflow(kind, dialect, name, Stack::Other, branch) }]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W29-R27: New's TypeScript workflow is erasable, imports `@kiln/sdk` alone, and keeps the
    /// YAML one's commands; a reusable workflow, a copy and a starter stay YAML.
    #[test]
    fn w29_r27_new_writes_a_typescript_workflow() {
        let files = scaffold(Kind::Workflow, Dialect::TypeScript, "build-all", Start::Recommended, &["Cargo.toml"], "trunk").unwrap_or_default();
        assert_eq!(files.len(), 1, "{files:?}");
        let Some(f) = files.first() else { return };
        assert_eq!(f.path, ".kiln/build-all.ts");
        assert!(crate::wf::code::erase(&f.text).is_ok(), "{}", f.text);
        let imports: Vec<Option<String>> = crate::wf::code::imports(&f.text).into_iter().map(|(n, _)| n).collect();
        assert_eq!(imports, vec![Some("@kiln/sdk".to_owned())]);
        for part in ["\"build-all\": job(", "branches: [\"trunk\"]", "step({ run: \"cargo test --locked\" })", "export default workflow("] {
            assert!(f.text.contains(part), "{part}: {}", f.text);
        }
        for (kind, start) in [(Kind::Reusable, Start::Blank), (Kind::Workflow, Start::Starter("rust")), (Kind::Workflow, Start::Copy("name: x\n"))] {
            assert!(scaffold(kind, Dialect::TypeScript, "x", start, &[], "main").is_err(), "{kind:?} {start:?}");
        }
    }

    const KINDS: [Kind; 7] = [Kind::Workflow, Kind::Reusable, Kind::Composite, Kind::JavaScript, Kind::Docker, Kind::Firecracker, Kind::Shards];

    #[test]
    fn w29_r08_every_kind_from_every_start_reads_clean() {
        let paths = ["Cargo.toml", "src/main.rs"];
        for kind in KINDS {
            for dialect in [Dialect::Gha, Dialect::Kiln] {
                // A copy is of the same kind of thing: a workflow's of a workflow, an action's of an action.
                let copy = if kind.is_action() { "name: other\nruns:\n  using: composite\n  steps:\n    - run: echo\n      shell: bash\n" } else { "name: other\non: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n" };
                for start in [Start::Blank, Start::Recommended, Start::Starter("rust"), Start::Copy(copy)] {
                    let files = match scaffold(kind, dialect, "fuzz-target", start, &paths, "main") {
                        Ok(f) => f,
                        // A starter is GitHub's dialect, run on push.
                        Err(e) => {
                            assert!(matches!(start, Start::Starter(_)) && !kind.is_action() && (dialect == Dialect::Kiln || kind == Kind::Reusable), "{kind:?} {dialect:?} {start:?}: {e}");
                            continue;
                        }
                    };
                    let main = files.first().map(|f| f.path.clone()).unwrap_or_default();
                    assert_eq!(main, path_of(kind, dialect, "fuzz-target"), "{kind:?} {dialect:?} {start:?}");
                    for f in &files {
                        let Some(file_kind) = crate::wf::file_kind(&f.path) else { continue };
                        let diags = match file_kind {
                            crate::wf::FileKind::Action => crate::wf::parse_action(&f.text).1,
                            _ => crate::wf::parse_workflow(&f.path, &f.text, if f.path.starts_with(".kiln/") { crate::expr::Dialect::Kiln } else { crate::expr::Dialect::Gha }).1,
                        };
                        let errors: Vec<String> = diags.iter().filter(|d| d.is_error()).map(|d| d.render(&f.path)).collect();
                        assert!(errors.is_empty(), "{kind:?} {dialect:?} {start:?} {}: {errors:?}\n{}", f.path, f.text);
                    }
                }
            }
        }
    }

    #[test]
    fn w29_r08_what_each_kind_makes() {
        let names = |kind| scaffold(kind, Dialect::Gha, "x", Start::Blank, &[], "main").map(|f| f.into_iter().map(|f| f.path).collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(names(Kind::Workflow), [".github/workflows/x.yml"]);
        assert_eq!(names(Kind::Composite), [".github/actions/x/action.yml"]);
        assert_eq!(names(Kind::JavaScript), [".github/actions/x/action.yml", ".github/actions/x/src/index.ts", ".github/actions/x/package.json"]);
        assert_eq!(names(Kind::Docker), [".github/actions/x/action.yml", ".github/actions/x/Dockerfile", ".github/actions/x/entrypoint.sh"]);
        assert_eq!(names(Kind::Firecracker), [".github/actions/x/action.yml", ".github/actions/x/Dockerfile", ".github/actions/x/run.sh"]);
        assert_eq!(path_of(Kind::Workflow, Dialect::Kiln, "x"), ".kiln/x.yaml");
        // Recommended by what the repository is; a starter with its default branch.
        assert_eq!(detect(&["package.json"]), Stack::Node);
        assert_eq!(detect(&["go.mod", "package.json"]), Stack::Node);
        assert_eq!(detect(&["README.md"]), Stack::Other);
        let rec = scaffold(Kind::Workflow, Dialect::Kiln, "ci", Start::Recommended, &["go.mod"], "trunk").unwrap_or_default();
        assert!(rec.first().is_some_and(|f| f.text.contains("go test ./...") && f.text.contains("branches: [trunk]") && f.text.contains("kiln/checkout@v1")), "{rec:?}");
        let starter = scaffold(Kind::Workflow, Dialect::Gha, "ci", Start::Starter("rust"), &[], "trunk").unwrap_or_default();
        assert!(starter.first().is_some_and(|f| f.text.starts_with("name: ci\n") && f.text.contains("trunk") && !f.text.contains("$default-branch")), "{starter:?}");
        // A copy is named for itself; a bad name is said.
        assert_eq!(copied("on: push\n", "c"), "name: c\non: push\n");
        assert!(scaffold(Kind::Workflow, Dialect::Gha, "a b", Start::Blank, &[], "main").is_err());
        assert!(scaffold(Kind::Workflow, Dialect::Gha, "x", Start::Starter("cobol"), &[], "main").is_err());
    }
}
