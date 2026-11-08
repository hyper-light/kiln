use super::*;
use anyhow::{anyhow, ensure};

fn usage(agent: &str, kind: UseKind, at: i64) -> Use {
    Use { agent: agent.into(), kind, at }
}

fn pick(at: i64, chosen: &str, candidates: &[&str]) -> Choice {
    Choice { at, chosen: chosen.into(), candidates: candidates.iter().map(|c| (*c).to_owned()).collect(), propensity: None, correction: None, evidence: Evidence::Pick }
}

/// A seeded xorshift: the tests' randomness, passed in as the model's always is.
struct Rng(u64);

impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// `∫ t^−d dt ÷ (b − a)` over [a, b] by composite Simpson on `u = ln t`: the reference the closed form is held to.
fn mean_power(a: f64, b: f64, d: f64) -> f64 {
    let (lo, hi) = (a.ln(), b.ln());
    let n = 20_000;
    let h = (hi - lo) / n as f64;
    let f = |u: f64| ((1.0 - d) * u).exp();
    let mut sum = f(lo) + f(hi);
    for i in 1..n {
        sum += f(lo + i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
    }
    sum * h / 3.0 / (b - a)
}

#[test]
fn mc_attention_petrov_tail_is_the_mean_of_age_power_in_closed_form() -> anyhow::Result<()> {
    let at = 10_000_000_000;
    for (lower_age, upper_age) in [(1_000.0_f64, 9_000_000.0_f64), (60_000.0, 60_000.0), (5.0, 7.0)] {
        for d in [0.05, 0.5, 1.0, 1.7] {
            let newest = at - lower_age as i64;
            let oldest = at - upper_age as i64;
            let trace = Trace { kind: UseKind::Message, recent: vec![newest], count: 2, oldest };
            let (ln_s, _) = strength(&trace, at, d).ok_or_else(|| anyhow!("no strength"))?;
            let exact = lower_age.powf(-d);
            let tail = if upper_age > lower_age { mean_power(lower_age, upper_age, d) } else { lower_age.powf(-d) };
            let want = (exact + tail).ln();
            ensure!((ln_s - want).abs() <= 1e-9 * want.abs().max(1.0), "ages {lower_age}..{upper_age}, d {d}: {ln_s} vs {want}");
        }
    }
    Ok(())
}

#[test]
fn mc_attention_petrov_is_exact_within_its_recent_uses_and_its_decay_derivative_holds() -> anyhow::Result<()> {
    let at = 1_000_000;
    let one = Trace { kind: UseKind::Turn, recent: vec![at - 400], count: 1, oldest: at - 400 };
    let (ln_s, d_ln_s) = strength(&one, at, 0.5).ok_or_else(|| anyhow!("no strength"))?;
    ensure!((ln_s - (-0.5 * 400f64.ln())).abs() < 1e-12 && (d_ln_s + 400f64.ln()).abs() < 1e-12);
    // The derivative in d of a trace with a tail, against central differences.
    let many = Trace { kind: UseKind::Turn, recent: vec![at - 30], count: 40, oldest: at - 900_000 };
    for d in [0.2, 0.5, 0.999_999, 1.0, 1.3] {
        let h = 1e-6;
        let (_, analytic) = strength(&many, at, d).ok_or_else(|| anyhow!("no strength"))?;
        let up = strength(&many, at, d + h).ok_or_else(|| anyhow!("no strength"))?.0;
        let down = strength(&many, at, d - h).ok_or_else(|| anyhow!("no strength"))?.0;
        let numeric = (up - down) / (2.0 * h);
        ensure!((analytic - numeric).abs() <= 1e-5 * numeric.abs().max(1.0), "d {d}: {analytic} vs {numeric}");
    }
    // phi' is continuous where its two forms meet, and is phi's derivative.
    ensure!((phi_prime(0.0) - 0.5).abs() < 1e-15);
    for x in [-3.0, -1.0, -0.5, 1e-9, 0.5, 1.0, 2.5] {
        let h = 1e-6;
        let numeric = (phi(x + h) - phi(x - h)) / (2.0 * h);
        ensure!((phi_prime(x) - numeric).abs() <= 1e-6 * numeric.abs().max(1.0), "x {x}");
    }
    Ok(())
}

#[test]
fn mc_attention_uses_are_read_strictly_before_their_moment_newest_first() -> anyhow::Result<()> {
    let index = UseIndex::new(&[usage("a", UseKind::Message, 30), usage("a", UseKind::Message, 10), usage("a", UseKind::Turn, 20), usage("a", UseKind::Message, 50)]);
    let read = index.candidate("a", 50);
    let message = read.traces.iter().find(|t| t.kind == UseKind::Message).ok_or_else(|| anyhow!("no messages"))?;
    ensure!(message.count == 2 && message.oldest == 10 && message.recent.first() == Some(&30), "{message:?}");
    ensure!(index.candidate("a", 10).traces.is_empty(), "nothing at a use's own moment");
    ensure!(index.candidate("nobody", 99).traces.is_empty());
    Ok(())
}

#[test]
fn mc_attention_both_turn_kinds_keep_work_visible_but_cannot_become_human_uses() {
    for kind in [UseKind::Turn, UseKind::SubagentTurn] {
        assert!(!kind.human() && kind.activates());
        let uses = [usage("busy", kind, 1), usage("you", UseKind::Message, 2)];
        let fallback = rank(None, &uses, &["busy".into(), "you".into()], 3);
        assert_eq!(fallback.first().map(|r| r.agent.as_str()), Some("you"));
        assert!(fallback.iter().any(|r| r.agent == "busy" && r.reason == "No recorded interaction from you"));
    }
    assert!(!UseKind::Park.activates() && !UseKind::Archive.activates() && !UseKind::Park.human());
}

#[test]
fn mc_attention_sparse_history_never_invents_a_model() {
    let (ensemble, report) = fit(&[usage("a", UseKind::Message, 1)], &[pick(2, "a", &["a", "b"])]);
    assert!(report.model.is_none() && !report.improves_baseline && ensemble.learned().is_none());
    assert!(report.reason.starts_with("Not enough of your choices"));
}

#[test]
fn mc_attention_unreadable_choices_teach_nothing_and_history_less_alternatives_take_no_share() {
    let uses = [usage("a", UseKind::Message, 1), usage("b", UseKind::Message, 2)];
    let (_, report) = fit(&uses, &[pick(5, "a", &["a", "a"]), pick(5, "a", &["a"]), pick(5, "c", &["a", "b"]), pick(5, "fresh", &["fresh", "a"])]);
    // A duplicate, a single candidate and an absent chosen chat are unreadable; a chosen chat with no use at all is
    // evidence of nothing the model reads.
    assert_eq!((report.observations, report.ignored), (1, 4));
    // A history-less alternative is no reason to drop a choice: it takes no share.
    let (_, kept) = fit(&uses, &[pick(5, "a", &["a", "b", "new"])]);
    assert_eq!((kept.observations, kept.ignored), (1, 0));
}

#[test]
fn mc_attention_observed_and_relative_examination_weights_choices() {
    let mut weighting = Weighting::default();
    let observed = Choice { propensity: Some(0.25), ..pick(1, "a", &["a", "b"]) };
    assert_eq!(weighting.weight(&observed), Some((4.0, true)));
    assert_eq!(weighting.weight(&pick(1, "a", &["a", "b"])), Some((1.0, false)));
    for bad in [0.0, -1.0, 1.5, f64::NAN] {
        assert_eq!(weighting.weight(&Choice { propensity: Some(bad), ..observed.clone() }), None);
    }
    let corrected = |e: f64| Choice { correction: Some(RelativeCorrection { layout: "desktop".into(), reference: 1, examination: e }), ..pick(1, "a", &["a", "b"]) };
    // Self-normalized within its layout and reference: the first is the mean, the next its ratio to the running mean.
    assert_eq!(weighting.weight(&corrected(0.5)), Some((1.0, true)));
    assert_eq!(weighting.weight(&corrected(0.25)), Some((4.0 / 3.0, true)));
    assert_eq!(weighting.weight(&Choice { propensity: Some(0.5), ..corrected(0.5) }), None, "an absolute and a relative examination at once");
    assert_eq!(weighting.weight(&corrected(0.0)), None);
}

/// A synthetic person: `agents` chats that take turns at random, each moment a choice of one drawn from `truth`, which
/// the person then messages. `turns` is each chat's chance of a turn between choices.
fn world(truth: &Model, agents: usize, choices: usize, turns: f64, seed: u64) -> (Vec<Use>, Vec<Choice>) {
    let mut rng = Rng(seed);
    let ids: Vec<String> = (0..agents).map(|i| format!("agent-{i}")).collect();
    let mut uses = Vec::new();
    let mut index = UseIndex::default();
    let mut at = 1_000_000_000;
    for id in &ids {
        for kind in [UseKind::Message, UseKind::Turn] {
            let u = usage(id, kind, at - 1 - (rng.uniform() * 3_600_000.0) as i64);
            index.add(&u);
            uses.push(u);
        }
    }
    let mut made = Vec::new();
    for _ in 0..choices {
        // Turns between this choice and the last, never back into a choice already drawn.
        let prev = at;
        at += 2 + (-rng.uniform().ln() * 600_000.0) as i64;
        for id in &ids {
            if rng.uniform() < turns {
                let u = usage(id, UseKind::Turn, prev + 1 + (rng.uniform() * (at - prev - 1) as f64) as i64);
                index.add(&u);
                uses.push(u);
            }
        }
        let read: Vec<Candidate> = ids.iter().map(|id| index.candidate(id, at)).collect();
        let activations: Vec<f64> = read.iter().map(|c| activation_of(truth, c, at).unwrap_or(f64::NEG_INFINITY)).collect();
        let norm = logsum(activations.iter().copied().filter(|a| a.is_finite())).unwrap_or(0.0);
        let mut draw = rng.uniform();
        let mut chosen = ids.len().saturating_sub(1);
        for (i, a) in activations.iter().enumerate() {
            draw -= (a - norm).exp();
            if draw <= 0.0 {
                chosen = i;
                break;
            }
        }
        let name = ids.get(chosen).cloned().unwrap_or_default();
        made.push(Choice { at, chosen: name.clone(), candidates: ids.clone(), propensity: None, correction: None, evidence: Evidence::Pick });
        let u = usage(&name, UseKind::Message, at);
        index.add(&u);
        uses.push(u);
    }
    (uses, made)
}

fn truth(decay: f64, turn: f64) -> Model {
    Model { decay, weights: [(UseKind::Message, 1.0), (UseKind::Turn, turn)].into_iter().collect(), range_weight: None, scales: BTreeMap::new(), family: Family::ActR }
}

fn estimate<'a>(report: &'a FitReport, name: &str) -> anyhow::Result<&'a Estimate> {
    report.parameters.iter().find(|e| e.name == name).ok_or_else(|| anyhow!("no {name} in {:?}", report.parameters))
}

/// The plan's step 2 test: histories generated from a known decay and weights; the fit recovers them within its own
/// reported uncertainty (a 95% normal interval, 1.96 standard errors).
#[test]
fn mc_attention_fit_recovers_synthetic_decay_and_weights_within_its_uncertainty() -> anyhow::Result<()> {
    let generating = truth(0.5, 0.3);
    let (uses, choices) = world(&generating, 12, 1_500, 0.2, 7);
    let (_, report) = fit(&uses, &choices);
    ensure!(report.model.is_some(), "{}", report.reason);
    for (name, value) in [("decay", 0.5), ("weight.turn", 0.3)] {
        let e = estimate(&report, name)?;
        let se = e.standard_error.ok_or_else(|| anyhow!("{name} has no standard error"))?;
        ensure!((e.value - value).abs() <= 1.96 * se, "{name}: {} ± {se} vs {value}", e.value);
    }
    Ok(())
}

/// The plan's step 2 test: on a person's history in time order, next-choice likelihood on choices each fit hadn't seen
/// is reported for the score and every simpler order, on the same choices; here, where agents' turns matter, the
/// learned order beats them all.
#[test]
fn mc_attention_prequential_comparison_reports_every_order_on_the_same_unseen_choices() -> anyhow::Result<()> {
    let (uses, choices) = world(&truth(0.5, 0.8), 12, 1_200, 0.3, 11);
    let (ensemble, report) = fit(&uses, &choices);
    let names: Vec<&str> = report.comparisons.iter().map(|c| c.name.as_str()).collect();
    ensure!(names == ["act_r", "act_r_with_range", "your_recency", "without_agent_turns", "latest_activity"], "{names:?}");
    // Each simpler order is scored on the same picks as the learned one, and loses on them.
    for c in report.comparisons.iter().filter(|c| matches!(c.name.as_str(), "your_recency" | "without_agent_turns" | "latest_activity")) {
        let learned = c.learned.ok_or_else(|| anyhow!("{} has no learned score beside it: {:?}", c.name, report.comparisons))?;
        ensure!(c.fitted && c.choices > 0 && learned.choices == c.choices, "{c:?}");
        ensure!(learned.beats(&Score { choices: c.choices, refuted: c.refuted, log_likelihood: c.log_likelihood }), "{c:?}");
    }
    ensure!(report.improves_baseline && ensemble.learned().is_some(), "{}\n{:?}", report.reason, report.comparisons);
    Ok(())
}

#[test]
fn mc_attention_an_order_that_gives_the_chosen_chat_no_chance_is_refuted_first() {
    let mine = Score { choices: 3, refuted: 0, log_likelihood: -9.0 };
    let theirs = Score { choices: 3, refuted: 1, log_likelihood: -1.0 };
    assert!(mine.beats(&theirs) && !theirs.beats(&mine));
    assert!(Score { log_likelihood: -2.0, ..mine }.beats(&mine) && !mine.beats(&mine));
    assert!(!Score::default().beats(&Score::default()), "nothing scored beats nothing");
}

/// The plan's step 5 test: seeded randomness gives the same orders, and the posterior narrows as choices come.
#[test]
fn mc_attention_seeded_draws_repeat_and_the_posterior_narrows_as_choices_come() -> anyhow::Result<()> {
    let (uses, choices) = world(&truth(0.5, 0.3), 12, 1_600, 0.2, 3);
    let index = UseIndex::new(&uses);
    let mut ensemble = Ensemble::new();
    let mut widths = Vec::new();
    for (i, choice) in choices.iter().enumerate() {
        ensemble.observe_choice(&index, choice);
        if i + 1 == 400 || i + 1 == 1_600 {
            widths.push(estimate(&ensemble.report(), "decay")?.standard_error.ok_or_else(|| anyhow!("no width"))?);
        }
    }
    ensure!(matches!(widths.as_slice(), [early, late] if late < early), "{widths:?}");
    let dim = ensemble.dimension(Family::ActR);
    let mut rng = Rng(99);
    let normals: Vec<f64> = (0..dim).map(|_| rng.normal()).collect();
    let a = ensemble.draw(Family::ActR, &normals)?;
    let b = ensemble.draw(Family::ActR, &normals)?;
    ensure!(a.is_some() && a == b, "the same seed draws the same model");
    let other: Vec<f64> = normals.iter().map(|z| -z).collect();
    ensure!(ensemble.draw(Family::ActR, &other)? != a, "another draw differs");
    ensure!(ensemble.draw(Family::ActR, &vec![0.0; dim])? == ensemble.point(Family::ActR), "a zero draw is the point estimate");
    let candidates: Vec<String> = (0..12).map(|i| format!("agent-{i}")).collect();
    let now = choices.last().map_or(0, |c| c.at) + 1;
    ensure!(rank(a.as_ref(), &uses, &candidates, now) == rank(b.as_ref(), &uses, &candidates, now), "the same draw, the same order");
    ensure!(matches!(ensemble.draw(Family::ActR, normals.get(1..).unwrap_or_default()), Err(Error::Randomness)));
    ensure!(matches!(ensemble.draw(Family::ActR, &vec![f64::NAN; dim]), Err(Error::Randomness)));
    Ok(())
}

/// Parks are worst choices with a strength of their own, learned; a park is not a use that raises activation.
#[test]
fn mc_attention_parks_are_learned_set_aside_evidence_never_interest() -> anyhow::Result<()> {
    let generating = truth(0.5, 0.3);
    let (mut uses, mut choices) = world(&generating, 10, 900, 0.2, 5);
    let index = UseIndex::new(&uses);
    let ids: Vec<String> = (0..10).map(|i| format!("agent-{i}")).collect();
    // Every few choices the person parks one chat, drawn as worst-choice evidence of strength 1.5: the less active,
    // the likelier.
    let mut rng = Rng(17);
    let extra: Vec<Choice> = choices
        .iter()
        .step_by(6)
        .filter_map(|c| {
            let at = c.at - 1;
            let set_aside: Vec<(f64, &String)> = ids.iter().filter_map(|id| activation_of(&generating, &index.candidate(id, at), at).map(|a| (-1.5 * a, id))).collect();
            let norm = logsum(set_aside.iter().map(|(u, _)| *u))?;
            let mut draw = rng.uniform();
            let chosen = set_aside.iter().find(|(u, _)| {
                draw -= (u - norm).exp();
                draw <= 0.0
            }).or(set_aside.last())?.1.clone();
            Some(Choice { at, chosen, candidates: ids.clone(), propensity: None, correction: None, evidence: Evidence::Park })
        })
        .collect();
    for park in &extra {
        uses.push(usage(&park.chosen, UseKind::Park, park.at));
    }
    choices.extend(extra);
    let (_, report) = fit(&uses, &choices);
    let strength = estimate(&report, "scale.park")?;
    ensure!(strength.value > 0.0 && strength.standard_error.is_some(), "{strength:?}");
    ensure!(report.evidence.get(&Evidence::Park).is_some_and(|n| *n > 0));
    let model = report.model.ok_or_else(|| anyhow!("no model"))?;
    let plain = Candidate { agent: "a".into(), traces: vec![Trace { kind: UseKind::Message, recent: vec![10], count: 1, oldest: 10 }] };
    let parked = Candidate { traces: vec![plain.traces.first().cloned().ok_or_else(|| anyhow!("no trace"))?, Trace { kind: UseKind::Park, recent: vec![20], count: 1, oldest: 20 }], ..plain.clone() };
    ensure!(activation_of(&model, &plain, 30) == activation_of(&model, &parked, 30), "a park raises nothing");
    Ok(())
}

#[test]
fn mc_attention_validation_refuses_impossible_models() {
    let good = truth(0.5, 0.3);
    assert!(validate(&good).is_ok());
    assert!(validate(&Model { decay: 0.0, ..good.clone() }).is_err());
    assert!(validate(&Model { weights: BTreeMap::new(), ..good.clone() }).is_err());
    assert!(validate(&Model { scales: [(Evidence::Park, -1.0)].into_iter().collect(), ..good.clone() }).is_err());
    assert!(validate(&Model { range_weight: Some(1.0), ..good }).is_err(), "a span coefficient belongs to the span family alone");
}

fn request(agent: &str, kind: RequestKind, at: i64) -> RequestWait {
    RequestWait { agent: agent.into(), kind, at }
}

#[test]
fn mc_attention_interventions_use_personal_type_medians_and_global_stale_boundary() {
    let requests = [request("slow", RequestKind::Plan, 100), request("fast", RequestKind::Question, 700)];
    let responses = [
        ResponseTime { kind: RequestKind::Plan, duration: 600 },
        ResponseTime { kind: RequestKind::Question, duration: 50 },
        ResponseTime { kind: RequestKind::Question, duration: 100 },
        ResponseTime { kind: RequestKind::Question, duration: 150 },
    ];
    let report = urgency(&requests, &responses, 1000);
    assert!(report.learned_order);
    assert_eq!(report.requests.first().map(|r| r.agent.as_str()), Some("fast"));
    let fast = report.requests.iter().find(|r| r.agent == "fast");
    assert!(fast.is_some_and(|r| r.expected_wait == Some(100.0) && r.urgency == Some(3.0) && !r.stale && r.observations == 3));
    assert!(report.requests.iter().any(|r| r.agent == "slow" && r.stale && r.urgency == Some(1.5)));
    assert!(!urgency(&[request("equal", RequestKind::Question, 400)], &responses, 1000).requests.iter().any(|r| r.stale));
}

#[test]
fn mc_attention_interventions_unknown_types_use_total_oldest_first_order() {
    let requests = [request("middle", RequestKind::Tool, 200), request("new", RequestKind::Question, 300), request("old", RequestKind::Question, 100)];
    let none = urgency(&requests, &[], 1000);
    assert!(!none.learned_order);
    assert_eq!(none.requests.iter().map(|r| r.agent.as_str()).collect::<Vec<_>>(), vec!["old", "middle", "new"]);
    assert!(none.requests.iter().all(|r| !r.stale && r.expected_wait.is_none() && r.urgency.is_none()));
    let partial = urgency(&requests, &[ResponseTime { kind: RequestKind::Question, duration: 10 }], 1000);
    assert!(!partial.learned_order);
    assert_eq!(partial.requests.iter().map(|r| r.agent.as_str()).collect::<Vec<_>>(), vec!["old", "middle", "new"]);
    assert!(partial.requests.iter().find(|r| r.agent == "middle").is_some_and(|r| r.urgency.is_none()));
}

#[test]
fn mc_attention_interventions_zero_negative_even_and_extreme_times_stay_finite() {
    let requests = [request("future", RequestKind::Question, i64::MAX), request("old", RequestKind::Question, i64::MIN)];
    let negative = urgency(&requests, &[ResponseTime { kind: RequestKind::Question, duration: -1 }], 1);
    assert!(negative.requests.iter().all(|r| r.observations == 0 && !r.stale));
    let zero = urgency(&requests, &[ResponseTime { kind: RequestKind::Question, duration: 0 }], 1);
    assert!(!zero.learned_order);
    assert!(zero.requests.iter().all(|r| r.expected_wait == Some(0.0) && r.urgency.is_none()));
    assert!(zero.requests.iter().find(|r| r.agent == "future").is_some_and(|r| r.waited == 0 && !r.stale));
    let even = urgency(&requests, &[ResponseTime { kind: RequestKind::Question, duration: i64::MAX }, ResponseTime { kind: RequestKind::Question, duration: 1 }], 1);
    assert!(even.learned_order);
    assert!(even.requests.iter().all(|r| r.expected_wait.is_some_and(f64::is_finite) && r.urgency.is_some_and(f64::is_finite)));
    assert!(!even.requests.iter().any(|r| r.stale));
}
