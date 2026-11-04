use super::*;

fn usage(agent: &str, kind: UseKind, at: i64) -> Use {
    Use { agent: agent.into(), kind, at }
}

fn choice(at: i64, chosen: &str) -> Choice {
    Choice { at, chosen: chosen.into(), candidates: vec!["a".into(), "b".into()], propensity: None, correction: None }
}

#[test]
fn mc_attention_sparse_history_never_invents_a_model() {
    let uses = vec![usage("a", UseKind::Message, 10), usage("b", UseKind::Turn, 20)];
    let report = fit(&uses, &[choice(30, "b")]);
    assert!(report.model.is_none());
    assert!(!report.improves_baseline);
    assert!(report.unavailable_comparisons.iter().any(|c| c.name == "current_order" && c.reason.contains("not recorded")));
    let ranked = rank(None, &uses, &["b".into(), "a".into()], 40);
    assert_eq!(ranked.first().map(|r| r.agent.as_str()), Some("a"));
    assert!(ranked.iter().all(|r| r.score.is_none()));
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
    let requests = [
        request("middle", RequestKind::Tool, 200), request("new", RequestKind::Question, 300), request("old", RequestKind::Question, 100),
    ];
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
    let even = urgency(&requests, &[
        ResponseTime { kind: RequestKind::Question, duration: i64::MAX },
        ResponseTime { kind: RequestKind::Question, duration: 1 },
    ], 1);
    assert!(even.learned_order);
    assert!(even.requests.iter().all(|r| r.expected_wait.is_some_and(f64::is_finite) && r.urgency.is_some_and(f64::is_finite)));
    assert!(!even.requests.iter().any(|r| r.stale));
}

#[test]
fn mc_attention_both_turn_kinds_keep_work_visible_but_cannot_become_human_uses() {
    let model = Model { decay: 0.5, weights: BTreeMap::from([(UseKind::Message, 1.0), (UseKind::Turn, 0.3), (UseKind::SubagentTurn, 0.3)]), range_weight: None };
    let mut uses = vec![usage("a", UseKind::Message, 10), usage("b", UseKind::Message, 1)];
    let candidates = ["b".into(), "a".into()];
    assert_eq!(rank(Some(&model), &uses, &candidates, 1000).first().map(|r| r.agent.as_str()), Some("a"));
    uses.push(usage("b", UseKind::SubagentTurn, 999));
    assert_eq!(rank(Some(&model), &uses, &candidates, 1000).first().map(|r| r.agent.as_str()), Some("b"));
    assert_eq!(rank(None, &uses, &candidates, 1000).first().map(|r| r.agent.as_str()), Some("a"));
    uses.push(usage("b", UseKind::Turn, 999));
    let rows = rank(Some(&model), &uses, &candidates, 1000);
    assert!(rows.first().is_some_and(|r| r.reason.contains("1 agent turns") && r.reason.contains("1 subagent turns")));
}

#[test]
fn mc_attention_same_millisecond_and_future_choices_do_not_leak() {
    let uses = [usage("a", UseKind::Message, 1), usage("b", UseKind::Message, 2), usage("b", UseKind::Open, 10), usage("a", UseKind::Turn, i64::MAX)];
    let histories = history(&uses);
    let t = trace(histories.get("b").cloned().unwrap_or_default(), 10);
    assert_eq!(t.entries().count(), 1);
    let t = trace(histories.get("a").cloned().unwrap_or_default(), 10);
    assert_eq!(t.entries().count(), 1);
    let report = fit(&uses, &[choice(10, "b")]);
    assert_eq!(report.observations, 1);
}

#[test]
fn mc_attention_choice_traces_share_exact_sorted_history_with_independent_cutoffs(){
    let uses=[usage("a",UseKind::Turn,7),usage("a",UseKind::Message,2),usage("a",UseKind::SubagentTurn,3),usage("b",UseKind::Message,1)];
    let observations=prepare(&uses,&[choice(5,"a"),choice(10,"b")],None);
    let first=observations.first().and_then(|observation|observation.traces.first());
    let later=observations.get(1).and_then(|observation|observation.traces.first());
    assert!(first.is_some() && later.is_some());
    if let(Some(first),Some(later))=(first,later){
        assert!(Arc::ptr_eq(&first.history,&later.history),"each choice must share its agent's immutable event array");
        assert_eq!(first.history.len(),3,"no use was truncated to bound memory");
        assert_eq!(first.entries().collect::<Vec<_>>(),vec![(UseKind::Message,2),(UseKind::SubagentTurn,3)]);
        assert_eq!(later.entries().count(),3);
        let model=Model{decay:0.5,weights:BTreeMap::from([(UseKind::Message,1.0),(UseKind::SubagentTurn,0.3),(UseKind::Turn,0.4)]),range_weight:None};
        let expected=(1.0/3.0_f64.sqrt()+0.3/2.0_f64.sqrt()).ln();
        assert!(activation(&model,first).is_some_and(|value|(value-expected).abs()<=f64::EPSILON));
    }
}

#[test]
fn mc_attention_bad_exposure_and_randomness_are_values_not_crashes() {
    let uses = [usage("a", UseKind::Message, 1), usage("b", UseKind::Message, 2)];
    for propensity in [Some(0.0), Some(-1.0), Some(2.0), Some(f64::NAN), Some(f64::MIN_POSITIVE / 2.0)] {
        let mut c = choice(10, "b");
        c.propensity = propensity;
        let report = fit(&uses, &[c]);
        if propensity == Some(f64::MIN_POSITIVE / 2.0) { continue; }
        assert_eq!(report.ignored, 1);
        assert!(report.model.is_none());
    }
    assert_eq!(thompson(&uses, &[choice(10, "b")], &[]), Err(Error::Randomness));
    for u in [0.0, 1.0, -1.0, f64::INFINITY, f64::NAN] {
        assert_eq!(thompson(&uses, &[choice(10, "b")], &[u]), Err(Error::Randomness));
    }
    for decay in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        let model = Model { decay, weights: BTreeMap::from([(UseKind::Message, 1.0)]), range_weight: None };
        assert_eq!(validate(&model), Err(Error::Model));
    }
}

#[test]
fn mc_attention_observed_propensity_is_used_and_missing_history_is_not_invented() {
    let uses = [usage("a", UseKind::Message, 1), usage("b", UseKind::Message, 2)];
    let mut c = choice(10, "b");
    c.propensity = Some(0.25);
    let prepared = prepare(&uses, &[c], None);
    assert_eq!(prepared.first().map(|o| o.weight), Some(4.0));
    let mut c = choice(10, "b");
    c.candidates.push("unknown".into());
    let report = fit(&uses, &[c]);
    assert_eq!(report.ignored, 1);
    let mut c = choice(10, "b");
    c.candidates.push("a".into());
    assert!(prepare(&uses, &[c], None).is_empty());
}

/// Generate fractional-count observations from a known ACT-R process. Rather than rely on
/// a lucky random realization, each context is repeated for both choices with its exact
/// generating probability as likelihood weight. This tests recovery of the likelihood optimum.
fn synthetic() -> (Vec<Use>, Vec<Choice>, Vec<Observation>, Model) {
    let target = Model { decay: 0.7, weights: BTreeMap::from([(UseKind::Message, 1.0), (UseKind::Turn, 0.4)]), range_weight: None };
    let mut uses = Vec::new();
    let mut choices = Vec::new();
    for i in 0..24 {
        let at = 1000 + i * 1000;
        uses.push(usage("a", UseKind::Message, at - 10 - i * 3));
        uses.push(usage("b", UseKind::Message, at - 300 + i * 2));
        if i % 2 == 0 { uses.push(usage("b", UseKind::Turn, at - 1 - i)); }
        if i % 3 == 0 { uses.push(usage("a", UseKind::Turn, at - 120 - i)); }
        choices.push(choice(at, "a"));
    }
    let contexts = prepare(&uses, &choices, None);
    let mut observations = Vec::new();
    for o in contexts {
        let activations: Vec<_> = o.traces.iter().filter_map(|t| activation(&target, t)).collect();
        let z = logsum(activations.iter().copied()).unwrap_or_default();
        for (index, value) in activations.into_iter().enumerate() {
            let mut item = o.clone();
            item.chosen = index;
            item.weight = (value - z).exp();
            observations.push(item);
        }
    }
    (uses, choices, observations, target)
}

#[test]
fn mc_attention_likelihood_recovers_synthetic_weights_and_decay_with_uncertainty() {
    let (_, _, observations, target) = synthetic();
    let learned = maximum_likelihood(&observations, false);
    assert!(learned.is_some(), "finite, identified synthetic likelihood must fit");
    if let Some((model, uncertainty)) = learned {
        for estimate in &uncertainty {
            let actual = if estimate.name == "decay" { target.decay } else { *target.weights.get(&UseKind::Turn).unwrap_or(&0.0) };
            assert!((estimate.value - actual).abs() <= estimate.standard_error.unwrap_or_default(), "{estimate:?}, generating {actual}");
        }
        assert!((model.decay - target.decay).abs() < 0.001);
        assert!((model.weights.get(&UseKind::Turn).copied().unwrap_or_default() - 0.4).abs() < 0.001);
    }
}

#[test]
fn mc_attention_separated_likelihood_is_not_a_finite_fitted_decay(){
    let context=Observation{at:1000,traces:vec![
        trace(Arc::from([(UseKind::Message,999)]),1000),
        trace(Arc::from([(UseKind::Message,0)]),1000),
    ],chosen:0,weight:1.0,corrected:false};
    // Choosing age 1 over age 1000 every time has no finite likelihood maximum:
    // every larger decay improves it. A tiny gradient alone used to accept decay
    // about 3.11 here, although its dimensionless scoring step was still material.
    let separated=vec![context.clone();10];
    assert!(maximum_likelihood(&separated,false).is_none());
    // The same context has a finite optimum when both alternatives are chosen.
    let mut first=context.clone();first.weight=9.0;
    let mut second=context;second.chosen=1;
    let identified=maximum_likelihood(&[first,second],false);
    assert!(identified.is_some(),"a finite identified likelihood must remain learnable");
    if let Some((model,_))=identified{
        let expected=9.0_f64.ln()/1000.0_f64.ln();
        assert!((model.decay-expected).abs()<=f64::EPSILON.sqrt());
    }
}

#[test]
fn mc_attention_seeded_posterior_draw_is_reproducible_and_not_random_row_shuffle() {
    let (uses, _, observations, _) = synthetic();
    let choices: Vec<_> = observations.iter().enumerate().map(|(i, o)| {
        let chosen = if i % 3 == 0 { "b" } else { "a" };
        choice(o.at + i as i64, chosen)
    }).collect();
    let uniforms: Vec<_> = choices.iter().enumerate().map(|(i, _)| (i as f64 + 1.0) / (choices.len() as f64 + 1.0)).collect();
    let a = thompson(&uses, &choices, &uniforms);
    let b = thompson(&uses, &choices, &uniforms);
    assert_eq!(a, b);
    let report = a.ok();
    assert!(report.is_some());
    assert!(report.is_some_and(|r| !r.reason.is_empty() && r.observations > 0));
}

#[test]
fn mc_attention_unknown_activity_kind_cannot_enter_the_model() {
    // Type-level exclusion is also enforced on persisted JSON: status polls, scripts, shell
    // completions and monitors cannot masquerade as agent turns through deserialization.
    for kind in ["shell", "monitor", "status", "usage", "script", "heartbeat"] {
        assert!(serde_json::from_str::<UseKind>(&format!("\"{kind}\"")).is_err());
    }
}

#[test]
fn mc_attention_park_and_archive_are_exclusion_feedback_not_positive_interest() {
    let model = Model { decay: 0.5, weights: BTreeMap::from([(UseKind::Message, 1.0), (UseKind::Park, 1.0), (UseKind::Archive, 1.0)]), range_weight: None };
    let before = vec![usage("a", UseKind::Message, 10), usage("b", UseKind::Message, 1)];
    let mut after = before.clone();
    after.extend([usage("b", UseKind::Park, 998), usage("b", UseKind::Archive, 999)]);
    for model in [Some(&model), None] {
        assert_eq!(rank(model, &before, &["a".into(), "b".into()], 1000), rank(model, &after, &["a".into(), "b".into()], 1000));
    }
}

#[test]
fn mc_attention_relative_corrections_preserve_each_layout_reference_mass(){
    let uses=[usage("a",UseKind::Message,1),usage("b",UseKind::Message,2)];
    let mut choices=vec![choice(10,"a"),choice(20,"b"),choice(30,"a"),choice(40,"b"),choice(50,"a")];
    for(item,(layout,reference,examination))in choices.iter_mut().zip([("desktop",1,1.0),("desktop",1,0.25),("phone",3,1.0),("phone",3,0.5)]){
        item.correction=Some(RelativeCorrection{layout:layout.into(),reference,examination});
    }
    let observed=prepare(&uses,&choices,None);
    let weights:Vec<_>=observed.iter().map(|observation|observation.weight).collect();
    let expected=[0.4,1.6,2.0/3.0,4.0/3.0,1.0];
    assert!(weights.iter().zip(expected).all(|(weight,expected)|(weight-expected).abs()<=f64::EPSILON));
    let training=prepare_subset(&uses,&choices,None,|at|at<30);
    let isolated=prepare(&uses,&choices.into_iter().take(2).collect::<Vec<_>>(),None);
    assert_eq!(training.iter().map(|observation|observation.weight).collect::<Vec<_>>(),isolated.iter().map(|observation|observation.weight).collect::<Vec<_>>(),"later layout corrections cannot alter earlier normalization");
}

#[test]
fn mc_attention_invalid_relative_correction_is_not_an_absolute_propensity(){
    let uses=[usage("a",UseKind::Message,1),usage("b",UseKind::Message,2)];
    for examination in [0.0,-1.0,2.0,f64::NAN,f64::INFINITY]{
        let mut chosen=choice(10,"a");chosen.correction=Some(RelativeCorrection{layout:"desktop".into(),reference:1,examination});
        assert!(prepare(&uses,&[chosen],None).is_empty());
    }
    let mut chosen=choice(10,"a");chosen.propensity=Some(0.5);chosen.correction=Some(RelativeCorrection{layout:"desktop".into(),reference:1,examination:0.5});
    assert!(prepare(&uses,&[chosen],None).is_empty(),"two correction conventions cannot silently compound");
}
