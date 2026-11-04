//! Paired visits and exact rendered feedback, plan 01a199e1b177752e84dad5dc990e6a15.
//! A pair is committed before feedback. Missing frames remain censored, never guessed.
use std::collections::HashSet;
use kiln_ci::{attention::{Ranked,RelativeCorrection},attention_examination::{self as core,Exposure,Report,Selection}};
use rusqlite::{Connection,OptionalExtension as _,params};
use serde::{Deserialize,Serialize};
use serde_json::Value;

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct Visit{
    pub key:String,pub at:i64,pub order_a:Vec<String>,pub order_b:Vec<String>,pub arm:u8,
    pub ranks_a:Vec<Ranked>,pub ranks_b:Vec<Ranked>,pub report:Value,pub examination:Report,pub model_order:bool,
}
impl Visit{
    pub fn shown(&self)->&[String]{if self.arm==0{&self.order_a}else{&self.order_b}}
    pub fn ranks(&self)->&[Ranked]{if self.arm==0{&self.ranks_a}else{&self.ranks_b}}
}
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
pub struct Frame{
    pub context:String,pub order_a:Vec<String>,pub order_b:Vec<String>,pub shown:Vec<String>,pub model_order:bool,
}

pub fn visit(c:&Connection,person:&str,key:&str)->anyhow::Result<Option<Visit>>{
    let value:Option<String>=c.query_row("SELECT value FROM attention_visits WHERE principal=?1 AND key=?2",params![person,key],|r|r.get(0)).optional()?;
    value.map(|v|serde_json::from_str(&v).map_err(Into::into)).transpose()
}
pub fn save_visit(c:&Connection,person:&str,visit:&Visit)->anyhow::Result<Visit>{
    c.execute("INSERT OR IGNORE INTO attention_visits(principal,key,at,value) VALUES(?1,?2,?3,?4)",params![person,visit.key,visit.at,serde_json::to_string(visit)?])?;
    self::visit(c,person,&visit.key)?.ok_or_else(||anyhow::anyhow!("the visit was not retained"))
}
fn unique(ids:&[String])->bool{ids.iter().all(|id|!id.is_empty()) && ids.iter().collect::<HashSet<_>>().len()==ids.len()}

/// Movable rows must preserve both proposed relative orders. Identical fixed positions can
/// express pins/Needs/manual constraints; unknown/manual projections remain unlearnable.
fn valid_frame(visit:&Visit,frame:&Frame,allowed:&HashSet<String>)->bool{
    if frame.context.is_empty() || !unique(&frame.order_a) || !unique(&frame.order_b) || !unique(&frame.shown){return false;}
    let a:HashSet<_>=frame.order_a.iter().collect();let b:HashSet<_>=frame.order_b.iter().collect();
    let proposed:HashSet<_>=visit.order_a.iter().collect();
    if a!=b || a.iter().any(|id|!allowed.contains(*id) || !proposed.contains(*id)){return false;}
    let selected=if visit.arm==0{&frame.order_a}else{&frame.order_b};
    if &frame.shown!=selected{return false;}
    let moving:HashSet<_>=frame.order_a.iter().enumerate().filter(|(at,id)|frame.order_b.get(*at)!=Some(*id)).map(|(_,id)|id).collect();
    for (proposal,actual) in [(&visit.order_a,&frame.order_a),(&visit.order_b,&frame.order_b)]{
        let expected:Vec<_>=proposal.iter().filter(|id|moving.contains(id)).collect();
        let actual:Vec<_>=actual.iter().filter(|id|moving.contains(id)).collect();
        if expected!=actual{return false;}
    }
    true
}

pub fn render(c:&Connection,person:&str,key:&str,revision:i64,frame:&Frame,allowed:&HashSet<String>,now:i64)->anyhow::Result<bool>{
    let Some(visit)=visit(c,person,key)? else{return Ok(false);};
    if revision<0{return Ok(false);}
    let valid=valid_frame(&visit,frame,allowed);
    let value=serde_json::to_string(frame)?;
    let previous:Option<String>=c.query_row("SELECT value FROM attention_rendered WHERE principal=?1 AND exposure=?2 AND revision=?3",params![person,key,revision],|r|r.get(0)).optional()?;
    if previous.as_ref().is_some_and(|old|old!=&value){return Ok(false);}
    c.execute("INSERT OR IGNORE INTO attention_rendered(principal,exposure,revision,received_at,value,valid) VALUES(?1,?2,?3,?4,?5,?6)",params![person,key,revision,now,value,valid])?;
    reconcile(c,person,key)?;Ok(valid)
}

pub fn close(c:&Connection,person:&str,key:&str,revision:i64,now:i64)->anyhow::Result<bool>{
    if revision<0 || visit(c,person,key)?.is_none(){return Ok(false);}
    // First close is immutable; a real selected navigation may supersede cleanup below.
    c.execute("INSERT OR IGNORE INTO attention_visit_feedback(principal,exposure,final_revision,closed_at) VALUES(?1,?2,?3,?4)",params![person,key,revision,now])?;
    reconcile(c,person,key)?;Ok(true)
}

pub struct Pick<'a>{pub agent:&'a str,pub key:&'a str,pub at:i64,pub revision:i64,pub position:usize,pub source:&'a str,pub prior:&'a Report}
pub fn pick(c:&Connection,person:&str,exposure:&str,pick:&Pick<'_>)->anyhow::Result<bool>{
    if pick.revision<0 || pick.source!="list" || visit(c,person,exposure)?.is_none(){return Ok(false);}
    let Ok(position)=i64::try_from(pick.position) else{return Ok(false);};
    let previous:Option<(String,String)>=c.query_row("SELECT chosen,choice_key FROM attention_visit_feedback WHERE principal=?1 AND exposure=?2 AND chosen IS NOT NULL",params![person,exposure],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if previous.is_some(){return Ok(previous.is_some_and(|(id,key)|id==pick.agent && key==pick.key));}
    c.execute("INSERT INTO attention_visit_feedback(principal,exposure,final_revision,closed_at,chosen,choice_at,position,choice_key,source,prior_report) VALUES(?1,?2,?3,?4,?5,?4,?6,?7,?8,?9) ON CONFLICT(principal,exposure) DO UPDATE SET final_revision=excluded.final_revision,closed_at=MAX(closed_at,excluded.closed_at),chosen=excluded.chosen,choice_at=excluded.choice_at,position=excluded.position,choice_key=excluded.choice_key,source=excluded.source,prior_report=excluded.prior_report WHERE chosen IS NULL",params![person,exposure,pick.revision,pick.at,pick.agent,position,pick.key,pick.source,serde_json::to_string(pick.prior)?])?;
    reconcile(c,person,exposure)?;Ok(true)
}

type Feedback=(i64,i64,Option<String>,Option<i64>,Option<i64>,Option<String>,Option<String>,Option<String>);
fn feedback(c:&Connection,person:&str,key:&str)->anyhow::Result<Option<Feedback>>{
    Ok(c.query_row("SELECT final_revision,closed_at,chosen,choice_at,position,choice_key,source,prior_report FROM attention_visit_feedback WHERE principal=?1 AND exposure=?2",params![person,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?)
}
fn frame(c:&Connection,person:&str,key:&str,revision:i64)->anyhow::Result<Option<Frame>>{
    let json:Option<String>=c.query_row("SELECT value FROM attention_rendered WHERE principal=?1 AND exposure=?2 AND revision=?3 AND valid=1",params![person,key,revision],|r|r.get(0)).optional()?;
    json.map(|j|serde_json::from_str(&j).map_err(Into::into)).transpose()
}
fn reconcile(c:&Connection,person:&str,key:&str)->anyhow::Result<()>{
    let Some(visit)=visit(c,person,key)? else{return Ok(());};
    let Some((revision,_,Some(chosen),Some(at),Some(position),Some(choice_key),Some(source),prior))=feedback(c,person,key)? else{return Ok(());};
    let Some(frame)=frame(c,person,key,revision)? else{return Ok(());};
    let Ok(position)=usize::try_from(position) else{return Ok(());};
    if source!="list" || at<visit.at || frame.shown.get(position)!=Some(&chosen){return Ok(());}
    let report:Report=prior.map(|j|serde_json::from_str(&j)).transpose()?.unwrap_or_default();
    let correction:Option<RelativeCorrection>=if visit.model_order && frame.model_order{core::correction(&report,&frame.context,position.saturating_add(1))}else{None};
    let written=c.execute("INSERT OR IGNORE INTO attention_choices(principal,key,at,chosen,candidates,exposure,position,source,correction) VALUES(?1,?2,?3,?4,?5,?6,?7,'list',?8)",params![person,choice_key,at,chosen,serde_json::to_string(&frame.shown)?,key,i64::try_from(position)?,correction.map(|v|serde_json::to_string(&v)).transpose()?])?;
    let same=written>0 || c.query_row("SELECT EXISTS(SELECT 1 FROM attention_choices WHERE principal=?1 AND key=?2 AND source='list' AND exposure=?3 AND chosen=?4 AND at=?5 AND position=?6)",params![person,choice_key,key,chosen,at,i64::try_from(position)?],|r|r.get::<_,bool>(0))?;
    if same{c.execute("INSERT OR IGNORE INTO attention_choice_exposures(principal,choice_key,exposure,position) VALUES(?1,?2,?3,?4)",params![person,choice_key,key,i64::try_from(position)?])?;}
    Ok(())
}

pub fn prior_exposures(c:&Connection,person:&str,accessible:&HashSet<String>,current:Option<&str>,now:i64)->anyhow::Result<Vec<Exposure>>{
    let mut rows=exposures(c,person,accessible,now)?;rows.retain(|row|Some(row.id.as_str())!=current);Ok(rows)
}

pub fn exposures(c:&Connection,person:&str,accessible:&HashSet<String>,now:i64)->anyhow::Result<Vec<Exposure>>{
    let mut q=c.prepare("SELECT value FROM attention_visits WHERE principal=?1 ORDER BY at,key")?;
    let visits:Vec<Visit>=q.query_map([person],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?.into_iter().map(|j|serde_json::from_str(&j).map_err(Into::into)).collect::<anyhow::Result<_>>()?;
    let mut out=Vec::new();
    for visit in visits{
        if visit.order_a.iter().chain(&visit.order_b).any(|id|!accessible.contains(id)){continue;}
        let state=feedback(c,person,&visit.key)?;
        let revision=state.as_ref().map(|s|s.0).or(c.query_row("SELECT MAX(revision) FROM attention_rendered WHERE principal=?1 AND exposure=?2",params![person,visit.key],|r|r.get::<_,Option<i64>>(0))?);
        let rendered=revision.map(|revision|frame(c,person,&visit.key,revision)).transpose()?.flatten();
        let mut exposure=Exposure{id:visit.key.clone(),at:visit.at,closed_at:None,context:String::new(),order_a:visit.order_a.clone(),order_b:visit.order_b.clone(),shown:visit.shown().to_vec(),arm:visit.arm,choice:None,model_order:false};
        if let(Some(frame),Some((_,closed,chosen,choice_at,position,_,source,_)))=(rendered,state){
            if frame.order_a.iter().chain(&frame.order_b).any(|id|!accessible.contains(id)){continue;}
            exposure.context=frame.context;exposure.order_a=frame.order_a;exposure.order_b=frame.order_b;exposure.shown=frame.shown;
            exposure.model_order=visit.model_order && frame.model_order;
            if closed<=now{
                exposure.closed_at=Some(closed);
                if !chosen.as_ref().is_none_or(|agent|position.and_then(|p|usize::try_from(p).ok()).and_then(|p|exposure.shown.get(p))==Some(agent)){exposure.model_order=false;}
                if source.as_deref()==Some("list"){exposure.choice=chosen.zip(choice_at).map(|(agent,at)|Selection{agent,at});}
            }
        }else if let Some((revision,closed,_,_,_,_,_,_))=feedback(c,person,&visit.key)?{
            let present:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM attention_rendered WHERE principal=?1 AND exposure=?2 AND revision=?3)",params![person,visit.key,revision],|r|r.get(0))?;
            if present && closed<=now{exposure.closed_at=Some(closed);}
        }
        out.push(exposure);
    }
    Ok(out)
}

#[cfg(test)]
mod tests{
    use super::*;
    fn db()->anyhow::Result<Connection>{let c=Connection::open_in_memory()?;c.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261103-1800_w21_attention.sql"))?;c.execute_batch(include_str!("../../store/migrations/20261103-1900_w21_attention_examination.sql"))?;Ok(c)}
    fn pair()->Visit{Visit{key:"visit".into(),at:10,order_a:vec!["a".into(),"b".into()],order_b:vec!["b".into(),"a".into()],arm:0,ranks_a:Vec::new(),ranks_b:Vec::new(),report:Value::Null,examination:Report::default(),model_order:true}}
    fn frame()->Frame{Frame{context:"desktop".into(),order_a:vec!["a".into(),"b".into()],order_b:vec!["b".into(),"a".into()],shown:vec!["a".into(),"b".into()],model_order:true}}
    fn allowed()->HashSet<String>{["a".into(),"b".into()].into_iter().collect()}
    fn prior()->Report{Report{curves:vec![core::Curve{layout:"desktop".into(),reference:1,positions:vec![core::Position{position:1,relative:1.0,standard_error:None},core::Position{position:2,relative:0.5,standard_error:None}],opportunities:10,reason:"prior identified test curve".into()}],..Report::default()}}
    #[test]
    fn mc_attention_visit_pair_is_durable_idempotent_and_private()->anyhow::Result<()>{
        let c=db()?;let original=save_visit(&c,"person",&pair())?;
        let mut retry=pair();retry.arm=1;retry.at=20;
        assert_eq!(save_visit(&c,"person",&retry)?.arm,original.arm);
        assert!(visit(&c,"other","visit")?.is_none());
        assert!(!render(&c,"other","visit",1,&frame(),&allowed(),20)?);
        assert!(!close(&c,"other","visit",1,20)?);
        assert!(exposures(&c,"other",&allowed(),30)?.is_empty());Ok(())
    }
    #[test]
    fn mc_attention_close_pick_and_render_network_races_keep_exact_frame_and_prior_correction()->anyhow::Result<()>{
        let c=db()?;save_visit(&c,"person",&pair())?;
        close(&c,"person","visit",1,20)?;
        assert_eq!(core::fit(&exposures(&c,"person",&allowed(),21)?,21).censored,1);
        pick(&c,"person","visit",&Pick{agent:"b",key:"selection",at:21,revision:1,position:1,source:"list",prior:&prior()})?;
        assert_eq!(c.query_row("SELECT COUNT(*) FROM attention_choices",[],|r|r.get::<_,i64>(0))?,0,"missing render remains censored");
        render(&c,"person","visit",1,&frame(),&allowed(),22)?;
        let choices=super::super::store::choices(&c,"person")?;
        assert_eq!(c.query_row("SELECT exposure FROM attention_choice_exposures WHERE principal='person' AND choice_key='selection'",[],|r|r.get::<_,String>(0))?,"visit");
        assert_eq!(choices.len(),1);assert_eq!(choices.first().and_then(|c|c.correction.as_ref()).map(|c|c.examination),Some(0.5));
        // Later cleanup and revisions cannot overwrite selected navigation or its exact frame.
        close(&c,"person","visit",2,30)?;
        let mut next=frame();next.context="phone".into();render(&c,"person","visit",2,&next,&allowed(),31)?;
        let exposures=exposures(&c,"person",&allowed(),40)?;
        assert_eq!(exposures.first().map(|e|e.context.as_str()),Some("desktop"));
        assert_eq!(exposures.first().and_then(|e|e.choice.as_ref()).map(|c|c.agent.as_str()),Some("b"));
        assert_eq!(exposures.first().and_then(|e|e.closed_at),Some(21));
        assert_eq!(core::fit(&exposures,21).censored,1,"feedback at evaluation time is not past evidence");
        pick(&c,"person","visit",&Pick{agent:"b",key:"selection",at:40,revision:1,position:1,source:"list",prior:&Report::default()})?;
        assert_eq!(super::super::store::choices(&c,"person")?.len(),1);Ok(())
    }
    #[test]
    fn mc_attention_closed_no_clicks_count_but_unfinished_and_invalid_frames_do_not()->anyhow::Result<()>{
        let c=db()?;save_visit(&c,"person",&pair())?;render(&c,"person","visit",1,&frame(),&allowed(),11)?;
        assert_eq!(core::fit(&exposures(&c,"person",&allowed(),12)?,12).censored,1);
        close(&c,"person","visit",1,20)?;
        let completed=exposures(&c,"person",&allowed(),21)?;assert!(completed.first().is_some_and(|e|e.choice.is_none()));
        assert_eq!(core::fit(&completed,21).opportunities,2,"both non-click opportunities are recorded");
        let mut duplicate=frame();duplicate.order_a=vec!["a".into(),"a".into()];
        assert!(!render(&c,"person","visit",1,&duplicate,&allowed(),22)?);
        assert_eq!(core::fit(&exposures(&c,"person",&allowed(),23)?,23).exposures,1,"a later conflicting frame cannot rewrite committed data");
        let mut invalid=pair();invalid.key="invalid".into();save_visit(&c,"person",&invalid)?;
        assert!(!render(&c,"person","invalid",1,&duplicate,&allowed(),24)?);close(&c,"person","invalid",1,25)?;
        assert_eq!(core::fit(&exposures(&c,"person",&allowed(),26)?,26).ignored,1);Ok(())
    }
    #[test]
    fn mc_attention_invalid_position_and_foreign_choice_never_teach()->anyhow::Result<()>{
        let c=db()?;save_visit(&c,"person",&pair())?;render(&c,"person","visit",1,&frame(),&allowed(),11)?;
        assert!(!pick(&c,"other","visit",&Pick{agent:"a",key:"foreign",at:20,revision:1,position:0,source:"list",prior:&prior()})?);
        pick(&c,"person","visit",&Pick{agent:"a",key:"wrong-position",at:20,revision:1,position:1,source:"list",prior:&prior()})?;
        assert!(super::super::store::choices(&c,"person")?.is_empty());
        assert_eq!(core::fit(&exposures(&c,"person",&allowed(),21)?,21).ignored,1);Ok(())
    }
    #[test]
    fn mc_attention_raced_cleanup_cannot_supply_its_own_choice_correction()->anyhow::Result<()>{
        let c=db()?;save_visit(&c,"person",&pair())?;render(&c,"person","visit",1,&frame(),&allowed(),11)?;close(&c,"person","visit",1,20)?;
        let included=core::fit(&exposures(&c,"person",&allowed(),30)?,30);assert_eq!(included.opportunities,2);
        let prior=core::fit(&prior_exposures(&c,"person",&allowed(),Some("visit"),30)?,30);
        assert_eq!(prior,core::fit(&[],30),"exclude this closed episode even when its cleanup arrived before selection");
        pick(&c,"person","visit",&Pick{agent:"b",key:"late-selection",at:30,revision:1,position:1,source:"list",prior:&prior})?;
        assert!(super::super::store::choices(&c,"person")?.first().is_some_and(|choice|choice.correction.is_none()));
        assert_eq!(exposures(&c,"person",&allowed(),31)?.first().and_then(|e|e.choice.as_ref()).map(|c|c.agent.as_str()),Some("b"));Ok(())
    }
    #[test]
    fn mc_attention_historical_missing_position_stays_uncorrected()->anyhow::Result<()>{
        let c=db()?;c.execute("INSERT INTO attention_choices(principal,key,at,chosen,candidates) VALUES('person','legacy',1,'a','[\"a\",\"b\"]')",[])?;
        let choice=super::super::store::choices(&c,"person")?;
        assert!(choice.first().is_some_and(|c|c.correction.is_none() && c.propensity.is_none()));Ok(())
    }
}
