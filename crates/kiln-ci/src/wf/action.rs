//! Action metadata (`action.yml`, docs/ci.md W02-R10), GitHub's keys plus kiln actions' `secrets`,
//! `requires` and `permissions`. Unknown keys are warnings, as GitHub tolerates them; so is a
//! secret the action reads without declaring it (`KCI4041`), which inside it is nothing.

use std::collections::BTreeSet;

use super::diag::{Diag, Severity};
use super::front::Fe;
use super::ir::{Action, ActionInput, ActionOutput, ActionRuns, Step, Tpl};
use super::keys;
use super::yaml::{Node, Scalar};
use crate::expr::Dialect;

pub(crate) fn parse(root: &Node, diags: &mut Vec<Diag>) -> Action {
    let mut local = Vec::new();
    let a = {
        let mut fe = Fe { dialect: Dialect::Kiln, overlay: false, diags: &mut local };
        read(&mut fe, root)
    };
    for mut d in local {
        if d.code == "KCI1010" || d.code == "KCI5002" {
            d.severity = Severity::Warning;
        }
        diags.push(d);
    }
    undeclared(&a, diags);
    a
}

/// `KCI4041`, a warning: a secret the action reads without declaring it. Inside an action only
/// what it declares is there (W09-R07), so the read gets nothing.
fn undeclared(a: &Action, diags: &mut Vec<Diag>) {
    let declared: BTreeSet<String> = a.secrets.iter().map(|d| d.name.to_ascii_uppercase()).chain(["GITHUB_TOKEN".to_owned()]).collect();
    let mut found: Vec<(String, &Tpl)> = Vec::new();
    a.runs.env.iter().for_each(|(k, t)| found.push((format!("runs.env.{k}"), t)));
    a.runs.args.iter().for_each(|t| found.push(("runs.args".into(), t)));
    for (i, s) in a.runs.steps.iter().enumerate() {
        step_templates(s, &format!("runs.steps[{i}]"), &mut found);
    }
    for (path, t) in found {
        let mut used = crate::secrets::Used::default();
        used.add(&t.src);
        for n in used.names.iter().filter(|n| !declared.contains(n.as_str())) {
            diags.push(Diag::warning("KCI4041", format!("this action reads the secret `{n}` without declaring it: add it under `secrets:`, or it gets nothing"), t.range).at(path.as_str()));
        }
    }
}

/// Every template of a step (its parallel ones too), each with where it is.
fn step_templates<'a>(s: &'a Step, at: &str, out: &mut Vec<(String, &'a Tpl)>) {
    for t in s.run.iter().chain(&s.cond).chain(&s.name).chain(&s.working_directory).chain(&s.shell) {
        out.push((at.to_owned(), t));
    }
    s.env.iter().for_each(|(k, t)| out.push((format!("{at}.env.{k}"), t)));
    for (k, v) in &s.with {
        v.templates(&format!("{at}.with.{k}"), out);
    }
    for (i, p) in s.parallel.iter().enumerate() {
        step_templates(p, &format!("{at}.parallel[{i}]"), out);
    }
}

/// `true`, `false`, or the strings GitHub also accepts for them.
fn lenient_bool(n: &Node) -> bool {
    match n.as_scalar() {
        Some(Scalar::Bool(b)) => *b,
        Some(Scalar::Str(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn read(fe: &mut Fe<'_>, root: &Node) -> Action {
    let mut a = Action::default();
    let mut has_runs = false;
    for (k, v, _) in fe.entries(root, keys::ACTION, "") {
        match k.as_str() {
            "name" => a.name = v.text(),
            "description" => a.description = v.text(),
            "author" => a.author = v.text(),
            "branding" => a.branding = Some(fe.yval(v)),
            "inputs" => {
                if let Some(map) = v.as_map() {
                    for (ik, iv) in map {
                        let name = ik.text().unwrap_or_default();
                        let mut i = ActionInput { name: name.clone(), ..ActionInput::default() };
                        if !iv.is_null() {
                            for (key, x, _) in fe.entries(iv, keys::ACTION_INPUT, &format!("inputs.{name}")) {
                                match key.as_str() {
                                    "description" => i.description = x.text(),
                                    "required" => i.required = lenient_bool(x),
                                    "default" => i.default = fe.tpl(x, &format!("inputs.{name}.default")),
                                    "deprecationMessage" => i.deprecation_message = x.text(),
                                    _ => {}
                                }
                            }
                        }
                        a.inputs.push(i);
                    }
                }
            }
            "outputs" => {
                if let Some(map) = v.as_map() {
                    for (ok, ov) in map {
                        let name = ok.text().unwrap_or_default();
                        let mut o = ActionOutput { name: name.clone(), ..ActionOutput::default() };
                        if !ov.is_null() {
                            for (key, x, _) in fe.entries(ov, keys::ACTION_OUTPUT, &format!("outputs.{name}")) {
                                match key.as_str() {
                                    "description" => o.description = x.text(),
                                    "value" => o.value = fe.tpl(x, &format!("outputs.{name}.value")),
                                    _ => {}
                                }
                            }
                        }
                        a.outputs.push(o);
                    }
                }
            }
            "runs" => {
                has_runs = true;
                a.runs = runs(fe, v);
            }
            "secrets" => {
                if let Some(map) = v.as_map() {
                    for (sk, sv) in map {
                        a.secrets.push(super::ir::SecretDecl { name: sk.text().unwrap_or_default(), description: sv.get("description").and_then(Node::text), required: sv.get("required").is_some_and(lenient_bool) });
                    }
                }
            }
            "requires" => a.requires = fe.str_list(v, "requires"),
            "permissions" => a.permissions = Some(super::ir::Permissions::Scopes(v.as_map().map(|m| m.iter().map(|(k, x)| (k.text().unwrap_or_default(), x.text().unwrap_or_default())).collect()).unwrap_or_default())),
            _ => {}
        }
    }
    if !has_runs {
        fe.diags.push(Diag::error("KCI1014", "an action needs `runs`", root.range).at("runs"));
    }
    a
}

fn runs(fe: &mut Fe<'_>, n: &Node) -> ActionRuns {
    let mut r = ActionRuns::default();
    let mut steps_node = None;
    for (k, v, _) in fe.entries(n, keys::ACTION_RUNS, "runs") {
        let at = format!("runs.{k}");
        match k.as_str() {
            "using" => r.using = v.text().unwrap_or_default(),
            "main" => r.main = v.text(),
            "pre" => r.pre = v.text(),
            "post" => r.post = v.text(),
            "pre-if" => r.pre_if = fe.tpl(v, &at),
            "post-if" => r.post_if = fe.tpl(v, &at),
            "image" => r.image = v.text(),
            "entrypoint" => r.entrypoint = v.text(),
            "pre-entrypoint" => r.pre_entrypoint = v.text(),
            "post-entrypoint" => r.post_entrypoint = v.text(),
            "args" => r.args = v.as_seq().map(|s| s.iter().filter_map(|x| fe.tpl(x, &at)).collect()).unwrap_or_default(),
            "env" => r.env = fe.str_map(v, &at),
            "steps" => steps_node = Some(v),
            _ => {}
        }
    }
    match r.using.as_str() {
        "node12" | "node16" | "node20" | "node24" => {
            if r.main.is_none() {
                fe.diags.push(Diag::error("KCI1014", format!("a `{}` action needs `runs.main`", r.using), n.range).at("runs.main"));
            }
        }
        "docker" => {
            if r.image.is_none() {
                fe.diags.push(Diag::error("KCI1014", "a docker action needs `runs.image`", n.range).at("runs.image"));
            }
        }
        "composite" => match steps_node {
            Some(s) => {
                // Composite steps are GitHub's step syntax; a `run` step must say its `shell`.
                let mut gfe = Fe { dialect: Dialect::Gha, overlay: false, diags: &mut *fe.diags };
                r.steps = gfe.steps(s, "runs.steps");
                for (i, st) in r.steps.iter().enumerate() {
                    if st.run.is_some() && st.shell.is_none() {
                        fe.diags.push(Diag::error("KCI1020", "a composite action's `run` step needs `shell`", st.range).at(format!("runs.steps[{i}]")));
                    }
                    // GitHub: a composite action can run in the background, its steps can't.
                    if st.background.is_some() || !st.parallel.is_empty() {
                        fe.diags.push(Diag::error("KCI1020", "a composite action's steps can't use `background` or `parallel`: the action itself can run in the background", st.range).at(format!("runs.steps[{i}]")));
                    }
                }
            }
            None => fe.diags.push(Diag::error("KCI1014", "a composite action needs `runs.steps`", n.range).at("runs.steps")),
        },
        "" => fe.diags.push(Diag::error("KCI1014", "an action needs `runs.using`", n.range).at("runs.using")),
        other => fe.diags.push(Diag::error("KCI1015", format!("`runs.using` is node20, node24, composite or docker, not `{other}`"), n.range).at("runs.using")),
    }
    r
}
