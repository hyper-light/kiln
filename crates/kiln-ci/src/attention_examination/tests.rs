use super::*;

fn example(layout:&str,arm:u8,index:usize,choice:Option<&str>)->Exposure{
    let a=vec!["a".into(),"fixed".into(),"b".into()];let b=vec!["b".into(),"fixed".into(),"a".into()];
    Exposure{id:format!("{layout}-{arm}-{index}"),at:10,closed_at:Some(20),context:layout.into(),shown:if arm==0{a.clone()}else{b.clone()},order_a:a,order_b:b,arm,choice:choice.map(|agent|Selection{agent:agent.into(),at:20}),model_order:true}
}

/// Exact expected counts under randomized arms, heterogeneous row relevance (a=.8,
/// b=.4), and examination (rank1=1, rank3=.25). No relevance model is supplied.
fn history(layout:&str,reverse:bool)->Vec<Exposure>{
    (0..2).flat_map(|arm|(0..100).map(move|index|{
        let choice=match(arm,reverse){
            (0,false)=>if index<80{Some("a")}else if index<90{Some("b")}else{None},
            (1,false)=>if index<40{Some("b")}else if index<60{Some("a")}else{None},
            (0,true)=>if index<16{Some("a")}else if index<48{Some("b")}else{None},
            _=>if index<8{Some("b")}else if index<72{Some("a")}else{None},
        };example(layout,arm,index,choice)
    })).collect()
}

#[test]
fn mc_attention_allpairs_recovers_relative_examination_and_reports_uncertainty(){
    let report=fit(&history("desktop",false),21);
    assert_eq!(report.exposures,200);assert_eq!(report.opportunities,400);
    assert_eq!(report.curves.len(),1,"{:?}",report.unavailable);
    let curve=report.curves.first();assert!(curve.is_some_and(|curve|curve.reference==1));
    let low=curve.and_then(|curve|curve.positions.iter().find(|position|position.position==3));
    assert!(low.is_some_and(|estimate|estimate.standard_error.is_some_and(|se|(estimate.relative-0.25).abs()<=se)));
    assert!(correction(&report,"desktop",2).is_none(),"a fixed row supplied no position contrast");
    assert!(correction(&report,"phone",3).is_none(),"layouts cannot borrow another layout's scale");
}

#[test]
fn mc_attention_allpairs_layouts_have_separate_data_derived_reference_ranks(){
    let mut exposures=history("desktop",false);exposures.extend(history("phone",true));
    let report=fit(&exposures,21);assert_eq!(report.curves.len(),2,"{:?}",report.unavailable);
    let desktop=correction(&report,"desktop",3);let phone=correction(&report,"phone",1);
    assert!(desktop.is_some_and(|estimate|estimate.reference==1 && (estimate.examination-0.25).abs()<f64::EPSILON.sqrt()));
    assert!(phone.is_some_and(|estimate|estimate.reference==3 && (estimate.examination-0.25).abs()<f64::EPSILON.sqrt()),"rank1 is not assumed most examined");
}

#[test]
fn mc_attention_allpairs_components_without_rank_one_remain_explicit(){
    let mut exposures=history("desktop",false);
    for exposure in &mut exposures{
        for order in [&mut exposure.order_a,&mut exposure.order_b,&mut exposure.shown]{order.insert(0,"prefix".into());}
    }
    let report=fit(&exposures,21);
    assert!(correction(&report,"desktop",1).is_none());
    assert!(correction(&report,"desktop",4).is_some_and(|estimate|estimate.reference==2));
    assert!(report.curves.first().is_some_and(|curve|curve.positions.iter().all(|position|matches!(position.position,2|4))));
}

#[test]
fn mc_attention_allpairs_disconnected_components_never_share_a_normalization_scale(){
    let mut exposures=history("desktop",false);let mut shifted=history("desktop",false);
    for exposure in &mut shifted{
        exposure.id=format!("second-{}",exposure.id);
        for order in [&mut exposure.order_a,&mut exposure.order_b,&mut exposure.shown]{order.insert(0,"prefix".into());}
    }
    exposures.extend(shifted);let report=fit(&exposures,21);
    assert_eq!(report.curves.len(),2,"{:?}",report.unavailable);
    assert!(correction(&report,"desktop",3).is_some_and(|estimate|estimate.reference==1));
    assert!(correction(&report,"desktop",4).is_some_and(|estimate|estimate.reference==2));
    let mut relabeled=exposures;
    for exposure in &mut relabeled{std::mem::swap(&mut exposure.order_a,&mut exposure.order_b);exposure.arm=1-exposure.arm;}
    assert_eq!(report,fit(&relabeled,21),"naming the two fair arms differently cannot alter the estimator");
}

#[test]
fn mc_attention_allpairs_pending_future_manual_and_changed_orders_are_not_labels(){
    let mut pending=example("desktop",0,0,None);pending.closed_at=None;
    let mut future=example("desktop",0,1,Some("a"));future.closed_at=Some(30);
    let mut manual=example("desktop",0,2,Some("a"));manual.model_order=false;
    let mut changed=example("desktop",0,3,Some("a"));changed.shown.reverse();
    let report=fit(&[pending,future,manual,changed],21);
    assert_eq!(report.censored,2);assert_eq!(report.ignored,2);assert_eq!(report.exposures,0);assert!(report.curves.is_empty());
    let current=fit(&[example("desktop",0,4,Some("a"))],20);
    assert_eq!(current.censored,1,"the current choice cannot estimate its own correction");
}

#[test]
fn mc_attention_allpairs_closed_visits_without_choices_are_observed_not_censored(){
    let report=fit(&[example("desktop",0,0,None),example("desktop",1,1,None)],21);
    assert_eq!(report.exposures,2);assert_eq!(report.censored,0);assert_eq!(report.opportunities,4);
    assert!(report.curves.is_empty(),"zero clicks cannot identify examination");assert!(!report.unavailable.is_empty());
}

#[test]
fn mc_attention_allpairs_missing_arm_boundary_and_invalid_choice_never_invent_probabilities(){
    let mut invalid=example("desktop",0,0,Some("outside"));invalid.id="invalid".into();
    let single_arm=vec![example("desktop",0,1,Some("a")),example("desktop",0,2,None)];
    let report=fit(&single_arm,21);assert!(report.curves.is_empty());
    // Each proposal moves two rows in opposite directions, so n at both ranks is
    // observed even with one arm; zero clicks at the lower rank puts the fit on a boundary.
    assert!(report.unavailable.iter().any(|item|item.reason.contains("boundary") || item.reason.contains("interior")));
    let report=fit(&[invalid],21);assert_eq!(report.ignored,1);assert!(report.curves.is_empty());
}
