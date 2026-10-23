//! The data flow of a workflow's outputs and exported environment (docs/ci.md W04-R23), from the
//! workflow itself: where each job output comes from (the step whose output its value reads), and
//! every place a job reads `needs.<job>.outputs.<name>` (its `if`, matrix, `runs-on`, env, a step's
//! `if`, `run`, `with` or env, its own outputs, …); and which jobs take another's exported
//! environment (`env-from`). A run joins it with what each of its legs reported (the server's
//! `GET /api/ci/runs/:id/flow`).

use serde::Serialize;
use serde_json::Value as Json;

use super::ir::Workflow;
use crate::expr::{Condition, ContextPath, Dialect, ParseOptions, PathSeg, Template};

/// One job output: where its value comes from, and who reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputFlow {
    pub job: String,
    pub name: String,
    /// The step whose output its value reads: its index in the job's steps, and its id.
    pub step: Option<(usize, String)>,
    /// Declared `secret: true` (kiln): its value stays with the secrets (W04-R22).
    pub secret: bool,
    pub readers: Vec<Reader>,
}

/// A job that reads an output, and where in it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reader {
    pub job: String,
    /// `if`, `strategy.matrix`, `env.TOKEN`, `steps[1].run`, `steps[0].with.version`, …
    pub at: Vec<String>,
}

/// A job's exported environment (`exports: { env }`) and the jobs that take it (`env-from`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportFlow {
    pub job: String,
    pub names: Vec<String>,
    pub readers: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Flow {
    pub outputs: Vec<OutputFlow>,
    pub exports: Vec<ExportFlow>,
}

/// The flow of `wf`'s outputs and exports, its jobs in their order.
pub fn flow(wf: &Workflow) -> Flow {
    let dialect = wf.dialect.unwrap_or(Dialect::Gha);
    // Every read of another job's outputs: (the reader, the job read, its output or `None` for all
    // of them, where).
    let mut reads: Vec<(String, String, Option<String>, String)> = Vec::new();
    for j in &wf.jobs {
        let mut found = Vec::new();
        strings(&serde_json::to_value(j).unwrap_or_default(), &mut Vec::new(), &mut found, dialect);
        for (at, paths) in found {
            for p in &paths {
                if let Some((job, output)) = needs_output(p) {
                    reads.push((j.id.clone(), job, output, at.clone()));
                }
            }
        }
    }
    let outputs = wf
        .jobs
        .iter()
        .flat_map(|j| j.outputs.iter().map(move |o| (j, o)))
        .map(|(j, o)| {
            let step = Template::parse(&o.value.src, dialect)
                .ok()
                .and_then(|t| t.references().iter().find_map(step_output))
                .and_then(|id| j.steps.iter().position(|s| s.id.as_deref() == Some(id.as_str())).map(|i| (i, id)));
            let mut readers: Vec<Reader> = Vec::new();
            for (reader, job, output, at) in &reads {
                if !job.eq_ignore_ascii_case(&j.id) || output.as_ref().is_some_and(|n| !n.eq_ignore_ascii_case(&o.name)) {
                    continue;
                }
                match readers.iter_mut().find(|r| r.job == *reader) {
                    Some(r) if !r.at.contains(at) => r.at.push(at.clone()),
                    Some(_) => {}
                    None => readers.push(Reader { job: reader.clone(), at: vec![at.clone()] }),
                }
            }
            OutputFlow { job: j.id.clone(), name: o.name.clone(), step, secret: o.secret, readers }
        })
        .collect();
    let exports = wf
        .jobs
        .iter()
        .filter(|j| !j.kiln.exports_env.is_empty())
        .map(|j| ExportFlow {
            job: j.id.clone(),
            names: j.kiln.exports_env.clone(),
            readers: wf.jobs.iter().filter(|r| r.kiln.env_from.iter().any(|f| f.eq_ignore_ascii_case(&j.id))).map(|r| r.id.clone()).collect(),
        })
        .collect();
    Flow { outputs, exports }
}

/// `needs.<job>.outputs.<name>`: the job and the output (`None`: all of them, read whole).
fn needs_output(p: &ContextPath) -> Option<(String, Option<String>)> {
    let name = |i: usize| match p.0.get(i) {
        Some(PathSeg::Name(n)) => Some(n.as_str()),
        _ => None,
    };
    if !name(0)?.eq_ignore_ascii_case("needs") || !name(2)?.eq_ignore_ascii_case("outputs") {
        return None;
    }
    Some((name(1)?.to_owned(), name(3).map(str::to_owned)))
}

/// `steps.<id>.outputs.…`: the step's id.
fn step_output(p: &ContextPath) -> Option<String> {
    match (p.0.first(), p.0.get(1), p.0.get(2)) {
        (Some(PathSeg::Name(s)), Some(PathSeg::Name(id)), Some(PathSeg::Name(o))) if s.eq_ignore_ascii_case("steps") && o.eq_ignore_ascii_case("outputs") => Some(id.clone()),
        _ => None,
    }
}

/// Every string in a job's model that has expressions (an `if:` is one whether or not it's
/// wrapped in `${{ }}`), where it is (`steps[1].with.version`) and the context paths it reads.
fn strings(v: &Json, at: &mut Vec<String>, out: &mut Vec<(String, Vec<ContextPath>)>, dialect: Dialect) {
    match v {
        Json::String(s) => {
            let refs = if at.last().is_some_and(|k| k == "if") {
                Condition::parse(s, &ParseOptions::permissive(dialect)).map(|c| c.references()).ok()
            } else if s.contains("${{") {
                Template::parse(s, dialect).map(|t| t.references()).ok()
            } else {
                None
            };
            if let Some(r) = refs.filter(|r| !r.is_empty()) {
                let label = at.iter().filter(|s| !s.is_empty()).fold(String::new(), |acc, s| if acc.is_empty() || s.starts_with('[') { format!("{acc}{s}") } else { format!("{acc}.{s}") });
                out.push((label, r));
            }
        }
        Json::Array(items) => {
            // `[name, value]` pairs (`env`, `with`, a matrix's axes) are a mapping's entries, and
            // `{ name, value }` items (outputs) are named by their names; a job's steps by index.
            let pairs = !items.is_empty() && items.iter().all(|x| x.as_array().is_some_and(|p| p.len() == 2 && p.first().is_some_and(Json::is_string)));
            let steps = at.last().is_some_and(|k| k == "steps");
            for (i, x) in items.iter().enumerate() {
                let named = x.get("name").and_then(Json::as_str).filter(|_| x.get("value").is_some());
                if pairs {
                    at.push(x.get(0).and_then(Json::as_str).unwrap_or_default().to_owned());
                    strings(x.get(1).unwrap_or(&Json::Null), at, out, dialect);
                } else if let Some(n) = named {
                    at.push(n.to_owned());
                    strings(x.get("value").unwrap_or(&Json::Null), at, out, dialect);
                } else {
                    at.push(if steps { format!("[{i}]") } else { String::new() });
                    strings(x, at, out, dialect);
                }
                at.pop();
            }
        }
        Json::Object(m) => {
            for (k, x) in m {
                at.push(match k.as_str() {
                    "cond" => "if".to_owned(),
                    "expr" | "spec" | "values" | "labels" | "kiln" | "src" => String::new(),
                    other => other.replace('_', "-"),
                });
                strings(x, at, out, dialect);
                at.pop();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(src: &str) -> Workflow {
        super::super::parse_workflow(".github/workflows/ci.yml", src, Dialect::Gha).0.unwrap_or_default()
    }

    /// W04-R23: an output's producing step, and each job that reads it, with where.
    #[test]
    fn w04_r23_where_outputs_come_from_and_go() {
        let f = flow(&wf(r#"on: push
jobs:
  build:
    runs-on: x
    outputs:
      version: ${{ steps.v.outputs.version }}
      shards: ${{ steps.s.outputs.list }}
    steps:
      - run: echo setup
      - id: v
        run: echo "version=1" >> "$GITHUB_OUTPUT"
      - id: s
        run: echo "list=[1,2]" >> "$GITHUB_OUTPUT"
  test:
    needs: build
    if: needs.build.outputs.version != ''
    runs-on: x
    strategy:
      matrix:
        shard: ${{ fromJSON(needs.build.outputs.shards) }}
    env:
      VERSION: ${{ needs.build.outputs.version }}
    steps:
      - uses: actions/setup-node@v4
        with:
          node-version: ${{ needs.build.outputs.version }}
      - run: echo ${{ toJSON(needs.build.outputs) }}
  quiet:
    needs: build
    runs-on: x
    steps:
      - run: echo nothing read
"#));
        let version = f.outputs.iter().find(|o| o.name == "version").cloned();
        assert_eq!(version.as_ref().and_then(|o| o.step.clone()), Some((1, "v".to_owned())), "{f:?}");
        let test = version.as_ref().and_then(|o| o.readers.iter().find(|r| r.job == "test")).map(|r| r.at.clone()).unwrap_or_default();
        for at in ["if", "env.VERSION", "steps[0].with.node-version", "steps[1].run"] {
            assert!(test.contains(&at.to_owned()), "{at}: {test:?}");
        }
        let shards = f.outputs.iter().find(|o| o.name == "shards").cloned();
        assert_eq!(shards.as_ref().and_then(|o| o.step.clone()), Some((2, "s".to_owned())));
        let at = shards.as_ref().and_then(|o| o.readers.first()).map(|r| r.at.clone()).unwrap_or_default();
        assert!(at.iter().any(|a| a.starts_with("strategy.matrix")), "{at:?}");
        assert!(at.contains(&"steps[1].run".to_owned()), "read whole, by toJSON: {at:?}");
        assert!(f.outputs.iter().all(|o| o.readers.iter().all(|r| r.job != "quiet")), "{f:?}");
    }
}
