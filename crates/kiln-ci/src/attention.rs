//! Mission Control attention, plan 01a199e1b177752e84dad5dc990e6a15, steps 2, 4 and 5: which chat a person goes to
//! next, learned from where they went before.
//!
//! **Activation.** A chat's activation for a person at a moment is ACT-R's base-level learning over its uses before
//! then, `A = ln Σ_kind w_kind Σ_use age^−d` (Anderson & Schooler 1991; Anderson & Lebiere 1998): the person's own uses
//! of it (messages, answers, commands, opens, pins, unparks, restores) and its own and its subagents' turns, a weight for
//! each kind of use and one decay `d`. A time-span term, `ln(1 + span ÷ age of the latest use)` (none when its uses are
//! one moment), is tried as one more learned term.
//!
//! **Choice.** The rational analysis behind it reads activation as the log of a chat's need odds (Anderson & Schooler
//! 1991; Anderson et al. 2004), so when the person goes to one chat, the chance that it's a given one, among those there
//! to go to, is its odds over their sum: a softmax over activations. A pin is a choice of the same kind; a park or an
//! archive is the choice of the one to set aside, `∝ exp(−λ·A)` (best–worst scaling, Marley & Louviere 2005), each kind
//! of evidence with a scale `λ` of its own relative to a pick's, learned (Hawkins, Islam & Marley 2019 found worst
//! choices driven by the same utilities as best ones, usually with a scale of their own). A chat with no recorded use has activation −∞ (ACT-R's for no use): it takes no
//! share of a choice, and a choice of one is evidence of nothing the model reads.
//!
//! **Speed.** A chat's sum over its uses is Petrov (2006)'s hybrid approximation: its most recent use exactly, the rest
//! as spread evenly between it and its first, in closed form. An observation costs the same however long the history,
//! and none is cut off. Fisher, Houpt & Gunzelmann (2018) recommend the hybrid for estimating parameters: ACT-R's
//! standard approximation leaves the decay unidentifiable.
//!
//! **Learning.** Each family of order (the learned ones, with and without the time span, and the simpler orders the plan
//! compares them with) is learned one choice at a time, its posterior's mode under a unit-information prior (Kass &
//! Wasserman 1995: on the orders' null values, as informative as one choice) found by modified Newton steps (the loss's
//! own curvature, Gill, Murray & Wright 1981) as soon as its information is invertible; then one safeguarded Fisher
//! step per choice with the information accumulated, a Laplace approximation of the posterior carried forward
//! (assumed-density filtering). Before a choice teaches the fits, each scores it: a prequential log score (Dawid 1984), so every comparison is on choices the
//! fit hadn't seen, over the person's whole history, with no split or window.
//!
//! **Thompson sampling** draws the learned family's parameters from that Laplace posterior, N(θ̂, Λ⁻¹) in log
//! coordinates (Chapelle & Li 2011; Russo et al. 2018 §5.2); the randomness is passed in, as standard normals.
//!
//! Numerical tolerances come from floating-point precision, as optimizers' and rank tests' do (NAG's minimizers, after
//! Gill, Murray & Wright 1981; R's `optim`; LAPACK; NumPy), never from product behaviour. The optimizer starts from
//! ACT-R's recommended decay, 0.5, and equal weights: a start, never served as a fitted model. Times are injected, in
//! unix milliseconds; one millisecond, the clock's resolution, is the least age.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A map whose keys are tuples, kept as a list of pairs (JSON's keys are strings).
mod pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(map: &BTreeMap<K, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(map.iter())
    }

    pub fn deserialize<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<K, V>, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}

/// How many of a kind's most recent uses Petrov (2006)'s hybrid approximation keeps exact; the rest are spread evenly
/// between them and the first use. One: Petrov finds the approximation "nearly perfect even for k = 1" and that "greater
/// depths should hardly ever be necessary", the most recent use driving the boost after each use.
pub const EXACT_RECENT: usize = 1;

/// ACT-R's recommended decay: ".5 has emerged as the default value for the parameter d over a large range of
/// applications" (Anderson et al. 2004), and the reference manual's recommended `:bll` (the software's own default is
/// nil, base-level learning off). Only where a fit's optimizer starts: decay differs between people and materials
/// (Anderson & Schooler 1991 found exponents of .73 to .83 across environments), so each person's is learned.
const START_DECAY: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    /// A person's own use of a chat: not an agent's or a subagent's turn, and not setting it aside.
    pub fn human(self) -> bool {
        !matches!(self, Self::Turn | Self::SubagentTurn | Self::Park | Self::Archive)
    }

    /// A use that raises a chat's activation: every one but setting it aside, which is evidence of its own
    /// ([`Evidence::Park`], [`Evidence::Archive`]).
    pub fn activates(self) -> bool {
        !matches!(self, Self::Park | Self::Archive)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Answer => "answer",
            Self::Command => "command",
            Self::Open => "open",
            Self::Pin => "pin",
            Self::Park => "park",
            Self::Unpark => "unpark",
            Self::Archive => "archive",
            Self::Restore => "restore",
            Self::Turn => "turn",
            Self::SubagentTurn => "subagent_turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Use {
    pub agent: String,
    pub kind: UseKind,
    pub at: i64,
}

/// What a recorded choice says of the person's attention: where they went (a pick, from the list or from a
/// notification, a link or a search), what they pinned, or what they set aside (parked or archived). A pick or a pin is
/// a best choice among the chats there then; a park or an archive a worst.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    #[default]
    Pick,
    Pin,
    Park,
    Archive,
}

impl Evidence {
    fn worst(self) -> bool {
        matches!(self, Self::Park | Self::Archive)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Pin => "pin",
            Self::Park => "park",
            Self::Archive => "archive",
        }
    }
}

fn is_pick(evidence: &Evidence) -> bool {
    *evidence == Evidence::Pick
}

/// A recorded choice. `candidates` are the chats there to choose from at `at`, the chosen among them. `propensity` is a
/// known probability that its row was looked at; `correction` a relative one, learned (`attention_examination`). A pick
/// with neither (from a notification, a link or a search) counts as it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub at: i64,
    pub chosen: String,
    pub candidates: Vec<String>,
    #[serde(default)]
    pub propensity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<RelativeCorrection>,
    #[serde(default, skip_serializing_if = "is_pick")]
    pub evidence: Evidence,
}

/// Relative PBM examination, not an absolute probability. Its unidentified scale cancels by self-normalizing inverse
/// weights within each layout and reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelativeCorrection {
    pub layout: String,
    pub reference: usize,
    pub examination: f64,
}

/// The families of order the plan compares: the learned ACT-R order with and without its time span, and the simpler
/// orders it must beat to be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Every kind of use, the person's and the agents', each with its weight.
    #[default]
    ActR,
    /// [`Family::ActR`] with the time span of a chat's uses as one more term.
    ActRWithRange,
    /// The person's latest use of a chat alone.
    YourRecency,
    /// ACT-R over the person's own uses alone: no agent or subagent turns.
    YourUsesOnly,
    /// The latest use of any kind: today's order, kiln's last activity without its refreshes.
    LatestActivity,
}

impl Family {
    pub const ALL: [Self; 5] = [Self::ActR, Self::ActRWithRange, Self::YourRecency, Self::YourUsesOnly, Self::LatestActivity];

    pub fn name(self) -> &'static str {
        match self {
            Self::ActR => "act_r",
            Self::ActRWithRange => "act_r_with_range",
            Self::YourRecency => "your_recency",
            Self::YourUsesOnly => "without_agent_turns",
            Self::LatestActivity => "latest_activity",
        }
    }

    fn learned(self) -> bool {
        matches!(self, Self::ActR | Self::ActRWithRange)
    }

    fn range(self) -> bool {
        self == Self::ActRWithRange
    }

    /// For a family that reads one latest use, which kinds of use it reads.
    fn latest(self) -> Option<fn(UseKind) -> bool> {
        match self {
            Self::YourRecency => Some(UseKind::human),
            Self::LatestActivity => Some(UseKind::activates),
            _ => None,
        }
    }

    /// The kinds of use this family reads.
    fn reads(self, kind: UseKind) -> bool {
        match self {
            Self::YourUsesOnly | Self::YourRecency => kind.human(),
            Self::ActR | Self::ActRWithRange | Self::LatestActivity => kind.activates(),
        }
    }
}

/// A chat's uses of one kind before a moment, as Petrov (2006)'s approximation reads them: its most recent ones exactly
/// (newest first, at most [`EXACT_RECENT`]), how many there were, and when the first was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    pub kind: UseKind,
    pub recent: Vec<i64>,
    pub count: u64,
    pub oldest: i64,
}

/// A chat there to choose from, with its uses before the moment of a choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub agent: String,
    pub traces: Vec<Trace>,
}

/// One recorded choice as the model reads it: its candidates' uses before it, and its weight (inverse examination).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub at: i64,
    pub evidence: Evidence,
    pub chosen: usize,
    pub candidates: Vec<Candidate>,
    pub weight: f64,
    pub corrected: bool,
}

/// Every recorded use, by chat and kind, in time order: what a [`Candidate`] at any moment is read from.
#[derive(Debug, Clone, Default)]
pub struct UseIndex {
    by_agent: BTreeMap<String, BTreeMap<UseKind, Vec<i64>>>,
}

impl UseIndex {
    pub fn new(uses: &[Use]) -> Self {
        let mut index = Self::default();
        for u in uses {
            index.add(u);
        }
        index
    }

    pub fn add(&mut self, u: &Use) {
        let times = self.by_agent.entry(u.agent.clone()).or_default().entry(u.kind).or_default();
        let place = times.partition_point(|t| *t <= u.at);
        times.insert(place, u.at);
    }

    /// What chat `agent` had of each kind of use strictly before `at`: nothing at `at` itself, so an action never
    /// predicts itself.
    pub fn candidate(&self, agent: &str, at: i64) -> Candidate {
        let traces = self
            .by_agent
            .get(agent)
            .map(|kinds| {
                kinds
                    .iter()
                    .filter_map(|(kind, times)| {
                        let end = times.partition_point(|t| *t < at);
                        let count = u64::try_from(end).ok().filter(|n| *n > 0)?;
                        let oldest = *times.first()?;
                        let recent = times.get(end.saturating_sub(EXACT_RECENT)..end)?.iter().rev().copied().collect();
                        Some(Trace { kind: *kind, recent, count, oldest })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Candidate { agent: agent.to_owned(), traces }
    }
}

/// `expm1(x) ÷ x`, and 1 at 0: the mean of `e^(x·u)` over `u` in [0, 1].
fn phi(x: f64) -> f64 {
    if x == 0.0 { 1.0 } else { x.exp_m1() / x }
}

/// [`phi`]'s derivative, `(e^x·(x − 1) + 1) ÷ x²`: in closed form away from 0, and near it as its series
/// `Σ_{j≥1} j·x^(j−1) ÷ (j+1)!`, summed to working precision (the closed form cancels there).
fn phi_prime(x: f64) -> f64 {
    if x.abs() > 1.0 {
        return (x.exp() * (x - 1.0) + 1.0) / (x * x);
    }
    let (mut sum, mut term, mut j) = (0.0, 0.5, 1.0_f64);
    loop {
        sum += term;
        let next = term * x * (j + 1.0) / (j * (j + 2.0));
        j += 1.0;
        if next.abs() <= f64::EPSILON * sum.abs() {
            return sum + next;
        }
        term = next;
    }
}

/// [`phi`]'s second derivative, `(e^x·(x² − 2x + 2) − 2) ÷ x³`: in closed form away from 0, and near it as its series
/// `Σ_{j≥2} j·(j−1)·x^(j−2) ÷ (j+1)!`, summed to working precision (the closed form cancels there).
fn phi_second(x: f64) -> f64 {
    if x.abs() > 1.0 {
        return (x.exp() * (x * x - 2.0 * x + 2.0) - 2.0) / (x * x * x);
    }
    let (mut sum, mut term, mut j) = (0.0, 1.0 / 3.0, 2.0_f64);
    loop {
        sum += term;
        let next = term * x * (j + 1.0) / ((j - 1.0) * (j + 2.0));
        j += 1.0;
        if next.abs() <= f64::EPSILON * sum.abs() {
            return sum + next;
        }
        term = next;
    }
}

/// A log-sum of terms kept as they come (no list), each with its first and second derivatives in one variable:
/// `ln Σ e^l`, and that log's derivatives `Σ π·l′` and `Σ π·(l″ + l′²) − (Σ π·l′)²`, `π` each term's share.
#[derive(Default)]
struct LogSum {
    top: Option<f64>,
    sum: f64,
    first: f64,
    second: f64,
}

impl LogSum {
    fn add(&mut self, l: f64, d1: f64, d2: f64) {
        let (weight, rescale) = match self.top {
            Some(top) if l <= top => ((l - top).exp(), 1.0),
            Some(top) => (1.0, (top - l).exp()),
            None => (1.0, 1.0),
        };
        if self.top.is_none_or(|top| l > top) {
            self.top = Some(l);
        }
        self.sum = self.sum * rescale + weight;
        self.first = self.first * rescale + weight * d1;
        self.second = self.second * rescale + weight * (d2 + d1 * d1);
    }

    fn finish(&self) -> Option<(f64, f64, f64)> {
        let value = self.top? + self.sum.ln();
        let first = self.first / self.sum;
        let second = self.second / self.sum - first * first;
        (value.is_finite() && first.is_finite() && second.is_finite()).then_some((value, first, second))
    }
}

fn logsum(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let values: Vec<_> = values.into_iter().collect();
    let max = values.iter().copied().reduce(f64::max)?;
    if !max.is_finite() {
        return None;
    }
    let sum: f64 = values.iter().map(|v| (v - max).exp()).sum();
    let result = max + sum.ln();
    result.is_finite().then_some(result)
}

/// One kind's `ln Σ age^−d` over its uses before `at`, and that log's first and second derivatives in `d`: the most
/// recent uses exactly, the rest as Petrov (2006) spreads them evenly between the last exact one and the first. Their
/// mean of `age^−d` is `t^−d · φ((1−d)·L) ÷ φ(L)`, with `t` the last exact one's age and `L = ln(first's age ÷ t)`:
/// closed, and stable at `L = 0` and at `d = 1`.
fn strength(trace: &Trace, at: i64, decay: f64) -> Option<(f64, f64, f64)> {
    let ln_age = |t: i64| (at.saturating_sub(t).max(1) as f64).ln();
    let mut sum = LogSum::default();
    for t in &trace.recent {
        sum.add(-decay * ln_age(*t), -ln_age(*t), 0.0);
    }
    let rest = trace.count.checked_sub(u64::try_from(trace.recent.len()).ok()?)?;
    if rest > 0 {
        let lower = ln_age(*trace.recent.last()?);
        let span = (ln_age(trace.oldest) - lower).max(0.0);
        let x = (1.0 - decay) * span;
        let (f, slope) = (phi(x), phi_prime(x) / phi(x));
        sum.add((rest as f64).ln() - decay * lower + f.ln() - phi(span).ln(), -lower - span * slope, span * span * (phi_second(x) / f - slope * slope));
    }
    sum.finish()
}

/// A fitted (or drawn) order: its family, decay, a weight for each kind of use its fit has seen (uses of a kind no
/// choice has shown yet count once one does), its time-span coefficient, and the strength of each kind of evidence
/// relative to a pick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub decay: f64,
    pub weights: BTreeMap<UseKind, f64>,
    #[serde(default)]
    pub range_weight: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scales: BTreeMap<Evidence, f64>,
    #[serde(default)]
    pub family: Family,
}

fn valid(model: &Model) -> bool {
    model.decay.is_finite()
        && model.decay > 0.0
        && !model.weights.is_empty()
        && model.weights.values().all(|w| w.is_finite() && *w > 0.0)
        && model.range_weight.is_none_or(f64::is_finite)
        && model.scales.values().all(|s| s.is_finite() && *s > 0.0)
        && model.family.range() == model.range_weight.is_some()
}

/// Its activation and the gradient of it in `layout`'s log coordinates (when `layout` is given): none for a candidate
/// with no use the family reads, whose activation is −∞.
fn activation(model: &Model, layout: Option<&Layout>, candidate: &Candidate, at: i64) -> Option<(f64, Vec<f64>)> {
    activation_with(model, layout, candidate, at, false).map(|(value, gradient, _)| (value, gradient))
}

/// [`activation`], and its Hessian in `layout`'s log coordinates too when `curvature` (with `layout`).
fn activation_with(model: &Model, layout: Option<&Layout>, candidate: &Candidate, at: i64, curvature: bool) -> Option<(f64, Vec<f64>, Vec<Vec<f64>>)> {
    // The recency families read one use: −d·ln(age), its first and second derivatives in ln d being −d·ln(age) too.
    if let Some(keep) = model.family.latest() {
        let newest = candidate.traces.iter().filter(|t| keep(t.kind)).filter_map(|t| t.recent.first().copied()).max()?;
        let ln_age = (at.saturating_sub(newest).max(1) as f64).ln();
        let value = -model.decay * ln_age;
        if !value.is_finite() {
            return None;
        }
        let Some(layout) = layout else { return Some((value, Vec::new(), Vec::new())) };
        let mut gradient = vec![0.0; layout.dim()];
        *gradient.get_mut(0)? = value;
        let mut hessian = Vec::new();
        if curvature {
            hessian = vec![vec![0.0; layout.dim()]; layout.dim()];
            *hessian.get_mut(0)?.get_mut(0)? = value;
        }
        return Some((value, gradient, hessian));
    }
    let read = |kind: UseKind| model.family.reads(kind);
    // Each kind's weighted log strength and that log's first and second derivatives in d, on the stack (there are
    // eleven kinds at most).
    let mut terms = [(UseKind::Message, 0.0, 0.0, 0.0); 11];
    let mut n = 0;
    for t in candidate.traces.iter().filter(|t| read(t.kind)) {
        let Some(w) = model.weights.get(&t.kind) else { continue };
        let Some((ln_s, d1, d2)) = strength(t, at, model.decay) else { continue };
        if let Some(slot) = terms.get_mut(n) {
            *slot = (t.kind, w.ln() + ln_s, d1, d2);
            n += 1;
        }
    }
    let terms = terms.get(..n)?;
    let max = terms.iter().map(|t| t.1).reduce(f64::max)?;
    let base = max + terms.iter().map(|t| (t.1 - max).exp()).sum::<f64>().ln();
    let span = model.range_weight.map(|_| {
        let first = candidate.traces.iter().filter(|t| read(t.kind)).map(|t| t.oldest).min();
        let last = candidate.traces.iter().filter(|t| read(t.kind)).filter_map(|t| t.recent.first().copied()).max();
        first.zip(last).map_or(0.0, |(first, last)| (last.saturating_sub(first) as f64 / at.saturating_sub(last).max(1) as f64).ln_1p())
    });
    let value = base + model.range_weight.zip(span).map_or(0.0, |(w, s)| w * s);
    if !value.is_finite() {
        return None;
    }
    let Some(layout) = layout else { return Some((value, Vec::new(), Vec::new())) };
    let (dim, decay) = (layout.dim(), model.decay);
    let mut gradient = vec![0.0; dim];
    for (kind, ln_term, d1, _) in terms {
        let share = (ln_term - base).exp();
        *gradient.get_mut(0)? += decay * share * d1;
        if let Some(Some(i)) = layout.weight_index(*kind) {
            *gradient.get_mut(i)? += share;
        }
    }
    let mut hessian = Vec::new();
    if curvature {
        // `base = ln Σ e^a`, each kind's `a = ln w + ln S(d)` linear in its `ln w`, with `∂a/∂ln d = d·S′` and
        // `∂²a/∂(ln d)² = d·S′ + d²·S″`: its Hessian is `Σ share·(∂²a + ∂a·∂aᵀ) − ∂base·∂baseᵀ`. The span's term is
        // linear in its coefficient, so adds none.
        hessian = vec![vec![0.0; dim]; dim];
        for (kind, ln_term, d1, d2) in terms {
            let share = (ln_term - base).exp();
            let slope = decay * d1;
            *hessian.get_mut(0)?.get_mut(0)? += share * (slope + decay * decay * d2 + slope * slope);
            if let Some(Some(i)) = layout.weight_index(*kind) {
                *hessian.get_mut(0)?.get_mut(i)? += share * slope;
                *hessian.get_mut(i)?.get_mut(0)? += share * slope;
                *hessian.get_mut(i)?.get_mut(i)? += share;
            }
        }
        for (row, a) in hessian.iter_mut().zip(&gradient) {
            for (cell, b) in row.iter_mut().zip(&gradient) {
                *cell -= a * b;
            }
        }
    }
    if let (Some(i), Some(s)) = (layout.range_index(), span) {
        *gradient.get_mut(i)? = s;
    }
    (gradient.iter().all(|g| g.is_finite()) && hessian.iter().flatten().all(|h| h.is_finite())).then_some((value, gradient, hessian))
}

/// What a family's parameters are, in order: `ln d`, then `ln w` of each kind but the first (whose weight, 1, sets the
/// scale), then the time span's coefficient (with it), then `ln λ` of each kind of evidence but picks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Layout {
    family: Family,
    kinds: Vec<UseKind>,
    evidence: Vec<Evidence>,
}

impl Layout {
    fn free(&self) -> usize {
        self.kinds.len().saturating_sub(1)
    }

    fn dim(&self) -> usize {
        1 + self.free() + usize::from(self.family.range()) + self.evidence.len()
    }

    /// Some(None) for the first kind (weight 1), Some(Some(i)) for another's, None for one the layout lacks.
    fn weight_index(&self, kind: UseKind) -> Option<Option<usize>> {
        let at = self.kinds.iter().position(|k| *k == kind)?;
        Some(at.checked_sub(1).map(|i| 1 + i))
    }

    fn range_index(&self) -> Option<usize> {
        self.family.range().then(|| 1 + self.free())
    }

    fn evidence_index(&self, evidence: Evidence) -> Option<usize> {
        let base = 1 + self.free() + usize::from(self.family.range());
        self.evidence.iter().position(|e| *e == evidence).map(|i| base + i)
    }

    fn model(&self, theta: &[f64]) -> Option<Model> {
        let decay = theta.first()?.exp();
        let mut weights = BTreeMap::new();
        for (i, kind) in self.kinds.iter().enumerate() {
            let w = match i.checked_sub(1) {
                None => 1.0,
                Some(j) => theta.get(1 + j)?.exp(),
            };
            weights.insert(*kind, w);
        }
        let range_weight = match self.range_index() {
            Some(i) => Some(*theta.get(i)?),
            None => None,
        };
        let mut scales = BTreeMap::new();
        for e in &self.evidence {
            scales.insert(*e, theta.get(self.evidence_index(*e)?)?.exp());
        }
        let model = Model { decay, weights, range_weight, scales, family: self.family };
        valid(&model).then_some(model)
    }

    fn names(&self) -> Vec<String> {
        let mut names = vec!["decay".to_owned()];
        names.extend(self.kinds.iter().skip(1).map(|k| format!("weight.{}", k.name())));
        if self.family.range() {
            names.push("range_weight".into());
        }
        names.extend(self.evidence.iter().map(|e| format!("scale.{}", e.name())));
        names
    }
}

/// How an observation went under a fit.
enum Outcome {
    /// None of its candidates has a use the family reads.
    Uninformative,
    /// Its chosen chat has none, while another does: the fit gave it no chance.
    Refuted,
    Scored(Scored),
}

struct Scored {
    log_likelihood: f64,
    gradient: Vec<f64>,
    information: Vec<Vec<f64>>,
    /// The Hessian of `−log p`, when asked for: the information plus `Σ p·∇²U − ∇²U_chosen`, the curvature of the
    /// utilities themselves, which the information leaves out.
    hessian: Vec<Vec<f64>>,
}

fn outcome(layout: &Layout, model: &Model, obs: &Observation) -> Option<Outcome> {
    outcome_with(layout, model, obs, false)
}

/// [`outcome`], with its Hessian too when `curvature`.
fn outcome_with(layout: &Layout, model: &Model, obs: &Observation, curvature: bool) -> Option<Outcome> {
    let sign = if obs.evidence.worst() { -1.0 } else { 1.0 };
    let scale = if obs.evidence == Evidence::Pick { 1.0 } else { *model.scales.get(&obs.evidence)? };
    let evidence_index = layout.evidence_index(obs.evidence);
    let mut finite = Vec::new();
    let mut chosen = None;
    for (i, c) in obs.candidates.iter().enumerate() {
        let Some((a, ga, ha)) = activation_with(model, Some(layout), c, obs.at, curvature) else { continue };
        let u = sign * scale * a;
        let mut g: Vec<f64> = ga.iter().map(|v| sign * scale * v).collect();
        let mut h: Vec<Vec<f64>> = ha.iter().map(|row| row.iter().map(|v| sign * scale * v).collect()).collect();
        if let Some(e) = evidence_index {
            // `U = sign·λ·a` in `ln λ`: `∂U/∂ln λ = U`, `∂²U/∂(ln λ)² = U` and `∂²U/∂ln λ·∂θ = ∂U/∂θ`.
            if curvature {
                for (r, gr) in g.iter().enumerate().filter(|(r, _)| *r != e) {
                    *h.get_mut(e)?.get_mut(r)? = *gr;
                    *h.get_mut(r)?.get_mut(e)? = *gr;
                }
                *h.get_mut(e)?.get_mut(e)? = u;
            }
            *g.get_mut(e)? = u;
        }
        if i == obs.chosen {
            chosen = Some(finite.len());
        }
        finite.push((u, g, h));
    }
    let Some(chosen) = chosen else {
        return Some(if finite.is_empty() { Outcome::Uninformative } else { Outcome::Refuted });
    };
    let norm = logsum(finite.iter().map(|(u, _, _)| *u))?;
    let (u_chosen, g_chosen, h_chosen) = finite.get(chosen)?;
    let dim = layout.dim();
    let mut mean = vec![0.0; dim];
    for (u, g, _) in &finite {
        let p = (u - norm).exp();
        for (m, v) in mean.iter_mut().zip(g) {
            *m += p * v;
        }
    }
    let gradient: Vec<f64> = g_chosen.iter().zip(&mean).map(|(g, m)| g - m).collect();
    let mut information = vec![vec![0.0; dim]; dim];
    for (u, g, _) in &finite {
        let p = (u - norm).exp();
        for (row, (ga, ma)) in information.iter_mut().zip(g.iter().zip(&mean)) {
            for (cell, (gb, mb)) in row.iter_mut().zip(g.iter().zip(&mean)) {
                *cell += p * (ga - ma) * (gb - mb);
            }
        }
    }
    let mut hessian = Vec::new();
    if curvature {
        hessian.clone_from(&information);
        for (u, _, h) in &finite {
            let p = (u - norm).exp();
            for (row, hrow) in hessian.iter_mut().zip(h) {
                for (cell, v) in row.iter_mut().zip(hrow) {
                    *cell += p * v;
                }
            }
        }
        for (row, hrow) in hessian.iter_mut().zip(h_chosen) {
            for (cell, v) in row.iter_mut().zip(hrow) {
                *cell -= v;
            }
        }
    }
    let log_likelihood = u_chosen - norm;
    let finite_all = |m: &[Vec<f64>]| m.iter().flatten().all(|v| v.is_finite());
    (log_likelihood.is_finite() && gradient.iter().all(|v| v.is_finite()) && finite_all(&information) && finite_all(&hessian))
        .then_some(Outcome::Scored(Scored { log_likelihood, gradient, information, hessian }))
}

/// Invert a likelihood information matrix, refusing unidentified or numerically singular dimensions. The pivot
/// tolerance is floating-point resolution times the matrix's dimension and scale: LAPACK's DPSTRF stops a positive
/// semidefinite factorization at a pivot of at most `n·u·max a_kk` (`u = ε/2`), and NumPy's `matrix_rank` drops singular
/// values below `σ_max·n·ε`.
pub(super) fn inverse(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = matrix.len();
    if n == 0 || matrix.iter().any(|r| r.len() != n) {
        return None;
    }
    let scale = matrix.iter().flatten().map(|v| v.abs()).reduce(f64::max)?;
    let tolerance = f64::EPSILON * n as f64 * scale;
    if !scale.is_finite() || scale == 0.0 {
        return None;
    }
    let mut rows: Vec<Vec<f64>> = matrix.iter().enumerate().map(|(i, row)| row.iter().copied().chain((0..n).map(move |j| if i == j { 1.0 } else { 0.0 })).collect()).collect();
    for col in 0..n {
        let pivot = rows
            .iter()
            .enumerate()
            .skip(col)
            .max_by(|(_, a), (_, b)| a.get(col).copied().unwrap_or_default().abs().total_cmp(&b.get(col).copied().unwrap_or_default().abs()))
            .map(|(i, _)| i)?;
        rows.swap(col, pivot);
        let d = *rows.get(col)?.get(col)?;
        if d.abs() <= tolerance {
            return None;
        }
        let row: Vec<f64> = rows.get(col)?.iter().map(|v| v / d).collect();
        if row.iter().any(|v| !v.is_finite()) {
            return None;
        }
        for (i, target) in rows.iter_mut().enumerate() {
            if i == col {
                target.clone_from(&row);
                continue;
            }
            let factor = *target.get(col)?;
            for (value, pivot) in target.iter_mut().zip(&row) {
                *value -= factor * pivot;
            }
        }
    }
    Some(rows.into_iter().map(|r| r.into_iter().skip(n).collect()).collect())
}

/// The lower Cholesky factor of a positive-definite matrix, with the same pivot tolerance as [`inverse`].
fn cholesky(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = matrix.len();
    let scale = matrix.iter().flatten().map(|v| v.abs()).reduce(f64::max)?;
    let tolerance = f64::EPSILON * n as f64 * scale;
    let mut l = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let mut s = *matrix.get(i)?.get(j)?;
            for k in 0..j {
                s -= l.get(i)?.get(k)? * l.get(j)?.get(k)?;
            }
            let value = if i == j {
                if s <= tolerance {
                    return None;
                }
                s.sqrt()
            } else {
                s / l.get(j)?.get(j)?
            };
            *l.get_mut(i)?.get_mut(j)? = value;
        }
    }
    l.iter().flatten().all(|v| v.is_finite()).then_some(l)
}

fn product(matrix: &[Vec<f64>], vector: &[f64]) -> Vec<f64> {
    matrix.iter().map(|row| row.iter().zip(vector).map(|(a, b)| a * b).sum()).collect()
}

fn norm(values: impl IntoIterator<Item = f64>) -> f64 {
    values.into_iter().map(f64::abs).fold(0.0, f64::max)
}

/// What was known of a family's first parameters before a new kind of use or evidence gave it more: a Gaussian, its
/// mean and precision (the Laplace posterior then).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Prior {
    mean: Vec<f64>,
    precision: Vec<Vec<f64>>,
}

/// `−Σ w·log p` over `observations`, plus the prior's `½(θ−m)ᵀP(θ−m)`.
fn objective(layout: &Layout, theta: &[f64], prior: Option<&Prior>, observations: &[Observation]) -> Option<f64> {
    objective_parts(layout, theta, prior, observations, false).map(|(loss, ..)| loss)
}

/// [`objective`]'s value, its likelihood part `−Σ w·log p` alone, its gradient, its Fisher information and (when
/// `curvature`) its Hessian.
#[allow(clippy::type_complexity)]
fn objective_parts(layout: &Layout, theta: &[f64], prior: Option<&Prior>, observations: &[Observation], curvature: bool) -> Option<(f64, f64, Vec<f64>, Vec<Vec<f64>>, Vec<Vec<f64>>)> {
    let model = layout.model(theta)?;
    let dim = layout.dim();
    let (mut loss, mut gradient, mut information) = (0.0, vec![0.0; dim], vec![vec![0.0; dim]; dim]);
    let mut hessian = if curvature { vec![vec![0.0; dim]; dim] } else { Vec::new() };
    let add = |total: &mut Vec<Vec<f64>>, part: &[Vec<f64>], weight: f64| {
        for (row, prow) in total.iter_mut().zip(part) {
            for (cell, v) in row.iter_mut().zip(prow) {
                *cell += weight * v;
            }
        }
    };
    for obs in observations {
        if let Outcome::Scored(s) = outcome_with(layout, &model, obs, curvature)? {
            loss -= obs.weight * s.log_likelihood;
            for (g, v) in gradient.iter_mut().zip(&s.gradient) {
                *g -= obs.weight * v;
            }
            add(&mut information, &s.information, obs.weight);
            add(&mut hessian, &s.hessian, obs.weight);
        }
    }
    let likelihood = loss;
    if let Some(prior) = prior {
        let delta: Vec<f64> = theta.iter().zip(&prior.mean).map(|(t, m)| t - m).collect();
        let pd = product(&prior.precision, &delta);
        loss += 0.5 * delta.iter().zip(&pd).map(|(a, b)| a * b).sum::<f64>();
        for (g, v) in gradient.iter_mut().zip(&pd) {
            *g += v;
        }
        add(&mut information, &prior.precision, 1.0);
        add(&mut hessian, &prior.precision, 1.0);
    }
    let finite_all = |m: &[Vec<f64>]| m.iter().flatten().all(|v| v.is_finite());
    (loss.is_finite() && gradient.iter().all(|g| g.is_finite()) && finite_all(&information) && finite_all(&hessian)).then_some((loss, likelihood, gradient, information, hessian))
}

/// The modified Newton step `(H + E)⁻¹·g`, `E ≥ 0` the diagonal that Gill, Murray & Wright's (1981, ch. 4) modified
/// Cholesky factorization adds to make the Hessian `H` safely positive definite, and none where it already is: the
/// loss's own curvature wherever it has some. Each pivot is `max(δ, |a_k|, ‖c_k‖∞² ÷ β²)`, with
/// `β² = max(η, ξ ÷ √(n² − 1), ε)` (`η` and `ξ` the largest diagonal and off-diagonal magnitudes) and `δ = ε`, pivoting
/// on the largest remaining diagonal, as Fang & O'Leary (2008, §2.1) state GMW81: bounds from the matrix and
/// floating-point precision, none picked.
fn modified_newton(hessian: &[Vec<f64>], gradient: &[f64]) -> Option<Vec<f64>> {
    let n = gradient.len();
    if n == 0 || hessian.len() != n || hessian.iter().any(|r| r.len() != n) {
        return None;
    }
    let mut a = hessian.to_vec();
    let eta = a.iter().enumerate().filter_map(|(i, row)| row.get(i)).map(|v| v.abs()).fold(0.0, f64::max);
    let xi = a.iter().enumerate().flat_map(|(i, row)| row.iter().enumerate().filter(move |(j, _)| *j != i).map(|(_, v)| v.abs())).fold(0.0, f64::max);
    let beta2 = if n > 1 { eta.max(xi / ((n * n - 1) as f64).sqrt()) } else { eta }.max(f64::EPSILON);
    let delta = f64::EPSILON;
    let mut order: Vec<usize> = (0..n).collect();
    let mut l = vec![vec![0.0; n]; n];
    let mut d = vec![0.0; n];
    let diagonal = |a: &[Vec<f64>], i: usize| a.get(i).and_then(|r| r.get(i)).map_or(0.0, |v| v.abs());
    for k in 0..n {
        let pivot = (k..n).reduce(|best, i| if diagonal(&a, i) > diagonal(&a, best) { i } else { best })?;
        a.swap(k, pivot);
        for row in &mut a {
            row.swap(k, pivot);
        }
        l.swap(k, pivot);
        order.swap(k, pivot);
        let akk = *a.get(k)?.get(k)?;
        let column: Vec<f64> = a.iter().skip(k + 1).map(|row| row.get(k).copied()).collect::<Option<_>>()?;
        let largest = column.iter().map(|v| v.abs()).fold(0.0, f64::max);
        let dk = delta.max(akk.abs()).max(largest * largest / beta2);
        *d.get_mut(k)? = dk;
        for (row, c) in l.iter_mut().skip(k + 1).zip(&column) {
            *row.get_mut(k)? = c / dk;
        }
        for (row, ci) in a.iter_mut().skip(k + 1).zip(&column) {
            for (cell, cj) in row.iter_mut().skip(k + 1).zip(&column) {
                *cell -= ci * cj / dk;
            }
        }
    }
    // L·D·Lᵀ·z = P·g, then the step is Pᵀ·z.
    let mut z: Vec<f64> = order.iter().map(|i| gradient.get(*i).copied()).collect::<Option<_>>()?;
    for i in 0..n {
        let s: f64 = l.get(i)?.iter().take(i).zip(&z).map(|(lv, zv)| lv * zv).sum();
        *z.get_mut(i)? -= s;
    }
    for (zi, di) in z.iter_mut().zip(&d) {
        *zi /= di;
    }
    for i in (0..n).rev() {
        let s = (i + 1..n).map(|j| Some(l.get(j)?.get(i)? * z.get(j)?)).sum::<Option<f64>>()?;
        *z.get_mut(i)? -= s;
    }
    let mut step = vec![0.0; n];
    for (k, i) in order.iter().enumerate() {
        *step.get_mut(*i)? = *z.get(k)?;
    }
    step.iter().all(|v| v.is_finite()).then_some(step)
}


/// Converged when the Newton decrement `gᵀH⁻¹g ÷ 2`, what one more step could still gain (Boyd & Vandenberghe 2004
/// §9.5.1, affine-invariant: a strong prior doesn't change its scale), is negligible against the loss (`√ε` relative,
/// R's `optim` default `reltol`), and the step is negligible against the parameters (`√ε`, the least step
/// tolerance NAG's minimizers allow, since to machine accuracy `F(x + √ε·eⱼ) = F(x)`; Gill, Murray & Wright 1981). A flat
/// objective whose step doesn't vanish is a separated likelihood heading off to infinity, never a fit.
fn converged(loss: f64, gradient: &[f64], step: &[f64], theta: &[f64], weight: f64) -> bool {
    settled(loss, gradient, step, weight) && norm(step.iter().copied()) <= f64::EPSILON.sqrt() * (1.0 + norm(theta.iter().copied()))
}

/// What one more Newton step could still gain is within the loss's resolution: past this, a step changes the loss by
/// no more than floating point can say, however far it moves the parameters.
fn settled(loss: f64, gradient: &[f64], step: &[f64], weight: f64) -> bool {
    let decrement = 0.5 * gradient.iter().zip(step).map(|(g, s)| g * s).sum::<f64>();
    decrement.abs() <= f64::EPSILON.sqrt() * (loss.abs() + weight)
}

/// The maximum of the (prior-weighted) likelihood by modified Newton steps ([`modified_newton`]: the loss's own
/// curvature, which Fisher scoring's expected information leaves out, sending its steps far past the minimum) with
/// monotone backtracking, in log coordinates: every step taken improves it. A fit is a minimum: converged
/// ([`converged`]), its Hessian positive definite as it is, its likelihood not certainty. It ends there, or when a step
/// improves the loss by no more than its floating-point resolution, a fit by the same test or none: a loss that
/// flattens while its step doesn't vanish heads off to infinity (a kind whose weight goes to zero, a likelihood that
/// separates), with no finite maximum yet; the next choices decide. No iteration budget: each step taken improves the
/// loss by more than its resolution, and the loss is bounded below.
fn fit_batch(layout: &Layout, start: &[f64], prior: Option<&Prior>, observations: &[Observation]) -> Option<(Vec<f64>, Vec<Vec<f64>>)> {
    let weight: f64 = observations.iter().map(|o| o.weight).sum();
    let resolution = |loss: f64| f64::EPSILON * (loss.abs() + weight);
    let mut theta = start.to_vec();
    loop {
        let (loss, likelihood, gradient, _, hessian) = objective_parts(layout, &theta, prior, observations, true)?;
        let step = modified_newton(&hessian, &gradient)?;
        if converged(loss, &gradient, &step, &theta, weight) {
            return (cholesky(&hessian).is_some() && likelihood > resolution(likelihood)).then_some((theta, hessian));
        }
        let along = |factor: f64| -> Option<(Vec<f64>, f64)> {
            let next: Vec<f64> = theta.iter().zip(&step).map(|(t, s)| t - factor * s).collect();
            let proposed = objective(layout, &next, prior, observations)?;
            Some((next, proposed))
        };
        let mut factor = 1.0;
        let (mut next, mut next_loss) = loop {
            if let Some((next, proposed)) = along(factor)
                && proposed < loss
            {
                if next == theta {
                    return None;
                }
                break (next, proposed);
            }
            factor /= 2.0;
            if factor < f64::EPSILON {
                return None;
            }
        };
        // A whole step that improves is doubled while that improves it further, by more than its resolution: the
        // least power of two along the step, so a loss falling away toward a boundary is followed there in a few
        // evaluations rather than a step at a time.
        if factor == 1.0 {
            while let Some((further, proposed)) = along(factor * 2.0)
                && next_loss - proposed > resolution(next_loss)
            {
                factor *= 2.0;
                (next, next_loss) = (further, proposed);
            }
        }
        let flat = loss - next_loss <= resolution(loss);
        theta = next;
        if flat {
            let (loss, likelihood, gradient, _, hessian) = objective_parts(layout, &theta, prior, observations, true)?;
            let step = modified_newton(&hessian, &gradient)?;
            return (converged(loss, &gradient, &step, &theta, weight) && cholesky(&hessian).is_some() && likelihood > resolution(likelihood)).then_some((theta, hessian));
        }
    }
}

/// A family's prequential record: the choices it scored before learning from them, how many it gave no chance, and the
/// sum of its weighted log-probabilities of the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub choices: usize,
    pub refuted: usize,
    pub log_likelihood: f64,
}

impl Score {
    fn add(&mut self, other: &Self) {
        self.choices += other.choices;
        self.refuted += other.refuted;
        self.log_likelihood += other.log_likelihood;
    }

    /// Better on the same choices: fewer it gave no chance, then a higher log score (the log score is strictly proper).
    fn beats(&self, other: &Self) -> bool {
        self.choices > 0 && (self.refuted < other.refuted || (self.refuted == other.refuted && self.log_likelihood > other.log_likelihood))
    }
}

/// One family, fitted one observation at a time. What waits for its first fit isn't kept between runs (a server that
/// starts again gathers it afresh): only the fit is, its size the parameters', never the history's.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Learner {
    layout: Layout,
    theta: Vec<f64>,
    precision: Vec<Vec<f64>>,
    identified: bool,
    prior: Option<Prior>,
    #[serde(skip)]
    pending: Vec<Observation>,
    /// The pending observations' information at the current parameters: a fit is tried once it (with the prior's) is
    /// invertible.
    #[serde(skip)]
    pending_information: Vec<Vec<f64>>,
    /// After a fit that didn't converge, the next is tried once the pending observations have doubled (the doubling
    /// trick, Cesa-Bianchi & Lugosi 2006 §2.3), so an unidentified family costs linear time, never quadratic.
    #[serde(skip)]
    retry_at: usize,
}

impl Learner {
    fn new(family: Family) -> Self {
        let layout = Layout { family, kinds: Vec::new(), evidence: Vec::new() };
        let theta = Self::start(&layout);
        Self { layout, theta, precision: Vec::new(), identified: false, prior: None, pending: Vec::new(), pending_information: Vec::new(), retry_at: 0 }
    }

    fn start(layout: &Layout) -> Vec<f64> {
        let mut theta = vec![0.0; layout.dim()];
        if let Some(d) = theta.first_mut() {
            *d = START_DECAY.ln();
        }
        theta
    }

    fn model(&self) -> Option<Model> {
        self.layout.model(&self.theta)
    }

    /// Take in the kinds of use and evidence `obs` brings that the family hasn't seen: their parameters start from the
    /// start, and what was known of the rest becomes the prior of a new fit, made once they're identified.
    fn grow(&mut self, obs: &Observation) {
        let mut kinds = self.layout.kinds.clone();
        let family = self.layout.family;
        for t in obs.candidates.iter().flat_map(|c| c.traces.iter()).filter(|t| family.reads(t.kind)) {
            // A family that reads one latest use has one kind of term, whichever kind that use was.
            let kind = if family.latest().is_some() { UseKind::Message } else { t.kind };
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        let mut evidence = self.layout.evidence.clone();
        if obs.evidence != Evidence::Pick && !evidence.contains(&obs.evidence) {
            evidence.push(obs.evidence);
        }
        if kinds == self.layout.kinds && evidence == self.layout.evidence {
            return;
        }
        let old = self.layout.clone();
        let grown = Layout { family: old.family, kinds, evidence };
        let mut theta = Self::start(&grown);
        // The old parameters keep their values in the new order: ln d, the old kinds' weights, the span, old evidence.
        let mut map = vec![Some(0)];
        map.extend(old.kinds.iter().skip(1).map(|k| grown.weight_index(*k).flatten()));
        if let (Some(_), Some(new)) = (old.range_index(), grown.range_index()) {
            map.push(Some(new));
        }
        map.extend(old.evidence.iter().map(|e| grown.evidence_index(*e)));
        for (from, to) in map.iter().enumerate() {
            if let (Some(to), Some(v)) = (to, self.theta.get(from))
                && let Some(slot) = theta.get_mut(*to)
            {
                *slot = *v;
            }
        }
        if self.identified {
            // Known: the old parameters' Laplace posterior, in the new order, the new ones unknown.
            let dim = grown.dim();
            let mut precision = vec![vec![0.0; dim]; dim];
            for (i, ti) in map.iter().enumerate() {
                for (j, tj) in map.iter().enumerate() {
                    if let (Some(ti), Some(tj), Some(v)) = (ti, tj, self.precision.get(i).and_then(|r| r.get(j)))
                        && let Some(cell) = precision.get_mut(*ti).and_then(|r| r.get_mut(*tj))
                    {
                        *cell = *v;
                    }
                }
            }
            self.prior = Some(Prior { mean: theta.clone(), precision });
            self.identified = false;
            self.pending.clear();
        } else if let Some(prior) = self.prior.take() {
            let dim = grown.dim();
            let mut mean = Self::start(&grown);
            let mut precision = vec![vec![0.0; dim]; dim];
            for (i, ti) in map.iter().enumerate() {
                if let (Some(ti), Some(v)) = (ti, prior.mean.get(i))
                    && let Some(slot) = mean.get_mut(*ti)
                {
                    *slot = *v;
                }
                for (j, tj) in map.iter().enumerate() {
                    if let (Some(ti), Some(tj), Some(v)) = (ti, tj, prior.precision.get(i).and_then(|r| r.get(j)))
                        && let Some(cell) = precision.get_mut(*ti).and_then(|r| r.get_mut(*tj))
                    {
                        *cell = *v;
                    }
                }
            }
            self.prior = Some(Prior { mean, precision });
        }
        self.layout = grown;
        self.theta = theta;
        self.pending_information = self.information_of(&self.pending);
    }

    /// `Σ w·I` of `observations` at the current parameters.
    fn information_of(&self, observations: &[Observation]) -> Vec<Vec<f64>> {
        let dim = self.layout.dim();
        let mut total = vec![vec![0.0; dim]; dim];
        let Some(model) = self.model() else { return total };
        for obs in observations {
            if let Some(Outcome::Scored(s)) = outcome(&self.layout, &model, obs) {
                for (row, srow) in total.iter_mut().zip(&s.information) {
                    for (cell, v) in row.iter_mut().zip(srow) {
                        *cell += obs.weight * v;
                    }
                }
            }
        }
        total
    }

    fn outcome(&self, obs: &Observation) -> Option<Outcome> {
        outcome(&self.layout, &self.model()?, obs)
    }

    /// Learn from `obs`; `before` is its outcome at the current parameters when the caller has it already.
    fn observe(&mut self, obs: &Observation, before: Option<Outcome>) {
        let layout = self.layout.clone();
        self.grow(obs);
        let before = before.filter(|_| self.layout == layout);
        if !self.identified {
            if self.pending_information.len() != self.layout.dim() {
                self.pending_information = self.information_of(&self.pending);
            }
            let added = self.information_of(std::slice::from_ref(obs));
            for (row, arow) in self.pending_information.iter_mut().zip(&added) {
                for (cell, v) in row.iter_mut().zip(arow) {
                    *cell += v;
                }
            }
            self.pending.push(obs.clone());
            let mut known = self.pending_information.clone();
            if let Some(prior) = &self.prior {
                for (row, prow) in known.iter_mut().zip(&prior.precision) {
                    for (cell, v) in row.iter_mut().zip(prow) {
                        *cell += v;
                    }
                }
            }
            if self.pending.len() < self.retry_at || inverse(&known).is_none() {
                return;
            }
            let Some(prior) = self.unit_prior() else { return };
            match fit_batch(&self.layout, &self.theta, Some(&prior), &self.pending) {
                Some((theta, precision)) => {
                    self.theta = theta;
                    self.precision = precision;
                    self.identified = true;
                    self.prior = None;
                    self.pending.clear();
                    self.pending_information = Vec::new();
                    self.retry_at = 0;
                }
                None => self.retry_at = self.pending.len().saturating_mul(2),
            }
            return;
        }
        self.step(obs, before);
    }

    /// The first fit's prior: what was known before (the old posterior, when the family grew), and for the parameters it
    /// says nothing of, the unit-information prior (Kass & Wasserman 1995): centred where the fit starts, on the orders'
    /// null values (ACT-R's recommended decay, every kind of use weighted alike, no span effect, worst choices as variable
    /// as best), with the information of one pending choice on average. As informative as one choice, the person's own
    /// choices soon outweigh it; it gives the posterior a finite mode where the likelihood's maximum is at infinity (a kind
    /// whose weight heads to zero, choices that separate), so a family is fitted once its information is invertible
    /// rather than waiting, its choices held, until the likelihood alone has a maximum.
    fn unit_prior(&self) -> Option<Prior> {
        let count: f64 = self.pending.iter().map(|o| o.weight).sum();
        if !count.is_finite() || count <= 0.0 {
            return None;
        }
        let dim = self.layout.dim();
        let known = |i: usize| self.prior.as_ref().and_then(|p| p.precision.get(i)).is_some_and(|row| row.iter().any(|v| *v != 0.0));
        let mut precision = self.prior.as_ref().map_or_else(|| vec![vec![0.0; dim]; dim], |p| p.precision.clone());
        for (i, (row, info)) in precision.iter_mut().zip(&self.pending_information).enumerate() {
            for (j, (cell, v)) in row.iter_mut().zip(info).enumerate() {
                if !known(i) && !known(j) {
                    *cell += v / count;
                }
            }
        }
        let mean = self.prior.as_ref().map_or_else(|| self.theta.clone(), |p| p.mean.clone());
        Some(Prior { mean, precision })
    }

    /// One Fisher step on the posterior so far times this observation's likelihood, from the current estimate: halved
    /// until it improves that posterior (or kept where it is when nothing does), then the information at the new point.
    fn step(&mut self, obs: &Observation, before: Option<Outcome>) {
        let s = match before {
            Some(Outcome::Scored(s)) => s,
            Some(_) => return,
            None => {
                let Some(model) = self.model() else { return };
                let Some(Outcome::Scored(s)) = outcome(&self.layout, &model, obs) else { return };
                s
            }
        };
        let mut precision = self.precision.clone();
        for (row, srow) in precision.iter_mut().zip(&s.information) {
            for (cell, v) in row.iter_mut().zip(srow) {
                *cell += obs.weight * v;
            }
        }
        let Some(covariance) = inverse(&precision) else { return };
        let push: Vec<f64> = s.gradient.iter().map(|g| obs.weight * g).collect();
        let step = product(&covariance, &push);
        let posterior = |theta: &[f64]| -> Option<(f64, Scored)> {
            let model = self.layout.model(theta)?;
            let Outcome::Scored(s) = outcome(&self.layout, &model, obs)? else { return None };
            let delta: Vec<f64> = theta.iter().zip(&self.theta).map(|(t, m)| t - m).collect();
            let pd = product(&self.precision, &delta);
            Some((obs.weight * s.log_likelihood - 0.5 * delta.iter().zip(&pd).map(|(a, b)| a * b).sum::<f64>(), s))
        };
        let here = obs.weight * s.log_likelihood;
        let mut factor = 1.0;
        let (theta, information) = loop {
            let next: Vec<f64> = self.theta.iter().zip(&step).map(|(t, s)| t + factor * s).collect();
            if let Some((q, at_next)) = posterior(&next)
                && q > here
            {
                break (next, at_next.information);
            }
            factor /= 2.0;
            if factor < f64::EPSILON {
                break (self.theta.clone(), s.information);
            }
        };
        let mut precision = self.precision.clone();
        for (row, srow) in precision.iter_mut().zip(&information) {
            for (cell, v) in row.iter_mut().zip(srow) {
                *cell += obs.weight * v;
            }
        }
        if inverse(&precision).is_some() {
            self.theta = theta;
            self.precision = precision;
        }
    }

    fn estimates(&self) -> Vec<Estimate> {
        let covariance = inverse(&self.precision);
        let names = self.layout.names();
        names
            .into_iter()
            .zip(&self.theta)
            .enumerate()
            .map(|(i, (name, theta))| {
                let variance = covariance.as_ref().and_then(|c| c.get(i)).and_then(|r| r.get(i)).copied().filter(|v| v.is_finite() && *v >= 0.0);
                // A coefficient as it is; a log parameter by the delta method on its exponential.
                let (value, scale) = if name == "range_weight" { (*theta, 1.0) } else { (theta.exp(), theta.exp()) };
                Estimate { name, value, standard_error: variance.map(|v| scale * v.sqrt()).filter(|v| v.is_finite()) }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub name: String,
    pub value: f64,
    /// From the Laplace posterior's covariance: local, not a claim that a small history pins the parameter down.
    pub standard_error: Option<f64>,
}

/// One family's prequential score against the learned family's, on the picks both scored before learning from them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub name: String,
    /// Fitted: its parameters identified from the person's choices.
    #[serde(default)]
    pub fitted: bool,
    pub choices: usize,
    #[serde(default)]
    pub refuted: usize,
    pub log_likelihood: f64,
    /// The learned family's own score on the same picks (none on its own row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<Score>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitReport {
    /// The learned family's point estimate, once fitted, whether or not it beats the simpler orders.
    pub model: Option<Model>,
    pub observations: usize,
    /// Choices whose chosen chat had no recorded use at all: no order this model reads can learn from them.
    pub ignored: usize,
    pub position_corrected: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<Evidence, usize>,
    pub parameters: Vec<Estimate>,
    /// Each family's prequential score, on the same choices.
    pub comparisons: Vec<Comparison>,
    /// The learned family predicted the person's next choice better than every simpler order, on choices none of them
    /// had seen. Only then does it order the list.
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
    #[error("one finite standard normal is needed for each parameter of the order")]
    Randomness,
    #[error("the attention model contains an invalid decay, weight, span coefficient or strength")]
    Model,
}

/// Inverse examination weights, self-normalized within each layout and reference over the choices so far: a learned
/// relative examination's unidentified scale cancels.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Weighting {
    #[serde(with = "pairs")]
    groups: BTreeMap<(String, usize), (f64, usize)>,
}

impl Weighting {
    fn weight(&mut self, choice: &Choice) -> Option<(f64, bool)> {
        if choice.propensity.is_some() && choice.correction.is_some() {
            return None;
        }
        if let Some(r) = &choice.correction {
            if r.layout.is_empty() || r.reference == 0 || !r.examination.is_finite() || r.examination <= 0.0 || r.examination > 1.0 {
                return None;
            }
            let (sum, count) = self.groups.entry((r.layout.clone(), r.reference)).or_default();
            *sum += 1.0 / r.examination;
            *count += 1;
            let weight = (1.0 / r.examination) / (*sum / *count as f64);
            return (weight.is_finite() && weight > 0.0).then_some((weight, true));
        }
        match choice.propensity {
            Some(p) if p.is_finite() && p > 0.0 && p <= 1.0 => Some((1.0 / p, true)),
            Some(_) => None,
            None => Some((1.0, false)),
        }
    }
}

/// Every family, fitted together over one person's choices in time order, each scoring a pick before learning from it.
/// Two families are compared on the picks both scored: one not yet fitted holds back no other's comparison. It's kept as
/// it is (serde), so a server that starts again goes on from it rather than learning every choice again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ensemble {
    learners: Vec<Learner>,
    #[serde(with = "pairs")]
    pairs: BTreeMap<(Family, Family), (Score, Score)>,
    weighting: Weighting,
    observations: usize,
    ignored: usize,
    corrected: usize,
    evidence: BTreeMap<Evidence, usize>,
}

impl Default for Ensemble {
    fn default() -> Self {
        Self::new()
    }
}

impl Ensemble {
    pub fn new() -> Self {
        Self { learners: Family::ALL.iter().map(|f| Learner::new(*f)).collect(), pairs: BTreeMap::new(), weighting: Weighting::default(), observations: 0, ignored: 0, corrected: 0, evidence: BTreeMap::new() }
    }

    /// Read `choice` from `index` (its candidates' uses strictly before it) and learn from it. A choice that can't be
    /// read (a candidate twice, fewer than two, its chosen not among them, an invalid weight) teaches nothing.
    pub fn observe_choice(&mut self, index: &UseIndex, choice: &Choice) {
        self.read_choice(index, choice, true);
    }

    /// [`Ensemble::observe_choice`] for a choice recorded after later ones were learned: it teaches the fits, but no
    /// family scores it, as one that has learned from what came after it would forecast it from the future.
    pub fn learn_late_choice(&mut self, index: &UseIndex, choice: &Choice) {
        self.read_choice(index, choice, false);
    }

    fn read_choice(&mut self, index: &UseIndex, choice: &Choice, score: bool) {
        let unique: std::collections::BTreeSet<_> = choice.candidates.iter().collect();
        let Some(chosen) = choice.candidates.iter().position(|c| c == &choice.chosen) else {
            self.ignored += 1;
            return;
        };
        if unique.len() != choice.candidates.len() || unique.len() < 2 {
            self.ignored += 1;
            return;
        }
        let Some((weight, corrected)) = self.weighting.weight(choice) else {
            self.ignored += 1;
            return;
        };
        let candidates = choice.candidates.iter().map(|id| index.candidate(id, choice.at)).collect();
        self.learn(&Observation { at: choice.at, evidence: choice.evidence, chosen, candidates, weight, corrected }, score);
    }

    pub fn observe(&mut self, obs: &Observation) {
        self.learn(obs, true);
    }

    fn learn(&mut self, obs: &Observation, score: bool) {
        self.observations += 1;
        *self.evidence.entry(obs.evidence).or_default() += 1;
        if obs.corrected {
            self.corrected += 1;
        }
        // A choice of a chat with no recorded use: no family reads anything of it.
        if obs.candidates.get(obs.chosen).is_none_or(|c| !c.traces.iter().any(|t| Family::ActR.reads(t.kind))) {
            self.ignored += 1;
            return;
        }
        // Scored by each fitted family before any learns from it, when the richest can read its chosen chat (one whose
        // uses are all of kinds no fit has seen yet is nobody's to score); each pair of families that both read it
        // records it, so they're compared on the same picks. Each family's outcome then goes on to teach it.
        let outcomes: Vec<(Family, Option<Outcome>)> = self.learners.iter().map(|l| (l.layout.family, l.identified.then(|| l.outcome(obs)).flatten())).collect();
        if score && obs.evidence == Evidence::Pick {
            let richest = outcomes.iter().find(|(f, _)| *f == Family::ActR).and_then(|(_, o)| o.as_ref());
            if matches!(richest, Some(Outcome::Scored(_))) {
                let scores: Vec<(Family, Score)> = outcomes
                    .iter()
                    .filter_map(|(f, o)| match o {
                        Some(Outcome::Scored(s)) => Some((*f, Score { choices: 1, refuted: 0, log_likelihood: obs.weight * s.log_likelihood })),
                        Some(Outcome::Refuted) => Some((*f, Score { choices: 1, refuted: 1, log_likelihood: 0.0 })),
                        _ => None,
                    })
                    .collect();
                for (i, (fa, a)) in scores.iter().enumerate() {
                    for (fb, b) in scores.iter().skip(i + 1) {
                        let (sa, sb) = self.pairs.entry((*fa, *fb)).or_default();
                        sa.add(a);
                        sb.add(b);
                    }
                }
            }
        }
        for (learner, (_, before)) in self.learners.iter_mut().zip(outcomes) {
            learner.observe(obs, before);
        }
    }

    fn learner(&self, family: Family) -> Option<&Learner> {
        self.learners.iter().find(|l| l.layout.family == family)
    }

    /// `a`'s and `b`'s scores on the picks both scored.
    fn pair(&self, a: Family, b: Family) -> (Score, Score) {
        match self.pairs.get(&(a, b)) {
            Some((sa, sb)) => (*sa, *sb),
            None => self.pairs.get(&(b, a)).map(|(sb, sa)| (*sa, *sb)).unwrap_or_default(),
        }
    }

    /// The learned family: ACT-R's, or with its time span when that scored better on the picks both scored.
    fn learned_family(&self) -> Option<Family> {
        self.learner(Family::ActR).filter(|l| l.identified)?;
        let (plain, span) = self.pair(Family::ActR, Family::ActRWithRange);
        Some(if span.beats(&plain) { Family::ActRWithRange } else { Family::ActR })
    }

    /// The learned family, when it predicted the person's next choice better than every simpler order, each on the picks
    /// both had scored before learning from them: the family whose draws order the list. Otherwise none, and the person's
    /// latest interaction orders it.
    pub fn learned(&self) -> Option<Family> {
        let family = self.learned_family()?;
        Family::ALL.iter().filter(|b| !b.learned()).all(|b| {
            let (mine, theirs) = self.pair(family, *b);
            mine.beats(&theirs)
        })
        .then_some(family)
    }

    pub fn point(&self, family: Family) -> Option<Model> {
        self.learner(family).filter(|l| l.identified)?.model()
    }

    /// How many standard normals a draw of `family` takes.
    pub fn dimension(&self, family: Family) -> usize {
        self.learner(family).map_or(0, |l| l.layout.dim())
    }

    /// One Thompson draw of `family`'s parameters from its Laplace posterior, `θ̂ + L⁻ᵀz` with `Λ = LLᵀ`, given one
    /// standard normal `z` per parameter. None while it isn't fitted.
    pub fn draw(&self, family: Family, normals: &[f64]) -> Result<Option<Model>, Error> {
        let Some(learner) = self.learner(family).filter(|l| l.identified) else { return Ok(None) };
        if normals.len() != learner.layout.dim() || normals.iter().any(|z| !z.is_finite()) {
            return Err(Error::Randomness);
        }
        let Some(l) = cholesky(&learner.precision) else { return Ok(learner.model()) };
        // Solve Lᵀx = z by back substitution.
        let n = normals.len();
        let mut x = vec![0.0; n];
        for i in (0..n).rev() {
            let mut s = normals.get(i).copied().unwrap_or_default();
            for k in i + 1..n {
                s -= l.get(k).and_then(|r| r.get(i)).copied().unwrap_or_default() * x.get(k).copied().unwrap_or_default();
            }
            let d = l.get(i).and_then(|r| r.get(i)).copied().unwrap_or_default();
            if let Some(slot) = x.get_mut(i) {
                *slot = s / d;
            }
        }
        let theta: Vec<f64> = learner.theta.iter().zip(&x).map(|(t, v)| t + v).collect();
        Ok(learner.layout.model(&theta))
    }

    pub fn report(&self) -> FitReport {
        let learned = self.learned_family();
        let learner = learned.and_then(|f| self.learner(f));
        let improves = self.learned().is_some();
        // Each family against the learned one, on the picks both scored.
        let comparisons: Vec<Comparison> = self
            .learners
            .iter()
            .map(|l| {
                let family = l.layout.family;
                let (mine, theirs) = match learned {
                    // Its own row: its score on the most picks it was compared on.
                    Some(f) if f == family => (Score::default(), Family::ALL.iter().filter(|o| **o != f).map(|o| self.pair(f, *o).0).max_by_key(|s| s.choices).unwrap_or_default()),
                    Some(f) => self.pair(f, family),
                    None => (Score::default(), Score::default()),
                };
                Comparison { name: family.name().into(), fitted: l.identified, choices: theirs.choices, refuted: theirs.refuted, log_likelihood: theirs.log_likelihood, learned: (mine.choices > 0).then_some(mine) }
            })
            .collect();
        let scored = comparisons.iter().map(|c| c.choices).max().unwrap_or(0);
        let position = if self.corrected > 0 {
            "Picks from the list are weighted by how likely their row was to be looked at, learned from your picks; picks from a notification, a link or a search count as they are."
        } else {
            "No examination evidence is recorded yet, so the list's position bias isn't corrected."
        };
        let reason = match (learner, improves) {
            (None, _) => format!("Not enough of your choices yet to fit decay and use weights; ordered by your latest interaction. {position}"),
            (Some(_), false) if scored == 0 => format!("Decay and use weights are fitted, but no choice has yet been scored by every order before it was learned from; ordered by your latest interaction. {position}"),
            (Some(_), false) => {
                let behind: Vec<&str> = Family::ALL
                    .iter()
                    .filter(|b| !b.learned() && !learned.is_some_and(|f| { let (mine, theirs) = self.pair(f, **b); mine.beats(&theirs) }))
                    .map(|b| b.name())
                    .collect();
                format!("Fitted decay and use weights don't yet predict your next choice better than {} on {scored} choices they hadn't seen; ordered by your latest interaction. {position}", behind.join(", "))
            }
            (Some(_), true) => format!("Learned decay and use weights predict your next choice better than every simpler order, on {scored} choices none had seen; your list follows them, drawn afresh each time you come to it. {position}"),
        };
        FitReport {
            model: learner.and_then(Learner::model),
            observations: self.observations,
            ignored: self.ignored,
            position_corrected: self.corrected,
            evidence: self.evidence.clone(),
            parameters: learner.map(Learner::estimates).unwrap_or_default(),
            comparisons,
            improves_baseline: improves,
            reason,
        }
    }
}

/// Fit every family over `choices` in time order, reading their candidates' uses from `uses`.
pub fn fit(uses: &[Use], choices: &[Choice]) -> (Ensemble, FitReport) {
    let index = UseIndex::new(uses);
    let mut ordered: Vec<&Choice> = choices.iter().collect();
    ordered.sort_by_key(|c| c.at);
    let mut ensemble = Ensemble::new();
    for choice in ordered {
        ensemble.observe_choice(&index, choice);
    }
    let report = ensemble.report();
    (ensemble, report)
}

/// Explicit model validation for stored models before they are used by a server.
pub fn validate(model: &Model) -> Result<(), Error> {
    if valid(model) { Ok(()) } else { Err(Error::Model) }
}

/// A model's activation for one candidate at `now`, or none (−∞) when it has no use the model's family reads.
pub fn activation_of(model: &Model, candidate: &Candidate, now: i64) -> Option<f64> {
    activation(model, None, candidate, now).map(|(a, _)| a)
}

/// Order `candidates` by `model`'s activation at `now`; without a model (or opted out), by the person's most recent
/// real interaction. Pins, parks and requests are placement, outside. Agents' and subagents' turns never stand in for
/// a person's interaction.
pub fn rank_candidates(model: Option<&Model>, candidates: &[Candidate], now: i64) -> Vec<Ranked> {
    let model = model.filter(|m| valid(m));
    let count = |c: &Candidate, keep: fn(UseKind) -> bool| c.traces.iter().filter(|t| keep(t.kind)).map(|t| t.count).sum::<u64>();
    let mut rows: Vec<(Ranked, Option<i64>)> = candidates
        .iter()
        .map(|c| {
            let human = c.traces.iter().filter(|t| t.kind.human()).filter_map(|t| t.recent.first().copied()).max();
            let score = model.and_then(|m| activation_of(m, c, now));
            let reason = if score.is_some() {
                format!(
                    "{} interactions from you · {} agent turns · {} subagent turns",
                    count(c, UseKind::human),
                    count(c, |k| k == UseKind::Turn),
                    count(c, |k| k == UseKind::SubagentTurn)
                )
            } else if human.is_some() {
                "Your latest interaction".into()
            } else {
                "No recorded interaction from you".into()
            };
            (Ranked { agent: c.agent.clone(), score, reason }, human)
        })
        .collect();
    rows.sort_by(|(a, ah), (b, bh)| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| bh.cmp(ah)).then_with(|| a.agent.cmp(&b.agent)));
    rows.into_iter().map(|(r, _)| r).collect()
}

/// [`rank_candidates`], reading each candidate's uses before `now` from `uses`.
pub fn rank(model: Option<&Model>, uses: &[Use], candidates: &[String], now: i64) -> Vec<Ranked> {
    let index = UseIndex::new(uses);
    let read: Vec<Candidate> = candidates.iter().map(|id| index.candidate(id, now)).collect();
    rank_candidates(model, &read, now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Question,
    Tool,
    Plan,
    Goals,
    Stop,
}

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
    let estimates: BTreeMap<_, _> = by_kind
        .into_iter()
        .filter_map(|(kind, mut durations)| {
            durations.sort_unstable();
            let middle = durations.len() / 2;
            let high = *durations.get(middle)? as f64;
            let median = if durations.len() % 2 == 0 {
                let low = *durations.get(middle.checked_sub(1)?)? as f64;
                // Split the sum to keep the median finite for arbitrarily large timestamps.
                low / 2.0 + high / 2.0
            } else {
                high
            };
            Some((kind, (median, durations.len())))
        })
        .collect();
    let mut ordered: Vec<_> = requests
        .iter()
        .map(|request| {
            let waited = now.saturating_sub(request.at).max(0);
            let estimate = estimates.get(&request.kind);
            let expected_wait = estimate.map(|(median, _)| *median);
            let urgency = expected_wait.filter(|median| *median > 0.0).map(|median| waited as f64 / median);
            RequestPriority {
                agent: request.agent.clone(),
                kind: request.kind,
                at: request.at,
                waited,
                expected_wait,
                urgency,
                stale: longest.is_some_and(|duration| waited > duration),
                observations: estimate.map_or(0, |(_, count)| *count),
            }
        })
        .collect();
    let learned_order = !ordered.is_empty() && ordered.iter().all(|request| request.urgency.is_some());
    ordered.sort_by(|a, b| {
        let learned = if learned_order { b.urgency.partial_cmp(&a.urgency).unwrap_or(std::cmp::Ordering::Equal) } else { std::cmp::Ordering::Equal };
        learned.then_with(|| a.at.cmp(&b.at)).then_with(|| a.agent.cmp(&b.agent)).then_with(|| a.kind.cmp(&b.kind))
    });
    let reason = if learned_order {
        "Wait divided by your median answered wait for each request type; stale means longer than your longest recorded answered wait."
    } else {
        "Oldest first until every pending request type has a positive observed median answered wait; stale requires recorded response history."
    }
    .into();
    UrgencyReport { requests: ordered, learned_order, reason }
}

#[cfg(test)]
mod tests;
