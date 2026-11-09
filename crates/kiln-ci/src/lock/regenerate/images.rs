//! W29-R24/R25: image inputs of every static matrix combination and declared routing choice.
//! Inputs needing event payloads or upstream job outputs cannot be guessed into a lock.

use std::sync::Arc;

use super::*;
use crate::expr::{self, MapContexts, Template};
use crate::plan::{self, PlanAction, RunInput, RunPlan};
use crate::sched::{pools::Pool, routing::{self, LegRoute, Rule}};
use crate::wf::ir::{Axis, Matrix, RunsOn, Tpl, Workflow as Model, YVal};

pub struct ImageContext<'a> { pub repo: &'a str, pub workflow: &'a str, pub job_prefix: &'a str }

fn known(template: &Tpl, dialect: expr::Dialect, matrix: bool) -> Result<(), String> {
    let template = Template::parse(&template.src, dialect).map_err(|e| e.to_string())?;
    if template.references().iter().any(|r| !matrix || !matches!(r.root().as_str(), "matrix" | "strategy")) { return Err("an image or routing input depends on data unavailable when locks are written".into()); }
    Ok(())
}

fn static_value(value: &YVal, dialect: expr::Dialect) -> Result<(), String> {
    match value {
        YVal::Str(t) => known(t, dialect, false),
        YVal::Seq(v) => { for value in v { static_value(value, dialect)?; } Ok(()) }
        YVal::Map(v) => { for (_, value) in v { static_value(value, dialect)?; } Ok(()) }
        _ => Ok(()),
    }
}

fn axis(axis: &Axis, dialect: expr::Dialect) -> Result<(), String> {
    match axis { Axis::Expr(t) => known(t, dialect, false), Axis::Values(v) => { for v in v { static_value(v, dialect)?; } Ok(()) } }
}

/// Resolve only finite image/routing inputs. Job conditions do not remove a pin that another
/// event can need. Matrix expansion is the planner's own; this function queues no real work.
pub fn images(model: &Model, route_context: ImageContext<'_>, rules: &[Rule], pools: &[Pool], defaults: &BTreeMap<String, String>, settings: &plan::Settings) -> Result<Vec<String>, Vec<Diag>> {
    let repo = route_context.repo;
    let dialect = model.dialect.unwrap_or(expr::Dialect::Gha);
    let mut images = BTreeSet::new();
    let mut count = 0usize;
    for original in model.jobs.iter().filter(|j| j.uses.is_none()) {
        let resolve = || -> Result<Vec<plan::LegSpec>, String> {
            let named = rules.iter().filter_map(|r| r.matches.job.as_ref()).any(|s| {
                expr::Condition::parse(s, &expr::ParseOptions::permissive(expr::Dialect::Kiln)).map_or(true, |c| c.references().iter().any(|r| r.root() == "job" && r.name(1).is_none_or(|n| n == "name")))
            });
            let mut templates: Vec<_> = original.container.iter().map(|c| &c.image).chain(original.services.iter().map(|(_, s)| &s.image)).chain(original.kiln.image.iter()).collect();
            if named { templates.extend(original.name.iter()); }
            if let Some(RunsOn::Labels(labels) | RunsOn::Group { labels, .. }) = &original.runs_on { templates.extend(labels); }
            let mut matrix = false;
            for t in templates {
                known(t, dialect, true)?;
                matrix |= !Template::parse(&t.src, dialect).map_err(|e| e.to_string())?.references().is_empty();
            }
            let mut job = original.clone();
            job.needs.clear(); job.cond = None; job.environment = None; job.concurrency = None;
            job.kiln.env_from.clear(); job.kiln.requires.clear(); job.env.clear(); job.timeout_minutes = None; job.continue_on_error = None;
            if let Some(RunsOn::Group { labels, .. }) = &job.runs_on { job.runs_on = Some(RunsOn::Labels(labels.clone())); }
            if !named { job.name = None; }
            if !matrix { job.strategy = None; }
            if let Some(strategy) = &mut job.strategy {
                strategy.when = None; strategy.shard = None; strategy.chunk = None; strategy.fail_fast = None; strategy.max_parallel = None;
                match &mut strategy.matrix {
                    Some(Matrix::Expr(t)) => known(t, dialect, false)?,
                    Some(Matrix::Spec(m)) => {
                        for (_, a) in &m.axes { axis(a, dialect)?; }
                        if let Some(a) = &m.platforms { axis(a, dialect)?; }
                        for v in m.include.iter().chain(m.exclude.iter()) { static_value(v, dialect)?; }
                        m.filter = None;
                    }
                    None => {}
                }
            }
            let mut workflow = model.clone();
            workflow.jobs = vec![job]; workflow.concurrency = None; workflow.run_name = None; workflow.env.clear(); workflow.run_env = false; workflow.supersede = None; workflow.coalesce = None; workflow.cadence = None;
            let input = RunInput { trusted: true, github: serde_json::json!({"repository": repo}), ..Default::default() };
            let (_, actions) = RunPlan::start(Arc::new(workflow), input, settings.clone(), &plan::NoSource)?;
            let mut legs = Vec::new();
            for action in actions {
                match action {
                    PlanAction::Queue(leg) => legs.push(*leg),
                    PlanAction::Job { conclusion: plan::Conclusion::Failure, reason, .. } | PlanAction::Settle { conclusion: plan::Conclusion::Failure, reason, .. } => return Err(reason),
                    _ => {}
                }
            }
            Ok(legs)
        };
        let legs = resolve().map_err(|why| vec![diag("KCI4050", &model.path, format!("{}: {why}", original.id))])?;
        for leg in legs {
            count = count.saturating_add(1);
            if count > kiln::MAX_ITEMS { return Err(vec![diag("KCI1022", &model.path, "image routing exceeds 4096 matrix combinations")]); }
            for image in leg.container.iter().chain(leg.services.iter().map(|(_, image)| image)).filter(|i| !i.is_empty()) { images.insert(format!("image://{image}")); }
            let cx = MapContexts::default().with("matrix", expr::Value::from_json(&leg.matrix)).with("strategy", expr::Value::from_json(&leg.strategy));
            let image = original.kiln.image.as_ref().map(|t| Template::parse(&t.src, dialect).and_then(|t| expr::render(&t, &cx, &mut expr::Budget::default()))).transpose().map_err(|e| vec![diag("KCI4050", &model.path, e.to_string())])?;
            if let Some(image) = &image { images.insert(format!("image://{image}")); }
            let events: BTreeSet<_> = crate::wf::keys::GHA_EVENTS.iter().chain(crate::wf::keys::KILN_EVENTS).copied().chain(model.on.iter().map(|t| t.event.as_str())).collect();
            for event in events {
                let nests = |steps: &[crate::wf::ir::Step]| {
                    let mut todo = vec![steps];
                    let mut found = false;
                    while let Some(steps) = todo.pop() { for step in steps { found |= step.run.as_ref().is_some_and(|r| routing::runs_containers(&r.src)); todo.push(&step.parallel); } }
                    found
                };
                let route = LegRoute { repo: repo.into(), workflow: route_context.workflow.into(), job: format!("{}{}", route_context.job_prefix, leg.job), job_name: leg.job_name.clone(), event: event.into(), labels: leg.runs_on.labels.clone(), group: leg.runs_on.group.clone(), nests_containers: nests(&original.steps), ..Default::default() };
                let pool = pools.iter().find(|p| p.serves(&route.labels));
                let req = routing::resolve(&route, rules).map_err(|e| vec![diag("KCI4050", &model.path, e.to_string())])?;
                let mut executors = pool.map_or(req.executors.clone(), |p| vec![p.executor.clone()]);
                if executors.is_empty() && req.image.is_some() { executors.push("docker".into()); }
                for executor in executors {
                    if !matches!(executor.as_str(), "docker" | "k8s" | "firecracker" | "shards") || executor == "docker" && leg.container.is_some() && pool.is_none() { continue; }
                    let selected = if executor == "k8s" {
                        leg.container.as_ref().or(image.as_ref()).or_else(|| original.kiln.toolchain.as_ref().and_then(|_| defaults.get(&original.id))).or_else(|| pool.and_then(|p| p.image.as_ref())).or(req.image.as_ref()).or_else(|| defaults.get(&original.id))
                    } else { pool.and_then(|p| p.image.as_ref()).or(req.image.as_ref()) };
                    let selected = selected.ok_or_else(|| vec![diag("KCI4050", &model.path, format!("{}: {executor} has no declared executor image", original.id))])?;
                    images.insert(format!("{}://{selected}", if matches!(executor.as_str(), "firecracker" | "shards") { "microvm" } else { "executor" }));
                }
            }
        }
    }
    Ok(images.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r25_matrix_images_and_executor_images_are_lock_inputs_without_guessed_context() -> Result<(), Vec<Diag>> {
        let text = "on: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        image: [acme/a:1, acme/b:2]\n    container: ${{ matrix.image }}\n    steps: [{run: echo ok}]\n  plain:\n    runs-on: ubuntu-latest\n    steps: [{run: echo plain}]\n";
        let (model, ds) = crate::wf::parse_workflow(".github/workflows/ci.yml", text, expr::Dialect::Gha);
        if ds.iter().any(Diag::is_error) { return Err(ds); }
        let model = model.ok_or_else(|| vec![diag("KCI1022", "", "no fixture model")])?;
        let context = || ImageContext { repo: "acme/app", workflow: "ci", job_prefix: "" };
        let pins = images(&model, context(), &routing::github_label_rules(), &[], &BTreeMap::new(), &plan::Settings::default())?;
        assert_eq!(pins, ["executor://kiln/runner-ubuntu-24.04", "image://acme/a:1", "image://acme/b:2"]);
        let mut dynamic = model;
        if let Some(job) = dynamic.jobs.first_mut() && let Some(container) = &mut job.container { container.image.src = "${{ needs.choose.outputs.image }}".into(); }
        assert!(images(&dynamic, context(), &[], &[], &BTreeMap::new(), &plan::Settings::default()).is_err_and(|ds| ds.iter().any(|d| d.code == "KCI4050")));
        Ok(())
    }

    #[test]
    fn w29_r25_images_use_the_callers_name_and_nested_job_path_without_unrelated_dynamic_matrices() -> Result<(), Vec<Diag>> {
        let text = "name: Callee\non: workflow_call\njobs:\n  check:\n    runs-on: baremetal\n    strategy:\n      matrix: ${{ fromJSON(needs.prepare.outputs.rows) }}\n    steps: [{run: echo ok}]\n";
        let (model, ds) = crate::wf::parse_workflow(".github/workflows/callee.yml", text, expr::Dialect::Gha);
        if ds.iter().any(Diag::is_error) { return Err(ds); }
        let model = model.ok_or_else(|| vec![diag("KCI1022", "", "no fixture model")])?;
        let rules = vec![Rule { matches: routing::RuleMatch { job: Some("workflow == 'Release' && job.id == 'outer/check' && event == 'workflow_dispatch'".into()), ..Default::default() }, executor: vec!["docker".into()], image: Some("acme/manual:1".into()), ..Default::default() }];
        let context = ImageContext { repo: "acme/app", workflow: "Release", job_prefix: "outer/" };
        assert_eq!(images(&model, context, &rules, &[], &BTreeMap::new(), &plan::Settings::default())?, ["executor://acme/manual:1"]);
        let context = ImageContext { repo: "acme/app", workflow: "Callee", job_prefix: "" };
        assert!(images(&model, context, &rules, &[], &BTreeMap::new(), &plan::Settings::default())?.is_empty());
        Ok(())
    }
}
