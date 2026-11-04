//! Mission Control attention, plan 01a199e1b177752e84dad5dc990e6a15, steps 2, 4 and 5.
//!
//! ACT-R base-level activation is `ln(sum(weight(kind) * age^(-decay)))`. Both the
//! decay and relative weights are fitted to observed choices, rather than hand-tuned.
//! The weight of the first observed human use is one: a scale convention, since scaling
//! every weight equally cannot change a choice probability. A model can additionally learn
//! the span of a chat's history as a contextual feature.
//!
//! Sources: Anderson & Schooler (1991), doi:10.1111/j.1467-9280.1991.tb00174.x;
//! Stocco et al. (2024), arXiv:2410.18055 (conditional likelihood fitting of ACT-R);
//! Rubin (1981), doi:10.1214/aos/1176345338 (Bayesian bootstrap);
//! Russo et al. (2018), arXiv:1707.02038 (Thompson sampling).
//!
//! Thompson sampling here uses a *Bayesian-bootstrap approximation*: independent uniforms
//! supplied by the caller give exponential observation weights; their weighted MLE is one
//! draw. No exploration rate or fitted-parameter prior is invented. A singular/unbounded
//! likelihood is reported, and sparse history falls back to deliberate human recency.
//! Unit parameters are only the numerical optimizer's initial point; they are never served
//! as a fitted model if optimization fails. Numerical stopping tolerances come from
//! floating-point resolution, not product behavior settings. Times are injected in unix ms.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UseKind {
    Message,
    Answer,
    Command,
    Open,
    Pin,
    Park,
    Unpark,
    Archive,
    Restore,
    Turn,
    SubagentTurn,
}

impl UseKind {
    pub fn human(self) -> bool {
        !matches!(self, Self::Turn | Self::SubagentTurn | Self::Park | Self::Archive)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Message => "message", Self::Answer => "answer", Self::Command => "command",
            Self::Open => "open", Self::Pin => "pin", Self::Park => "park", Self::Unpark => "unpark",
            Self::Archive => "archive", Self::Restore => "restore", Self::Turn => "turn", Self::SubagentTurn => "subagent_turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Use {
    pub agent: String,
    pub kind: UseKind,
    pub at: i64,
}

/// Candidate IDs must be the chats actually available to choose from *at this time*.
/// `propensity` is a known logging-policy/exposure probability, never guessed from clicks.
/// `None` keeps the observation, explicitly reporting that position bias is uncorrected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub at: i64,
    pub chosen: String,
    pub candidates: Vec<String>,
    #[serde(default)]
    pub propensity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<RelativeCorrection>,
}

/// Relative PBM examination, not an absolute probability. Its unidentified scale cancels
/// by self-normalizing inverse weights within each layout/reference stratum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelativeCorrection {
    pub layout: String,
    pub reference: usize,
    pub examination: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub decay: f64,
    pub weights: BTreeMap<UseKind, f64>,
    /// Coefficient on log(history span / age of latest use), or None without that feature.
    #[serde(default)]
    pub range_weight: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub name: String,
    pub value: f64,
    /// Local likelihood uncertainty, not a claim that a small dataset identifies the parameter.
    pub standard_error: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub name: String,
    pub choices: usize,
    pub log_likelihood: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnavailableComparison {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitReport {
    pub model: Option<Model>,
    pub observations: usize,
    pub ignored: usize,
    pub position_corrected: usize,
    pub parameters: Vec<Estimate>,
    pub comparisons: Vec<Comparison>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_comparisons: Vec<UnavailableComparison>,
    /// The fitted family beat both user recency and the model without agent turns on the
    /// newest held-out visit. The server must not enable it merely because an MLE exists.
    pub improves_baseline: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranked {
    pub agent: String,
    pub score: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("one independent uniform in (0, 1) is needed per recorded choice")]
    Randomness,
    #[error("the attention model contains an invalid decay, weight or range coefficient")]
    Model,
}

#[derive(Clone)]
struct Trace {
    // One immutable sorted event array per agent is shared by every choice. A trace stores
    // its exact earlier-event bounds; long histories are never copied per choice or cut off.
    history: Arc<[(UseKind,i64)]>,
    start: usize,
    end: usize,
    at: i64,
    kind_override: Option<UseKind>,
    range: f64,
}

impl Trace{
    fn entries(&self)->impl Iterator<Item=(UseKind,i64)>+'_ {
        self.history.iter().skip(self.start).take(self.end.saturating_sub(self.start)).map(|(kind,at)|(self.kind_override.unwrap_or(*kind),*at))
    }
    fn events(&self)->impl Iterator<Item=(UseKind,f64)>+'_ {
        self.entries().map(|(kind,at)|(kind,(self.at.saturating_sub(at).max(1)as f64).ln()))
    }
}

#[derive(Clone)]
struct Observation {
    at: i64,
    traces: Vec<Trace>,
    chosen: usize,
    weight: f64,
    corrected: bool,
}

struct Layout {
    kinds: Vec<UseKind>,
    anchor: UseKind,
    range: bool,
}

impl Layout {
    fn from(observations: &[Observation], range: bool) -> Option<Self> {
        let kinds: BTreeSet<_> = observations.iter().flat_map(|o| o.traces.iter()).flat_map(|t| t.entries().map(|(kind,_)|kind)).collect();
        let anchor = kinds.iter().find(|k| k.human()).copied()?;
        Some(Self { kinds: kinds.into_iter().collect(), anchor, range })
    }

    fn free(&self) -> impl Iterator<Item = UseKind> + '_ {
        self.kinds.iter().copied().filter(|k| *k != self.anchor)
    }

    fn dim(&self) -> usize {
        self.kinds.len() + usize::from(self.range)
    }

    fn model(&self, x: &[f64]) -> Option<Model> {
        let decay = x.first().copied()?.exp();
        let mut weights = BTreeMap::from([(self.anchor, 1.0)]);
        for (k, v) in self.free().zip(x.iter().skip(1)) {
            weights.insert(k, v.exp());
        }
        let range_weight = self.range.then(|| x.last().copied()).flatten();
        let m = Model { decay, weights, range_weight };
        valid(&m).then_some(m)
    }
}

fn valid(model: &Model) -> bool {
    model.decay.is_finite() && model.decay > 0.0 && !model.weights.is_empty()
        && model.weights.values().all(|w| w.is_finite() && *w > 0.0)
        && model.range_weight.is_none_or(f64::is_finite)
}

fn history(uses: &[Use]) -> BTreeMap<&str, Arc<[(UseKind,i64)]>> {
    let mut out: BTreeMap<&str, Vec<(UseKind,i64)>> = BTreeMap::new();
    for u in uses.iter().filter(|u|!matches!(u.kind,UseKind::Park|UseKind::Archive)) {
        out.entry(&u.agent).or_default().push((u.kind,u.at));
    }
    out.into_iter().map(|(id,mut events)|{events.sort_by_key(|(_,at)|*at);(id,Arc::from(events))}).collect()
}

fn trace(history:Arc<[(UseKind,i64)]>, at: i64) -> Trace {
    // ACT-R never has a zero-age exposure. One ms is the input clock's resolution, not a
    // relevance lifetime. Strictly earlier events also avoid learning from the chosen action.
    let end=history.partition_point(|(_,when)|*when<at);
    let first=history.first().filter(|_|end>0).map(|(_,at)|*at);
    let last=end.checked_sub(1).and_then(|index|history.get(index)).map(|(_,at)|*at);
    let range = first.zip(last).map(|(first, last)| {
        let span = last.saturating_sub(first) as f64;
        (span / at.saturating_sub(last).max(1) as f64).ln_1p()
    }).unwrap_or(0.0);
    Trace { history,start:0,end,at,kind_override:None,range }
}

fn prepare(uses: &[Use], choices: &[Choice], weights: Option<&[f64]>) -> Vec<Observation> {
    let histories = history(uses);
    let mut prepared:Vec<_>=choices.iter().enumerate().filter_map(|(index, c)| {
        let ids: BTreeSet<_> = c.candidates.iter().collect();
        if ids.len() != c.candidates.len() || ids.len() < 2 { return None; }
        let chosen = c.candidates.iter().position(|id| id == &c.chosen)?;
        if c.propensity.is_some() && c.correction.is_some(){return None;}
        let group=c.correction.as_ref().map(|r|(r.layout.clone(),r.reference));
        if c.correction.as_ref().is_some_and(|r|r.layout.is_empty() || r.reference==0){return None;}
        let propensity = c.correction.as_ref().map(|r|r.examination).or(c.propensity).unwrap_or(1.0);
        if !propensity.is_finite() || propensity <= 0.0 || propensity > 1.0 { return None; }
        let weight = 1.0 / propensity;
        let bootstrap=weights.and_then(|ws|ws.get(index)).copied().unwrap_or(1.0);
        if !bootstrap.is_finite() || bootstrap<=0.0{return None;}
        if !weight.is_finite() || weight <= 0.0 { return None; }
        let traces: Vec<_> = c.candidates.iter().map(|id| trace(histories.get(id.as_str()).cloned().unwrap_or_default(), c.at)).collect();
        // Missing exposure history is not evidence for zero interest. Such a choice cannot
        // identify ACT-R parameters; keep its ignored count rather than inventing a prior use.
        if traces.iter().any(|t| t.start==t.end) { return None; }
        Some((Observation { at: c.at, traces, chosen, weight, corrected: c.propensity.is_some() || c.correction.is_some() },group,bootstrap))
    }).collect();
    let mut groups:BTreeMap<_,(f64,usize)>=BTreeMap::new();
    for(o,group,_)in &prepared{if let Some(group)=group{let(sum,count)=groups.entry(group.clone()).or_default();*sum+=o.weight;*count+=1;}}
    prepared.iter_mut().filter_map(|(o,group,bootstrap)|{
        if let Some(group)=group{
            let(sum,count)=groups.get(group)?;
            if !sum.is_finite() || *sum<=0.0{return None;}
            o.weight=o.weight / *sum * *count as f64;
        }
        o.weight*=*bootstrap;
        (o.weight.is_finite() && o.weight>0.0).then(||o.clone())
    }).collect()
}

fn prepare_subset(uses:&[Use],choices:&[Choice],weights:Option<&[f64]>,include:impl Fn(i64)->bool)->Vec<Observation>{
    let selected:Vec<_>=choices.iter().enumerate().filter(|(_,choice)|include(choice.at)).collect();
    let subset:Vec<_>=selected.iter().map(|(_,choice)|(*choice).clone()).collect();
    let weights=weights.map(|weights|selected.iter().map(|(index,_)|weights.get(*index).copied().unwrap_or(1.0)).collect::<Vec<_>>());
    prepare(uses,&subset,weights.as_deref())
}

fn logsum(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let values: Vec<_> = values.into_iter().collect();
    let max = values.iter().copied().reduce(f64::max)?;
    if !max.is_finite() { return None; }
    let sum: f64 = values.iter().map(|v| (v - max).exp()).sum();
    let result = max + sum.ln();
    result.is_finite().then_some(result)
}

fn activation(model: &Model, trace: &Trace) -> Option<f64> {
    let base = logsum(trace.events().filter_map(|(kind, age)| model.weights.get(&kind).map(|w| w.ln() - model.decay * age)))?;
    let a = base + model.range_weight.unwrap_or_default() * trace.range;
    a.is_finite().then_some(a)
}

fn derivatives(model: &Model, layout: &Layout, trace: &Trace) -> Option<(f64, Vec<f64>)> {
    let base = logsum(trace.events().filter_map(|(kind, age)| model.weights.get(&kind).map(|w| w.ln() - model.decay * age)))?;
    let mut kind_mass: BTreeMap<UseKind, f64> = BTreeMap::new();
    let mut logage = 0.0;
    for (kind, age) in trace.events() {
        let w = *model.weights.get(&kind)?;
        let mass = (w.ln() - model.decay * age - base).exp();
        *kind_mass.entry(kind).or_default() += mass;
        logage += mass * age;
    }
    let mut g = vec![-model.decay * logage];
    g.extend(layout.free().map(|k| kind_mass.get(&k).copied().unwrap_or_default()));
    if layout.range { g.push(trace.range); }
    Some((base + model.range_weight.unwrap_or_default() * trace.range, g))
}

struct Objective {
    loss: f64,
    gradient: Vec<f64>,
    information: Vec<Vec<f64>>,
}

fn objective(model: &Model, layout: &Layout, observations: &[Observation]) -> Option<Objective> {
    let dim = layout.dim();
    let mut out = Objective { loss: 0.0, gradient: vec![0.0; dim], information: vec![vec![0.0; dim]; dim] };
    for o in observations {
        let values = o.traces.iter().map(|t| derivatives(model, layout, t)).collect::<Option<Vec<_>>>()?;
        let norm = logsum(values.iter().map(|(a, _)| *a))?;
        let (chosen, chosen_g) = values.get(o.chosen)?;
        out.loss += o.weight * (norm - chosen);
        let mut mean = vec![0.0; dim];
        for (a, g) in &values {
            let p = (a - norm).exp();
            for (m, v) in mean.iter_mut().zip(g) { *m += p * v; }
        }
        for ((g, mean), chosen) in out.gradient.iter_mut().zip(&mean).zip(chosen_g) { *g += o.weight * (mean - chosen); }
        for (a, g) in &values {
            let p = o.weight * (a - norm).exp();
            for (row, (ga, ma)) in out.information.iter_mut().zip(g.iter().zip(&mean)) {
                for (cell, (gb, mb)) in row.iter_mut().zip(g.iter().zip(&mean)) { *cell += p * (ga - ma) * (gb - mb); }
            }
        }
    }
    (out.loss.is_finite() && out.gradient.iter().all(|g| g.is_finite()) && out.information.iter().flatten().all(|v| v.is_finite())).then_some(out)
}

/// Invert a likelihood information matrix, rejecting unidentified or numerically singular
/// dimensions. Pivot tolerance is f64 resolution times matrix scale and dimension.
pub(super) fn inverse(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = matrix.len();
    if n == 0 || matrix.iter().any(|r| r.len() != n) { return None; }
    let scale = matrix.iter().flatten().map(|v| v.abs()).reduce(f64::max)?;
    let tolerance = f64::EPSILON * n as f64 * scale;
    if !scale.is_finite() || scale == 0.0 { return None; }
    let mut rows: Vec<Vec<f64>> = matrix.iter().enumerate().map(|(i, row)| row.iter().copied().chain((0..n).map(move |j| if i == j { 1.0 } else { 0.0 })).collect()).collect();
    for col in 0..n {
        let pivot = rows.iter().enumerate().skip(col).max_by(|(_, a), (_, b)| {
            a.get(col).copied().unwrap_or_default().abs().total_cmp(&b.get(col).copied().unwrap_or_default().abs())
        }).map(|(i, _)| i)?;
        rows.swap(col, pivot);
        let d = *rows.get(col)?.get(col)?;
        if d.abs() <= tolerance { return None; }
        let row: Vec<f64> = rows.get(col)?.iter().map(|v| v / d).collect();
        if row.iter().any(|v| !v.is_finite()) { return None; }
        for (i, target) in rows.iter_mut().enumerate() {
            if i == col { target.clone_from(&row); continue; }
            let factor = *target.get(col)?;
            for (value, pivot) in target.iter_mut().zip(&row) { *value -= factor * pivot; }
        }
    }
    Some(rows.into_iter().map(|r| r.into_iter().skip(n).collect()).collect())
}

fn norm(values: &[f64]) -> f64 {
    values.iter().map(|v| v.abs()).fold(0.0, f64::max)
}

fn scoring_step(covariance:&[Vec<f64>],gradient:&[f64])->Option<Vec<f64>>{
    let step:Vec<f64>=covariance.iter().map(|row|row.iter().zip(gradient).map(|(a,b)|a*b).sum()).collect();
    step.iter().all(|value|value.is_finite()).then_some(step)
}

/// Vanishing probabilities can make both score and information tiny on a separated
/// likelihood while the parameter keeps moving toward infinity. Require the Fisher step
/// to converge too, relative to the dimensionless log-parameter coordinates. One is their
/// unit scale at zero; the tolerance itself comes only from floating-point precision.
fn scoring_converged(gradient:&[f64],step:&[f64],parameters:&[f64],total_weight:f64)->bool{
    let accuracy=f64::EPSILON.sqrt();
    norm(gradient)<=accuracy*total_weight && norm(step)<=accuracy*(1.0+norm(parameters))
}

fn estimates(model: &Model, layout: &Layout, covariance: &[Vec<f64>]) -> Vec<Estimate> {
    let mut values = vec![("decay".to_owned(), model.decay)];
    values.extend(layout.free().filter_map(|kind| model.weights.get(&kind).map(|v| (format!("weight.{}", kind.name()), *v))));
    if let Some(v) = model.range_weight { values.push(("range_weight".into(), v)); }
    values.into_iter().enumerate().map(|(i, (name, value))| {
        let variance = covariance.get(i).and_then(|r| r.get(i)).copied().filter(|v| v.is_finite() && *v >= 0.0);
        let scale = if name == "range_weight" { 1.0 } else { value };
        Estimate { name, value, standard_error: variance.map(|v| scale * v.sqrt()).filter(|v| v.is_finite()) }
    }).collect()
}

/// Fisher scoring with monotone backtracking, operating in log-parameter coordinates. Every
/// retained iterate improves actual conditional likelihood. The iteration budget is derived
/// from floating-point precision and parameter dimension; exhaustion is failure, not a model.
fn maximum_likelihood(observations: &[Observation], range: bool) -> Option<(Model, Vec<Estimate>)> {
    let layout = Layout::from(observations, range)?;
    let dim = layout.dim();
    if observations.len() <= dim { return None; }
    let mut x = vec![0.0; dim];
    let total_weight: f64 = observations.iter().map(|o| o.weight).sum();
    for _ in 0..(f64::MANTISSA_DIGITS as usize).saturating_mul(dim) {
        let model = layout.model(&x)?;
        let current = objective(&model, &layout, observations)?;
        let covariance = inverse(&current.information)?;
        let step=scoring_step(&covariance,&current.gradient)?;
        if scoring_converged(&current.gradient,&step,&x,total_weight) {
            return Some((model.clone(), estimates(&model, &layout, &covariance)));
        }
        let mut factor = 1.0;
        let next = loop {
            let next: Vec<f64> = x.iter().zip(&step).map(|(v, s)| v - factor * s).collect();
            if next == x { break None; }
            if let Some(model) = layout.model(&next)
                && let Some(proposed) = objective(&model, &layout, observations)
                && proposed.loss < current.loss
            {
                break Some((next, proposed.loss));
            }
            factor /= 2.0;
            if factor < f64::EPSILON { break None; }
        };
        let (next, loss) = next?;
        let change = current.loss - loss;
        x = next;
        if change <= f64::EPSILON * (current.loss.abs() + total_weight) {
            // A flat objective is only convergence when its gradient is also small. This keeps
            // separation and nonidentifiability from being presented as learned confidence.
            let model = layout.model(&x)?;
            let end = objective(&model, &layout, observations)?;
            let covariance = inverse(&end.information)?;
            let step=scoring_step(&covariance,&end.gradient)?;
            if scoring_converged(&end.gradient,&step,&x,total_weight) {
                return Some((model.clone(), estimates(&model, &layout, &covariance)));
            }
            return None;
        }
    }
    None
}

fn likelihood(model: &Model, observations: &[Observation]) -> Option<f64> {
    let mut total = 0.0;
    for o in observations {
        let values: Vec<_> = o.traces.iter().map(|t| activation(model, t)).collect::<Option<Vec<_>>>()?;
        total += o.weight * (values.get(o.chosen)? - logsum(values.iter().copied())?);
    }
    total.is_finite().then_some(total)
}

fn recency_likelihood(uses: &[Use], choices: &[Choice], weights: Option<&[f64]>, newest: i64, count: usize) -> Option<f64> {
    // A rank-only baseline has no calibrated likelihood. Learn its scalar temperature using
    // the same conditional likelihood machinery: one latest-human trace per candidate.
    let human: Vec<_> = uses.iter().filter(|u| u.kind.human()).cloned().collect();
    let mut train=prepare_subset(&human,choices,weights,|at|at<newest);
    let mut test=prepare_subset(&human,choices,weights,|at|at==newest);
    for o in train.iter_mut().chain(&mut test) {
        for t in &mut o.traces {
            if let Some(last)=t.end.checked_sub(1) {
                t.start=last;
                t.kind_override=Some(UseKind::Message);
                t.range = 0.0;
            }
        }
    }
    if test.len() != count { return None; }
    let (m, _) = maximum_likelihood(&train, false)?;
    likelihood(&m, &test)
}

fn fit_weighted(uses: &[Use], choices: &[Choice], weights: Option<&[f64]>) -> FitReport {
    let observations = prepare(uses, choices, weights);
    let mut report = FitReport {
        model: None, observations: observations.len(), ignored: choices.len().saturating_sub(observations.len()),
        position_corrected: observations.iter().filter(|o| o.corrected).count(), parameters: Vec::new(),
        comparisons: Vec::new(),
        unavailable_comparisons: vec![UnavailableComparison {
            name: "current_order".into(),
            reason: "Historical legacy last-activity values and row positions were not recorded. Today's order cannot reconstruct the order available at a past choice; no legacy-order likelihood is claimed.".into(),
        }],
        improves_baseline: false,
        reason: "Not enough independent choice history to fit decay and use weights; ordered by your latest interaction.".into(),
    };
    let newest = observations.iter().map(|o| o.at).max();
    let training=prepare_subset(uses,choices,weights,|at|Some(at)<newest);
    let heldout=prepare_subset(uses,choices,weights,|at|Some(at)==newest);
    // The newest visit is a natural, observed boundary, rather than an invented train/test
    // split fraction. No event after a recorded choice is visible to its feature calculation.
    let base_fit = maximum_likelihood(&observations, false);
    let range_fit = maximum_likelihood(&observations, true);
    let base_validation = maximum_likelihood(&training, false).and_then(|(m, _)| likelihood(&m, &heldout));
    let range_validation = maximum_likelihood(&training, true).and_then(|(m, _)| likelihood(&m, &heldout));
    let use_range = range_fit.is_some() && range_validation.zip(base_validation).is_some_and(|(r, b)| r > b);
    let chosen = if use_range { range_fit.or(base_fit) } else { base_fit };
    let Some((model, parameters)) = chosen else { return report; };
    report.model = Some(model);
    report.parameters = parameters;
    if let Some(v) = base_validation { report.comparisons.push(Comparison { name: "act_r".into(), choices: heldout.len(), log_likelihood: v }); }
    if let Some(v) = range_validation { report.comparisons.push(Comparison { name: "act_r_with_range".into(), choices: heldout.len(), log_likelihood: v }); }
    let recency = newest.and_then(|at| recency_likelihood(uses, choices, weights, at, heldout.len()));
    if let Some(v) = recency { report.comparisons.push(Comparison { name: "your_recency".into(), choices: heldout.len(), log_likelihood: v }); }
    let human: Vec<_> = uses.iter().filter(|u| u.kind.human()).cloned().collect();
    let human_training=prepare_subset(&human,choices,weights,|at|Some(at)<newest);
    let human_heldout=prepare_subset(&human,choices,weights,|at|Some(at)==newest);
    let without_agent = if human_heldout.len() == heldout.len() {
        maximum_likelihood(&human_training, false).and_then(|(m, _)| likelihood(&m, &human_heldout))
    } else { None };
    if let Some(v) = without_agent { report.comparisons.push(Comparison { name: "without_agent_turns".into(), choices: heldout.len(), log_likelihood: v }); }
    let validation = if use_range { range_validation } else { base_validation };
    report.improves_baseline = validation.zip(recency).zip(without_agent).is_some_and(|((v, r), h)| v > r && v > h);
    let position = if observations.iter().any(|o|o.corrected){"Recorded absolute propensities or model-based relative PBM corrections were used; relative inverse weights are self-normalized by layout/reference. ACT-R uncertainty is conditional on those learned corrections; examination uncertainty is reported separately. Legacy and direct choices remain unweighted; global unbiasedness is not claimed."}else{"Historical position bias cannot be corrected where examination evidence was not recorded."};
    report.reason = if report.improves_baseline {
        format!("Learned ACT-R decay and use weights beat human recency and the model without agent turns on the newest held-out visit. {position}")
    } else {
        format!("Fitted ACT-R decay and use weights; a held-out improvement over both simpler orders is not established, so human recency remains the fallback. {position}")
    };
    report
}

/// Maximum-likelihood fit with local standard errors and a chronological held-out comparison.
/// No model is returned when the likelihood is unidentified or optimization cannot converge.
pub fn fit(uses: &[Use], choices: &[Choice]) -> FitReport {
    fit_weighted(uses, choices, None)
}

/// One approximate posterior draw for Thompson sampling. This is the Bayesian bootstrap,
/// not epsilon-greedy random shuffling: every uncertainty comes from the recorded choices.
/// Supply exactly one independent open-interval uniform per choice. Cache by history revision
/// and perform a fresh draw when the person returns, rather than on every render or state poll.
pub fn thompson(uses: &[Use], choices: &[Choice], uniforms: &[f64]) -> Result<FitReport, Error> {
    if uniforms.len() != choices.len() || uniforms.iter().any(|u| !u.is_finite() || *u <= 0.0 || *u >= 1.0) { return Err(Error::Randomness); }
    let weights: Vec<_> = uniforms.iter().map(|u| -u.ln()).collect();
    Ok(fit_weighted(uses, choices, Some(&weights)))
}

/// A cheaper posterior draw after the caller has cached `fit`'s validation report for this
/// history revision. Keeps that report's selected family fixed; a draw must not switch the
/// family based on randomly weighted validation data. Eligibility comes from the cached fit.
pub fn sample_model(uses: &[Use], choices: &[Choice], uniforms: &[f64], with_range: bool) -> Result<Option<Model>, Error> {
    if uniforms.len() != choices.len() || uniforms.iter().any(|u| !u.is_finite() || *u <= 0.0 || *u >= 1.0) { return Err(Error::Randomness); }
    let weights: Vec<_> = uniforms.iter().map(|u| -u.ln()).collect();
    let observations = prepare(uses, choices, Some(&weights));
    Ok(maximum_likelihood(&observations, with_range).map(|(model, _)| model))
}

/// Explicit model validation for stored models before they are used by a server.
pub fn validate(model: &Model) -> Result<(), Error> {
    if valid(model) { Ok(()) } else { Err(Error::Model) }
}

/// Rank only the caller's visible candidates. Pins, parks and requests are placement constraints
/// in the server, outside this function. Supplying None (including opt-out) orders by the
/// person's most recent real interaction. Agents and subagents never become human interactions.
pub fn rank(model: Option<&Model>, uses: &[Use], candidates: &[String], now: i64) -> Vec<Ranked> {
    let model = model.filter(|m| valid(m));
    let histories = history(uses);
    let mut rows: Vec<_> = candidates.iter().map(|id| {
        let events=histories.get(id.as_str()).cloned().unwrap_or_default();
        let earlier = trace(events, now);
        let score = model.and_then(|m| activation(m, &earlier));
        let human = earlier.entries().filter(|(kind,_)|kind.human()).map(|(_,at)|at).max();
        let reason = if score.is_some() {
            let yours = earlier.entries().filter(|(k, _)| k.human()).count();
            let main = earlier.entries().filter(|(k, _)| *k == UseKind::Turn).count();
            let subagents = earlier.entries().filter(|(k, _)| *k == UseKind::SubagentTurn).count();
            format!("{yours} interactions from you · {main} agent turns · {subagents} subagent turns")
        } else if human.is_some() { "Your latest interaction".into() } else { "No recorded interaction from you".into() };
        (Ranked { agent: id.clone(), score, reason }, human)
    }).collect();
    rows.sort_by(|(a, ah), (b, bh)| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| bh.cmp(ah)).then_with(|| a.agent.cmp(&b.agent))
    });
    rows.into_iter().map(|(r, _)| r).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind { Question, Tool, Plan, Goals, Stop }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestWait {
    pub agent: String,
    pub kind: RequestKind,
    /// When this request first needed this person's response, in unix milliseconds.
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseTime {
    pub kind: RequestKind,
    /// Observed elapsed milliseconds until an accepted human answer, never an agent reply.
    /// The caller supplies only responses completed by the injected evaluation time.
    pub duration: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestPriority {
    pub agent: String,
    pub kind: RequestKind,
    pub at: i64,
    pub waited: i64,
    pub expected_wait: Option<f64>,
    /// Elapsed time divided by this person's median response time for this request kind.
    /// Higher values come first when every pending kind has a positive empirical median.
    pub urgency: Option<f64>,
    pub stale: bool,
    pub observations: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UrgencyReport {
    pub requests: Vec<RequestPriority>,
    pub learned_order: bool,
    pub reason: String,
}

/// Empirical request ordering for plan 01a199e1b177752e84dad5dc990e6a15 step 4.
/// No assumed response window or urgency multiplier is introduced. Unknown request kinds
/// make the entire pending set oldest-first: mixing ratios with raw time would compare
/// incompatible units, and comparing unknown pairs by age would break transitivity.
/// Zero-millisecond observations remain valid but cannot supply a normalization divisor.
/// Negative response durations are invalid observations; future requests have not waited.
pub fn urgency(requests: &[RequestWait], responses: &[ResponseTime], now: i64) -> UrgencyReport {
    let mut by_kind: BTreeMap<RequestKind, Vec<i64>> = BTreeMap::new();
    for response in responses.iter().filter(|r| r.duration >= 0) {
        by_kind.entry(response.kind).or_default().push(response.duration);
    }
    let longest = by_kind.values().flat_map(|v| v.iter()).copied().max();
    let estimates: BTreeMap<_, _> = by_kind.into_iter().filter_map(|(kind, mut durations)| {
        durations.sort_unstable();
        let middle = durations.len() / 2;
        let high = *durations.get(middle)? as f64;
        let median = if durations.len() % 2 == 0 {
            let low = *durations.get(middle.checked_sub(1)?)? as f64;
            // Split the sum to keep the median finite for arbitrarily large timestamps.
            low / 2.0 + high / 2.0
        } else { high };
        Some((kind, (median, durations.len())))
    }).collect();
    let mut ordered: Vec<_> = requests.iter().map(|request| {
        let waited = now.saturating_sub(request.at).max(0);
        let estimate = estimates.get(&request.kind);
        let expected_wait = estimate.map(|(median, _)| *median);
        let urgency = expected_wait.filter(|median| *median > 0.0).map(|median| waited as f64 / median);
        RequestPriority {
            agent: request.agent.clone(), kind: request.kind, at: request.at, waited,
            expected_wait, urgency, stale: longest.is_some_and(|duration| waited > duration),
            observations: estimate.map_or(0, |(_, count)| *count),
        }
    }).collect();
    let learned_order = !ordered.is_empty() && ordered.iter().all(|request| request.urgency.is_some());
    ordered.sort_by(|a, b| {
        let learned = if learned_order {
            b.urgency.partial_cmp(&a.urgency).unwrap_or(std::cmp::Ordering::Equal)
        } else { std::cmp::Ordering::Equal };
        learned.then_with(|| a.at.cmp(&b.at)).then_with(|| a.agent.cmp(&b.agent)).then_with(|| a.kind.cmp(&b.kind))
    });
    let reason = if learned_order {
        "Wait divided by your median answered wait for each request type; stale means longer than your longest recorded answered wait."
    } else {
        "Oldest first until every pending request type has a positive observed median answered wait; stale requires recorded response history."
    }.into();
    UrgencyReport { requests: ordered, learned_order, reason }
}

#[cfg(test)]
mod tests;
