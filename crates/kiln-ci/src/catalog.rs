//! The catalog of what kiln's repositories share (docs/ci.md W29-R20): each action and reusable
//! workflow as an entry, made from its parsed file. The server reads the files, each repository's
//! Access and its tags, and passes them in.

use serde::Serialize;
use serde_json::Value;

use crate::sharing::Access;
use crate::wf::ir::{Action, Workflow};

/// One action or reusable workflow (W29-R20).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    /// What a workflow writes to use it.
    pub uses: String,
    /// `javascript`, `composite`, `docker`, `firecracker`, `shards` or `workflow`.
    pub kind: String,
    pub owner: String,
    pub repo: String,
    /// An action's directory (`""` at the root), a reusable workflow's file.
    pub path: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub inputs: Vec<Param>,
    pub outputs: Vec<Param>,
    pub secrets: Vec<Param>,
    /// Who may use it (W29-R17).
    pub access: Access,
}

/// An input, an output or a secret, as `action.yml` or `on.workflow_call` declares it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Param {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<Value>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
}

/// An action's kind, from its `runs.using`: `node*` is `javascript`; the rest as written.
pub fn kind_of(using: &str) -> String {
    let u = using.trim().to_ascii_lowercase();
    if u.starts_with("node") { "javascript".into() } else { u }
}

/// What to write in `uses:` (W29-R18): the plain name where the server sends the owner's names to
/// kiln (`native`), else `kiln://`.
pub fn uses(native: bool, repo: &str, path: &str, git_ref: &str) -> String {
    let path = path.trim_matches('/');
    let name = if path.is_empty() { repo.to_owned() } else { format!("{repo}/{path}") };
    format!("{}{name}@{git_ref}", if native { "" } else { "kiln://" })
}

/// The directory of an action's metadata file at `path`, where the catalog looks for one: the
/// root's `action.yml`, and those under `.github/actions/` and `.kiln/actions/`.
pub fn action_dir(path: &str) -> Option<&str> {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    if file != "action.yml" && file != "action.yaml" {
        return None;
    }
    (dir.is_empty() || dir.starts_with(".github/actions/") || dir.starts_with(".kiln/actions/")).then_some(dir)
}

fn split(repo: &str) -> (String, String) {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    (owner.to_owned(), name.to_owned())
}

/// The entry of `repo`'s action in `dir`.
pub fn of_action(repo: &str, dir: &str, a: &Action, access: Access, uses: String) -> Entry {
    let (owner, name) = split(repo);
    Entry {
        uses,
        kind: kind_of(&a.runs.using),
        owner,
        repo: name,
        path: dir.to_owned(),
        name: a.name.clone(),
        description: a.description.clone(),
        inputs: a.inputs.iter().map(|i| Param { name: i.name.clone(), description: i.description.clone(), required: i.required, default: i.default.as_ref().map(|d| Value::String(d.src.clone())), ty: None }).collect(),
        outputs: a.outputs.iter().map(|o| Param { name: o.name.clone(), description: o.description.clone(), ..Param::default() }).collect(),
        secrets: a.secrets.iter().map(|s| Param { name: s.name.clone(), description: s.description.clone(), required: s.required, ..Param::default() }).collect(),
        access,
    }
}

/// The entry of `repo`'s reusable workflow `w`: one that's `on: workflow_call`, else none.
pub fn of_workflow(repo: &str, w: &Workflow, access: Access, uses: String) -> Option<Entry> {
    let call = w.on.iter().find(|t| t.event == "workflow_call")?;
    let (owner, name) = split(repo);
    Some(Entry {
        uses,
        kind: "workflow".into(),
        owner,
        repo: name,
        path: w.path.clone(),
        name: w.name.clone(),
        description: None,
        inputs: call.inputs.iter().map(|i| Param { name: i.name.clone(), description: i.description.clone(), required: i.required, default: i.default.as_ref().and_then(|d| serde_json::to_value(d).ok()), ty: i.ty.clone() }).collect(),
        outputs: call.outputs.iter().map(|o| Param { name: o.name.clone(), description: o.description.clone(), ..Param::default() }).collect(),
        secrets: call.secrets.iter().map(|s| Param { name: s.name.clone(), description: s.description.clone(), required: s.required, ..Param::default() }).collect(),
        access,
    })
}

/// `q=`, `kind=` and `owner=` (W29-R20): each kept when empty.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// In its `uses`, name or description, any case.
    pub q: String,
    pub kind: String,
    pub owner: String,
}

impl Filter {
    pub fn keeps(&self, e: &Entry) -> bool {
        let q = self.q.trim().to_ascii_lowercase();
        let found = q.is_empty() || [Some(e.uses.as_str()), e.name.as_deref(), e.description.as_deref()].into_iter().flatten().any(|t| t.to_ascii_lowercase().contains(&q));
        found && (self.kind.is_empty() || e.kind.eq_ignore_ascii_case(self.kind.trim())) && (self.owner.is_empty() || e.owner.eq_ignore_ascii_case(self.owner.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Dialect;

    #[test]
    fn w29_r20_where_actions_are() {
        for (path, dir) in [("action.yml", Some("")), (".github/actions/x/action.yml", Some(".github/actions/x")), (".kiln/actions/a/b/action.yaml", Some(".kiln/actions/a/b"))] {
            assert_eq!(action_dir(path), dir, "{path}");
        }
        for path in ["src/action.yml", ".github/actions/x/README.md", ".github/workflows/ci.yml", "action.json"] {
            assert_eq!(action_dir(path), None, "{path}");
        }
        assert_eq!(uses(true, "acme/kit", ".github/actions/x", "v1"), "acme/kit/.github/actions/x@v1");
        assert_eq!(uses(false, "acme/kit", "", "v1"), "kiln://acme/kit@v1");
        assert_eq!(uses(false, "acme/kit", ".github/workflows/build.yml", "v1"), "kiln://acme/kit/.github/workflows/build.yml@v1");
        for (using, kind) in [("node20", "javascript"), ("node24", "javascript"), ("composite", "composite"), ("docker", "docker"), ("Firecracker", "firecracker"), ("shards", "shards")] {
            assert_eq!(kind_of(using), kind, "{using}");
        }
    }

    #[test]
    fn w29_r20_entries_and_their_interfaces() {
        let (a, _) = crate::wf::parse_action("name: Hello\ndescription: Says hello\ninputs:\n  who:\n    description: Whom\n    required: true\n  how:\n    default: loudly\noutputs:\n  said:\n    description: What\n    value: ${{ steps.s.outputs.said }}\nruns:\n  using: composite\n  steps:\n    - id: s\n      run: echo said=hi >> $GITHUB_OUTPUT\n      shell: bash\n");
        let a = a.unwrap_or_default();
        let e = of_action("acme/kit", ".github/actions/hello", &a, Access::Owner, uses(true, "acme/kit", ".github/actions/hello", "v1"));
        assert_eq!((e.kind.as_str(), e.owner.as_str(), e.repo.as_str(), e.path.as_str(), e.access), ("composite", "acme", "kit", ".github/actions/hello", Access::Owner));
        assert_eq!(e.description.as_deref(), Some("Says hello"));
        assert_eq!(e.inputs.iter().map(|i| (i.name.as_str(), i.required, i.default.clone())).collect::<Vec<_>>(), [("who", true, None), ("how", false, Some(Value::String("loudly".into())))]);
        assert_eq!(e.outputs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["said"]);

        let src = "name: build\non:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n        required: true\n    outputs:\n      digest:\n        description: The image\n        value: ${{ jobs.b.outputs.digest }}\n    secrets:\n      token:\n        required: true\njobs:\n  b:\n    runs-on: local\n    outputs:\n      digest: x\n    steps:\n      - run: echo built\n";
        let (w, _) = crate::wf::parse_workflow(".github/workflows/build.yml", src, Dialect::Gha);
        let w = w.unwrap_or_default();
        let e = of_workflow("acme/kit", &w, Access::Everyone, uses(false, "acme/kit", &w.path, "v1"));
        let e = e.unwrap_or_else(|| of_action("", "", &Action::default(), Access::None, String::new()));
        assert_eq!((e.kind.as_str(), e.path.as_str(), e.uses.as_str()), ("workflow", ".github/workflows/build.yml", "kiln://acme/kit/.github/workflows/build.yml@v1"));
        assert_eq!(e.inputs.iter().map(|i| (i.name.as_str(), i.required, i.ty.as_deref())).collect::<Vec<_>>(), [("target", true, Some("string"))]);
        assert_eq!(e.outputs.iter().map(|o| (o.name.as_str(), o.description.as_deref())).collect::<Vec<_>>(), [("digest", Some("The image"))]);
        assert_eq!(e.secrets.iter().map(|s| (s.name.as_str(), s.required)).collect::<Vec<_>>(), [("token", true)]);
        // One that isn't called isn't a reusable workflow.
        let (ci, _) = crate::wf::parse_workflow(".github/workflows/ci.yml", "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo\n", Dialect::Gha);
        assert_eq!(of_workflow("acme/kit", &ci.unwrap_or_default(), Access::Everyone, String::new()), None);

        // `q=`, `kind=` and `owner=`.
        let keeps = |q: &str, kind: &str, owner: &str| Filter { q: q.into(), kind: kind.into(), owner: owner.into() }.keeps(&e);
        assert!(keeps("", "", "") && keeps("BUILD", "", "") && keeps("", "workflow", "") && keeps("", "", "ACME"));
        assert!(!keeps("lint", "", "") && !keeps("", "composite", "") && !keeps("", "", "other"));
    }
}
