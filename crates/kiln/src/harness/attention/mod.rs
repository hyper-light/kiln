//! Mission Control attention: plan 01a199e1b177752e84dad5dc990e6a15.
//! The person owns placement; an archive affects the session and requires command rights.
mod store;
mod requests;
mod examination;
mod learned;
pub use requests::Response as HumanResponse;
pub use requests::{approval_key,bind as bind_response};

use std::collections::{BTreeMap, HashMap, HashSet};
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

/// An agent gone from kiln (ended, removed, archived): what's kept in memory for it goes too, so nothing here grows with
/// every agent there ever was. A true subagent's lineage stays while its row does: a person's command that ended it is
/// still recorded on that row (`display_row`); its row's end takes it, and every subagent under it, along.
pub fn forget(id:&str){
    QUIET_STATES.lock().remove(id);
    OBSERVED.lock().remove(id);
    let mut known=ROW_LINEAGE.lock();
    if known.get(id).is_some_and(|l|l.subagent && l.parent.as_ref().is_some_and(|p|known.contains_key(p))){return;}
    let mut gone=HashSet::from([id.to_owned()]);
    loop{
        let under:Vec<String>=known.iter().filter(|(k,l)|!gone.contains(*k) && l.subagent && l.parent.as_ref().is_some_and(|p|gone.contains(p))).map(|(k,_)|k.clone()).collect();
        if under.is_empty(){break;}
        gone.extend(under);
    }
    known.retain(|k,_|!gone.contains(k));
}
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
        // A park ends when the person messages it, answers it or restores it, saying so (step 6), and the quiet that
        // parks it automatically counts from then (step 8).
        if fresh && let Some(why)=woken_by(&kind){
            let p=store::placement(c,&who,&id)?;
            if p.parked_until.is_some(){store::put(c,&who,&id,&p,&p.woke(at,why.into(),false),false)?;}
        }
        if fresh{signal();}
        Ok(())
    }).await
}

/// What a person's use that ends a park says of it.
fn woken_by(kind:&str)->Option<&'static str>{
    match kind{"message"=>Some("You messaged it"),"answer"=>Some("You answered it"),"restore"=>Some("You restored it"),_=>None}
}

// Detached-task completion changes automatic quiet eligibility without creating a turn
// or a use. Wake maintenance only when that predicate changes, never on every heartbeat.
static QUIET_STATES:LazyLock<Mutex<BTreeMap<String,bool>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn quiet_transition(info:&Info)->bool{let now=quiet(info);QUIET_STATES.lock().insert(info.id.clone(),now).is_some_and(|before|before!=now)}

/// What of an agent's info its attention reads (its lineage, its turns and its subagents', whether it's quiet, what it
/// asks its person), hashed without allocating: the registry's update path calls [`observe`] only when this changed, so an
/// update that changes nothing of it (a summary, what it's doing) costs its owner's shard nothing more (goal 9's budget).
pub fn signature(info:&Info)->u64{
    use std::hash::{Hash as _,Hasher as _};
    let mut h=std::hash::DefaultHasher::new();
    (&info.id,&info.parent,info.subagent,std::mem::discriminant(&info.status),info.turn_started,info.last_turn.as_ref().map(|t|t.at)).hash(&mut h);
    for s in &info.subagents{(&s.id,s.started,s.ended,&s.status).hash(&mut h);}
    for a in &info.approvals{(&a.id,a.at).hash(&mut h);}
    (info.held.as_ref().map(|held|(held.since,&held.why)),info.exit.as_ref().map(|e|(e.at,&e.error,e.again_at)),info.tasks_running,info.left_behind).hash(&mut h);
    h.finish()
}

/// One end of a turn, the agent's own or a subagent's, as a use of the row it's on (step 1).
#[derive(Debug,Clone,PartialEq,Eq)]
struct TurnUse{row:String,kind:&'static str,key:String,at:i64,edge:store::Edge}

/// What a hub's store last took for an agent from its observations: the row it's on, its turns' ends and its open
/// requests. The store stays the record; this only spares writing again what it holds already, at thousands of
/// sessions, where every change of every agent and every snapshot wrote them all again. Set once a write commits, so
/// a write still on its way is written again rather than taken as done; gone with its agent (`forget`). Whose store
/// it is goes with it (its hub's configuration, held weakly): another hub in this process, a test's, writes its own.
#[derive(Debug,Clone,Default)]
struct Observed{hub:std::sync::Weak<crate::config::Config>,row:String,turns:Vec<TurnUse>,pending:Vec<requests::Pending>}
static OBSERVED:LazyLock<Mutex<HashMap<String,Observed>>>=LazyLock::new(||Mutex::new(HashMap::new()));

/// Whether what an observation would write of `id` (`row`, its `turns` when known, `pending`) differs from what
/// `hub`'s store last took. Requests alone (a snapshot's) compare the requests alone.
fn unwritten(hub:&Hub,id:&str,row:&str,turns:Option<&[TurnUse]>,pending:&[requests::Pending])->bool{
    OBSERVED.lock().get(id).is_none_or(|o|!o.hub.upgrade().is_some_and(|cfg|Arc::ptr_eq(&cfg,&hub.cfg)) || o.row!=row || o.pending!=pending || turns.is_some_and(|t|o.turns!=t))
}
fn written(hub:std::sync::Weak<crate::config::Config>,id:&str,row:String,turns:Option<Vec<TurnUse>>,pending:Vec<requests::Pending>){
    let mut observed=OBSERVED.lock();let o=observed.entry(id.to_owned()).or_default();
    if !o.hub.ptr_eq(&hub){o.turns.clear();}
    o.hub=hub;o.row=row;o.pending=pending;if let Some(turns)=turns{o.turns=turns;}
}

fn turn_uses(info:&Info,row:&str)->Vec<TurnUse>{
    let kind=if row==info.id{"turn"}else{"subagent_turn"};
    let mut uses=Vec::new();
    let mut push=|kind,key,at,edge|uses.push(TurnUse{row:row.to_owned(),kind,key,at,edge});
    if let Some(at)=info.turn_started{push(kind,format!("{}:start:{at}",info.id),at,store::Edge::Start);}
    if let Some(end)=&info.last_turn{push(kind,format!("{}:end:{}",info.id,end.at),end.at,store::Edge::End);}
    for task in &info.subagents{
        if task.started>0{push("subagent_turn",format!("{}:{}:start:{}",info.id,task.id,task.started),task.started,store::Edge::Start);}
        if let Some(at)=task.ended{push("subagent_turn",format!("{}:{}:end:{at}",info.id,task.id),at,store::Edge::End);}
    }
    uses
}

/// Called after the mirror lock is released. Polling an unchanged state adds no event, and writes nothing: only a
/// change in its turns, its requests or the row it's on goes to the store. Whether it went.
pub fn observe(hub:&Hub,info:&Info)->bool{
    remember_lineage(info);
    if quiet_transition(info){signal();}
    let mut row=info.id.clone();
    let mut current=info.clone();
    let mut visited=HashSet::new();
    while current.subagent && visited.insert(current.id.clone()){
        let Some(parent)=current.parent.as_deref().and_then(|p|hub.harness.info(p)) else{break;};
        row=parent.id.clone();current=parent;
    }
    let uses=turn_uses(info,&row);
    let pending=requests::pending(info);
    if !unwritten(hub,&info.id,&row,Some(&uses),&pending){return false;}
    // A request that came or went may end a park (step 6).
    if unwritten(hub,&info.id,&row,None,&pending){signal();}
    let id=info.id.clone();
    let revision=crate::util::new_id();
    let observed_at=crate::util::now_ms();
    let(db,whose)=(hub.store.clone(),Arc::downgrade(&hub.cfg));
    drop(crate::rt::spawn(async move{
        let (turns,taken)=(uses.clone(),pending.clone());let(agent,on)=(id.clone(),row.clone());
        match db.call(move|c|{for u in &uses{if store::record_edge(c,"",&u.row,u.kind,&u.key,u.at,Some(u.edge))?{signal();}}requests::observe(c,&id,&row,&pending,&revision,observed_at)}).await{
            Ok(people)=>{written(whose,&agent,on,Some(turns),taken);queue_request_refresh(people)},
            Err(e)=>tracing::warn!(error=%format!("{e:#}"),"attention activity wasn't recorded"),
        }
    }));
    true
}

/// Only explicit, accepted human responses teach request response times.
pub async fn record_response(hub:&Hub,caller:&Caller,id:&str,key:&str,response:HumanResponse,at:i64)->anyhow::Result<()>{
    let Some(who)=person(caller) else{return Ok(())};
    let id=id.to_owned();let key=key.to_owned();
    let people=hub.store.call(move|c|requests::response(c,&who,&id,&key,&response,at)).await?;
    for principal in people{hub.emit(&crate::wire::events::ServerEvent::Attention{principal});}
    Ok(())
}


/// An archived agent as the server keeps it: its info and state, the group archived with it (the root of its
/// family), and why kiln archived it by itself (none when a person did). Its status is its row's, not kept here.
#[derive(Debug,Clone,Serialize,Deserialize)]
struct Archived {info:Info,state:Option<State>,#[serde(skip)]status:Option<String>,group:String,reason:Option<String>}
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
    let shared=learned::refresh(hub,&who).await.map_err(failed)?;
    let visit=if cached.is_some() || matches!(&mode,SnapshotMode::Refresh(_)){cached}else{
        let key=key.clone().unwrap_or_else(crate::util::new_id);
        let arrival=learned::arrive(std::sync::Arc::clone(&shared),candidates.clone(),settings.sort=="learned",now).await.map_err(failed)?;
        let model_order=arrival.model_a.is_some();
        let visit=examination::Visit{key,at:now,order_a:arrival.ranks_a.iter().map(|r|r.agent.clone()).collect(),order_b:arrival.ranks_b.iter().map(|r|r.agent.clone()).collect(),arm:u8::from(rand::random::<bool>()),ranks_a:arrival.ranks_a,ranks_b:arrival.ranks_b,report:serde_json::to_value(arrival.report).unwrap_or(Value::Null),examination:arrival.examination,model_order,model_a:arrival.model_a,model_b:arrival.model_b};
        let p=who.clone();Some(hub.store.call(move|c|examination::save_visit(c,&p,&visit)).await.map_err(failed)?)
    };
    // Within a visit only the person's own actions move rows: the ones they acted on, scored again by its own draw.
    let mut ranks=match &visit{
        Some(v)=>learned::after_own_actions(std::sync::Arc::clone(&shared),v.ranks().to_vec(),v.drawn().cloned(),v.at,now).await.map_err(failed)?,
        None=>Vec::new(),
    };
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
    Ok(json!({"you":who,"agents":agents,"order":order,"settings":settings,"settings_defaults":{"auto_park_days":store::AUTO_PARK_DAYS_WHEN_ON},"archived":archived,"model":model,"exposure":visit.as_ref().map(|v|&v.key),"pair":pair,"examination":visit.as_ref().map(|v|&v.examination),"request_order":request_order,"urgency":urgency}))
}

/// Keep urgency comparable across the whole Needs you section. Multiple true subagents
/// contribute requests to their parent's row, while commands still answer the actual owner.
async fn request_snapshot(hub:&Hub,who:&str,all:&[Info],accessible:&HashSet<String>,baseline:&[String],agents:&mut BTreeMap<String,Value>,observed:(&str,i64))->anyhow::Result<(Vec<String>,kiln_ci::attention::UrgencyReport)>{
    let (revision,now)=observed;
    let by_id:HashMap<&str,&Info>=all.iter().map(|i|(i.id.as_str(),i)).collect();
    // Only what the store doesn't hold yet (an observation still on its way): what its observer wrote as it changed
    // isn't written again for every visible agent on every snapshot.
    let records:Vec<_>=all.iter().filter_map(|info|{
        let mut row=info;let mut seen=HashSet::new();
        while row.subagent && seen.insert(row.id.as_str()){let Some(parent)=row.parent.as_deref().and_then(|p|by_id.get(p)) else{break;};row=parent;}
        let pending=requests::pending(info);
        unwritten(hub,&info.id,&row.id,None,&pending).then(||(info.id.clone(),row.id.clone(),pending))
    }).collect();
    let principal=who.to_owned();let accessible=accessible.clone();let revision=revision.to_owned();
    let(records,open,responses,people)=if records.is_empty(){
        let(open,responses)=hub.store.read(move|c|requests::history(c,&principal,&accessible,now)).await?;(records,open,responses,Vec::new())
    }else{
        hub.store.call(move|c|{
            let mut people=Vec::new();
            for(agent,row,pending)in &records{people.extend(requests::observe(c,agent,row,pending,&revision,now)?);}
            let(open,responses)=requests::history(c,&principal,&accessible,now)?;people.sort();people.dedup();Ok((records,open,responses,people))
        }).await?
    };
    for(agent,row,pending)in records{written(Arc::downgrade(&hub.cfg),&agent,row,None,pending);}
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
fn quiet_row(info:&Info,all:&[Info])->bool{Families::of(all).quiet(info)}

/// Finished, in step 3's words: nothing of its own runs, no turn, no subagent, shell, monitor or script, and nothing
/// waits on a person.
fn quiet(info:&Info)->bool{
    info.status==Status::Idle && info.approvals.is_empty() && info.held.is_none() && info.turn_started.is_none() && info.tasks_running==0 && info.left_behind==0 && !info.subagents.iter().any(|s|super::model::live_task(&s.status))
}

/// Each agent's true subagents (`Info::subagent`) by its id, found once for a whole list: an agent's row is it and
/// those under it, recursively (step 3; a harness's own subagents are in its info already). A standalone agent another
/// started is a row of its own (step 4).
struct Families<'a>{under:HashMap<&'a str,Vec<&'a Info>>}
impl<'a> Families<'a>{
    fn of(all:&'a [Info])->Self{
        let mut under:HashMap<&'a str,Vec<&'a Info>>=HashMap::new();
        for i in all.iter().filter(|i|i.subagent){if let Some(p)=i.parent.as_deref(){under.entry(p).or_default().push(i);}}
        Self{under}
    }
    /// `root` and every true subagent under it, each once.
    fn family(&self,root:&'a Info)->Vec<&'a Info>{
        let mut out=vec![root];let mut seen=HashSet::from([root.id.as_str()]);let mut next=0;
        while let Some(i)=out.get(next).copied(){
            next+=1;
            for child in self.under.get(i.id.as_str()).into_iter().flatten(){if seen.insert(child.id.as_str()){out.push(child);}}
        }
        out
    }
    fn quiet(&self,root:&'a Info)->bool{self.family(root).into_iter().all(quiet)}
}

pub async fn route(hub:&Arc<Hub>,caller:&Caller,method:&Method,rest:&[&str],req:Request)->Result<Response,Fail>{
    let who=person(caller).ok_or((StatusCode::FORBIDDEN,"attention belongs to a person".into()))?;
    let receipt_at=crate::util::now_ms();
    if method==Method::GET && rest==["replay"]{return replay(hub,caller,&req).await.map(|value|srv::json(StatusCode::OK,&value));}
    if method==Method::GET && rest==["uses"]{return uses_of(hub,caller,who).await.map(|value|srv::json(StatusCode::OK,&value));}
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
    act(hub,caller,who,action,receipt_at).await.map(|value|srv::json(StatusCode::OK,&value))
}

/// Each agent's uses the caller may see, theirs and its own turns (kind, time, and for a turn which end), oldest first (step 1).
async fn uses_of(hub:&Hub,caller:&Caller,who:String)->Result<Value,Fail>{
    let mut visible:HashSet<String>=super::lineage::seen(hub,caller,hub.harness.list()).await.into_iter().map(|a|a.id).collect();
    visible.extend(archive_rows(hub,caller).await?.into_iter().map(|(id,_)|id));
    let p=who.clone();let mut uses=hub.store.read(move|c|store::use_rows(c,&p)).await.map_err(failed)?;uses.retain(|u|visible.contains(&u.agent));
    Ok(json!({"you":who,"uses":uses}))
}

/// A person's action on their attention (`POST /api/harness/attention`), received at `now`.
async fn act(hub:&Arc<Hub>,caller:&Caller,who:String,action:Action,now:i64)->Result<Value,Fail>{
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
        return Ok(json!({"ok":true,"accepted":accepted}));
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
        hub.store.call(move|c|{let tx=c.savepoint()?;for(i,id)in ids.iter().enumerate(){let before=store::placement(&tx,&p,id)?;anyhow::ensure!(before.pinned.is_some(),"only pinned agents can be reordered");let after=Placement{pinned:Some(i64::try_from(i)?.saturating_add(1)),..before.clone()};store::put(&tx,&p,id,&before,&after,false)?;store::record(&tx,&p,id,"command",&format!("reorder:{key}"),now)?;}tx.commit()?;Ok(())}).await.map_err(|e|(StatusCode::CONFLICT,e.to_string()))?;
    }else{
        let id=action.agent.as_deref().filter(|id|!id.is_empty()).ok_or((StatusCode::BAD_REQUEST,"agent is required".into()))?;
        if matches!(action.action.as_str(),"restore"|"restore_copy"){restore(hub,caller,id,action.action=="restore_copy").await?;super::attention_hooks::record(hub,caller,id,"restore",&action_key,now).await;}
        else if action.action!="archive"{super::lineage::may_see(hub,caller,id).await?;}
        if action.action=="archive"{archive(hub,caller,id).await?;super::attention_hooks::record(hub,caller,id,"archive",&action_key,now).await;evidence(hub,caller,&who,id,"archive",&action_key,now).await.map_err(failed)?;}
        else if matches!(action.action.as_str(),"visit"|"seen"){
            let p=who.clone();let id=id.to_owned();let key=action.key.clone().unwrap_or_else(crate::util::new_id);let exposure=action.exposure;let revision=action.frame_revision;let position=action.chosen_position;let source=action.source;
            let is_visit=action.action=="visit";
            let visible=super::lineage::seen(hub,caller,hub.harness.list()).await;
            let row=display_row(&id,&visible,&ROW_LINEAGE.lock());let use_key=row_event_key(&id,&row,"open",&key);
            let direct=if is_visit && source.as_deref()!=Some("list"){row_candidates(&visible)}else{Vec::new()};
            // The position weight from every finished visit before this one (the person's learned examination).
            let prior=if is_visit && source.as_deref()==Some("list"){
                let shared=learned::refresh(hub,&who).await.map_err(failed)?;
                let mut state=shared.lock().await;state.examination()
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
            let deferred=if what=="park"{deferred_requests(&id,&super::lineage::seen(hub,caller,hub.harness.list()).await)}else{Vec::new()};
            let revision=hub.store.call(move|c|{
                let before=store::placement(c,&p,&id)?;
                if what=="undo"{let restored=store::undo(c,&p,&id,expected.ok_or_else(||anyhow::anyhow!("Undo needs its revision"))?,now)?;store::record(c,&p,&id,"command",&format!("undo:{key}"),now)?;return Ok(restored.revision);}
                if let Some(expected)=expected{anyhow::ensure!(before.revision==expected,"placement changed on another device");}
                let after=match (what.as_str(),until){
                    ("pin",_)=>Placement{pinned:Some(now),parked_at:None,parked_until:None,deferred_requests:Vec::new(),reason:None,automatic:false,correction:false,..before.clone()},
                    ("unpin",_)=>Placement{pinned:None,..before.clone()},
                    ("park",Some(until))=>before.parked(now,until,deferred,"You parked it".into(),false),
                    // Step 8: bringing back a chat kiln parked by itself corrects kiln, and its row says so.
                    ("unpark",_) if before.automatic=>before.woke(now,"You brought it back: kiln had parked it automatically".into(),true),
                    ("unpark",_)=>before.woke(now,"You brought it back".into(),false),
                    _=>anyhow::bail!("unknown attention action {what}"),
                };
                let kind=if what=="unpin"{"command"}else{what.as_str()};
                store::record(c,&p,&id,kind,&format!("placement:{key}"),now)?;
                store::put(c,&p,&id,&before,&after,true).map(|p|p.revision)
            }).await.map_err(|e|(StatusCode::CONFLICT,e.to_string()))?;
            if action.action!="undo"{undo=Some(json!({"agent":action.agent,"revision":revision}));}
            if let Some(agent)=action.agent.as_deref(){evidence(hub,caller,&who,agent,&action.action,&action_key,now).await.map_err(failed)?;}
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
    Ok(result)
}

/// Whether automation must leave an agent alone, including an archive not yet acknowledged.
pub async fn is_archived(hub:&Hub,id:&str)->anyhow::Result<bool>{
    let id=id.to_owned();
    hub.store.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM attention_archive WHERE agent=?1)",[id],|r|r.get(0))?)).await
}

type ArchiveLock=Arc<futures::lock::Mutex<()>>;
/// One archive or restore of a group at a time, by its root: held only while one is under way.
static ARCHIVE_LOCKS:LazyLock<Mutex<BTreeMap<String,ArchiveLock>>>=LazyLock::new(||Mutex::new(BTreeMap::new()));
fn archive_lock(id:&str)->ArchiveLock{Arc::clone(ARCHIVE_LOCKS.lock().entry(id.into()).or_insert_with(||Arc::new(futures::lock::Mutex::new(()))))}
/// The operation on `id`'s group is over: its lock goes unless another operation holds or waits on it.
fn archive_unlock(id:&str,lock:ArchiveLock){
    drop(lock);
    let mut locks=ARCHIVE_LOCKS.lock();
    if locks.get(id).is_some_and(|held|Arc::strong_count(held)==1){locks.remove(id);}
}

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
    let root=saved.get(id).map_or_else(||id.to_owned(),|a|a.group.clone());
    let operation=archive_lock(&root);
    let result=async{
        let _held=operation.lock().await;
        let saved=archive_rows(hub,caller).await?.into_iter().collect();
        archive_locked(hub,caller,&root,saved,reason.as_deref(),automatic).await
    }.await;
    archive_unlock(&root,operation);
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
            Archived{info,state:hub.harness.state(id),status:None,group:root.to_owned(),reason:reason.map(str::to_owned)}
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
    let root=data.group.clone();
    let operation=archive_lock(&root);
    let result=async{
        let _held=operation.lock().await;
        let rows=archive_rows(hub,caller).await?.into_iter().collect();
        restore_locked(hub,caller,&root,rows,from_copy).await
    }.await;
    archive_unlock(&root,operation);
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
    let key=root.to_owned();hub.store.call(move|c|{let tx=c.savepoint()?;tx.execute("UPDATE attention_archive_groups SET status='restoring' WHERE root=?1",[&key])?;if let Some((ids,_))=store::archive_members(&tx,&key)?{for id in ids{tx.execute("UPDATE attention_archive SET status='restoring' WHERE agent=?1",[id])?;}}tx.commit()?;Ok(())}).await.map_err(failed)?;
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

/// The requests the person sees on `root`'s row as they park it, which the park defers (step 6).
fn deferred_requests(root:&str,all:&[Info])->Vec<DeferredRequest>{
    let Some(root)=all.iter().find(|i|i.id==root) else{return Vec::new();};
    let mut captured:Vec<_>=Families::of(all).family(root).into_iter().flat_map(requests::identities).filter_map(|identity|identity.request).collect();
    captured.sort();captured.dedup();captured
}

/// Why `info`'s current requests end a park, if they do: the first the person didn't defer as they parked it, in
/// words (step 6). `stop`: its stops with an error rather than its requests.
fn park_wake(info:&Info,placement:&Placement,stop:bool)->Option<&'static str>{
    let current:Vec<_>=requests::identities(info).into_iter().filter(|r|r.stop==stop).collect();
    let unknown=current.iter().any(|r|r.request.is_none());
    let all_deferred=current.iter().filter_map(|r|r.request.as_ref()).all(|r|placement.deferred_requests.contains(r));
    if !kiln_ci::attention_lifecycle::wake_park_requests(placement.parked_until.is_some(),unknown,all_deferred){return None;}
    current.iter().find(|r|r.request.as_ref().is_none_or(|d|!placement.deferred_requests.contains(d))).map(|r|r.says)
}

/// Whether a person other than `principal` used any of `members` at or after `since`.
fn used_by_another(c:&rusqlite::Connection,principal:&str,members:&[&Info],since:Option<i64>)->anyhow::Result<bool>{
    let ids:Vec<&str>=members.iter().map(|i|i.id.as_str()).collect();
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1)) AND principal<>'' AND principal<>?2 AND at>=?3)",params![serde_json::to_string(&ids)?,principal,since],|r|r.get(0))?)
}

/// The person's rows' placements as they stand at `now` (steps 6 and 8). A park ends when a request it didn't defer
/// comes, the chat stops with a new error, or its time comes, saying which; a park that runs to the archive deadline is
/// archived then instead, when that's due (`maintain`). A quiet row is parked automatically, saying why, once
/// `auto_park_days` pass with nothing between the person and it since its last use or its last park's end
/// (`store::idle_since`), until `store::automatic_park_ends`.
async fn refresh_placements(hub:&Hub,who:&str,all:&[Info],settings:&Settings,now:i64)->anyhow::Result<BTreeMap<String,Placement>>{
    let p=who.to_owned();let a=all.to_vec();let settings=settings.clone();
    hub.store.call(move|c|{
        let mut rows=store::placements(c,&p)?;
        let archived:HashSet<String>=c.prepare("SELECT agent FROM attention_archive")?.query_map([],|r|r.get(0))?.collect::<Result<_,_>>()?;
        let families=Families::of(&a);
        let listed:HashSet<&str>=a.iter().map(|i|i.id.as_str()).collect();
        for info in &a{
            // A true subagent is on its parent's row (step 4): not a row of its own to place.
            if archived.contains(&info.id) || (info.subagent && info.parent.as_deref().is_some_and(|parent|listed.contains(parent))){continue;}
            let before=rows.get(&info.id).cloned().unwrap_or_default();
            let members=families.family(info);
            let row_quiet=members.iter().all(|i|quiet(i));
            if let Some(until)=before.parked_until{
                let asks=members.iter().find_map(|i|park_wake(i,&before,false));
                let stopped=members.iter().find_map(|i|park_wake(i,&before,true));
                let ended=until<=now;
                let to_archive=before.parked_at.is_some_and(|at|until>=at.saturating_add(i64::from(settings.archive_after_days).saturating_mul(store::DAY_MS)));
                let archiving=ended && to_archive && kiln_ci::attention_lifecycle::archive_due(settings.auto_archive,settings.archive_after_days,before.parked_at,now,row_quiet,false) && !used_by_another(c,&p,&members,before.parked_at)?;
                if let Some(why)=asks.or(stopped).or((ended && !archiving).then_some("Its park time ended")){
                    rows.insert(info.id.clone(),store::put(c,&p,&info.id,&before,&before.woke(now,why.into(),false),false)?);
                }
            }else if let Some(days)=settings.auto_park_days && before.pinned.is_none() && row_quiet{
                let since=store::idle_since(c,&p,&info.id,&before)?;
                if kiln_ci::attention_lifecycle::park_due(Some(days),since,now,row_quiet,false,false){
                    let back=if before.woke_at.is_some() && since==before.woke_at{" since it came back"}else{""};
                    let after=before.parked(now,store::automatic_park_ends(&settings,days,now),Vec::new(),format!("Automatically parked: no interaction or turns for {days} days{back}"),true);
                    rows.insert(info.id.clone(),store::put(c,&p,&info.id,&before,&after,true)?);
                }
            }
        }
        Ok(rows)
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
        let families=Families::of(&all);
        for info in &all{
            let Some(placed)=rows.get(&info.id) else{continue;};
            if !settings.auto_archive || placed.parked_at.is_none(){continue;}
            let members=family_ids(&info.id,&all);let ids=members.clone();let person=who.clone();let parked_at=placed.parked_at;
            let(other_used,usage)=hub.store.call(move|c|Ok((c.query_row("SELECT EXISTS(SELECT 1 FROM attention_uses WHERE agent IN (SELECT value FROM json_each(?1)) AND principal<>'' AND principal<>?2 AND at>=?3)",params![serde_json::to_string(&ids)?,person,parked_at],|r|r.get::<_,bool>(0))?,use_revision(c,&ids)?))).await?;
            if kiln_ci::attention_lifecycle::archive_due(settings.auto_archive,settings.archive_after_days,placed.parked_at,now,families.quiet(info),other_used){
                // Permission is re-evaluated on every attempt; no park grants command authority.
                if super::lineage::may_steer(hub,&caller,&info.id).await.is_ok() && let Some(parked_at)=placed.parked_at{
                    let expected=AutoIntent{principal:who.clone(),revision:placed.revision,parked_at,use_revision:usage,members,days:settings.archive_after_days};
                    if let Err((_,why))=archive_checked(hub,&caller,&info.id,Some(format!("Automatically archived after {} days parked",settings.archive_after_days)),Some(expected)).await{tracing::warn!(agent=%info.id,%why,"automatic archive was not confirmed");}
                }
            }
        }
        let fresh=super::lineage::seen(hub,&caller,hub.harness.list()).await;
        let rows=refresh_placements(hub,&who,&fresh,&settings,crate::util::now_ms()).await?;
        let families=Families::of(&fresh);
        let listed:HashSet<&str>=fresh.iter().map(|i|i.id.as_str()).collect();
        for info in fresh.iter().filter(|i|!(i.subagent && i.parent.as_deref().is_some_and(|parent|listed.contains(parent)))){
            let p=rows.get(&info.id).cloned().unwrap_or_default();
            if let Some(until)=p.parked_until && until>now{next=Some(next.map_or(until,|t|t.min(until)));}
            let quiet=families.quiet(info);
            if settings.auto_archive && quiet && let Some(at)=p.parked_at{
                let due=at.saturating_add(i64::from(settings.archive_after_days).saturating_mul(store::DAY_MS));
                if due>now{next=Some(next.map_or(due,|t|t.min(due)));}
            }
            if quiet && p.pinned.is_none() && p.parked_until.is_none() && let Some(days)=settings.auto_park_days{
                let (id,person)=(info.id.clone(),who.clone());
                if let Some(since)=hub.store.read(move|c|store::idle_since(c,&person,&id,&p)).await?{
                    let due=since.saturating_add(i64::from(days).saturating_mul(store::DAY_MS));
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
    let at=req.uri().query().into_iter().flat_map(|q|q.split('&')).find_map(|q|q.strip_prefix("at=")).and_then(|s|s.parse::<i64>().ok()).filter(|at|*at>=0).ok_or((StatusCode::BAD_REQUEST,"at is a Unix time in milliseconds".into()))?;
    // A time past the server's clock is now (a page's clock may run a little ahead): nothing has happened since.
    let at=at.min(crate::util::now_ms());
    let who=person(caller).ok_or((StatusCode::FORBIDDEN,"attention belongs to a person".into()))?;
    let all=super::lineage::seen(hub,caller,hub.harness.list()).await;
    let rows=row_candidates(&all);
    let shared=learned::refresh(hub,&who).await.map_err(failed)?;
    let mut value=learned::replay(hub,shared,&who,at,rows).await.map_err(failed)?;
    let accessible:HashSet<String>=all.iter().map(|i|i.id.clone()).collect();
    let p=who.clone();let (open,responses)=hub.store.call(move|c|requests::history(c,&p,&accessible,at)).await.map_err(failed)?;
    let visible=all.iter().map(|i|i.id.clone()).collect();
    if let Some(o)=value.as_object_mut(){o.insert("urgency".into(),json!(kiln_ci::attention::urgency(&requests::waits(&open,&visible),&responses,at)));}
    Ok(value)
}

/// A pin, a park or an archive, recorded as evidence of the person's attention among the chats in their list then
/// (attention plan 01a199e1b177752e84dad5dc990e6a15, step 5): a best choice (pin) or a worst (park, archive).
async fn evidence(hub:&Hub,caller:&Caller,who:&str,id:&str,action:&str,key:&str,at:i64)->anyhow::Result<()>{
    let kind=match action{"pin"=>kiln_ci::attention::Evidence::Pin,"park"=>kiln_ci::attention::Evidence::Park,"archive"=>kiln_ci::attention::Evidence::Archive,_=>return Ok(())};
    let all=super::lineage::seen(hub,caller,hub.harness.list()).await;
    let mut candidates=row_candidates(&all);
    if !candidates.iter().any(|c|c==id){candidates.push(id.to_owned());}
    let(p,id,key)=(who.to_owned(),id.to_owned(),key.to_owned());
    hub.store.call(move|c|learned::record_evidence(c,&p,&id,kind,&key,&candidates,at)).await?;
    Ok(())
}

#[cfg(test)]
mod tests{
    use super::*;
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
            hub.store.call(move|c|{let before=store::placement(c,"owner","parked")?;store::put(c,"owner","parked",&before,&Placement{parked_at:Some(20),parked_until:Some(100),deferred_requests:captured,..before.clone()},false)?;Ok(())}).await?;
            let existing=refresh_placements(&hub,"owner",&[info.clone()],&Settings::default(),30).await?;
            assert_eq!(existing.get("parked").and_then(|p|p.parked_until),Some(100));
            info.approvals=vec![approval(40)];
            let fresh=refresh_placements(&hub,"owner",&[info.clone()],&Settings::default(),50).await?;
            assert!(fresh.get("parked").is_some_and(|p|p.parked_until.is_none() && p.deferred_requests.is_empty()));
            assert_eq!(fresh.get("parked").and_then(|p|p.reason.as_deref()),Some("It asks you something"),"the new question brings it back saying so");
            let parked=Placement{parked_at:Some(20),parked_until:Some(100),..Placement::default()};
            info.approvals=vec![approval(0)];assert!(park_wake(&info,&parked,false).is_some());
            info.approvals.clear();assert_eq!(park_wake(&info,&parked,false),Some("It waits for you"));Ok(())
        })
    }
    #[test]
    fn mc_attention_park_request_identity_is_independent_of_runner_clock(){
        use super::super::session::ApprovalBrief;
        let approval=|id:&str,at|ApprovalBrief{id:id.into(),tool:super::super::model::ToolKind::Question,tool_name:"question".into(),title:"Choose".into(),at,agent:None};
        let old=Info{id:"parent".into(),status:Status::Waiting,approvals:vec![approval("existing",1000)],..Info::default()};
        let parked=Placement{parked_at:Some(20),parked_until:Some(2000),deferred_requests:deferred_requests("parent",std::slice::from_ref(&old)),..Placement::default()};
        assert!(park_wake(&old,&parked,false).is_none(),"captured current request stays deferred even when its runner clock is ahead");
        let older=Info{approvals:vec![approval("new",5)],..old.clone()};
        assert!(park_wake(&older,&parked,false).is_some(),"a new request wakes even when the runner timestamp is behind");
        let equal=Info{approvals:vec![approval("new",1000)],..old.clone()};
        assert!(park_wake(&equal,&parked,false).is_some(),"a different request in the same millisecond is new");
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
        let parked=Placement{parked_at:Some(20),parked_until:Some(2000),deferred_requests:captured,..Placement::default()};
        assert!(park_wake(&held,&parked,false).is_none());assert!(park_wake(&stop,&parked,true).is_none());
        let new_held=Info{held:Some(Held{why:"Review another thing".into(),plan:None,since:1000}),..held};
        let new_stop=Info{exit:Some(ExitInfo{error:Some("Another error".into()),at:5,..ExitInfo::default()}),..stop};
        assert_eq!(park_wake(&new_held,&parked,false),Some("Its goals wait for you"));assert_eq!(park_wake(&new_stop,&parked,true),Some("It stopped with an error"));
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
            let saved=Archived{info:Info{id:"automatic".into(),name:"Automatic".into(),..Info::default()},state:None,status:None,group:"automatic".into(),reason:Some(why.into())};
            let payload=serde_json::to_string(&saved)?;
            assert_eq!(serde_json::from_str::<Archived>(&payload)?.reason.as_deref(),Some(why));
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
        let mut c=rusqlite::Connection::open_in_memory()?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;
        store::save_settings(&c,"owner",&Settings{auto_archive:true,archive_after_days:1,..Settings::default()})?;
        let before=store::placement(&c,"owner","root")?;let parked=store::put(&c,"owner","root",&before,&Placement{parked_at:Some(1),parked_until:Some(200_000_000),..before.clone()},false)?;
        let expected=AutoIntent{principal:"owner".into(),revision:parked.revision,parked_at:1,use_revision:0,members:vec!["root".into(),"sub".into()],days:1};
        store::record(&c,"owner","sub","message","fresh",1)?;
        let members=vec![("root".into(),"owner".into(),"payload".into())];
        assert!(store::begin_archive_checked(&mut c,"root",&members,100_000_000,|tx|admit_auto(tx,"root",&expected,100_000_000)).is_err(),"same-clock-time child message still changes the append-only revision");
        assert!(store::archive_members(&c,"root")?.is_none());assert_eq!(store::placement(&c,"owner","root")?.revision,parked.revision);
        let expected=AutoIntent{use_revision:use_revision(&c,&expected.members)?,..expected};
        let current=store::placement(&c,"owner","root")?;store::put(&c,"owner","root",&current,&Placement{parked_at:None,parked_until:None,..current.clone()},false)?;
        assert!(store::begin_archive_checked(&mut c,"root",&members,100_000_000,|tx|admit_auto(tx,"root",&expected,100_000_000)).is_err());assert!(store::archive_members(&c,"root")?.is_none());Ok(())
    }
    #[test]
    fn mc_attention_declined_fresh_auto_archive_removes_only_its_unstarted_intent()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;let dir=tempfile::tempdir()?;let id="automatic-fresh";
            let session=hub.harness.local.open(Some(id.into()),super::super::session::Spec{kind:"claude".into(),program:Some(dir.path().join("no-provider").to_string_lossy().into_owned()),cwd:Some(dir.path().to_string_lossy().into_owned()),..Default::default()})?;
            let info=Info{id:id.into(),name:"Fresh work".into(),kind:"claude".into(),status:Status::Idle,..Info::default()};
            hub.harness.ingest(&super::super::registry::Location::Local,id,super::super::core::Update::Open{info:Box::new(info),since:None}).await;
            let parked=hub.store.call(move|c|{store::save_settings(c,"owner",&Settings{auto_archive:true,archive_after_days:1,..Settings::default()})?;let before=store::placement(c,"owner",id)?;store::put(c,"owner",id,&before,&Placement{parked_at:Some(1),parked_until:Some(i64::MAX),..before.clone()},false)}).await?;
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
            hub.store.call(|c|{let before=store::placement(c,"owner","timer")?;store::put(c,"owner","timer",&before,&Placement{parked_at:Some(1),parked_until:Some(100),..before.clone()},false)?;Ok(())}).await?;
            let rows=refresh_placements(&hub,"owner",std::slice::from_ref(&info),&settings,100_000_000).await?;assert!(rows.get("timer").is_some_and(|p|p.parked_until.is_none()));
            hub.store.call(|c|{let before=store::placement(c,"owner","timer")?;store::put(c,"owner","timer",&before,&Placement{parked_at:Some(1),parked_until:Some(86_400_001),..before.clone()},false)?;Ok(())}).await?;
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
            hub.store.call(|c|{let before=store::placement(c,"owner","row-root")?;store::put(c,"owner","row-root",&before,&Placement{parked_at:Some(1),parked_until:Some(1000),..before.clone()},false)?;Ok(())}).await?;
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
            let saved=Archived{info,state:None,status:None,group:"restore-contract".into(),reason:None};
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
            assert!(!ARCHIVE_LOCKS.lock().contains_key("parent"),"the operation's lock goes with it");
            Ok(())
        })
    }
    /// A row is quiet only while nothing of its own runs: a kiln subagent's owned Reach command keeps it from quiet.
    #[test]
    fn mc_attention_a_subagents_running_owned_work_keeps_its_row_from_quiet(){
        assert!(quiet(&Info{status:Status::Idle,..Info::default()}),"an idle agent with nothing running is quiet");
        let parent=Info{id:"parent".into(),status:Status::Idle,..Info::default()};
        let child=Info{id:"child".into(),parent:Some("parent".into()),subagent:true,status:Status::Idle,..Info::default()};
        assert!(quiet_row(&parent,&[parent.clone(),child.clone()]),"a quiet subagent leaves its row quiet");
        let owned=crate::harness::model::State{status:Status::Idle,tasks:vec![crate::harness::model::Task{id:format!("{}run",crate::reach::owned::PREFIX),kind:"reach".into(),class:Some("shell".into()),status:"running".into(),background:true,..Default::default()}],..Default::default()};
        let mut working=child;working.apply(&owned);
        assert_eq!((working.tasks_running,working.left_behind),(1,1),"its owned command is work of its own");
        assert!(!quiet_row(&parent,&[parent.clone(),working]),"so its row isn't quiet, and isn't parked or archived by itself");
    }
    #[test]
    fn mc_attention_a_real_kiln_subagent_prevents_automatic_parking(){
        let parent=Info{id:"parent".into(),status:Status::Idle,..Info::default()};
        let sub=Info{id:"sub".into(),parent:Some("parent".into()),subagent:true,status:Status::Running,..Info::default()};
        let independent=Info{id:"independent".into(),parent:Some("parent".into()),status:Status::Running,..Info::default()};
        assert!(!quiet_row(&parent,&[parent.clone(),sub]));
        assert!(quiet_row(&parent,&[parent.clone(),independent]));
    }

    async fn ingest(hub:&Hub,id:&str,u:super::super::core::Update){
        hub.harness.ingest(&super::super::registry::Location::Machine("box".into()),id,u).await;
    }
    fn opened(info:Info)->super::super::core::Update{super::super::core::Update::Open{info:Box::new(info),since:None}}
    fn stated(state:State)->super::super::core::Update{super::super::core::Update::State{state:Box::new(state)}}
    fn question(id:&str,at:i64)->anyhow::Result<super::super::model::Approval>{
        Ok(serde_json::from_value(json!({"id":id,"tool_id":null,"tool_name":"AskUserQuestion","tool":"question","title":"Which database?","input":{},"cwd":"/w","at":at}))?)
    }
    fn running(id:&str,class:&str)->super::super::model::Task{
        super::super::model::Task{id:id.into(),kind:class.into(),class:Some(class.into()),status:"running".into(),background:class!="agent",started:1,..Default::default()}
    }
    async fn acted(hub:&Arc<Hub>,action:Value,now:i64)->anyhow::Result<Value>{
        act(hub,&Caller::owner(),"owner".into(),serde_json::from_value(action)?,now).await.map_err(|(code,why)|anyhow::anyhow!("{code}: {why}"))
    }
    async fn placed(hub:&Hub,id:&str)->anyhow::Result<Placement>{let id=id.to_owned();hub.store.call(move|c|store::placement(c,"owner",&id)).await}

    /// Step 3: every combination of its turn, a harness subagent, a kiln subagent, a shell and a monitor gives the
    /// status the plan defines, in a notification's words (`push::watch`) and in what automatic parking and archive
    /// take for quiet: Working while its turn or a subagent on its row is at work; Ready while neither is but its
    /// shells or monitors run; Finished when nothing of its own runs.
    #[test]
    fn mc_attention_step3_every_combination_of_its_work_says_working_ready_or_finished(){
        use crate::push::watch::{Kind,for_display,line};
        for case in 0u8..32{
            let [turn,harness_subagent,kiln_subagent,shell,monitor]=[0u8,1,2,3,4].map(|bit|case&(1<<bit)!=0);
            let mut tasks=Vec::new();
            if harness_subagent{tasks.push(running("native-subagent","agent"));}
            if shell{tasks.push(running("shell","shell"));}
            if monitor{tasks.push(running("monitor","monitor"));}
            let mut parent=Info{id:"parent".into(),..Info::default()};
            parent.apply(&State{status:if turn{Status::Running}else{Status::Idle},turn_started:turn.then_some(1),summary:"Done".into(),tasks,..Default::default()});
            let mut subagent=Info{id:"kiln-subagent".into(),parent:Some("parent".into()),subagent:true,..Info::default()};
            subagent.apply(&State{status:if kiln_subagent{Status::Running}else{Status::Idle},turn_started:kiln_subagent.then_some(1),..Default::default()});
            let all=[parent.clone(),subagent];
            let expected=if turn || harness_subagent || kiln_subagent{"Working"}else if shell || monitor{"Ready"}else{"Finished"};
            let case=format!("turn {turn}, harness subagent {harness_subagent}, kiln subagent {kiln_subagent}, shell {shell}, monitor {monitor}");
            assert_eq!(line(&for_display(&parent,&all),Kind::Finished),format!("{expected}: Done"),"{case}");
            assert_eq!(quiet_row(&parent,&all),expected=="Finished","{case}");
        }
    }

    /// Step 4: an agent starts a standalone agent through kiln's MCP/API, and has a harness-native subagent and a
    /// kiln subagent. The standalone agent is a row of its own, with its own status and requests; both subagents stay
    /// on the parent's row and count toward its status, and neither becomes a row.
    #[test]
    fn mc_attention_step4_a_standalone_agent_is_its_own_row_and_subagents_stay_on_their_parents()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use crate::push::watch::{Kind,for_display,line};
            // Through kiln's API, as the agent's MCP tool asks it: what the agent starts stands on its own.
            let hub=crate::hub::tests::scratch_hub_with(|c|{c.terminal.enabled=true;}).await?;
            let h=Arc::clone(&hub);crate::rt::spawn(async move{let _=super::super::registry::Registry::run_local(h).await;});
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir=tempfile::tempdir()?;let cwd=dir.path().to_string_lossy().into_owned();
            let spec=super::super::session::Spec{kind:"claude".into(),cwd:Some(cwd.clone()),mode:Some("default".into()),allow_bypass:true,program:Some(dir.path().join("no-claude").to_string_lossy().into_owned()),..Default::default()};
            let starter=hub.harness.open(&hub,"local",spec,1).await?.first().map(|i|i.id.clone()).ok_or_else(||anyhow::anyhow!("no agent"))?;
            // Its machine reports its mode, which what it starts is held within.
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
            while !hub.harness.info(&starter).is_some_and(|i|i.mode=="default"){
                anyhow::ensure!(std::time::Instant::now()<deadline,"the starting agent's mode never showed");
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            let caller=crate::mcp::session_caller(&hub,&starter,None).await.map_err(anyhow::Error::msg)?;
            let started=crate::ops::call(crate::ops::Cx{hub:Arc::clone(&hub),caller},"agents.start",json!({"kind":"claude","cwd":cwd,"prompt":"Check the parser"})).await.map_err(|(code,why)|anyhow::anyhow!("{code}: {why}"))?;
            let standalone=started.pointer("/agents/0/id").and_then(Value::as_str).ok_or_else(||anyhow::anyhow!("nothing started: {started}"))?.to_owned();
            let info=hub.harness.info(&standalone).ok_or_else(||anyhow::anyhow!("the started agent isn't known"))?;
            assert_eq!(info.parent.as_deref(),Some(starter.as_str()));assert!(!info.subagent,"an agent an agent starts through kiln's API isn't its subagent");
            let rows=row_candidates(&hub.harness.list());assert!(rows.contains(&standalone) && rows.contains(&starter),"both are rows: {rows:?}");
            // Its rows, statuses and requests, as the snapshot gives them.
            let hub=crate::hub::tests::scratch_hub().await?;
            let parent=Info{id:"parent".into(),name:"Parent".into(),kind:"claude".into(),status:Status::Idle,..Info::default()};
            let kiln=Info{id:"kiln-subagent".into(),name:"Its subagent".into(),kind:"claude".into(),parent:Some("parent".into()),subagent:true,status:Status::Idle,..Info::default()};
            let alone=Info{id:"standalone".into(),name:"Standalone".into(),kind:"claude".into(),parent:Some("parent".into()),subagent:false,status:Status::Idle,..Info::default()};
            for info in [parent,kiln,alone]{let id=info.id.clone();ingest(&hub,&id,opened(info)).await;}
            let now=crate::util::now_ms();
            ingest(&hub,"parent",stated(State{status:Status::Idle,tasks:vec![running("native-subagent","agent")],..Default::default()})).await;
            ingest(&hub,"kiln-subagent",stated(State{status:Status::Waiting,approvals:vec![question("kiln-asks",now-1)?],..Default::default()})).await;
            ingest(&hub,"standalone",stated(State{status:Status::Waiting,approvals:vec![question("standalone-asks",now-1)?],..Default::default()})).await;
            let snapshot=snapshot(&hub,&Caller::owner()).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            let order:Vec<&str>=snapshot.get("order").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            assert!(order.contains(&"parent") && order.contains(&"standalone") && !order.contains(&"kiln-subagent"),"rows: {order:?}");
            let mut asking:Vec<&str>=snapshot.get("request_order").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();asking.sort_unstable();
            assert_eq!(asking,vec!["parent","standalone"],"the kiln subagent asks on its parent's row; the standalone agent on its own");
            let all=hub.harness.list();
            let of=|id:&str|all.iter().find(|i|i.id==id).cloned().ok_or_else(||anyhow::anyhow!("no {id}"));
            assert!(line(&for_display(&of("parent")?,&all),Kind::Finished).starts_with("Working"),"both subagents count toward their parent's status");
            assert!(!quiet_row(&of("parent")?,&all));
            // Its own work is the standalone agent's alone: the parent, its subagents settled, is Finished beside it.
            ingest(&hub,"parent",stated(State{status:Status::Idle,..Default::default()})).await;
            ingest(&hub,"kiln-subagent",stated(State{status:Status::Idle,..Default::default()})).await;
            ingest(&hub,"standalone",stated(State{status:Status::Running,turn_started:Some(now),..Default::default()})).await;
            let all=hub.harness.list();
            let of=|id:&str|all.iter().find(|i|i.id==id).cloned().ok_or_else(||anyhow::anyhow!("no {id}"));
            assert!(line(&for_display(&of("parent")?,&all),Kind::Finished).starts_with("Finished"));
            assert!(line(&for_display(&of("standalone")?,&all),Kind::Finished).starts_with("Working"));
            assert!(quiet_row(&of("parent")?,&all) && !quiet_row(&of("standalone")?,&all));
            Ok(())
        })
    }

    /// Step 6: a chat parked until tomorrow morning asks a question tonight. It comes back at once, no longer parked,
    /// saying it asks you something, and is in Needs you.
    #[test]
    fn mc_attention_step6_parked_until_morning_it_asks_tonight_and_comes_back_at_once_saying_so()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let id="evening-chat";
            ingest(&hub,id,opened(Info{id:id.into(),name:"Evening".into(),kind:"claude".into(),status:Status::Idle,..Info::default()})).await;
            let now=crate::util::now_ms();let morning=now+12*3_600_000;
            let parked=acted(&hub,json!({"action":"park","agent":id,"until":morning}),now).await?;
            assert_eq!(parked.pointer(&format!("/agents/{id}/parked_until")),Some(&json!(morning)));
            // Tonight it asks: its state reaches the registry, which observes it, and the maintenance pass that wakes.
            ingest(&hub,id,stated(State{status:Status::Waiting,approvals:vec![question("tonight",now)?],..Default::default()})).await;
            maintain(&hub).await?;
            let p=placed(&hub,id).await?;
            assert!(p.parked_until.is_none() && p.parked_at.is_none(),"no longer parked, hours before its time: {p:?}");
            assert_eq!(p.reason.as_deref(),Some("It asks you something"));
            let snapshot=snapshot(&hub,&Caller::owner()).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
            assert!(snapshot.pointer(&format!("/agents/{id}/parked_until")).is_none());
            assert_eq!(snapshot.get("request_order"),Some(&json!([id])),"and it's in Needs you");
            Ok(())
        })
    }

    /// Step 6: five chats, each placed its own way, are parked, then each park undone: each is placed exactly as it
    /// was. Its revision moves on (park, then undo), so a device still showing the park can't act on it.
    #[test]
    fn mc_attention_step6_five_parked_chats_undone_return_to_their_prior_placement()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let now=crate::util::now_ms();
            let priors=[
                ("pinned-chat",Placement{pinned:Some(2),..Placement::default()}),
                ("seen-chat",Placement{updated_seen:Some(now-60_000),..Placement::default()}),
                ("woken-chat",Placement{reason:Some("Its park time ended".into()),woke_at:Some(now-3_600_000),updated_seen:Some(now-120_000),..Placement::default()}),
                ("corrected-chat",Placement{reason:Some("You brought it back: kiln had parked it automatically".into()),correction:true,woke_at:Some(now-7_200_000),..Placement::default()}),
                ("untouched-chat",Placement::default()),
            ];
            let mut before=Vec::new();
            for(id,prior)in priors{
                ingest(&hub,id,opened(Info{id:id.into(),name:id.into(),status:Status::Idle,..Info::default()})).await;
                let prior=if prior==Placement::default(){prior}else{hub.store.call(move|c|{let none=store::placement(c,"owner",id)?;store::put(c,"owner",id,&none,&prior,false)}).await?};
                before.push((id,prior));
            }
            let until=now+12*3_600_000;
            let mut undos=Vec::new();
            for(id,prior)in &before{
                let snapshot=acted(&hub,json!({"action":"park","agent":id,"until":until,"revision":prior.revision}),now).await?;
                let undo=snapshot.pointer("/undo/revision").and_then(Value::as_i64).ok_or_else(||anyhow::anyhow!("no Undo for {id}"))?;
                let p=placed(&hub,id).await?;
                assert_eq!((p.parked_until,p.pinned,p.revision),(Some(until),None,undo),"{id} is parked");
                undos.push((*id,prior.clone(),undo));
            }
            for(id,prior,undo)in undos{
                acted(&hub,json!({"action":"undo","agent":id,"revision":undo}),now).await?;
                let p=placed(&hub,id).await?;
                assert_eq!(Placement{revision:prior.revision,..p.clone()},prior,"{id} is placed as it was");
                assert_eq!(p.revision,prior.revision+2,"{id}'s revision moved on");
                assert!(acted(&hub,json!({"action":"undo","agent":id,"revision":undo}),now).await.is_err(),"a second Undo of the same park changes nothing");
            }
            Ok(())
        })
    }

    /// Step 8: automatic parking on, with chats untouched past the setting, one with a monitor running. Those with
    /// nothing running are parked, each saying why; the one with a monitor isn't. Unparking one is a correction.
    #[test]
    fn mc_attention_step8_quiet_chats_park_by_themselves_saying_why_and_unparking_one_is_a_correction()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let settings=Settings{auto_park_days:Some(3),auto_archive:false,..Settings::default()};
            let s=settings.clone();hub.store.call(move|c|store::save_settings(c,"owner",&s)).await?;
            let now=crate::util::now_ms();
            let mut chats=Vec::new();
            for id in ["quiet-a","quiet-b","watching"]{
                let mut info=Info{id:id.into(),name:id.into(),status:Status::Idle,..Info::default()};
                if id=="watching"{info.apply(&State{status:Status::Idle,tasks:vec![running("watch","monitor")],..Default::default()});}
                ingest(&hub,id,opened(info.clone())).await;
                chats.push(info);
                // Its person's last word and its last turn's end: five days ago.
                hub.store.call(move|c|{store::record(c,"owner",id,"message",&format!("{id}:asked"),now-5*store::DAY_MS)?;store::record_edge(c,"",id,"turn",&format!("{id}:end"),now-5*store::DAY_MS+1,Some(store::Edge::End))?;Ok(())}).await?;
            }
            let rows=refresh_placements(&hub,"owner",&chats,&settings,now).await?;
            for id in ["quiet-a","quiet-b"]{
                let p=rows.get(id).ok_or_else(||anyhow::anyhow!("{id} has no placement"))?;
                assert!(p.automatic && p.parked_at==Some(now),"{id} is parked by kiln: {p:?}");
                assert_eq!(p.parked_until,Some(now+3*store::DAY_MS),"automatic archive off: it comes back after as long as the quiet that parked it");
                assert_eq!(p.reason.as_deref(),Some("Automatically parked: no interaction or turns for 3 days"));
            }
            assert!(rows.get("watching").is_none_or(|p|p.parked_until.is_none()),"its monitor is work of its own: not parked");
            let revision=rows.get("quiet-a").map(|p|p.revision);
            let snapshot=acted(&hub,json!({"action":"unpark","agent":"quiet-a","revision":revision}),now+1).await?;
            let p=placed(&hub,"quiet-a").await?;
            assert!(p.correction && !p.automatic && p.parked_until.is_none(),"{p:?}");
            assert_eq!(p.reason.as_deref(),Some("You brought it back: kiln had parked it automatically"));
            assert_eq!(snapshot.pointer("/agents/quiet-a/correction"),Some(&json!(true)),"Mission Control is told it's a correction");
            assert_eq!(snapshot.pointer("/agents/quiet-b/automatic"),Some(&json!(true)));
            // A person's own park, brought back, corrects nothing.
            let revision=snapshot.pointer("/agents/watching/revision").and_then(Value::as_i64).unwrap_or(0);
            acted(&hub,json!({"action":"park","agent":"watching","until":now+store::DAY_MS,"revision":revision}),now+2).await?;
            let revision=placed(&hub,"watching").await?.revision;
            acted(&hub,json!({"action":"unpark","agent":"watching","revision":revision}),now+3).await?;
            let p=placed(&hub,"watching").await?;assert!(!p.correction);assert_eq!(p.reason.as_deref(),Some("You brought it back"));
            Ok(())
        })
    }

    /// Step 8: a park always ends, and its end restarts the quiet that parks a chat by itself. With automatic archive
    /// off, a chat kiln parked comes back when its time comes, saying so, and isn't parked again until the setting's
    /// days pass from its return. With it on, the park lasts until the archive: a quiet chat is left for maintenance
    /// to archive then, and one with work of its own running comes back instead.
    #[test]
    fn mc_attention_step8_an_automatic_park_ends_and_its_return_restarts_the_quiet()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let day=store::DAY_MS;let t0=100*day;
            for id in ["off-chat","on-chat"]{hub.store.call(move|c|{store::record(c,"owner",id,"message",&format!("{id}:last"),t0-4*day)?;Ok(())}).await?;}
            let off=Settings{auto_park_days:Some(3),auto_archive:false,..Settings::default()};
            let quiet=[Info{id:"off-chat".into(),status:Status::Idle,..Info::default()}];
            let p=refresh_placements(&hub,"owner",&quiet,&off,t0).await?.remove("off-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert_eq!((p.automatic,p.parked_until),(true,Some(t0+3*day)));
            let back=t0+3*day;
            for pass in 0..2{
                let p=refresh_placements(&hub,"owner",&quiet,&off,back).await?.remove("off-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
                assert!(p.parked_until.is_none(),"pass {pass}: it came back and stays back: {p:?}");
                assert_eq!((p.reason.as_deref(),p.woke_at),(Some("Its park time ended"),Some(back)),"pass {pass}");
            }
            let p=refresh_placements(&hub,"owner",&quiet,&off,back+3*day-1).await?.remove("off-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert!(p.parked_until.is_none(),"not before the setting's days of quiet from its return");
            let p=refresh_placements(&hub,"owner",&quiet,&off,back+3*day).await?.remove("off-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert_eq!(p.parked_until,Some(back+6*day));
            assert_eq!(p.reason.as_deref(),Some("Automatically parked: no interaction or turns for 3 days since it came back"));
            let on=Settings{auto_archive:true,archive_after_days:7,..off.clone()};
            let quiet=[Info{id:"on-chat".into(),status:Status::Idle,..Info::default()}];
            let p=refresh_placements(&hub,"owner",&quiet,&on,t0).await?.remove("on-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert_eq!(p.parked_until,Some(t0+7*day),"automatic archive on: it lasts until the archive");
            let p=refresh_placements(&hub,"owner",&quiet,&on,t0+7*day).await?.remove("on-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert_eq!(p.parked_until,Some(t0+7*day),"quiet at its end: left for maintenance's archive");
            let mut busy=quiet;if let Some(chat)=busy.first_mut(){chat.apply(&State{status:Status::Idle,tasks:vec![running("shell","shell")],..Default::default()});}
            let p=refresh_placements(&hub,"owner",&busy,&on,t0+7*day).await?.remove("on-chat").ok_or_else(||anyhow::anyhow!("not placed"))?;
            assert!(p.parked_until.is_none(),"work of its own at its end: it comes back instead");
            assert_eq!(p.reason.as_deref(),Some("Its park time ended"));
            Ok(())
        })
    }

    /// Step 1: an agent's turn with two subagents, its harness's own and a kiln one, is recorded on the agent through
    /// `observe`: its turn and each subagent's turn, each with when it started and when it ended, and the `uses` API
    /// says which end each is. Nothing is recorded on the kiln subagent: it's on its parent's row.
    #[test]
    fn mc_attention_step1_a_turn_and_its_two_subagents_turns_are_recorded_with_their_starts_and_ends()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use super::super::model::{Task,TurnEnd};
            let hub=crate::hub::tests::scratch_hub().await?;
            let (agent,kiln)=("turning-agent","its-kiln-subagent");
            ingest(&hub,agent,opened(Info{id:agent.into(),name:"Turning".into(),status:Status::Idle,..Info::default()})).await;
            ingest(&hub,kiln,opened(Info{id:kiln.into(),name:"Its subagent".into(),parent:Some(agent.into()),subagent:true,status:Status::Idle,..Info::default()})).await;
            let native=|status:&str,ended:Option<i64>|Task{id:"native-subagent".into(),kind:"agent".into(),class:Some("agent".into()),status:status.into(),started:110,ended,..Default::default()};
            let end=|at|Some(TurnEnd{at,how:"done".into(),..TurnEnd::default()});
            ingest(&hub,agent,stated(State{status:Status::Running,turn_started:Some(100),tasks:vec![native("running",None)],..Default::default()})).await;
            ingest(&hub,kiln,stated(State{status:Status::Running,turn_started:Some(120),..Default::default()})).await;
            ingest(&hub,kiln,stated(State{status:Status::Idle,last_turn:end(180),..Default::default()})).await;
            ingest(&hub,agent,stated(State{status:Status::Running,turn_started:Some(100),tasks:vec![native("completed",Some(190))],..Default::default()})).await;
            ingest(&hub,agent,stated(State{status:Status::Idle,last_turn:end(200),tasks:vec![native("completed",Some(190))],..Default::default()})).await;
            let expected=json!([
                {"agent":agent,"kind":"turn","at":100,"edge":"start"},
                {"agent":agent,"kind":"subagent_turn","at":110,"edge":"start"},
                {"agent":agent,"kind":"subagent_turn","at":120,"edge":"start"},
                {"agent":agent,"kind":"subagent_turn","at":180,"edge":"end"},
                {"agent":agent,"kind":"subagent_turn","at":190,"edge":"end"},
                {"agent":agent,"kind":"turn","at":200,"edge":"end"},
            ]);
            // Each change's write goes on its own (`observe`); a bound on a hang, not a pace.
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
            loop{
                let uses=uses_of(&hub,&Caller::owner(),"owner".into()).await.map_err(|(_,why)|anyhow::anyhow!(why))?;
                if uses.get("uses")==Some(&expected){break;}
                anyhow::ensure!(std::time::Instant::now()<deadline,"its turns weren't recorded as they happened: {uses}");
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            Ok(())
        })
    }

    /// Item 8: what the store already holds of an agent isn't written again, however often the agent is observed or
    /// seen in a snapshot; a change is written once, by whichever sees it first.
    #[test]
    fn mc_attention_unchanged_agents_write_nothing_and_a_change_writes_once()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let hub=crate::hub::tests::scratch_hub().await?;
            let id=crate::util::new_id();
            let asking=|at:i64|super::super::session::ApprovalBrief{id:"q".into(),tool:super::super::model::ToolKind::Question,tool_name:"question".into(),title:"Choose".into(),at,agent:None};
            let info=Info{id:id.clone(),status:Status::Waiting,approvals:vec![asking(10)],turn_started:Some(5),..Info::default()};
            assert!(observe(&hub,&info),"a first observation is written");
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
            while unwritten(&hub,&id,&id,Some(&turn_uses(&info,&id)),&requests::pending(&info)){
                anyhow::ensure!(std::time::Instant::now()<deadline,"the first observation was never written");
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            for _ in 0..3{assert!(!observe(&hub,&info),"unchanged: nothing to write");}
            let head=|hub:&Arc<Hub>|{let(hub,id)=(Arc::clone(hub),id.clone());async move{hub.store.call(move|c|Ok(c.query_row("SELECT revision FROM attention_request_heads WHERE agent=?1",[id],|r|r.get::<_,String>(0))?)).await}};
            let written_head=head(&hub).await?;
            let accessible:HashSet<String>=[id.clone()].into_iter().collect();
            request_snapshot(&hub,"owner",std::slice::from_ref(&info),&accessible,std::slice::from_ref(&id),&mut BTreeMap::new(),("zz-snapshot",crate::util::now_ms())).await?;
            assert_eq!(head(&hub).await?,written_head,"a snapshot doesn't write what the store holds");
            let asked_again=Info{approvals:vec![asking(20)],..info};
            request_snapshot(&hub,"owner",std::slice::from_ref(&asked_again),&accessible,std::slice::from_ref(&id),&mut BTreeMap::new(),("zz-snapshot",crate::util::now_ms())).await?;
            assert_eq!(head(&hub).await?,"zz-snapshot","a new request the observer hasn't written yet is written by the snapshot");
            Ok(())
        })
    }

    /// Item 8: an agent's end frees what was kept in memory for it. A true subagent's lineage stays while its row
    /// does (a person's command that ended it is still theirs on that row), and goes with its row.
    #[test]
    fn mc_attention_an_agents_end_frees_what_was_kept_for_it(){
        let parent=Info{id:crate::util::new_id(),status:Status::Idle,..Info::default()};
        let child=Info{id:crate::util::new_id(),parent:Some(parent.id.clone()),subagent:true,status:Status::Idle,..Info::default()};
        for info in [&parent,&child]{remember_lineage(info);quiet_transition(info);written(std::sync::Weak::new(),&info.id,parent.id.clone(),Some(Vec::new()),Vec::new());}
        forget(&child.id);
        assert!(!QUIET_STATES.lock().contains_key(&child.id) && !OBSERVED.lock().contains_key(&child.id));
        assert_eq!(display_row(&child.id,std::slice::from_ref(&parent),&ROW_LINEAGE.lock()),parent.id,"its row still takes a person's command that ended it");
        forget(&parent.id);
        let known=ROW_LINEAGE.lock();
        assert!(!known.contains_key(&parent.id) && !known.contains_key(&child.id),"its row's end takes its subagents' lineage along");
        assert!(!QUIET_STATES.lock().contains_key(&parent.id) && !OBSERVED.lock().contains_key(&parent.id));
    }

    /// A local agent on host.rs's Claude Code stand-in (it answers kiln's initialize and each turn, and notes each line
    /// it reads in `heard`), once it's idle.
    #[cfg(unix)]
    async fn standing_in(hub:&Arc<Hub>,dir:&std::path::Path)->anyhow::Result<(String,std::path::PathBuf)>{
        let(program,heard)=super::super::host::stand_in(dir)?;
        let spec=super::super::session::Spec{kind:"claude".into(),cwd:Some(dir.to_string_lossy().into_owned()),program:Some(program.to_string_lossy().into_owned()),..Default::default()};
        let id=hub.harness.open(hub,"local",spec,1).await?.first().map(|i|i.id.clone()).ok_or_else(||anyhow::anyhow!("no agent"))?;
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(120);
        while !hub.harness.info(&id).is_some_and(|i|i.status==Status::Idle){
            anyhow::ensure!(std::time::Instant::now()<deadline,"the stand-in never became idle");
            crate::rt::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok((id,heard))
    }

    /// `POST path` with `body` to the API at `port` as the owner, as a client sends it: its status.
    #[cfg(unix)]
    fn post(port:u16,path:&str,body:&Value)->anyhow::Result<u16>{
        use std::io::{Read as _,Write as _};
        let body=body.to_string();
        let mut stream=std::net::TcpStream::connect(("127.0.0.1",port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
        write!(stream,"POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer test-admin-token\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len())?;
        let mut head=Vec::new();let mut byte=[0u8;1];
        while !head.ends_with(b"\r\n\r\n"){anyhow::ensure!(stream.read(&mut byte)?==1,"{path}: closed before its answer");head.extend_from_slice(&byte);}
        let head=String::from_utf8_lossy(&head);
        head.split_whitespace().nth(1).and_then(|code|code.parse().ok()).ok_or_else(||anyhow::anyhow!("{path}: no status in {head}"))
    }

    /// The composer's socket to `agent` at `port`, as the owner: `command` sent under `id`, and its answer read.
    #[cfg(unix)]
    fn composer(port:u16,agent:&str,id:&str,command:&Value)->anyhow::Result<String>{
        use tungstenite::client::IntoClientRequest as _;
        let stream=std::net::TcpStream::connect(("127.0.0.1",port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
        let mut req=format!("ws://127.0.0.1:{port}/api/harness/sessions/{agent}/ws").into_client_request()?;
        req.headers_mut().insert("authorization","Bearer test-admin-token".parse()?);
        let(mut ws,_)=tungstenite::client::client(req,stream).map_err(|e|anyhow::anyhow!("{e}"))?;
        ws.send(tungstenite::Message::text(json!({"t":"cmd","id":id,"c":command}).to_string()))?;
        loop{
            if let tungstenite::Message::Text(t)=ws.read()? && t.as_str().contains(&format!("\"{id}\"")){return Ok(t.as_str().to_owned());}
        }
    }

    /// Until what the store holds of top-level agent `id`'s turns and requests is what its info says now.
    #[cfg(unix)]
    async fn written_through(hub:&Hub,id:&str)->anyhow::Result<()>{
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
        loop{
            let info=hub.harness.info(id).ok_or_else(||anyhow::anyhow!("no {id}"))?;
            if !unwritten(hub,id,id,Some(&turn_uses(&info,id)),&requests::pending(&info)){return Ok(());}
            anyhow::ensure!(std::time::Instant::now()<deadline,"{id}'s observations were never written");
            crate::rt::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Step 1: a person messages an agent through the composer's socket, Mission Control's message box
    /// (`/api/harness/broadcast`), the CLI (`kiln op agents.send`: `/api/ops`) and MCP (`/api/mcp`); another agent
    /// messages it through its MCP tool; the goal driver sends it a goal; its usage poll's state comes in. Exactly
    /// the four person messages are the person's uses of it, the rest aren't, and the poll records nothing. Each
    /// person's path is driven through the server's own routes, over a socket, as its client sends it.
    #[cfg(unix)]
    #[test]
    fn mc_attention_step1_only_a_persons_four_messages_are_their_uses()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let tcp=hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127,0,0,1],0)),16).map_err(|e|anyhow::anyhow!("{e}"))?;
            let port=tcp.local_addr().map_err(|e|anyhow::anyhow!("{e}"))?.port();
            let hub=crate::hub::tests::scratch_hub_with(|c|{c.terminal.enabled=true;}).await?;
            let api=Arc::new(crate::api::Api{hub:Arc::clone(&hub)});
            crate::rt::spawn(async move{
                while let Ok(stream)=tcp.accept().await{
                    let(api,remote)=(Arc::clone(&api),stream.peer_addr().ok());
                    crate::rt::spawn(async move{crate::http::server::serve_accepted(stream,api,remote,None).await});
                }
            });
            let h=Arc::clone(&hub);crate::rt::spawn(async move{let _=super::super::registry::Registry::run_local(h).await;});
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir=tempfile::tempdir()?;
            let(target,heard)=standing_in(&hub,dir.path()).await?;
            let spec=super::super::session::Spec{kind:"claude".into(),cwd:Some(dir.path().to_string_lossy().into_owned()),program:Some(dir.path().join("no-claude").to_string_lossy().into_owned()),..Default::default()};
            let other=hub.harness.open(&hub,"local",spec,1).await?.first().map(|i|i.id.clone()).ok_or_else(||anyhow::anyhow!("no other agent"))?;
            // The goal driver: a goal on its stack, which kiln's goal machinery sends it.
            crate::goals::add(&hub,&target,vec!["Write the release notes".into()],false).await.map_err(|e|anyhow::anyhow!("{e:?}"))?;
            crate::goals::control(&hub,&target,crate::goals::Action::Start).await.map_err(|e|anyhow::anyhow!("{e:?}"))?;
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
            while !std::fs::read_to_string(&heard).unwrap_or_default().contains("Write the release notes"){
                anyhow::ensure!(std::time::Instant::now()<deadline,"the goal driver's goal never reached it");
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Another agent messages it, as its MCP tool asks kiln.
            let as_other=crate::ops::Cx{hub:Arc::clone(&hub),caller:crate::mcp::session_caller(&hub,&other,None).await.map_err(anyhow::Error::msg)?};
            crate::ops::call(as_other,"agents.send",json!({"agent":target,"text":"from another agent"})).await.map_err(|(code,why)|anyhow::anyhow!("{code}: {why}"))?;
            // The person, by each path.
            let t=target.clone();
            let ack=crate::rt::blocking(move||composer(port,&t,"composer-1",&json!({"type":"send","id":"m-composer","text":"from the composer"}))).await??;
            assert!(ack.contains("\"ack\""),"the composer's message wasn't taken: {ack}");
            for(path,body)in [
                ("/api/harness/broadcast",json!({"to":{"ids":[target]},"text":"from Mission Control"})),
                ("/api/ops/agents.send",json!({"agent":target,"text":"from the CLI"})),
                ("/api/mcp",json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agents_send","arguments":{"agent":target,"text":"from MCP"}}})),
            ]{
                let status=crate::rt::blocking(move||post(port,path,&body)).await??;
                assert_eq!(status,200,"{path}");
            }
            let t=target.clone();
            let theirs=||{let t=t.clone();let hub=Arc::clone(&hub);async move{hub.store.call(move|c|{let mut q=c.prepare("SELECT agent,kind,key FROM attention_uses WHERE principal='owner' ORDER BY key")?;Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<Result<Vec<_>,_>>()?)}).await.map(|rows|rows.into_iter().filter(|(agent,_,_)|agent==&t).collect::<Vec<_>>())}};
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(60);
            let mine=loop{
                let rows=theirs().await?;
                if rows.len()>=4 || std::time::Instant::now()>deadline{break rows;}
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            };
            assert_eq!(mine.len(),4,"the four person messages, and nothing else, are the person's: {mine:?}");
            assert!(mine.iter().all(|(_,kind,_)|kind=="message"),"{mine:?}");
            let keys:Vec<&str>=mine.iter().map(|(_,_,key)|key.as_str()).collect();
            assert_eq!(keys.iter().filter(|k|**k=="m-composer").count(),1,"the composer's: {keys:?}");
            assert_eq!(keys.iter().filter(|k|k.starts_with("bc-")).count(),1,"Mission Control's box: {keys:?}");
            assert_eq!(keys.iter().filter(|k|k.starts_with("agents.send:")).count(),2,"the CLI's and MCP's: {keys:?}");
            let anyone:i64=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_uses WHERE principal<>'' AND principal<>'owner'",[],|r|r.get(0))?)).await?;
            assert_eq!(anyone,0,"another agent's message and the goal driver's are no one's");
            // Its usage poll, once every message has reached it, its turns are over and they're written: a state with
            // new usage and nothing else of its work changed.
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(120);
            loop{
                let heard=std::fs::read_to_string(&heard).unwrap_or_default();
                let all_in=["from another agent","from the composer","from Mission Control","from the CLI","from MCP"].iter().all(|text|heard.contains(text));
                if all_in && hub.harness.info(&target).is_some_and(|i|i.status==Status::Idle && i.queued==0 && i.turn_started.is_none()){break;}
                anyhow::ensure!(std::time::Instant::now()<deadline,"its messages' turns never ended");
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            written_through(&hub,&target).await?;
            let t=target.clone();let count=||{let t=t.clone();let hub=Arc::clone(&hub);async move{hub.store.call(move|c|Ok(c.query_row("SELECT COUNT(*) FROM attention_uses WHERE agent=?1",[t],|r|r.get::<_,i64>(0))?)).await}};
            let before=count().await?;
            let mut polled=hub.harness.state(&target).ok_or_else(||anyhow::anyhow!("no state"))?;
            polled.usage.cost_usd+=0.25;polled.usage.input_tokens=polled.usage.input_tokens.saturating_add(1_000);polled.account=Some("polled-account".into());
            hub.harness.ingest(&super::super::registry::Location::Local,&target,super::super::core::Update::State{state:Box::new(polled)}).await;
            written_through(&hub,&target).await?;
            assert_eq!(count().await?,before,"the usage poll records nothing");
            hub.harness.close(&hub,&other).await;hub.harness.close(&hub,&target).await;
            Ok(())
        })
    }
}
