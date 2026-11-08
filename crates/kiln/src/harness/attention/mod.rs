//! Mission Control attention: plan 01a199e1b177752e84dad5dc990e6a15.
//! The person owns placement; an archive affects the session and requires command rights.
mod store;
mod requests;
mod examination;
pub use requests::Response as HumanResponse;
pub use requests::{approval_key,bind as bind_response};

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock};
use parking_lot::Mutex;

use http::{Method, StatusCode};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::authz::{Caller, Scope};
use crate::authz::gate::Kind;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use super::model::{State, Status};
use super::session::Info;

pub use store::{Placement, Settings, DeferredRequest};
type Fail=(StatusCode,String);
fn failed(e: impl std::fmt::Display)->Fail{(StatusCode::INTERNAL_SERVER_ERROR,e.to_string())}

pub fn person(caller:&Caller)->Option<String>{
    if caller.agent.is_some() || matches!(caller.kind,Kind::Agent|Kind::Job|Kind::Peer){return None;}
    match (caller.kind,&caller.user){(Kind::Owner,_)=>Some("owner".into()),(_,Some(u))=>Some(format!("user:{u}")),_ if caller.is_owner()=>Some("owner".into()),_=>None}
}

#[derive(Clone)]
struct RowLineage{parent:Option<String>,subagent:bool}
// Retain the small observed identity relation across a successful command that removes
// its target. Current visibility is always checked; this is never an authorization cache.
static ROW_LINEAGE:LazyLock<Mutex<BTreeMap<String,RowLineage>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn remember_lineage(info:&Info){ROW_LINEAGE.lock().insert(info.id.clone(),RowLineage{parent:info.parent.clone(),subagent:info.subagent});}
fn display_row(id:&str,visible:&[Info],known:&BTreeMap<String,RowLineage>)->String{
    let ids:HashSet<_>=visible.iter().map(|i|i.id.as_str()).collect();let mut row=id.to_owned();let mut seen=HashSet::new();
    while seen.insert(row.clone()){
        let relation=visible.iter().find(|i|i.id==row).map(|i|RowLineage{parent:i.parent.clone(),subagent:i.subagent}).or_else(||known.get(&row).cloned());
        let Some(RowLineage{parent:Some(parent),subagent:true})=relation else{break;};
        if !ids.contains(parent.as_str()){break;}row=parent;
    }row
}
fn row_event_key(actual:&str,row:&str,kind:&str,key:&str)->String{if actual==row{key.to_owned()}else{crate::util::derive_id(&["row-use",actual,kind,key])}}
fn row_candidates(all:&[Info])->Vec<String>{let ids:HashSet<_>=all.iter().map(|i|i.id.as_str()).collect();all.iter().filter(|i|i.helper.is_none() && !(i.subagent && i.parent.as_deref().is_some_and(|p|ids.contains(p)))).map(|i|i.id.clone()).collect()}

/// Commands are recorded at the authenticated boundary, not guessed from user-role text.
/// A retry carries the same key and therefore creates no additional interest.
pub async fn record_person(hub:&Hub,caller:&Caller,id:&str,kind:&str,key:&str,at:i64)->anyhow::Result<()>{
    let Some(who)=person(caller) else{return Ok(())};
    let visible=super::lineage::seen(hub,caller,hub.harness.list()).await;
    let row=display_row(id,&visible,&ROW_LINEAGE.lock());
    let candidates=if matches!(kind,"message"|"answer"|"command"){row_candidates(&visible)}else{Vec::new()};
    let key=row_event_key(id,&row,kind,key);
    let (id,kind)=(row,kind.to_owned());
    hub.store.call(move|c|{
        let fresh=store::record(c,&who,&id,&kind,&key,at)?;
        if fresh && candidates.contains(&id){
            let choice_key=crate::util::derive_id(&["attention-choice",&who,&id,&kind,&key]);
            c.execute("INSERT OR IGNORE INTO attention_choices(principal,key,at,chosen,candidates,source) VALUES(?1,?2,?3,?4,?5,'command')",params![who,choice_key,at,id,serde_json::to_string(&candidates)?])?;
        }
        if fresh && matches!(kind.as_str(),"message"|"answer"|"restore"){
            let p=store::placement(c,&who,&id)?;
            if p.parked_until.is_some(){let next=Placement{parked_until:None,parked_at:None,deferred_requests:None,reason:Some("You messaged it".into()),..p.clone()};store::put(c,&who,&id,&p,&next,false)?;}
        }
        if fresh{signal();}
        Ok(())
    }).await
}

// Detached-task completion changes automatic quiet eligibility without creating a turn
// or a use. Wake maintenance only when that predicate changes, never on every heartbeat.
static QUIET_STATES:LazyLock<Mutex<BTreeMap<String,bool>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn quiet_transition(info:&Info)->bool{let now=quiet(info);QUIET_STATES.lock().insert(info.id.clone(),now).is_some_and(|before|before!=now)}

/// Called after the mirror lock is released. Polling an unchanged state adds no event.
pub fn observe(hub:&Hub,info:&Info){
    remember_lineage(info);
    if quiet_transition(info){signal();}
    let mut row=info.id.clone();
    let mut current=info.clone();
    let mut visited=HashSet::new();
    while current.subagent && visited.insert(current.id.clone()){
        let Some(parent)=current.parent.as_deref().and_then(|p|hub.harness.info(p)) else{break;};
        row=parent.id.clone();current=parent;
    }
    let kind=if row==info.id{"turn"}else{"subagent_turn"};
    let mut uses=Vec::new();
    if let Some(at)=info.turn_started{uses.push((row.clone(),kind.to_owned(),format!("{}:start:{at}",info.id),at));}
    if let Some(end)=&info.last_turn{uses.push((row.clone(),kind.to_owned(),format!("{}:end:{}",info.id,end.at),end.at));}
    for task in &info.subagents{
        if task.started>0{uses.push((row.clone(),"subagent_turn".into(),format!("{}:{}:start:{}",info.id,task.id,task.started),task.started));}
        if let Some(at)=task.ended{uses.push((row.clone(),"subagent_turn".into(),format!("{}:{}:end:{at}",info.id,task.id),at));}
    }
    if info.status==Status::Waiting || !info.approvals.is_empty() || info.held.is_some() || (info.status==Status::Exited && info.exit.as_ref().is_some_and(|e|e.error.is_some())){signal();}
    let pending=requests::pending(info);
    let id=info.id.clone();
    let revision=crate::util::new_id();
    let observed_at=crate::util::now_ms();
    let db=hub.store.clone();
    drop(crate::rt::spawn(async move{
        match db.call(move|c|{for(agent,kind,key,at)in uses{if store::record(c,"",&agent,&kind,&key,at)?{signal();}}requests::observe(c,&id,&row,&pending,&revision,observed_at)}).await{
            Ok(people)=>queue_request_refresh(people),
            Err(e)=>tracing::warn!(error=%format!("{e:#}"),"attention activity wasn't recorded"),
        }
    }));
}

/// Only explicit, accepted human responses teach request response times.
pub async fn record_response(hub:&Hub,caller:&Caller,id:&str,key:&str,response:HumanResponse,at:i64)->anyhow::Result<()>{
    let Some(who)=person(caller) else{return Ok(())};
    let id=id.to_owned();let key=key.to_owned();
    let people=hub.store.call(move|c|requests::response(c,&who,&id,&key,&response,at)).await?;
    for principal in people{hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
    Ok(())
}

/// Historical alternatives are immutable. Dropping an inaccessible alternative would
/// manufacture an easier choice and let present-day visibility rewrite the model.
fn scoped_history(uses:&mut Vec<kiln_ci::attention::Use>,choices:&mut Vec<kiln_ci::attention::Choice>,accessible:&HashSet<String>,at:i64)->usize{
    uses.retain(|u|accessible.contains(&u.agent) && u.at<=at);
    let mut omitted=0;
    choices.retain(|c|{if c.at>at{return false;}let available=accessible.contains(&c.chosen) && c.candidates.iter().all(|id|accessible.contains(id));if !available{omitted+=1;}available});
    omitted
}
fn report_omissions(report:&mut kiln_ci::attention::FitReport,omitted:usize){
    if omitted>0{report.ignored=report.ignored.saturating_add(omitted);report.reason.push_str(&format!(" {omitted} historical choices were omitted because at least one alternative is no longer accessible; their original alternatives were retained."));}
}

#[derive(Debug,Clone,Serialize,Deserialize)]
struct Archived {info:Info,state:Option<State>,#[serde(default)]status:Option<String>,#[serde(default)]group:Option<String>,#[serde(default)]reason:Option<String>}
#[derive(Debug,Clone,Deserialize)]
struct Action{
    action:String,
    agent:Option<String>,
    until:Option<i64>,
    revision:Option<i64>,
    settings:Option<Value>,
    #[serde(default, alias = "order")]
    agents:Vec<String>,
    exposure:Option<String>,
    key:Option<String>,
    mode:Option<String>,visit:Option<String>,frame_revision:Option<i64>,context:Option<String>,
    #[serde(default)]order_a:Vec<String>,#[serde(default)]order_b:Vec<String>,#[serde(default)]shown:Vec<String>,
    model_order:Option<bool>,chosen_position:Option<usize>,source:Option<String>,
}

fn visible_archive(caller:&Caller,owner:&str)->bool{
    caller.is_owner() || caller.can("agents.manage",&Scope::Server) || person(caller).as_deref()==Some(owner)
}

async fn archive_rows(hub:&Hub,caller:&Caller)->Result<Vec<(String,Archived)>,Fail>{
    let rows:Vec<(String,String,String,String)>=hub.store.call(|c|{
        let mut q=c.prepare("SELECT agent,principal,value,status FROM attention_archive ORDER BY at DESC,agent")?;
        Ok(q.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?)
    }).await.map_err(failed)?;
    rows.into_iter().filter(|(_,owner,_,_)|visible_archive(caller,owner)).map(|(id,_,j,status)|serde_json::from_str::<Archived>(&j).map(|mut a|{a.status=Some(status);(id,a)}).map_err(failed)).collect()
}

/// A snapshot is a visit boundary: the UI keeps this ordering until the next visit or own action.
#[derive(Clone)]
enum SnapshotMode{Arrival(String),Refresh(Option<String>)}
static VISIT_LOCKS:LazyLock<Mutex<BTreeMap<String,Arc<futures::lock::Mutex<()>>>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn visit_lock(person:&str)->Arc<futures::lock::Mutex<()>>{Arc::clone(VISIT_LOCKS.lock().entry(person.to_owned()).or_insert_with(||Arc::new(futures::lock::Mutex::new(()))))}
#[cfg(test)]
async fn snapshot(hub:&Arc<Hub>,caller:&Caller)->Result<Value,Fail>{snapshot_mode(hub,caller,SnapshotMode::Arrival(crate::util::new_id())).await}
async fn snapshot_mode(hub:&Arc<Hub>,caller:&Caller,mode:SnapshotMode)->Result<Value,Fail>{
    let who=person(caller).ok_or((StatusCode::FORBIDDEN,"attention belongs to a person".into()))?;
    let now=crate::util::now_ms();let observed_revision=crate::util::new_id();
    let all=super::lineage::seen(hub,caller,hub.harness.list()).await;
    backfill(hub,caller,&all).await.map_err(failed)?;
    let visible:HashSet<String>=all.iter().map(|a|a.id.clone()).collect();
    let candidates=row_candidates(&all);
    let saved=archive_rows(hub,caller).await?;
    let mut accessible=visible.clone();accessible.extend(saved.iter().map(|(id,_)|id.clone()));
    let p=who.clone();let settings=hub.store.call(move|c|store::settings(c,&p)).await.map_err(failed)?;
    let mut placements=refresh_placements(hub,&who,&all,&settings,now).await.map_err(failed)?;
    placements.retain(|id,_|visible.contains(id));
    let archive_ids:HashSet<String>=saved.iter().map(|(id,_)|id.clone()).collect();
    let candidates:Vec<String>=candidates.into_iter().filter(|id|!archive_ids.contains(id)).collect();
    let key=match &mode{SnapshotMode::Arrival(key)=>Some(key.clone()),SnapshotMode::Refresh(key)=>key.clone()};
    let operation=visit_lock(&who);
    let _held=if matches!(&mode,SnapshotMode::Arrival(_)){Some(operation.lock().await)}else{None};
    let cached=if let Some(key)=&key{let(p,key)=(who.clone(),key.clone());hub.store.call(move|c|examination::visit(c,&p,&key)).await.map_err(failed)?}else{None};
    let visit=if cached.is_some() || matches!(&mode,SnapshotMode::Refresh(_)){cached}else{
        let p=who.clone();let a=accessible.clone();
        let(mut uses,mut choices,exposures)=hub.store.call(move|c|Ok((store::uses(c,&p)?,store::choices(c,&p)?,examination::exposures(c,&p,&a,now)?))).await.map_err(failed)?;
        let omitted=scoped_history(&mut uses,&mut choices,&accessible,now);
        let rank_ids=candidates.clone();let learned=settings.sort=="learned";let fit_person=who.clone();let key=key.clone().unwrap_or_else(crate::util::new_id);
        let visit=crate::rt::blocking(move||{
            use rand::distr::{Distribution as _,Open01};
            let mut report=fitted(&fit_person,&uses,&choices);report_omissions(&mut report,omitted);
            let validated=report.model.as_ref().filter(|_|learned && report.improves_baseline);
            let draw=||{let uniforms:Vec<f64>=choices.iter().map(|_|Open01.sample(&mut rand::rng())).collect();validated.and_then(|m|kiln_ci::attention::sample_model(&uses,&choices,&uniforms,m.range_weight.is_some()).ok().flatten())};
            let a=draw();let b=draw();
            let ranks_a=kiln_ci::attention::rank(a.as_ref().or(validated),&uses,&rank_ids,now);
            let ranks_b=kiln_ci::attention::rank(b.as_ref().or(validated),&uses,&rank_ids,now);
            let model_order=validated.is_some();
            examination::Visit{key,at:now,order_a:ranks_a.iter().map(|r|r.agent.clone()).collect(),order_b:ranks_b.iter().map(|r|r.agent.clone()).collect(),arm:u8::from(rand::random::<bool>()),ranks_a,ranks_b,report:serde_json::to_value(report).unwrap_or(Value::Null),examination:kiln_ci::attention_examination::fit(&exposures,now),model_order}
        }).await.map_err(failed)?;
        let p=who.clone();Some(hub.store.call(move|c|examination::save_visit(c,&p,&visit)).await.map_err(failed)?)
    };
    let mut ranks=visit.as_ref().map(|v|v.ranks().to_vec()).unwrap_or_default();
    ranks.retain(|r|candidates.contains(&r.agent));
    for id in &candidates{if !ranks.iter().any(|r|&r.agent==id){ranks.push(kiln_ci::attention::Ranked{agent:id.clone(),score:None,reason:"Added during this visit".into()});}}
    let model=visit.as_ref().map(|v|v.report.clone()).unwrap_or(Value::Null);
    let mut agents:BTreeMap<String,Value>=placements.into_iter().map(|(id,p)|Ok((id,serde_json::to_value(p)?))).collect::<anyhow::Result<_>>().map_err(failed)?;
    let order:Vec<String>=ranks.iter().map(|r|r.agent.clone()).collect();
    for r in ranks{
        let value=agents.entry(r.agent).or_insert_with(||json!({"revision":0}));
        if let Some(o)=value.as_object_mut(){o.insert("rank".into(),json!(r.score));o.insert("rank_reason".into(),json!(r.reason));}
    }
    let (request_order,urgency)=request_snapshot(hub,&who,&all,&accessible,&order,&mut agents,(&observed_revision,now)).await.map_err(failed)?;
    let archived:Vec<Info>=saved.into_iter().map(|(id,a)|{if let Some(o)=agents.entry(id).or_insert_with(||json!({"revision":0})).as_object_mut(){o.insert("archived".into(),Value::Bool(true));o.insert("archive_status".into(),json!(a.status));if let Some(reason)=a.reason{o.insert("reason".into(),json!(reason));}}a.info}).collect();
    let pair=visit.as_ref().map(|v|json!({"key":v.key,"order_a":v.order_a.iter().filter(|id|accessible.contains(*id)).collect::<Vec<_>>(),"order_b":v.order_b.iter().filter(|id|accessible.contains(*id)).collect::<Vec<_>>(),"arm":v.arm,"model_order":v.model_order}));
    Ok(json!({"you":who,"agents":agents,"order":order,"settings":settings,"archived":archived,"model":model,"exposure":visit.as_ref().map(|v|&v.key),"pair":pair,"examination":visit.as_ref().map(|v|&v.examination),"request_order":request_order,"urgency":urgency}))
}

/// Keep urgency comparable across the whole Needs you section. Multiple true subagents
/// contribute requests to their parent's row, while commands still answer the actual owner.
async fn request_snapshot(hub:&Hub,who:&str,all:&[Info],accessible:&HashSet<String>,baseline:&[String],agents:&mut BTreeMap<String,Value>,observed:(&str,i64))->anyhow::Result<(Vec<String>,kiln_ci::attention::UrgencyReport)>{
    let (revision,now)=observed;
    let records:Vec<_>=all.iter().map(|info|{
        let mut row=info;let mut seen=HashSet::new();
        while row.subagent && seen.insert(row.id.as_str()){let Some(parent)=row.parent.as_ref().and_then(|p|all.iter().find(|i|&i.id==p)) else{break;};row=parent;}
        (info.id.clone(),row.id.clone(),requests::pending(info))
    }).collect();
    let principal=who.to_owned();let accessible=accessible.clone();let revision=revision.to_owned();
    let(open,responses,people)=hub.store.call(move|c|{
        let mut people=Vec::new();
        for(agent,row,pending)in records{people.extend(requests::observe(c,&agent,&row,&pending,&revision,now)?);}
        let(open,responses)=requests::history(c,&principal,&accessible,now)?;people.sort();people.dedup();Ok((open,responses,people))
    }).await?;
    for principal in people{hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
    let visible:HashSet<String>=baseline.iter().cloned().collect();
    let waits=requests::waits(&open,&visible);
    let mut report=kiln_ci::attention::urgency(&waits,&responses,now);
    let unknown:HashSet<String>=open.iter().filter(|r|visible.contains(&r.row) && r.at.is_none()).map(|r|r.row.clone()).collect();
    if !unknown.is_empty(){
        // A timestamp-free request cannot be compared with elapsed-time ratios. Keep
        // known requests oldest-first and leave unknown requests in the visit's order.
        report.requests.sort_by(|a,b|a.at.cmp(&b.at).then_with(||a.agent.cmp(&b.agent)).then_with(||a.kind.cmp(&b.kind)));
        report.learned_order=false;report.reason="Some requests have no supplied request time. Timed requests are oldest first; untimed requests retain the visit's order.".into();
    }
    let mut order=Vec::new();let mut included=HashSet::new();
    for request in &report.requests{if included.insert(request.agent.clone()){order.push(request.agent.clone());}}
    for id in baseline.iter().filter(|id|unknown.contains(*id)){if included.insert(id.clone()){order.push(id.clone());}}
    for id in &order{
        let first=report.requests.iter().find(|r|&r.agent==id);
        let stale=report.requests.iter().any(|r|&r.agent==id && r.stale);
        let data=if let Some(r)=first{json!({"score":r.urgency,"stale":stale,"known":r.expected_wait.is_some(),"since":r.at,"waited":r.waited,"expected_wait":r.expected_wait,"observations":r.observations,"reason":report.reason})}else{json!({"score":null,"stale":false,"known":false,"since":null,"waited":null,"expected_wait":null,"observations":0,"reason":"The provider did not supply a request time."})};
        if let Some(o)=agents.entry(id.clone()).or_insert_with(||json!({"revision":0})).as_object_mut(){o.insert("urgency".into(),data);}
    }
    Ok((order,report))
}

/// Shells and monitors may be quiet, but a Finished-only auto-park never hides running work.
fn quiet_row(info:&Info,all:&[Info])->bool{
    let family=super::lineage::subagents_under(all,&info.id);
    quiet(info) && all.iter().filter(|i|family.contains(&i.id)).all(quiet)
}

fn quiet(info:&Info)->bool{
    info.status==Status::Idle && info.approvals.is_empty() && info.held.is_none() && info.turn_started.is_none() && info.tasks_running==0 && info.left_behind==0 && !info.subagents.iter().any(|s|super::model::live_task(&s.status))
}

pub async fn route(hub:&Arc<Hub>,caller:&Caller,method:&Method,rest:&[&str],req:Request)->Result<Response,Fail>{
    let who=person(caller).ok_or((StatusCode::FORBIDDEN,"attention belongs to a person".into()))?;
    let receipt_at=crate::util::now_ms();
    if method==Method::GET && rest==["replay"]{return replay(hub,caller,&req).await.map(|value|srv::json(StatusCode::OK,&value));}
    if method==Method::GET && rest==["uses"]{
        let mut visible:HashSet<String>=super::lineage::seen(hub,caller,hub.harness.list()).await.into_iter().map(|a|a.id).collect();
        visible.extend(archive_rows(hub,caller).await?.into_iter().map(|(id,_)|id));
        let p=who.clone();let mut uses=hub.store.call(move|c|store::uses(c,&p)).await.map_err(failed)?;uses.retain(|u|visible.contains(&u.agent));
        return Ok(srv::json(StatusCode::OK,&json!({"you":who,"uses":uses})));
    }
    if !rest.is_empty(){return Err((StatusCode::NOT_FOUND,"no such attention route".into()));}
    if method==Method::GET{
        let query=req.uri().query().unwrap_or_default();let field=|name:&str|query.split('&').find_map(|part|part.strip_prefix(&format!("{name}="))).filter(|v|!v.is_empty()).map(str::to_owned);
        let visit=field("visit");if visit.as_ref().is_some_and(|v|v.len()>super::ops::KEY_MAX || v.chars().any(char::is_control)){return Err((StatusCode::BAD_REQUEST,"invalid visit key".into()));}
        let mode=if field("mode").as_deref()==Some("refresh"){SnapshotMode::Refresh(visit)}else{SnapshotMode::Arrival(visit.unwrap_or_else(crate::util::new_id))};
        return Ok(srv::json(StatusCode::OK,&snapshot_mode(hub,caller,mode).await?));
    }
    if method!=Method::POST{return Err((StatusCode::METHOD_NOT_ALLOWED,"use GET or POST".into()));}
    // Match the existing API JSON body allowance; three rendered orders can exceed64KiB.
    let action:Action=srv::body_json(req.into_body(),4<<20).await.map_err(|e|(StatusCode::BAD_REQUEST,e.to_string()))?;
    let now=receipt_at;
    if [&action.visit,&action.exposure,&action.key].into_iter().flatten().any(|key|key.is_empty() || key.len()>super::ops::KEY_MAX || key.chars().any(char::is_control)){return Err((StatusCode::BAD_REQUEST,"invalid attention key".into()));}
    let result_mode=if action.mode.as_deref()==Some("arrival") || (action.mode.is_none() && !matches!(action.action.as_str(),"visit"|"seen"|"render"|"close")){SnapshotMode::Arrival(action.visit.clone().unwrap_or_else(crate::util::new_id))}else{SnapshotMode::Refresh(action.visit.clone())};
    if matches!(action.action.as_str(),"render"|"close"){
        let exposure=action.exposure.as_deref().ok_or((StatusCode::BAD_REQUEST,"exposure is required".into()))?.to_owned();
        let revision=action.frame_revision.ok_or((StatusCode::BAD_REQUEST,"frame_revision is required".into()))?;
        let p=who.clone();
        let accepted=if action.action=="close"{hub.store.call(move|c|examination::close(c,&p,&exposure,revision,now)).await.map_err(failed)?}else{
            let allowed:HashSet<String>=super::lineage::seen(hub,caller,hub.harness.list()).await.into_iter().map(|i|i.id).collect();
            let frame=examination::Frame{context:action.context.unwrap_or_default(),order_a:action.order_a,order_b:action.order_b,shown:action.shown,model_order:action.model_order.unwrap_or(false)};
            hub.store.call(move|c|examination::render(c,&p,&exposure,revision,&frame,&allowed,now)).await.map_err(failed)?
        };
        return Ok(srv::json(StatusCode::OK,&json!({"ok":true,"accepted":accepted})));
    }
    let action_key=action.key.clone().unwrap_or_else(crate::util::new_id);
    let mut undo=None;
    if action.action=="settings"{
        let p=who.clone();let patch=action.settings.ok_or((StatusCode::BAD_REQUEST,"settings are required".into()))?;
        if patch.get("auto_archive").and_then(Value::as_bool)==Some(true) && caller.opener().as_deref()!=Some(who.as_str()){
            return Err((StatusCode::FORBIDDEN,"sign in as yourself to enable automatic archive; a restricted token cannot delegate broader rights".into()));
        }
        hub.store.call(move|c|{let mut j=serde_json::to_value(store::settings(c,&p)?)?;let o=j.as_object_mut().ok_or_else(||anyhow::anyhow!("invalid saved settings"))?;for(k,v)in patch.as_object().ok_or_else(||anyhow::anyhow!("settings must be an object"))?{o.insert(k.clone(),v.clone());}let settings:Settings=serde_json::from_value(j)?;store::save_settings(c,&p,&settings)}).await.map_err(|e|(StatusCode::BAD_REQUEST,e.to_string()))?;
    }else if action.action=="reorder"{
        let mut unique=HashSet::new();
        for id in &action.agents{super::lineage::may_see(hub,caller,id).await?;if !unique.insert(id){return Err((StatusCode::BAD_REQUEST,"pin order contains a duplicate".into()));}}
        let p=who.clone();let ids=action.agents;let key=action_key.clone();
        hub.store.call(move|c|{let tx=c.unchecked_transaction()?;for(i,id)in ids.iter().enumerate(){let before=store::placement(&tx,&p,id)?;anyhow::ensure!(before.pinned.is_some(),"only pinned agents can be reordered");let after=Placement{pinned:Some(i64::try_from(i)?.saturating_add(1)),..before.clone()};store::put(&tx,&p,id,&before,&after,false)?;store::record(&tx,&p,id,"command",&format!("reorder:{key}"),now)?;}tx.commit()?;Ok(())}).await.map_err(|e|(StatusCode::CONFLICT,e.to_string()))?;
    }else{
        let id=action.agent.as_deref().filter(|id|!id.is_empty()).ok_or((StatusCode::BAD_REQUEST,"agent is required".into()))?;
        if matches!(action.action.as_str(),"restore"|"restore_copy"){restore(hub,caller,id,action.action=="restore_copy").await?;super::attention_hooks::record(hub,caller,id,"restore",&action_key,now).await;}
        else if action.action!="archive"{super::lineage::may_see(hub,caller,id).await?;}
        if action.action=="archive"{archive(hub,caller,id).await?;super::attention_hooks::record(hub,caller,id,"archive",&action_key,now).await;}
        else if matches!(action.action.as_str(),"visit"|"seen"){
            let p=who.clone();let id=id.to_owned();let key=action.key.clone().unwrap_or_else(crate::util::new_id);let exposure=action.exposure;let revision=action.frame_revision;let position=action.chosen_position;let source=action.source;
            let is_visit=action.action=="visit";
            let visible=super::lineage::seen(hub,caller,hub.harness.list()).await;
            let row=display_row(&id,&visible,&ROW_LINEAGE.lock());let use_key=row_event_key(&id,&row,"open",&key);
            let direct=if is_visit && source.as_deref()!=Some("list"){row_candidates(&visible)}else{Vec::new()};
            let prior=if is_visit && source.as_deref()==Some("list"){
                let mut accessible:HashSet<String>=super::lineage::seen(hub,caller,hub.harness.list()).await.into_iter().map(|i|i.id).collect();accessible.extend(archive_rows(hub,caller).await?.into_iter().map(|(id,_)|id));
                let p=who.clone();let current=exposure.clone();let exposures=hub.store.call(move|c|examination::prior_exposures(c,&p,&accessible,current.as_deref(),now)).await.map_err(failed)?;
                crate::rt::blocking(move||kiln_ci::attention_examination::fit(&exposures,now)).await.map_err(failed)?
            }else{Default::default()};
            hub.store.call(move|c|{
                if is_visit{
                    store::record(c,&p,&row,"open",&use_key,now)?;
                    if source.as_deref()!=Some("list"){store::direct_choice(c,&p,&row,&use_key,&direct,now)?;}
                    if let (Some(exposure),Some(revision),Some(position),Some(source))=(exposure,revision,position,source){examination::pick(c,&p,&exposure,&examination::Pick{agent:&row,key:&use_key,at:now,revision,position,source:&source,prior:&prior})?;}
                }
                let before=store::placement(c,&p,&id)?;
                let after=Placement{updated_seen:Some(now),..before.clone()};store::put(c,&p,&id,&before,&after,false)?;crate::push::store::mark(c,&p,&[format!("seen:{id}")],"seen",now)?;Ok(())
            }).await.map_err(failed)?;
        }else if !matches!(action.action.as_str(),"restore"|"restore_copy"){
            let p=who.clone();let id=id.to_owned();let what=action.action.clone();let until=action.until;let expected=action.revision;let key=action_key.clone();
            if what=="park" && !until.is_some_and(|t|t>now){return Err((StatusCode::BAD_REQUEST,"park time must be in the future".into()));}
            let deferred=if what=="park"{Some(deferred_requests(&id,&super::lineage::seen(hub,caller,hub.harness.list()).await))}else{None};
            let revision=hub.store.call(move|c|{
                let before=store::placement(c,&p,&id)?;
                if what=="undo"{let restored=store::undo(c,&p,&id,expected.ok_or_else(||anyhow::anyhow!("Undo needs its revision"))?,now)?;store::record(c,&p,&id,"command",&format!("undo:{key}"),now)?;return Ok(restored.revision);}
                if let Some(expected)=expected{anyhow::ensure!(before.revision==expected,"placement changed on another device");}
                let after=match what.as_str(){
                    "pin"=>Placement{pinned:Some(now),parked_at:None,parked_until:None,deferred_requests:None,reason:None,..before.clone()},
                    "unpin"=>Placement{pinned:None,..before.clone()},
                    "park"=>Placement{pinned:None,parked_at:Some(now),parked_until:until,deferred_requests:deferred,reason:Some("You parked it".into()),..before.clone()},
                    "unpark"=>Placement{parked_at:None,parked_until:None,deferred_requests:None,reason:Some("You brought it back".into()),..before.clone()},
                    _=>anyhow::bail!("unknown attention action {what}"),
                };
                let kind=if what=="unpin"{"command"}else{what.as_str()};
                store::record(c,&p,&id,kind,&format!("placement:{key}"),now)?;
                store::put(c,&p,&id,&before,&after,true).map(|p|p.revision)
            }).await.map_err(|e|(StatusCode::CONFLICT,e.to_string()))?;
            if action.action!="undo"{undo=Some(json!({"agent":action.agent,"revision":revision}));}
        }
    }
    signal();
    if matches!(action.action.as_str(),"seen"|"visit") && let Some(id)=&action.agent{
        hub.emit(&crate::wire::events::ServerEvent::Notices(crate::wire::events::Notices{principal:who.clone(),keys:vec![format!("seen:{id}")],how:"seen".into(),at:now,tags:vec![id.clone()]}));
    }
    hub.emit(&crate::wire::events::ServerEvent::Attention{principal:who});
    hub.store.audit(caller.name.clone(),"agent.attention".into(),action.action).await;
    let mut result=snapshot_mode(hub,caller,result_mode).await?;
    if let Some(o)=result.as_object_mut(){o.insert("undo".into(),json!(undo));}
    Ok(srv::json(StatusCode::OK,&result))
}

/// Whether automation must leave an agent alone, including an archive not yet acknowledged.
pub async fn is_archived(hub:&Hub,id:&str)->anyhow::Result<bool>{
    let id=id.to_owned();
    hub.store.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM attention_archive WHERE agent=?1)",[id],|r|r.get(0))?)).await
}

type ArchiveLock=Arc<futures::lock::Mutex<()>>;
static ARCHIVE_LOCKS:LazyLock<Mutex<BTreeMap<String,ArchiveLock>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn archive_lock(id:&str)->ArchiveLock{Arc::clone(ARCHIVE_LOCKS.lock().entry(id.into()).or_insert_with(||Arc::new(futures::lock::Mutex::new(()))))}

async fn archive_target(hub:&Hub,caller:&Caller,saved:&Archived,what:&str)->Result<(),Fail>{
    if hub.harness.info(&saved.info.id).is_some(){super::lineage::may_steer(hub,caller,&saved.info.id).await?;}
    else{super::lineage::may_run_on(caller,&saved.info.target,what)?;}
    if let Some(machine)=saved.info.target.strip_prefix("agent:"){
        let caps=hub.instances.load().wide_caps_for(machine,&crate::instances::Owned::Agent(saved.info.id.clone()));
        if caps&crate::proto::CAP_HARNESS_ARCHIVE==0{return Err((StatusCode::CONFLICT,format!("{}'s runner does not support retaining archived sessions yet",saved.info.name)));}
        if !hub.agents.load().get(machine).is_some_and(|a|a.connected){return Err((StatusCode::CONFLICT,format!("{}'s runner must be connected to confirm this operation",saved.info.name)));}
    }
    Ok(())
}

async fn archive_group(hub:&Hub,id:&str)->Result<Option<(Vec<String>,String)>,Fail>{
    let id=id.to_owned();hub.store.call(move|c|store::archive_members(c,&id)).await.map_err(failed)
}

async fn archive(hub:&Arc<Hub>,caller:&Caller,id:&str)->Result<(),Fail>{
    archive_with_reason(hub,caller,id,None).await
}

#[derive(Clone)]
struct AutoIntent{principal:String,revision:i64,parked_at:i64,use_revision:i64,members:Vec<String>,days:u32}
fn family_ids(root:&str,all:&[Info])->Vec<String>{let mut ids=super::lineage::subagents_under(all,root);ids.push(root.to_owned());ids.sort();ids.dedup();ids}
fn use_revision(c:&rusqlite::Connection,members:&[String])->anyhow::Result<i64>{Ok(c.query_row("SELECT COUNT(*) FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1))",[serde_json::to_string(members)?],|r|r.get(0))?)}
fn admit_auto(c:&rusqlite::Connection,root:&str,expected:&AutoIntent,now:i64)->anyhow::Result<()>{
    let current=store::placement(c,&expected.principal,root)?;let settings=store::settings(c,&expected.principal)?;
    anyhow::ensure!(current.revision==expected.revision && current.parked_at==Some(expected.parked_at) && current.parked_until.is_some() && current.pinned.is_none(),"automatic archive cancelled: placement changed");
    anyhow::ensure!(use_revision(c,&expected.members)?==expected.use_revision,"automatic archive cancelled: recent interaction changed");
    anyhow::ensure!(settings.auto_archive && settings.archive_after_days==expected.days && kiln_ci::attention_lifecycle::archive_due(true,expected.days,current.parked_at,now,true,false),"automatic archive settings or deadline changed");
    let other:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1)) AND principal<>'' AND principal<>?2 AND at>=?3)",params![serde_json::to_string(&expected.members)?,expected.principal,expected.parked_at],|r|r.get(0))?;
    anyhow::ensure!(!other,"automatic archive cancelled: another person used this work");
    store::put(c,&expected.principal,root,&current,&current,false)?;Ok(())
}

async fn archive_with_reason(hub:&Arc<Hub>,caller:&Caller,id:&str,reason:Option<String>)->Result<(),Fail>{archive_checked(hub,caller,id,reason,None).await}
async fn archive_checked(hub:&Arc<Hub>,caller:&Caller,id:&str,reason:Option<String>,automatic:Option<AutoIntent>)->Result<(),Fail>{
    let saved: BTreeMap<String,Archived>=archive_rows(hub,caller).await?.into_iter().collect();
    let root=saved.get(id).and_then(|a|a.group.clone()).unwrap_or_else(||id.to_owned());
    let operation=archive_lock(&root);let _held=operation.lock().await;
    let saved=archive_rows(hub,caller).await?.into_iter().collect();
    let result=archive_locked(hub,caller,&root,saved,reason.as_deref(),automatic).await;
    if let Some(principal)=person(caller){hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
    result
}

async fn archive_locked(hub:&Arc<Hub>,caller:&Caller,root:&str,saved:BTreeMap<String,Archived>,reason:Option<&str>,automatic:Option<AutoIntent>)->Result<(),Fail>{
    let group=archive_group(hub,root).await?;let new_intent=group.is_none();
    if group.as_ref().is_some_and(|(_,status)|status=="restoring"){return Err((StatusCode::CONFLICT,"finish the pending Restore before archiving again".into()));}
    let all=hub.harness.list();
    let ids=if let Some((members,_))=group{members}else{
        super::lineage::may_steer(hub,caller,root).await?;
        let mut children=super::lineage::subagents_under(&all,root);
        // A previously archived subagent remains independently archived when its parent restores.
        children.retain(|id|!saved.contains_key(id));children.reverse();children.push(root.to_owned());children
    };
    let mut prepared=Vec::new();
    for id in &ids{
        let data=if let Some(data)=saved.get(id){data.clone()}else{
            let info=all.iter().find(|info|&info.id==id).cloned().ok_or((StatusCode::NOT_FOUND,format!("no agent {id} to archive")))?;
            Archived{info,state:hub.harness.state(id),status:None,group:Some(root.to_owned()),reason:reason.map(str::to_owned)}
        };
        archive_target(hub,caller,&data,"archive an agent").await?;
        let owner=crate::mcp::whose(hub,id).await.ok_or((StatusCode::FORBIDDEN,"an agent in this archive has no known owner".into()))?;
        prepared.push((id.clone(),owner,serde_json::to_string(&data).map_err(failed)?));
    }
    if let Some(expected)=&automatic{
        let fresh=hub.harness.list();let root_info=fresh.iter().find(|i|i.id==root);
        if !root_info.is_some_and(|i|quiet_row(i,&fresh)) || family_ids(root,&fresh)!=expected.members{return Err((StatusCode::CONFLICT,"automatic archive cancelled: current work changed".into()));}
    }
    let (key,members)=(root.to_owned(),prepared.clone());let now=crate::util::now_ms();let expected=automatic.clone();
    hub.store.call(move|c|store::begin_archive_checked(c,&key,&members,now,|tx|if let Some(expected)=&expected{admit_auto(tx,&key,expected,now)}else{Ok(())})).await.map_err(|e|(StatusCode::CONFLICT,e.to_string()))?;
    let mut confirmed=0usize;
    for(id,_,payload)in prepared{
        let data:Archived=serde_json::from_str(&payload).map_err(failed)?;
        if let Err(failure)=archive_command(hub,&data,ArchiveOperation::Archive{only_if_quiet:automatic.is_some()}).await{
            if automatic.is_some() && new_intent && confirmed==0 && failure.declined{
                let key=root.to_owned();hub.store.call(move|c|store::finish_restore(c,&key)).await.map_err(failed)?;
                return Err((failure.code,"automatic archive cancelled: fresh work was not stopped".into()));
            }
            return Err((failure.code,format!("Archive of {} is pending: {}. Its complete group is kept; retry Archive or Restore.",root,failure.why)));
        }
        confirmed=confirmed.saturating_add(1);
        hub.store.call(move|c|{c.execute("UPDATE attention_archive SET status='archived' WHERE agent=?1",[id])?;Ok(())}).await.map_err(failed)?;
    }
    let key=root.to_owned();hub.store.call(move|c|{c.execute("UPDATE attention_archive_groups SET status='archived' WHERE root=?1",[key])?;Ok(())}).await.map_err(failed)?;
    Ok(())
}

async fn restore(hub:&Arc<Hub>,caller:&Caller,id:&str,from_copy:bool)->Result<(),Fail>{
    let rows:BTreeMap<String,Archived>=archive_rows(hub,caller).await?.into_iter().collect();
    let data=rows.get(id).ok_or((StatusCode::NOT_FOUND,"no archived agent".into()))?;
    let root=data.group.clone().unwrap_or_else(||id.to_owned());
    let operation=archive_lock(&root);let _held=operation.lock().await;
    let rows=archive_rows(hub,caller).await?.into_iter().collect();
    let result=restore_locked(hub,caller,&root,rows,from_copy).await;
    if let Some(principal)=person(caller){hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
    result
}

async fn restore_locked(hub:&Arc<Hub>,caller:&Caller,root:&str,saved:BTreeMap<String,Archived>,from_copy:bool)->Result<(),Fail>{
    let mut ids=archive_group(hub,root).await?.ok_or((StatusCode::NOT_FOUND,"no archived group".into()))?.0;
    ids.reverse();
    for id in &ids{
        let data=saved.get(id).ok_or((StatusCode::NOT_FOUND,"archive member is not accessible".into()))?;
        archive_target(hub,caller,data,"restore an agent").await?;
    }
    let key=root.to_owned();hub.store.call(move|c|{let tx=c.unchecked_transaction()?;tx.execute("UPDATE attention_archive_groups SET status='restoring' WHERE root=?1",[&key])?;if let Some((ids,_))=store::archive_members(&tx,&key)?{for id in ids{tx.execute("UPDATE attention_archive SET status='restoring' WHERE agent=?1",[id])?;}}tx.commit()?;Ok(())}).await.map_err(failed)?;
    for id in ids{
        let data=saved.get(&id).ok_or((StatusCode::NOT_FOUND,"archive member disappeared".into()))?;
        let operation=if from_copy{ArchiveOperation::RestoreCopy}else{ArchiveOperation::Restore};
        if let Err(failure)=archive_command(hub,data,operation).await{
            let retry=if from_copy{"retry Continue from saved conversation"}else{"retry Restore or choose Continue from saved conversation"};
            return Err((failure.code,format!("Restore of {root} is pending: {}. Its complete group is kept; {retry}.",failure.why)));
        }
    }
    let key=root.to_owned();hub.store.call(move|c|store::finish_restore(c,&key)).await.map_err(failed)?;
    Ok(())
}

#[derive(Clone,Copy)]
enum ArchiveOperation{Archive{only_if_quiet:bool},Restore,RestoreCopy}

fn archive_message(id:&str,operation:ArchiveOperation)->crate::proto::ServerMsg{
    match operation{
        ArchiveOperation::Archive{only_if_quiet}=>crate::proto::ServerMsg::HarnessArchive{session:id.to_owned(),restore:false,only_if_quiet},
        ArchiveOperation::Restore=>crate::proto::ServerMsg::HarnessArchive{session:id.to_owned(),restore:true,only_if_quiet:false},
        ArchiveOperation::RestoreCopy=>crate::proto::ServerMsg::HarnessRestoreCopy{session:id.to_owned()},
    }
}

struct ArchiveFailure{code:StatusCode,why:String,declined:bool}
async fn archive_command(hub:&Hub,data:&Archived,operation:ArchiveOperation)->Result<(),ArchiveFailure>{
    let id=&data.info.id;let quiet=matches!(operation,ArchiveOperation::Archive{only_if_quiet:true});
    let error=|why|ArchiveFailure{code:StatusCode::CONFLICT,why,declined:false};
    if let Some(machine)=data.info.target.strip_prefix("agent:"){
        match hub.command(machine,&archive_message(id,operation),crate::hub::COMMAND_WITHIN).await{
            crate::wire::commands::Answer::Taken=>Ok(()),
            crate::wire::commands::Answer::Refused{code,why}=>Err(ArchiveFailure{code:StatusCode::CONFLICT,why,declined:quiet && code==crate::wire::channel::Refusal::Refused}),
            crate::wire::commands::Answer::Unreachable{why}|crate::wire::commands::Answer::Unanswered{why}=>Err(error(why)),
            _=>Err(error("its runner has not confirmed the operation".into())),
        }
    }else{
        match operation{
            ArchiveOperation::Archive{only_if_quiet:true}=>match hub.harness.local.archive_if_quiet(id).await{Ok(true)=>Ok(()),Ok(false)=>Err(ArchiveFailure{code:StatusCode::CONFLICT,why:"fresh work declined automatic archive".into(),declined:true}),Err(why)=>Err(error(why))},
            ArchiveOperation::Archive{only_if_quiet:false}=>hub.harness.local.archive(id).await.map_err(error),
            ArchiveOperation::Restore=>hub.harness.local.restore_archive(id).await.map_err(error),
            ArchiveOperation::RestoreCopy=>hub.harness.local.restore_archive_copy(id).await.map_err(error),
        }
    }
}

// Fits change only with observations; a fresh visit samples a model without recomputing diagnostics.
static FITS:LazyLock<Mutex<BTreeMap<String,(String,kiln_ci::attention::FitReport)>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn fitted(principal:&str,uses:&[kiln_ci::attention::Use],choices:&[kiln_ci::attention::Choice])->kiln_ci::attention::FitReport{
    let encoded=serde_json::to_vec(&(uses,choices)).unwrap_or_default();
    let hash=crate::util::sha256_hex(&encoded);
    if let Some((old,report))=FITS.lock().get(principal) && old==&hash{return report.clone();}
    let report=kiln_ci::attention::fit(uses,choices);
    FITS.lock().insert(principal.into(),(hash,report.clone()));report
}

fn deferred_requests(root:&str,all:&[Info])->Vec<DeferredRequest>{
    let family=super::lineage::subagents_under(all,root);
    let mut captured:Vec<_>=all.iter().filter(|i|i.id==root || family.contains(&i.id)).flat_map(requests::identities).filter_map(|(_,identity)|identity).collect();
    captured.sort();captured.dedup();captured
}

fn park_wakes(info:&Info,placement:&Placement,error:bool)->bool{
    let pending:Vec<_>=requests::identities(info).into_iter().filter(|(stop,_)|*stop==error).collect();
    let unknown=pending.iter().any(|(_,identity)|identity.is_none());
    let all_deferred=pending.iter().filter_map(|(_,identity)|identity.as_ref()).all(|identity|placement.deferred_requests.as_ref().is_some_and(|deferred|deferred.contains(identity)));
    kiln_ci::attention_lifecycle::wake_park_requests(placement.parked_until.is_some(),unknown,all_deferred)
}

async fn refresh_placements(hub:&Hub,who:&str,all:&[Info],settings:&Settings,now:i64)->anyhow::Result<BTreeMap<String,Placement>>{
    let p=who.to_owned();let a=all.to_vec();let settings2=settings.clone();
    hub.store.call(move|c|{
        let mut rows=store::placements(c,&p)?;
        let archived:HashSet<String>=c.prepare("SELECT agent FROM attention_archive")?.query_map([],|r|r.get(0))?.collect::<Result<_,_>>()?;
        for info in &a{
            if archived.contains(&info.id){continue;}
            let before=rows.get(&info.id).cloned().unwrap_or_default();
            let family=super::lineage::subagents_under(&a,&info.id);
            let members:Vec<&Info>=std::iter::once(info).chain(a.iter().filter(|child|family.contains(&child.id))).collect();
            let needs=members.iter().any(|i|park_wakes(i,&before,false));
            let error=members.iter().any(|i|park_wakes(i,&before,true));
            let row_quiet=members.iter().all(|i|quiet(i));
            let timer_due=before.parked_until.is_some_and(|at|at<=now);
            let parked_through_archive=before.parked_at.zip(before.parked_until).is_some_and(|(parked,until)|until>=parked.saturating_add(i64::from(settings2.archive_after_days).saturating_mul(86_400_000)));
            let archive_due=if timer_due && parked_through_archive && kiln_ci::attention_lifecycle::archive_due(settings2.auto_archive,settings2.archive_after_days,before.parked_at,now,row_quiet,false){
                let ids:Vec<_>=members.iter().map(|i|i.id.clone()).collect();
                let other:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1)) AND principal<>'' AND principal<>?2 AND at>=?3)",params![serde_json::to_string(&ids)?,p,before.parked_at],|r|r.get(0))?;!other
            }else{false};
            let wake=if needs{Some("A new request needs you")}else if error{Some("Stopped with a new error")}else if timer_due && !archive_due{Some("Its park time ended")}else{None};
            if before.parked_until.is_some() && let Some(why)=wake{
                let after=Placement{parked_until:None,parked_at:None,deferred_requests:None,reason:Some(why.into()),..before.clone()};
                rows.insert(info.id.clone(),store::put(c,&p,&info.id,&before,&after,false)?);
            }else if let Some(days)=settings2.auto_park_days && before.pinned.is_none() && before.parked_until.is_none() && row_quiet{
                    let at:Option<i64>=c.query_row("SELECT MAX(at) FROM attention_uses WHERE agent=?1 AND (principal=?2 OR principal='')",params![info.id,p],|r|r.get(0))?;
                    // With no real history there is no evidence of age; never use recreated_at.
                    if kiln_ci::attention_lifecycle::park_due(Some(days),at,now,row_quiet,before.pinned.is_some(),before.parked_until.is_some()){
                        let until=now.saturating_add(i64::from(settings2.archive_after_days)*86_400_000);
                        let after=Placement{parked_at:Some(now),parked_until:Some(until),deferred_requests:Some(Vec::new()),reason:Some(format!("Automatically parked: no interaction or turns for {days} days")),..before.clone()};
                        rows.insert(info.id.clone(),store::put(c,&p,&info.id,&before,&after,true)?);
                    }
            }
        }Ok(rows)
    }).await

}

// A single coalesced wake is sufficient: maintenance reads the durable state afresh.
static WAKE:LazyLock<(async_channel::Sender<()>,async_channel::Receiver<()>)>=LazyLock::new(||async_channel::bounded(1));
fn signal(){let _=WAKE.0.try_send(());}
// The observer holds &Hub; only the supervised service owns the Arc needed to emit
// after asynchronous persistence. Coalesce private refreshes without cloning the event bus.
static REQUEST_REFRESH:LazyLock<Mutex<HashSet<String>>>=LazyLock::new(||Mutex::new(HashSet::new()));
fn queue_request_refresh(people:Vec<String>){
    if people.is_empty(){return;}
    REQUEST_REFRESH.lock().extend(people);signal();
}

/// Supervised by the server. Timers are the declared deadlines, never an invented polling rate.
pub async fn run(hub:Arc<Hub>)->anyhow::Result<()>{
    loop{
        while WAKE.1.try_recv().is_ok(){}
        let people=std::mem::take(&mut*REQUEST_REFRESH.lock());
        for principal in people{hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
        let next=maintain(&hub).await?;
        // A timer only while something is due (crates/kiln/clippy.toml): the earliest park's end, or an automatic
        // park or archive coming due; else the next change's signal alone.
        let due=next.map(|at|std::time::Instant::now()+std::time::Duration::from_millis(u64::try_from(at.saturating_sub(crate::util::now_ms())).unwrap_or(0)));
        let timer=crate::rt::due(due);let wake=WAKE.1.recv();
        futures::pin_mut!(timer,wake);
        let _=futures::future::select(timer,wake).await;
    }
}

async fn maintain(hub:&Arc<Hub>)->anyhow::Result<Option<i64>>{
    let people:Vec<String>=hub.store.call(|c|{let mut q=c.prepare("SELECT principal FROM attention_settings UNION SELECT principal FROM attention_placement")?;Ok(q.query_map([],|r|r.get(0))?.collect::<Result<_,_>>()?)}).await?;
    let mut next:Option<i64>=None;
    for who in people{
        let caller=if who=="owner"{Some(Caller::owner())}else if let Some(user)=who.strip_prefix("user:"){crate::authz::gate::user_caller(hub,user).await}else{None};
        let Some(caller)=caller else{continue;};
        let all=super::lineage::seen(hub,&caller,hub.harness.list()).await;
        let p=who.clone();let settings=hub.store.call(move|c|store::settings(c,&p)).await?;
        let now=crate::util::now_ms();
        let p=who.clone();let before=hub.store.call(move|c|store::placements(c,&p)).await?;
        let rows=refresh_placements(hub,&who,&all,&settings,now).await?;
        if rows!=before{hub.emit(&crate::wire::events::ServerEvent::Attention{principal:who.clone()});}
        for info in &all{
            let Some(placed)=rows.get(&info.id) else{continue;};
            if !settings.auto_archive || placed.parked_at.is_none(){continue;}
            let members=family_ids(&info.id,&all);let ids=members.clone();let person=who.clone();let parked_at=placed.parked_at;
            let(other_used,usage)=hub.store.call(move|c|Ok((c.query_row("SELECT EXISTS(SELECT 1 FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1)) AND principal<>'' AND principal<>?2 AND at>=?3)",params![serde_json::to_string(&ids)?,person,parked_at],|r|r.get::<_,bool>(0))?,use_revision(c,&ids)?))).await?;
            if kiln_ci::attention_lifecycle::archive_due(settings.auto_archive,settings.archive_after_days,placed.parked_at,now,quiet_row(info,&all),other_used){
                // Permission is re-evaluated on every attempt; no park grants command authority.
                if super::lineage::may_steer(hub,&caller,&info.id).await.is_ok() && let Some(parked_at)=placed.parked_at{
                    let expected=AutoIntent{principal:who.clone(),revision:placed.revision,parked_at,use_revision:usage,members,days:settings.archive_after_days};
                    if let Err((_,why))=archive_checked(hub,&caller,&info.id,Some(format!("Automatically archived after {} days parked",settings.archive_after_days)),Some(expected)).await{tracing::warn!(agent=%info.id,%why,"automatic archive was not confirmed");}
                }
            }
        }
        let fresh=super::lineage::seen(hub,&caller,hub.harness.list()).await;
        let rows=refresh_placements(hub,&who,&fresh,&settings,crate::util::now_ms()).await?;
        for info in &fresh{
            let p=rows.get(&info.id).cloned().unwrap_or_default();
            if let Some(until)=p.parked_until && until>now{next=Some(next.map_or(until,|t|t.min(until)));}
            if settings.auto_archive && quiet_row(info,&all) && let Some(at)=p.parked_at{
                let due=at.saturating_add(i64::from(settings.archive_after_days)*86_400_000);
                if due>now{next=Some(next.map_or(due,|t|t.min(due)));}
            }
            if quiet_row(info,&all) && p.pinned.is_none() && p.parked_until.is_none() && let Some(days)=settings.auto_park_days{
                let (id,person)=(info.id.clone(),who.clone());
                let last:Option<i64>=hub.store.call(move|c|Ok(c.query_row("SELECT MAX(at) FROM attention_uses WHERE agent=?1 AND (principal=?2 OR principal='')",params![id,person],|r|r.get(0))?)).await?;
                if let Some(last)=last{
                    let due=last.saturating_add(i64::from(days)*86_400_000);
                    if due>now{next=Some(next.map_or(due,|t|t.min(due)));}
                }
            }
        }
    }
    Ok(next)
}

/// Only unambiguous authenticated human audit rows are backfilled; message text is never guessed.
async fn backfill(hub:&Hub,caller:&Caller,agents:&[Info])->anyhow::Result<()>{
    let Some(who)=person(caller) else{return Ok(())};
    let audit=if who=="owner"{"admin".to_owned()}else{caller.name.clone()};
    if audit.starts_with("agent:") || audit.starts_with("job:") || audit.starts_with("peer:"){return Ok(());}
    let ids:HashSet<String>=agents.iter().map(|a|a.id.clone()).collect();
    hub.store.call(move|c|{
        let imported:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM attention_meta WHERE key=?1)",[format!("backfilled:{who}")],|r|r.get(0))?;
        let mut q=c.prepare("SELECT id,ts,detail FROM audit2 WHERE action='agent.command' AND principal=?1 AND on_behalf_of IS NULL AND id <= (SELECT value FROM attention_meta WHERE key='audit_fence') AND NOT ?2 ORDER BY id")?;
        let rows:Vec<(i64,i64,String)>=q.query_map(params![audit,imported],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
        for(key,at,detail)in rows{
            let Some((id,command))=detail.split_once(": ") else{continue;};
            if id.len()!=32 || !id.chars().all(|c|c.is_ascii_hexdigit()){continue;}
            let kind=match command.trim(){"send"|"skill_run"=>"message","approve"|"resume"=>"answer","interrupt"|"restart"|"set_mode"|"set_model"|"control"=>"command",_=>continue};
            if let Some(at)=at.checked_mul(1000){store::record(c,&who,id,kind,&format!("audit:{key}"),at)?;}
        }
        c.execute("INSERT OR IGNORE INTO attention_meta(key,value) VALUES(?1,1)",[format!("backfilled:{who}")])?;
        // Notice watermarks may seed Updated, but placement persists beyond their thirty-day TTL.
        let marks=crate::push::store::marks(c,&who)?;
        for m in marks{
            if m.how!="seen"{continue;}
            let Some(id)=m.key.strip_prefix("seen:") else{continue;};if !ids.contains(id){continue;}
            let before=store::placement(c,&who,id)?;
            if before.updated_seen.is_none_or(|t|t<m.at){let after=Placement{updated_seen:Some(m.at),..before.clone()};store::put(c,&who,id,&before,&after,false)?;}
        }Ok(())
    }).await
}

async fn replay(hub:&Arc<Hub>,caller:&Caller,req:&Request)->Result<Value,Fail>{
    let at=req.uri().query().into_iter().flat_map(|q|q.split('&')).find_map(|q|q.strip_prefix("at=")).and_then(|s|s.parse::<i64>().ok()).filter(|at|*at>=0 && *at<=crate::util::now_ms()).ok_or((StatusCode::BAD_REQUEST,"at is a past Unix time in milliseconds".into()))?;
    let who=person(caller).ok_or((StatusCode::FORBIDDEN,"attention belongs to a person".into()))?;
    let all=super::lineage::seen(hub,caller,hub.harness.list()).await;
    let live:HashSet<String>=all.iter().map(|i|i.id.clone()).collect();
    let saved=archive_rows(hub,caller).await?;
    let archived:HashSet<String>=saved.iter().map(|(id,_)|id.clone()).collect();
    let mut accessible=live.clone();accessible.extend(archived.iter().cloned());
    let ids:Vec<String>=all.iter().filter(|i|i.helper.is_none() && !archived.contains(&i.id) && !(i.subagent && i.parent.as_ref().is_some_and(|p|live.contains(p)))).map(|i|i.id.clone()).collect();
    let a=accessible.clone();
    let (mut uses,mut choices,open,responses)=hub.store.call(move|c|{let(open,responses)=requests::history(c,&who,&a,at)?;Ok((store::uses(c,&who)?,store::choices(c,&who)?,open,responses))}).await.map_err(failed)?;
    let omitted=scoped_history(&mut uses,&mut choices,&accessible,at);
    crate::rt::blocking(move||{
        let mut report=kiln_ci::attention::fit(&uses,&choices);report_omissions(&mut report,omitted);
        let order=kiln_ci::attention::rank(report.model.as_ref().filter(|_|report.improves_baseline),&uses,&ids,at);
        let visible=ids.into_iter().collect();let urgency=kiln_ci::attention::urgency(&requests::waits(&open,&visible),&responses,at);
        json!({"at":at,"order":order,"model":report,"choices":choices,"urgency":urgency,"limitation":"Ranking includes only rows accessible now. Accessible archived history retains original alternatives; a choice with any inaccessible alternative is omitted. Historical audit rows lack candidate sets and positions; no position-bias correction is claimed."})
    }).await.map_err(failed)
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn mc_attention_archived_history_keeps_its_original_choice_alternatives(){
        use kiln_ci::attention::{Use,UseKind,Choice};
        let original=Choice{at:10,chosen:"live".into(),candidates:vec!["live".into(),"archived".into()],propensity:None,correction:None};
        let mut choices=vec![original.clone()];
        let mut uses=vec![Use{agent:"archived".into(),kind:UseKind::Message,at:1}];
        let accessible=["live".into(),"archived".into()].into_iter().collect();
        assert_eq!(scoped_history(&mut uses,&mut choices,&accessible,20),0);
        assert_eq!(choices,vec![original.clone()]);assert_eq!(uses.len(),1);
        let mut choices=vec![original];
        let inaccessible=["live".into()].into_iter().collect();
        assert_eq!(scoped_history(&mut uses,&mut choices,&inaccessible,20),1);
        assert!(choices.is_empty(),"skip the whole observation instead of shrinking its alternatives");
        assert!(uses.is_empty());
    }
    #[test]
    fn mc_attention_response_history_requires_a_real_human_and_real_request()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            hub.store.call(|c|requests::observe(c,"a","a",&[requests::Pending{key:requests::approval_key("q",Some(10)),kind:kiln_ci::attention::RequestKind::Question,at:Some(10),provider_id:Some("q".into())}],"01",11)).await?;
            let mut agent=Caller::owner();agent.agent=Some("helper".into());
            record_response(&hub,&agent,"a","agent-reply",HumanResponse::Bound(requests::approval_key("q",Some(10))),20).await?;
            let count:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_responses",[],|r|r.get(0))?)).await?;assert_eq!(count,0);
            record_response(&hub,&Caller::owner(),"a","human-answer",HumanResponse::Bound(requests::approval_key("q",Some(10))),25).await?;
            let duration:i64=hub.store.call(|c|Ok(c.query_row("SELECT duration FROM attention_responses",[],|r|r.get(0))?)).await?;assert_eq!(duration,15);
            Ok(())
        })
    }
    #[test]
    fn mc_attention_park_defers_existing_requests_but_new_requests_wake()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let approval=|at|super::super::session::ApprovalBrief{id:"q".into(),tool:super::super::model::ToolKind::Question,tool_name:"question".into(),title:"Choose".into(),at,agent:None};
            let mut info=Info{id:"parked".into(),status:Status::Waiting,approvals:vec![approval(10)],..Info::default()};
            let captured=deferred_requests("parked",&[info.clone()]);
            hub.store.call(move|c|{let before=store::placement(c,"owner","parked")?;store::put(c,"owner","parked",&before,&Placement{parked_at:Some(20),parked_until:Some(100),deferred_requests:Some(captured),..before.clone()},false)?;Ok(())}).await?;
            let existing=refresh_placements(&hub,"owner",&[info.clone()],&Settings::default(),30).await?;
            assert_eq!(existing.get("parked").and_then(|p|p.parked_until),Some(100));
            info.approvals=vec![approval(40)];
            let fresh=refresh_placements(&hub,"owner",&[info.clone()],&Settings::default(),50).await?;
            assert!(fresh.get("parked").is_some_and(|p|p.parked_until.is_none() && p.deferred_requests.is_none()));
            let parked=Placement{parked_at:Some(20),parked_until:Some(100),deferred_requests:Some(Vec::new()),..Placement::default()};
            info.approvals=vec![approval(0)];assert!(park_wakes(&info,&parked,false));
            info.approvals.clear();assert!(park_wakes(&info,&parked,false));Ok(())
        })
    }
    #[test]
    fn mc_attention_park_request_identity_is_independent_of_runner_clock(){
        use super::super::session::ApprovalBrief;
        let approval=|id:&str,at|ApprovalBrief{id:id.into(),tool:super::super::model::ToolKind::Question,tool_name:"question".into(),title:"Choose".into(),at,agent:None};
        let old=Info{id:"parent".into(),status:Status::Waiting,approvals:vec![approval("existing",1000)],..Info::default()};
        let parked=Placement{parked_at:Some(20),parked_until:Some(2000),deferred_requests:Some(deferred_requests("parent",std::slice::from_ref(&old))),..Placement::default()};
        assert!(!park_wakes(&old,&parked,false),"captured current request stays deferred even when its runner clock is ahead");
        let older=Info{approvals:vec![approval("new",5)],..old.clone()};
        assert!(park_wakes(&older,&parked,false),"a new request wakes even when the runner timestamp is behind");
        let equal=Info{approvals:vec![approval("new",1000)],..old.clone()};
        assert!(park_wakes(&equal,&parked,false),"a different request in the same millisecond is new");
        let legacy=Placement{deferred_requests:None,..parked};assert!(park_wakes(&old,&legacy,false));
    }
    #[test]
    fn mc_attention_park_captures_true_family_identities_and_new_reasons(){
        use super::super::{session::Held,model::ExitInfo};
        let parent=Info{id:"parent".into(),status:Status::Idle,..Info::default()};
        let held=Info{id:"child".into(),parent:Some("parent".into()),subagent:true,held:Some(Held{why:"Review this".into(),plan:None,since:1000}),status:Status::Idle,..Info::default()};
        let stop=Info{id:"grandchild".into(),parent:Some("child".into()),subagent:true,status:Status::Exited,exit:Some(ExitInfo{error:Some("First error".into()),at:1000,..ExitInfo::default()}),..Info::default()};
        let standalone=Info{id:"standalone".into(),subagent:false,..held.clone()};
        let captured=deferred_requests("parent",&[parent,held.clone(),stop.clone(),standalone]);
        assert_eq!(captured.len(),2);assert!(captured.iter().all(|r|r.agent!="standalone"));
        let parked=Placement{parked_at:Some(20),parked_until:Some(2000),deferred_requests:Some(captured),..Placement::default()};
        assert!(!park_wakes(&held,&parked,false));assert!(!park_wakes(&stop,&parked,true));
        let new_held=Info{held:Some(Held{why:"Review another thing".into(),plan:None,since:1000}),..held};
        let new_stop=Info{exit:Some(ExitInfo{error:Some("Another error".into()),at:5,..ExitInfo::default()}),..stop};
        assert!(park_wakes(&new_held,&parked,false));assert!(park_wakes(&new_stop,&parked,true));
    }
    #[test]
    fn mc_attention_unknown_wait_disables_normalized_request_order()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            hub.store.call(|c|{
                requests::observe(c,"history","history",&[requests::Pending{key:requests::approval_key("history",Some(1)),kind:kiln_ci::attention::RequestKind::Question,at:Some(1),provider_id:Some("history".into())}],"01",1)?;
                requests::response(c,"owner","history","answer",&HumanResponse::Bound(requests::approval_key("history",Some(1))),10)?;Ok(())
            }).await?;
            let info=Info{id:"unknown".into(),status:Status::Waiting,turn_started:Some(20),..Info::default()};
            let accessible=["unknown".into(),"history".into()].into_iter().collect();let mut agents=BTreeMap::new();
            let(order,report)=request_snapshot(&hub,"owner",&[info],&accessible,&["unknown".into()],&mut agents,("02",30)).await?;
            assert_eq!(order,vec!["unknown".to_owned()]);assert!(!report.learned_order);
            assert_eq!(agents.get("unknown").and_then(|v|v.pointer("/urgency/since")),Some(&Value::Null));
            assert_eq!(agents.get("unknown").and_then(|v|v.pointer("/urgency/stale")).and_then(Value::as_bool),Some(false));Ok(())
        })
    }
    #[test]
    fn mc_attention_automatic_archive_reason_survives_intent_and_snapshot()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let why="Automatically archived after 7 days parked";
            let saved=Archived{info:Info{id:"automatic".into(),name:"Automatic".into(),..Info::default()},state:None,status:None,group:Some("automatic".into()),reason:Some(why.into())};
            let payload=serde_json::to_string(&saved)?;
            assert_eq!(serde_json::from_str::<Archived>(&payload)?.reason.as_deref(),Some(why));
            let mut legacy=serde_json::to_value(&saved)?;if let Some(o)=legacy.as_object_mut(){o.remove("reason");}
            assert!(serde_json::from_value::<Archived>(legacy)?.reason.is_none());
            hub.store.call(move|c|store::begin_archive(c,"automatic",&[("automatic".into(),"owner".into(),payload)],1)).await?;
            // Intent is deliberately still pending: the explanation is durable before
            // any process is stopped, and stays visible if confirmation fails.
            let snapshot=snapshot(&hub,&Caller::owner()).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            assert_eq!(snapshot.pointer("/agents/automatic/reason").and_then(Value::as_str),Some(why));
            assert_eq!(snapshot.pointer("/agents/automatic/archive_status").and_then(Value::as_str),Some("archiving"));Ok(())
        })
    }
    #[test]
    fn mc_attention_refresh_reuses_pair_and_report_without_creating_an_exposure()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let first=snapshot_mode(&hub,&Caller::owner(),SnapshotMode::Arrival("stable-visit".into())).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            // Change deliberately unusable history. A refresh must not prepare/refit a
            // model or replace the stored pair/report; an actual next arrival may learn it.
            hub.store.call(|c|{store::record(c,"owner","new","message","new",1)?;Ok(())}).await?;
            let refresh=snapshot_mode(&hub,&Caller::owner(),SnapshotMode::Refresh(Some("stable-visit".into()))).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            assert_eq!(first.get("pair"),refresh.get("pair"));assert_eq!(first.get("model"),refresh.get("model"));
            let retry=snapshot_mode(&hub,&Caller::owner(),SnapshotMode::Arrival("stable-visit".into())).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            assert_eq!(first.get("pair"),retry.get("pair"));
            let count:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_visits",[],|r|r.get(0))?)).await?;assert_eq!(count,1);
            let _=snapshot_mode(&hub,&Caller::owner(),SnapshotMode::Refresh(Some("missing".into()))).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            let count:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_visits",[],|r|r.get(0))?)).await?;assert_eq!(count,1);Ok(())
        })
    }
    #[test]
    fn mc_attention_automatic_intent_cas_rejects_new_activity_and_unparking_atomically()->anyhow::Result<()>{
        let c=rusqlite::Connection::open_in_memory()?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;
        store::save_settings(&c,"owner",&Settings{auto_archive:true,archive_after_days:1,..Settings::default()})?;
        let before=store::placement(&c,"owner","root")?;let parked=store::put(&c,"owner","root",&before,&Placement{parked_at:Some(1),parked_until:Some(200_000_000),deferred_requests:Some(Vec::new()),..before.clone()},false)?;
        let expected=AutoIntent{principal:"owner".into(),revision:parked.revision,parked_at:1,use_revision:0,members:vec!["root".into(),"sub".into()],days:1};
        store::record(&c,"owner","sub","message","fresh",1)?;
        let members=vec![("root".into(),"owner".into(),"payload".into())];
        assert!(store::begin_archive_checked(&c,"root",&members,100_000_000,|tx|admit_auto(tx,"root",&expected,100_000_000)).is_err(),"same-clock-time child message still changes the append-only revision");
        assert!(store::archive_members(&c,"root")?.is_none());assert_eq!(store::placement(&c,"owner","root")?.revision,parked.revision);
        let expected=AutoIntent{use_revision:use_revision(&c,&expected.members)?,..expected};
        let current=store::placement(&c,"owner","root")?;store::put(&c,"owner","root",&current,&Placement{parked_at:None,parked_until:None,deferred_requests:None,..current.clone()},false)?;
        assert!(store::begin_archive_checked(&c,"root",&members,100_000_000,|tx|admit_auto(tx,"root",&expected,100_000_000)).is_err());assert!(store::archive_members(&c,"root")?.is_none());Ok(())
    }
    #[test]
    fn mc_attention_declined_fresh_auto_archive_removes_only_its_unstarted_intent()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;let dir=tempfile::tempdir()?;let id="automatic-fresh";
            let session=hub.harness.local.open(Some(id.into()),super::super::session::Spec{kind:"claude".into(),program:Some(dir.path().join("no-provider").to_string_lossy().into_owned()),cwd:Some(dir.path().to_string_lossy().into_owned()),..Default::default()})?;
            let info=Info{id:id.into(),name:"Fresh work".into(),kind:"claude".into(),status:Status::Idle,..Info::default()};
            hub.harness.ingest(&super::super::registry::Location::Local,id,super::super::core::Update::Open{info:Box::new(info),since:None}).await;
            let parked=hub.store.call(move|c|{store::save_settings(c,"owner",&Settings{auto_archive:true,archive_after_days:1,..Settings::default()})?;let before=store::placement(c,"owner",id)?;store::put(c,"owner",id,&before,&Placement{parked_at:Some(1),parked_until:Some(i64::MAX),deferred_requests:Some(Vec::new()),..before.clone()},false)}).await?;
            // The runner is authoritative: the server's old Idle mirror must not stop
            // this current turn. No executable exists, so the test launches no provider.
            session.core.lock().state.status=Status::Running;
            let expected=AutoIntent{principal:"owner".into(),revision:parked.revision,parked_at:1,use_revision:0,members:vec![id.into()],days:1};
            assert!(archive_checked(&hub,&Caller::owner(),id,Some("Automatic test".into()),Some(expected)).await.is_err());
            assert!(!is_archived(&hub,id).await?);assert!(hub.harness.local.get(id).is_some());hub.harness.local.close(id);Ok(())
        })
    }
    #[test]
    fn mc_attention_detached_task_quiet_transition_wakes_without_a_new_turn(){
        let busy=Info{id:crate::util::new_id(),status:Status::Idle,tasks_running:1,..Info::default()};
        assert!(!quiet_transition(&busy));assert!(!quiet_transition(&busy));
        let idle=Info{tasks_running:0,..busy};assert!(quiet_transition(&idle));assert!(!quiet_transition(&idle));
    }
    #[test]
    fn mc_attention_archive_deadline_does_not_extend_an_expired_short_manual_park()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let info=Info{id:"timer".into(),status:Status::Idle,..Info::default()};
            let settings=Settings{auto_archive:true,archive_after_days:1,..Settings::default()};
            hub.store.call(|c|{let before=store::placement(c,"owner","timer")?;store::put(c,"owner","timer",&before,&Placement{parked_at:Some(1),parked_until:Some(100),deferred_requests:Some(Vec::new()),..before.clone()},false)?;Ok(())}).await?;
            let rows=refresh_placements(&hub,"owner",std::slice::from_ref(&info),&settings,100_000_000).await?;assert!(rows.get("timer").is_some_and(|p|p.parked_until.is_none()));
            hub.store.call(|c|{let before=store::placement(c,"owner","timer")?;store::put(c,"owner","timer",&before,&Placement{parked_at:Some(1),parked_until:Some(86_400_001),deferred_requests:Some(Vec::new()),..before.clone()},false)?;Ok(())}).await?;
            let rows=refresh_placements(&hub,"owner",&[info],&settings,100_000_000).await?;assert!(rows.get("timer").is_some_and(|p|p.parked_until.is_some()),"a declared due archive gets fresh quiet/CAS admission before the coincident timer wake");Ok(())
        })
    }
    #[test]
    fn mc_attention_person_row_mapping_respects_true_lineage_orphans_and_visibility(){
        let parent=Info{id:"root".into(),..Info::default()};
        let child=Info{id:"child".into(),parent:Some("root".into()),subagent:true,..Info::default()};
        let standalone=Info{id:"standalone".into(),parent:Some("root".into()),subagent:false,..Info::default()};
        let known=[("child".into(),RowLineage{parent:Some("root".into()),subagent:true})].into_iter().collect();
        assert_eq!(display_row("child",&[parent.clone(),child.clone()],&known),"root");
        assert_eq!(display_row("child",std::slice::from_ref(&parent),&known),"root","successful removal retains observed lineage for recording");
        assert_eq!(display_row("child",std::slice::from_ref(&child),&known),"child","an inaccessible parent cannot become a recording target");
        assert_eq!(display_row("standalone",&[parent,standalone],&known),"standalone");
        assert_eq!(row_candidates(&[child]),vec!["child".to_owned()],"a visible orphan remains a row");
    }
    #[test]
    fn mc_attention_human_child_use_raises_and_unparks_parent_but_standalone_is_independent()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            for info in [Info{id:"row-root".into(),..Info::default()},Info{id:"row-child".into(),parent:Some("row-root".into()),subagent:true,..Info::default()},Info{id:"row-child-two".into(),parent:Some("row-root".into()),subagent:true,..Info::default()},Info{id:"row-standalone".into(),parent:Some("row-root".into()),subagent:false,..Info::default()}]{let id=info.id.clone();hub.harness.ingest(&super::super::registry::Location::Local,&id,super::super::core::Update::Open{info:Box::new(info),since:None}).await;}
            hub.store.call(|c|{let before=store::placement(c,"owner","row-root")?;store::put(c,"owner","row-root",&before,&Placement{parked_at:Some(1),parked_until:Some(1000),deferred_requests:Some(Vec::new()),..before.clone()},false)?;Ok(())}).await?;
            record_person(&hub,&Caller::owner(),"row-child","message","same-provider-key",100).await?;
            record_person(&hub,&Caller::owner(),"row-standalone","message","same-provider-key",101).await?;
            record_person(&hub,&Caller::owner(),"row-child-two","message","same-provider-key",102).await?;
            record_person(&hub,&Caller::owner(),"row-child","message","same-provider-key",200).await?;
            let(rows,choices,parked)=hub.store.call(|c|Ok((store::uses(c,"owner")?,store::choices(c,"owner")?,store::placement(c,"owner","row-root")?))).await?;
            assert!(rows.iter().any(|r|r.agent=="row-root" && r.at==100));assert!(rows.iter().any(|r|r.agent=="row-standalone" && r.at==101));assert!(!rows.iter().any(|r|r.agent=="row-child"));assert_eq!(rows.iter().filter(|r|r.agent=="row-root").count(),2,"different children do not collapse equal provider IDs; retries do not boost");
            assert!(choices.iter().any(|c|c.chosen=="row-root"));assert!(choices.iter().any(|c|c.chosen=="row-standalone"));assert!(parked.parked_until.is_none());
            hub.store.call(|c|requests::observe(c,"row-child","row-root",&[requests::Pending{key:requests::approval_key("q",Some(10)),kind:kiln_ci::attention::RequestKind::Question,at:Some(10),provider_id:Some("q".into())}],"01",11)).await?;
            record_response(&hub,&Caller::owner(),"row-child","answer",HumanResponse::Bound(requests::approval_key("q",Some(10))),20).await?;
            let actual:String=hub.store.call(|c|Ok(c.query_row("SELECT agent FROM attention_responses",[],|r|r.get(0))?)).await?;assert_eq!(actual,"row-child");Ok(())
        })
    }
    #[test]
    fn mc_attention_saved_copy_restore_requires_its_explicit_action(){
        assert!(matches!(archive_message("chat",ArchiveOperation::Restore),crate::proto::ServerMsg::HarnessArchive{session,restore:true,only_if_quiet:false} if session=="chat"));
        assert!(matches!(archive_message("chat",ArchiveOperation::RestoreCopy),crate::proto::ServerMsg::HarnessRestoreCopy{session} if session=="chat"));
        let request:Result<Action,_>=serde_json::from_value(json!({"action":"restore_copy","agent":"chat"}));
        assert!(request.is_ok_and(|a|a.action=="restore_copy"));
    }
    #[test]
    fn mc_attention_a_failed_native_or_saved_copy_restore_keeps_its_archive()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let info=Info{id:"restore-contract".into(),name:"Restore contract".into(),kind:"claude".into(),status:Status::Idle,..Info::default()};
            hub.harness.ingest(&super::super::registry::Location::Local,&info.id,super::super::core::Update::Open{info:Box::new(info.clone()),since:None}).await;
            let saved=Archived{info,state:None,status:None,group:Some("restore-contract".into()),reason:None};
            let payload=serde_json::to_string(&saved)?;
            hub.store.call(move|c|store::begin_archive(c,"restore-contract",&[("restore-contract".into(),"owner".into(),payload)],1)).await?;
            // There is deliberately no Host archive snapshot. Neither failure is permission
            // to discard the server's recoverable conversation and group metadata.
            assert!(restore(&hub,&Caller::owner(),"restore-contract",false).await.is_err());
            assert!(restore(&hub,&Caller::owner(),"restore-contract",true).await.is_err());
            let count:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_archive WHERE agent='restore-contract'",[],|r|r.get(0))?)).await?;
            assert_eq!(count,1);
            assert_eq!(archive_group(&hub,"restore-contract").await.map_err(|(_,why)|anyhow::anyhow!(why))?.map(|(_,status)|status).as_deref(),Some("restoring"));
            Ok(())
        })
    }
    #[test]
    fn mc_attention_an_agent_cannot_inherit_the_owners_private_list(){
        let mut caller=Caller::owner();assert_eq!(person(&caller).as_deref(),Some("owner"));
        caller.agent=Some("working-for-owner".into());assert_eq!(person(&caller),None);
        assert_eq!(person(&Caller::with(Kind::Agent,"agent",None,None)),None);
        assert_eq!(person(&Caller::with(Kind::Job,"job",None,None)),None);
        assert_eq!(person(&Caller::with(Kind::Peer,"peer",None,None)),None);
    }
    #[test]
    fn mc_attention_group_preflight_has_no_partial_intent_on_an_unsupported_runner()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let parent=Info{id:"parent".into(),name:"Parent".into(),status:Status::Idle,..Info::default()};
            let child=|id:&str|Info{id:id.into(),name:id.into(),parent:Some("parent".into()),subagent:true,status:Status::Idle,..Info::default()};
            for(info,at)in [(parent,super::super::registry::Location::Local),(child("z-local"),super::super::registry::Location::Local),(child("a-unsupported"),super::super::registry::Location::Machine("old-runner".into()))]{
                let id=info.id.clone();hub.harness.ingest(&at,&id,super::super::core::Update::Open{info:Box::new(info),since:None}).await;
            }
            let result=archive(&hub,&Caller::owner(),"parent").await;
            assert!(matches!(result,Err((StatusCode::CONFLICT,why)) if why.contains("does not support")));
            let count:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_archive",[],|r|r.get(0))?)).await?;
            assert_eq!(count,0,"no member gets an archive intent until every member passes preflight");
            assert!(archive_group(&hub,"parent").await.map_err(|(_,why)|anyhow::anyhow!(why))?.is_none());
            Ok(())
        })
    }
    #[test]
    fn mc_attention_unknown_owned_work_never_qualifies_for_automatic_quiet(){
        assert!(quiet(&Info{status:Status::Idle,..Info::default()}),"an idle agent with nothing running is quiet");
        let parent=Info{id:"parent".into(),status:Status::Idle,..Info::default()};
        let child=Info{id:"child".into(),parent:Some("parent".into()),subagent:true,status:Status::Idle,..Info::default()};
        assert!(!quiet_row(&parent,&[parent.clone(),child]));
    }
    #[test]
    fn mc_attention_a_real_kiln_subagent_prevents_automatic_parking(){
        let parent=Info{id:"parent".into(),status:Status::Idle,..Info::default()};
        let sub=Info{id:"sub".into(),parent:Some("parent".into()),subagent:true,status:Status::Running,..Info::default()};
        let independent=Info{id:"independent".into(),parent:Some("parent".into()),status:Status::Running,..Info::default()};
        assert!(!quiet_row(&parent,&[parent.clone(),sub]));
        assert!(quiet_row(&parent,&[parent.clone(),independent]));
    }
}
