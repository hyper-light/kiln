//! Overlays (docs/ci.md W02-R07): kiln keys added to a GitHub workflow without editing it, so the
//! workflow keeps working on GitHub. An overlay may set the workflow-level kiln keys, any kiln key
//! of a job (and the kiln parts of its `strategy`, `matrix` and `concurrency`), the kiln keys of a
//! step addressed by its `id`, and replace a job's `runs-on`. Nothing else.

use super::diag::{Diag, Range};
use super::front::Fe;
use super::ir::{JobKiln, Matrix, RunsOn, StepKiln, Workflow, YVal};
use super::keys;
use super::yaml::{Node, Scalar};
use crate::expr::Dialect;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    pub supersede: Option<YVal>,
    pub routing_infer: Option<bool>,
    pub honour_skip_ci: Option<bool>,
    pub chain_depth: Option<u32>,
    pub jobs: Vec<OverlayJob>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayJob {
    pub id: String,
    pub range: Range,
    pub runs_on: Option<RunsOn>,
    /// Only the keys the overlay set are `Some`/non-empty.
    pub kiln: JobKiln,
    pub set: Vec<String>,
    pub strategy_when: Option<super::ir::Tpl>,
    pub strategy_shard: Option<YVal>,
    pub strategy_chunk: Option<YVal>,
    pub fail_fast: Option<YVal>,
    pub max_parallel: Option<YVal>,
    pub matrix_where: Option<super::ir::Tpl>,
    pub matrix_platforms: Option<super::ir::Axis>,
    pub matrix_emulate: Vec<String>,
    pub concurrency_limit: Option<u32>,
    pub concurrency_queue: Option<String>,
    pub steps: Vec<(String, Range, StepKiln)>,
}

pub(crate) fn parse(root: &Node, diags: &mut Vec<Diag>) -> Overlay {
    let mut fe = Fe { dialect: Dialect::Kiln, overlay: true, diags };
    let mut ov = Overlay::default();
    for (k, v, _) in fe.entries(root, keys::WORKFLOW, "") {
        match k.as_str() {
            "supersede" => ov.supersede = Some(fe.yval(v)),
            "routing" => ov.routing_infer = Some(v.get("infer").and_then(Node::as_scalar) == Some(&Scalar::Bool(true))),
            "kiln" => {
                ov.honour_skip_ci = Some(v.get("honour-skip-ci").and_then(Node::as_scalar) == Some(&Scalar::Bool(true)));
                ov.chain_depth = fe.chain_depth(v);
            }
            "jobs" => {
                let Some(map) = v.as_map() else { continue };
                for (jk, jv) in map {
                    let id = jk.text().unwrap_or_default();
                    ov.jobs.push(job(&mut fe, &id, jv, jk.range));
                }
            }
            _ => {}
        }
    }
    ov
}

fn job(fe: &mut Fe<'_>, id: &str, n: &Node, range: Range) -> OverlayJob {
    let path = format!("jobs.{id}");
    let mut j = OverlayJob { id: id.to_owned(), range, ..OverlayJob::default() };
    let Some(map) = n.as_map() else {
        fe.diags.push(Diag::error("KCI1011", "an overlay's job is a mapping of kiln keys", n.range).at(&path));
        return j;
    };
    // Each kiln job key goes through the kiln dialect's reader, so it means what it means in .kiln/.
    let mut kiln_keys = Vec::new();
    for (k, v) in map {
        let name = k.text().unwrap_or_default();
        let at = format!("{path}.{name}");
        match name.as_str() {
            "runs-on" => {
                let labels = match v.as_seq() {
                    Some(items) => items.iter().filter_map(|x| fe.tpl(x, &at)).collect(),
                    None => fe.tpl(v, &at).into_iter().collect(),
                };
                j.runs_on = Some(RunsOn::Labels(labels));
            }
            "steps" => {
                let Some(steps) = v.as_map() else {
                    fe.diags.push(Diag::error("KCI1011", "an overlay's `steps` maps step ids to kiln keys", v.range).at(&at));
                    continue;
                };
                for (sk, sv) in steps {
                    let sid = sk.text().unwrap_or_default();
                    let s = fe.step(sv, &format!("{at}.{sid}"));
                    j.steps.push((sid, sk.range, s.kiln));
                }
            }
            "strategy" => {
                let Some(sm) = v.as_map() else { continue };
                for (sk, sv) in sm {
                    let key = sk.text().unwrap_or_default();
                    let sat = format!("{at}.{key}");
                    match key.as_str() {
                        "when" => j.strategy_when = fe.tpl(sv, &sat),
                        "shard" => j.strategy_shard = Some(fe.yval(sv)),
                        "chunk" => j.strategy_chunk = Some(fe.yval(sv)),
                        "fail-fast" if sv.as_map().is_some() => j.fail_fast = Some(fe.yval(sv)),
                        "max-parallel" if sv.as_map().is_some() => j.max_parallel = Some(fe.yval(sv)),
                        "matrix" => {
                            let Some(mm) = sv.as_map() else { continue };
                            for (mk, mv) in mm {
                                match mk.text().as_deref() {
                                    Some("where") => j.matrix_where = fe.tpl(mv, &format!("{sat}.where")),
                                    Some("platforms") => {
                                        j.matrix_platforms = Some(super::ir::Axis::Values(mv.as_seq().map(|s| s.iter().map(|x| fe.yval(x)).collect()).unwrap_or_default()));
                                    }
                                    Some("emulate") => j.matrix_emulate = fe.str_list(mv, &format!("{sat}.emulate")),
                                    other => fe.diags.push(
                                        Diag::error("KCI1010", format!("an overlay can set a matrix's `where`, `platforms` and `emulate`, not `{}`", other.unwrap_or_default()), mk.range).at(&sat),
                                    ),
                                }
                            }
                        }
                        other => fe.diags.push(Diag::error("KCI1010", format!("an overlay can set a strategy's kiln keys (`when`, `shard`, `chunk`, object forms), not `{other}`"), sk.range).at(&sat)),
                    }
                }
            }
            "concurrency" => {
                if let Some(l) = v.get("limit") {
                    j.concurrency_limit = l.text().and_then(|t| t.parse().ok());
                }
                if let Some(q) = v.get("queue") {
                    j.concurrency_queue = q.text();
                }
                if let Some(cm) = v.as_map() {
                    for (ck, _) in cm {
                        if !matches!(ck.text().as_deref(), Some("limit" | "queue")) {
                            fe.diags.push(Diag::error("KCI1010", "an overlay can set a concurrency group's `limit` and `queue`, nothing else", ck.range).at(&at));
                        }
                    }
                }
            }
            _ => match keys::find(keys::JOB, &name) {
                Some(key) if key.overlay => {
                    kiln_keys.push((k.clone(), v.clone()));
                    j.set.push(name);
                }
                Some(_) => fe.diags.push(Diag::error("KCI1010", format!("`{name}` can't be set by an overlay: overlays add kiln keys and may replace `runs-on`, nothing else"), k.range).at(&at)),
                None => {
                    let hint = super::diag::suggest(&name, keys::JOB.iter().filter(|x| x.overlay).map(|x| x.name)).map(|s| format!(" (did you mean `{s}`?)")).unwrap_or_default();
                    fe.diags.push(Diag::error("KCI1010", format!("unknown key `{name}`{hint}"), k.range).at(&at));
                }
            },
        }
    }
    if !kiln_keys.is_empty() {
        let synthetic = Node { kind: super::yaml::Kind::Map(kiln_keys), range: n.range };
        let mut kfe = Fe { dialect: Dialect::Kiln, overlay: true, diags: &mut *fe.diags };
        let parsed = kfe.job_kiln_only(id, &synthetic);
        j.kiln = parsed;
    }
    j
}

/// Applies an overlay to a GHA workflow; reports overlay entries that match nothing (`KCI4010`).
pub fn apply(wf: &mut Workflow, ov: &Overlay) -> Vec<Diag> {
    let mut diags = Vec::new();
    if let Some(s) = &ov.supersede {
        wf.supersede = Some(s.clone());
    }
    if let Some(b) = ov.routing_infer {
        wf.routing_infer = b;
    }
    if let Some(b) = ov.honour_skip_ci {
        wf.honour_skip_ci = b;
    }
    if ov.chain_depth.is_some() {
        wf.chain_depth = ov.chain_depth;
    }
    for oj in &ov.jobs {
        let Some(job) = wf.jobs.iter_mut().find(|j| j.id == oj.id) else {
            diags.push(Diag::warning("KCI4010", format!("the overlay sets job `{}`, which this workflow doesn't have", oj.id), oj.range).at(format!("jobs.{}", oj.id)));
            continue;
        };
        if let Some(r) = &oj.runs_on {
            job.runs_on = Some(r.clone());
        }
        merge_kiln(&mut job.kiln, &oj.kiln, &oj.set);
        if oj.strategy_when.is_some() || oj.strategy_shard.is_some() || oj.strategy_chunk.is_some() || oj.fail_fast.is_some() || oj.max_parallel.is_some() {
            let s = job.strategy.get_or_insert_with(Default::default);
            s.when = oj.strategy_when.clone().or(s.when.take());
            s.shard = oj.strategy_shard.clone().or(s.shard.take());
            s.chunk = oj.strategy_chunk.clone().or(s.chunk.take());
            s.fail_fast = oj.fail_fast.clone().or(s.fail_fast.take());
            s.max_parallel = oj.max_parallel.clone().or(s.max_parallel.take());
        }
        if oj.matrix_where.is_some() || oj.matrix_platforms.is_some() || !oj.matrix_emulate.is_empty() {
            match job.strategy.as_mut().and_then(|s| s.matrix.as_mut()) {
                Some(Matrix::Spec(m)) => {
                    if oj.matrix_where.is_some() {
                        m.filter = oj.matrix_where.clone();
                    }
                    if oj.matrix_platforms.is_some() {
                        m.platforms = oj.matrix_platforms.clone();
                    }
                    if !oj.matrix_emulate.is_empty() {
                        m.emulate = oj.matrix_emulate.clone();
                    }
                }
                _ => diags.push(Diag::warning("KCI4010", format!("the overlay sets a matrix on job `{}`, which has no matrix of axes", oj.id), oj.range).at(format!("jobs.{}.strategy.matrix", oj.id))),
            }
        }
        if let Some(c) = job.concurrency.as_mut() {
            if oj.concurrency_limit.is_some() {
                c.limit = oj.concurrency_limit;
            }
            if oj.concurrency_queue.is_some() {
                c.queue = oj.concurrency_queue.clone();
            }
        }
        for (sid, range, sk) in &oj.steps {
            match job.steps.iter_mut().find(|s| s.id.as_deref() == Some(sid)) {
                Some(step) => {
                    if sk.retry.is_some() {
                        step.kiln.retry = sk.retry.clone();
                    }
                    if sk.test_results.is_some() {
                        step.kiln.test_results = sk.test_results.clone();
                    }
                    if !sk.integrations.is_empty() {
                        step.kiln.integrations = sk.integrations.clone();
                    }
                }
                None => diags.push(Diag::warning("KCI4010", format!("the overlay sets step `{sid}` of job `{}`, which has no step with that id", oj.id), *range).at(format!("jobs.{}.steps.{sid}", oj.id))),
            }
        }
    }
    diags
}

fn merge_kiln(dst: &mut JobKiln, src: &JobKiln, set: &[String]) {
    for key in set {
        match key.as_str() {
            "exclusive" => dst.exclusive = src.exclusive,
            "resources" => dst.resources = src.resources.clone(),
            "requires" => dst.requires = src.requires.clone(),
            "prefer" => dst.prefer = src.prefer.clone(),
            "priority" => dst.priority = src.priority.clone(),
            "retry" => dst.retry = src.retry.clone(),
            "reuse" | "work" => dst.reuse = src.reuse.clone(),
            "cache" => dst.cache = src.cache.clone(),
            "exports" => dst.exports_env = src.exports_env.clone(),
            "env-from" => dst.env_from = src.env_from.clone(),
            "integrations" => dst.integrations = src.integrations.clone(),
            "registries" => dst.registries = src.registries.clone(),
            "network" => dst.network = src.network.clone(),
            "debug" => dst.debug = src.debug.clone(),
            "attest" => dst.attest = src.attest.clone(),
            "quarantine" => dst.quarantine_enforce = src.quarantine_enforce,
            "bench-gate" => dst.bench_gate = src.bench_gate.clone(),
            "agents" => dst.agents = src.agents.clone(),
            _ => {}
        }
    }
}
