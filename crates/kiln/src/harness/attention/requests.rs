//! Step 4 of plan 01a199e1b177752e84dad5dc990e6a15: observed requests and accepted
//! human responses. Request age is never guessed from a session's activity timestamp.
use std::collections::HashSet;

use kiln_ci::attention::{RequestKind, RequestWait, ResponseTime};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use super::super::{model::{Status, ToolKind}, session::Info};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending { pub key: String, pub kind: RequestKind, pub at: Option<i64>, pub provider_id: Option<String> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response { Approval(String), Goals, Restart, Bound(String) }

pub fn pending(info: &Info) -> Vec<Pending> {
    let mut requests: Vec<Pending> = info.approvals.iter().map(|a| Pending {
        key: approval_key(&a.id,(a.at>0).then_some(a.at)),
        provider_id: Some(a.id.clone()),
        kind: match a.tool { ToolKind::Question => RequestKind::Question, ToolKind::Plan => RequestKind::Plan, _ => RequestKind::Tool },
        at: (a.at > 0).then_some(a.at),
    }).collect();
    if let Some(held) = &info.held {
        requests.push(Pending { key: format!("held:{}:{}", held.since, crate::util::derive_id(&[&held.why])), kind: RequestKind::Goals, at: (held.since > 0).then_some(held.since), provider_id:None });
    }
    if info.status == Status::Exited && let Some(exit) = &info.exit && exit.error.is_some() && exit.again_at.is_none() {
        requests.push(Pending { key: format!("stop:{}:{}", exit.at, crate::util::derive_id(&[exit.error.as_deref().unwrap_or_default()])), kind: RequestKind::Stop, at: (exit.at > 0).then_some(exit.at), provider_id:None });
    }
    if info.status == Status::Waiting && requests.is_empty() {
        // The adapter supplied no request identity or clock. This marker records only
        // an unknown current wait; it supplies neither an age nor a response sample.
        requests.push(Pending { key:"waiting-without-request".into(),kind:RequestKind::Question,at:None,provider_id:None });
    }
    requests
}

/// One of an agent's current requests, as a park defers it or wakes for it (step 6).
pub struct Identity{
    /// A stop with an error, rather than a request.
    pub stop:bool,
    /// Its raw descriptor, shared with the client. None means the adapter supplied too little identity to promise
    /// that a later wait is the one this person deliberately deferred.
    pub request:Option<super::store::DeferredRequest>,
    /// What a park it wakes says of it.
    pub says:&'static str,
}

pub fn identities(info:&Info)->Vec<Identity>{
    use super::store::{DeferredKind,DeferredRequest};
    let descriptor=|kind,id:&str,at|if !id.is_empty() && at>0{Some(DeferredRequest{agent:info.id.clone(),kind,id:id.into(),at})}else{None};
    let mut current:Vec<_>=info.approvals.iter().map(|a|Identity{stop:false,request:descriptor(DeferredKind::Approval,&a.id,a.at),says:match a.tool{ToolKind::Question=>"It asks you something",ToolKind::Plan=>"It proposes a plan",_=>"It asks for your approval"}}).collect();
    if let Some(held)=&info.held{current.push(Identity{stop:false,request:descriptor(DeferredKind::Held,&held.why,held.since),says:"Its goals wait for you"});}
    if info.status==Status::Exited && let Some(exit)=&info.exit && let Some(error)=&exit.error && exit.again_at.is_none(){current.push(Identity{stop:true,request:descriptor(DeferredKind::Stop,error,exit.at),says:"It stopped with an error"});}
    if info.status==Status::Waiting && current.is_empty(){current.push(Identity{stop:false,request:None,says:"It waits for you"});}
    current
}

/// Bind only while the actual pending snapshot is still available before dispatch.
/// A later database view cannot establish which reused provider ID the person answered.
pub fn bind(info:&Info,selector:&Response)->Option<Response>{
    pending(info).into_iter().filter(|r|r.at.is_some()).filter(|r|match selector{
        Response::Approval(id)=>r.provider_id.as_ref()==Some(id),
        Response::Goals=>r.kind==RequestKind::Goals,
        Response::Restart=>r.kind==RequestKind::Stop,
        Response::Bound(key)=>&r.key==key,
    }).max_by_key(|r|r.at).map(|r|Response::Bound(r.key))
}

/// Raw JSON-RPC IDs can repeat after an adapter restarts. The actual request time
/// distinguishes occurrences; components are hashed separately, never parsed by colons.
pub fn approval_key(provider_id:&str,at:Option<i64>)->String{
    crate::util::derive_id(&["attention-approval",provider_id,&at.map(|at|at.to_string()).unwrap_or_default()])
}

fn kind_word(kind: RequestKind) -> anyhow::Result<String> {
    Ok(serde_json::to_value(kind)?.as_str().ok_or_else(|| anyhow::anyhow!("invalid request kind"))?.to_owned())
}
fn parse_kind(kind: String) -> anyhow::Result<RequestKind> { Ok(serde_json::from_value(serde_json::Value::String(kind))?) }

/// The revision is captured before spawning the writer. A late pending snapshot can add
/// historical evidence, but cannot reopen a request already closed by a newer snapshot.
pub fn observe(c: &Connection, agent: &str, row: &str, requests: &[Pending], revision: &str, at: i64) -> anyhow::Result<Vec<String>> {
    let before=current(c,agent)?;
    let previous: Option<(String,i64)> = c.query_row("SELECT revision,observed_at FROM attention_request_heads WHERE agent=?1", [agent], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    let newer = previous.as_ref().is_none_or(|(old,_)| revision > old.as_str());
    for request in requests {
        let closed = if newer { None } else { previous.as_ref().map(|(_,at)| *at) };
        // A first observation arriving behind a newer snapshot only bounds closure.
        // Without a complete transition ledger its response interval is uncertain.
        c.execute("INSERT OR IGNORE INTO attention_requests(agent,key,row_agent,kind,asked_at,observed_at,closed_at,ambiguous,provider_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![agent,request.key,row,kind_word(request.kind)?,request.at,at,closed,!newer,request.provider_id])?;
        c.execute("UPDATE attention_requests SET asked_at=COALESCE(asked_at,?3) WHERE agent=?1 AND key=?2", params![agent,request.key,request.at])?;
        if newer {
            c.execute("UPDATE attention_requests SET row_agent=?3,closed_at=NULL WHERE agent=?1 AND key=?2", params![agent,request.key,row])?;
        }
        c.execute("UPDATE attention_responses SET duration=at-?3 WHERE agent=?1 AND request_key=?2 AND duration IS NULL AND ?3 IS NOT NULL AND at>=?3", params![agent,request.key,request.at])?;
    }
    if newer {
        let keys: HashSet<&str> = requests.iter().map(|r| r.key.as_str()).collect();
        let mut query = c.prepare("SELECT key FROM attention_requests WHERE agent=?1 AND closed_at IS NULL")?;
        let open: Vec<String> = query.query_map([agent], |r| r.get(0))?.collect::<Result<_,_>>()?;
        for key in open.into_iter().filter(|key| !keys.contains(key.as_str())) {
            c.execute("UPDATE attention_requests SET closed_at=?3 WHERE agent=?1 AND key=?2 AND closed_at IS NULL", params![agent,key,at])?;
        }
        c.execute("INSERT INTO attention_request_heads(agent,revision,observed_at) VALUES(?1,?2,?3) ON CONFLICT(agent) DO UPDATE SET revision=excluded.revision,observed_at=excluded.observed_at", params![agent,revision,at])?;
    }
    let mut people=reconcile(c, agent)?;
    if before!=current(c,agent)? {
        // No identifiers leave this query: private principal events only ask that person's
        // client to reload its own authorized snapshot. A visitor can opt into this without
        // having sent any message or saved any settings yet.
        let mut q=c.prepare("SELECT principal FROM attention_uses WHERE agent=?1 AND principal<>'' UNION SELECT principal FROM attention_placement WHERE agent=?1 UNION SELECT principal FROM attention_settings UNION SELECT principal FROM attention_exposures")?;
        people.extend(q.query_map([agent],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?);
    }
    people.sort();people.dedup();Ok(people)
}

fn current(c:&Connection,agent:&str)->anyhow::Result<Vec<(String,String,String,Option<i64>)>>{
    let mut q=c.prepare("SELECT key,row_agent,kind,asked_at FROM attention_requests WHERE agent=?1 AND closed_at IS NULL ORDER BY key")?;
    Ok(q.query_map([agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?)
}

/// Keep an accepted answer even if its writer overtakes the pending-state writer. Only an
/// authenticated human action reaches this function, and retries cannot create a second sample.
pub fn response(c: &Connection, principal: &str, agent: &str, key: &str, response: &Response, at: i64) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(at >= 0, "response time cannot be negative");
    c.execute("INSERT OR IGNORE INTO attention_response_attempts(principal,agent,event_key,selector,at) VALUES(?1,?2,?3,?4,?5)", params![principal,agent,key,serde_json::to_string(response)?,at])?;
    reconcile(c, agent)
}

fn reconcile(c: &Connection, agent: &str) -> anyhow::Result<Vec<String>> {
    let mut people=Vec::new();
    let mut q = c.prepare("SELECT principal,event_key,selector,at FROM attention_response_attempts WHERE agent=?1 AND matched=0 ORDER BY at,event_key")?;
    let attempts: Vec<(String,String,String,i64)> = q.query_map([agent], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
    for (principal,event_key,selector,at) in attempts {
        let response: Response = serde_json::from_str(&selector)?;
        // Only a pre-dispatch binding proves the occurrence. If its observer writer
        // is delayed, leave the attempt unmatched until that exact record arrives.
        let Response::Bound(bound)=response else{continue;};
        let request:Option<(String,i64)>=c.query_row("SELECT kind,asked_at FROM attention_requests WHERE agent=?1 AND key=?2 AND asked_at IS NOT NULL AND asked_at<=?3",params![agent,bound,at],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((kind,asked))=request else{continue;};
        let Some(duration)=at.checked_sub(asked).filter(|d|*d>=0) else{continue;};
        let key=bound;
        let changed=c.execute("INSERT INTO attention_responses(principal,agent,request_key,kind,duration,at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(principal,agent,request_key) DO UPDATE SET duration=excluded.duration,at=excluded.at WHERE excluded.at<attention_responses.at", params![principal,agent,key,kind,duration,at])?;
        if changed>0{people.push(principal.clone());}
        c.execute("UPDATE attention_response_attempts SET matched=1 WHERE principal=?1 AND agent=?2 AND event_key=?3 AND selector=?4", params![principal,agent,event_key,selector])?;
    }
    people.sort();people.dedup();Ok(people)
}

#[derive(Debug, Clone)]
pub struct Open { pub row: String, pub kind: RequestKind, pub at: Option<i64> }

/// The requests open at `now` among the agents `accessible` to the person, and every answer time of theirs up to
/// `now`. Answer times are the person's own records: removing an agent, or losing sight of it, never shortens
/// "your usual time" or "your longest" (step 4).
pub fn history(c: &Connection, principal: &str, accessible: &HashSet<String>, now: i64) -> anyhow::Result<(Vec<Open>,Vec<ResponseTime>)> {
    let mut q = c.prepare("SELECT agent,row_agent,kind,asked_at FROM attention_requests WHERE ((asked_at IS NULL AND observed_at<=?1) OR asked_at<=?1) AND (closed_at IS NULL OR closed_at>=?1)")?;
    let rows: Vec<(String,String,String,Option<i64>)> = q.query_map([now], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
    let open = rows.into_iter().filter(|(agent,row,_,_)| accessible.contains(agent) && accessible.contains(row)).map(|(_,row,kind,at)| Ok(Open { row,kind:parse_kind(kind)?,at })).collect::<anyhow::Result<_>>()?;
    let mut q = c.prepare("SELECT kind,duration FROM attention_responses WHERE principal=?1 AND at<=?2 AND duration IS NOT NULL ORDER BY at,agent,request_key")?;
    let rows: Vec<(String,i64)> = q.query_map(params![principal,now], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    let responses = rows.into_iter().map(|(kind,duration)| Ok(ResponseTime { kind:parse_kind(kind)?,duration })).collect::<anyhow::Result<_>>()?;
    Ok((open,responses))
}

pub fn waits(open: &[Open], visible: &HashSet<String>) -> Vec<RequestWait> {
    open.iter().filter(|r| visible.contains(&r.row)).filter_map(|r| r.at.map(|at| RequestWait { agent:r.row.clone(),kind:r.kind,at })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> anyhow::Result<Connection> { let c=Connection::open_in_memory()?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;Ok(c) }
    fn approval_at(at:i64) -> Pending { Pending { key:approval_key("q",Some(at)),kind:RequestKind::Question,at:Some(at),provider_id:Some("q".into()) } }
    fn approval()->Pending{approval_at(100)}
    fn answer(at:i64)->Response{Response::Bound(approval_at(at).key)}
    fn samples(c: &Connection, who: &str) -> anyhow::Result<Vec<ResponseTime>> { Ok(history(c,who,&["a".into()].into_iter().collect(),1000)?.1) }
    #[test]
    fn mc_attention_waiting_without_a_request_has_no_invented_clock(){
        let info=Info{status:Status::Waiting,turn_started:Some(100),..Info::default()};
        let requests=pending(&info);assert_eq!(requests.len(),1);assert_eq!(requests.first().and_then(|r|r.at),None);
    }
    #[test]
    fn mc_attention_response_uses_receipt_time_even_after_observed_closure() -> anyhow::Result<()> {
        let c=db()?;observe(&c,"a","a",&[approval()],"01",110)?;observe(&c,"a","a",&[],"02",210)?;
        // Accepted actions can reach the writer in the opposite order from receipt.
        response(&c,"person","a","retry",&answer(100),205)?;
        response(&c,"person","a","answer",&answer(100),200)?;
        assert_eq!(samples(&c,"person")?,vec![ResponseTime {kind:RequestKind::Question,duration:100}]);
        assert!(samples(&c,"other")?.is_empty());Ok(())
    }
    #[test]
    fn mc_attention_late_observer_preserves_history_without_reopening_request() -> anyhow::Result<()> {
        let c=db()?;observe(&c,"a","a",&[],"02",210)?;
        response(&c,"person","a","answer",&answer(100),200)?;
        observe(&c,"a","a",&[approval()],"01",110)?;
        assert_eq!(samples(&c,"person")?,vec![ResponseTime{kind:RequestKind::Question,duration:100}],"the exact occurrence was bound before dispatch");
        assert!(history(&c,"person",&["a".into()].into_iter().collect(),1000)?.0.is_empty());Ok(())
    }
    #[test]
    fn mc_attention_ambiguous_late_request_does_not_teach_a_generic_response()->anyhow::Result<()>{
        let c=db()?;
        observe(&c,"a","a",&[],"02",150)?;
        observe(&c,"a","a",&[Pending{key:"stop:other".into(),kind:RequestKind::Stop,at:Some(170),provider_id:None}],"03",170)?;
        observe(&c,"a","a",&[],"04",210)?;
        response(&c,"person","a","resume",&Response::Goals,180)?;
        // The original held condition was first seen at 110, but its writer arrives
        // after several later transitions. The 210 absence is not its exact closure.
        observe(&c,"a","a",&[Pending{key:"held:old".into(),kind:RequestKind::Goals,at:Some(100),provider_id:None}],"01",110)?;
        assert!(samples(&c,"person")?.is_empty(),"a generic Resume must not be assigned to an ambiguous old interval");
        assert!(c.query_row("SELECT ambiguous FROM attention_requests WHERE key='held:old'",[],|r|r.get::<_,bool>(0))?);
        Ok(())
    }
    #[test]
    fn mc_attention_reused_approval_ids_keep_distinct_ages_and_response_samples()->anyhow::Result<()>{
        let c=db()?;observe(&c,"a","a",&[approval_at(100)],"01",110)?;
        response(&c,"person","a","first",&answer(100),150)?;
        observe(&c,"a","a",&[],"02",200)?;
        observe(&c,"a","a",&[approval_at(1000)],"03",1010)?;
        let accessible=["a".into()].into_iter().collect();
        let(open,responses)=history(&c,"person",&accessible,1050)?;
        assert_eq!(open.first().and_then(|r|r.at),Some(1000));
        let waits=waits(&open,&accessible);
        assert!(!kiln_ci::attention::urgency(&waits,&responses,1050).requests.first().is_some_and(|r|r.stale));
        assert!(kiln_ci::attention::urgency(&waits,&responses,1100).requests.first().is_some_and(|r|r.stale));
        response(&c,"person","a","second",&answer(1000),1300)?;
        assert_eq!(history(&c,"person",&accessible,1400)?.1,vec![ResponseTime{kind:RequestKind::Question,duration:50},ResponseTime{kind:RequestKind::Question,duration:300}]);
        assert_ne!(approval_at(100).key,approval_at(1000).key);Ok(())
    }
    #[test]
    fn mc_attention_reused_id_does_not_make_a_late_interval_exact()->anyhow::Result<()>{
        let c=db()?;observe(&c,"a","a",&[approval_at(100)],"01",110)?;
        response(&c,"person","a","first",&answer(100),120)?;
        observe(&c,"a","a",&[],"02",150)?;
        observe(&c,"a","a",&[],"04",2100)?;
        response(&c,"person","a","second",&Response::Approval("q".into()),1800)?;
        observe(&c,"a","a",&[approval_at(1000)],"03",1100)?;
        let accessible=["a".into()].into_iter().collect();
        assert_eq!(history(&c,"person",&accessible,2200)?.1,vec![ResponseTime{kind:RequestKind::Question,duration:20}],"the late second occurrence is uncertain despite its raw ID matching");Ok(())
    }
    #[test]
    fn mc_attention_reused_id_selects_latest_actual_request_before_receipt()->anyhow::Result<()>{
        let c=db()?;observe(&c,"a","a",&[approval_at(100)],"01",110)?;
        // The old observed closure lags the new request's real time. Both intervals
        // contain 1005, but the response belongs to the newer occurrence at 1000.
        observe(&c,"a","a",&[approval_at(1000)],"02",1010)?;
        response(&c,"person","a","answer",&answer(1000),1005)?;
        let accessible=["a".into()].into_iter().collect();
        assert_eq!(history(&c,"person",&accessible,1100)?.1,vec![ResponseTime{kind:RequestKind::Question,duration:5}]);Ok(())
    }
    #[test]
    fn mc_attention_bound_answer_waits_for_new_approval_observer_before_matching()->anyhow::Result<()>{
        let c=db()?;observe(&c,"a","a",&[approval_at(100)],"01",110)?;
        response(&c,"person","a","first",&answer(100),120)?;
        // Before dispatch the current approval snapshot was q at 1000, but the DB
        // still contains the previous q at 100. The binding must survive that lag.
        response(&c,"person","a","second",&answer(1000),1005)?;
        let accessible=["a".into()].into_iter().collect();
        assert_eq!(history(&c,"person",&accessible,1100)?.1,vec![ResponseTime{kind:RequestKind::Question,duration:20}]);
        observe(&c,"a","a",&[approval_at(1000)],"02",1010)?;
        assert_eq!(history(&c,"person",&accessible,1100)?.1,vec![ResponseTime{kind:RequestKind::Question,duration:20},ResponseTime{kind:RequestKind::Question,duration:5}]);Ok(())
    }
    #[test]
    fn mc_attention_bound_goals_and_stop_answers_wait_for_their_exact_occurrence()->anyhow::Result<()>{
        for kind in [RequestKind::Goals,RequestKind::Stop]{
            let c=db()?;
            let old=Pending{key:"old".into(),kind,at:Some(100),provider_id:None};
            let new=Pending{key:"new".into(),kind,at:Some(1000),provider_id:None};
            observe(&c,"a","a",&[old],"01",110)?;
            response(&c,"person","a","accepted",&Response::Bound(new.key.clone()),1005)?;
            assert!(history(&c,"person",&["a".into()].into_iter().collect(),1100)?.1.is_empty());
            // A newer closing snapshot may overtake the pending writer as well. The
            // pre-dispatch binding proves this exact occurrence despite that ordering.
            observe(&c,"a","a",&[],"03",1020)?;
            observe(&c,"a","a",&[new],"02",1010)?;
            assert_eq!(history(&c,"person",&["a".into()].into_iter().collect(),1100)?.1,vec![ResponseTime{kind,duration:5}]);
        }Ok(())
    }
    #[test]
    fn mc_attention_unbound_responses_never_guess_a_current_database_request()->anyhow::Result<()>{
        let c=db()?;observe(&c,"a","a",&[approval()],"01",110)?;
        response(&c,"person","a","unbound",&Response::Approval("q".into()),200)?;
        assert!(samples(&c,"person")?.is_empty());Ok(())
    }
    #[test]
    fn mc_attention_only_request_changes_and_new_responses_refresh_the_person()->anyhow::Result<()>{
        let c=db()?;c.execute("INSERT INTO attention_uses(principal,agent,kind,key,at) VALUES('person','a','open','view',1)",[])?;
        assert_eq!(observe(&c,"a","a",&[approval()],"01",110)?,vec!["person".to_owned()]);
        assert!(observe(&c,"a","a",&[approval()],"02",120)?.is_empty(),"unchanged activity does not refresh or reorder the list");
        assert_eq!(response(&c,"person","a","answer",&answer(100),200)?,vec!["person".to_owned()]);
        assert!(response(&c,"person","a","answer",&answer(100),201)?.is_empty());
        let accessible=["a".into()].into_iter().collect();
        assert!(history(&c,"person",&accessible,150)?.1.is_empty(),"replay cannot use a future human response");
        assert_eq!(history(&c,"person",&accessible,200)?.1.len(),1);Ok(())
    }
    /// Step 4: "your usual time" and "your longest" are the person's own answers; an agent removed since
    /// (or out of sight now) keeps its samples, while its open requests leave Needs you with it.
    #[test]
    fn mc_attention_answer_history_is_the_persons_own_whatever_is_listed_now()->anyhow::Result<()>{
        let c=db()?;
        observe(&c,"removed","removed",&[approval_at(100)],"01",110)?;response(&c,"person","removed","answer",&answer(100),400)?;
        observe(&c,"listed","listed",&[approval_at(1000)],"02",1010)?;
        let listed:HashSet<String>=["listed".into()].into_iter().collect();
        let(open,responses)=history(&c,"person",&listed,2000)?;
        assert_eq!(responses,vec![ResponseTime{kind:RequestKind::Question,duration:300}],"the removed agent's answer still teaches the person's own times");
        assert_eq!(open.iter().map(|r|r.row.as_str()).collect::<Vec<_>>(),vec!["listed"],"only requests of agents still listed wait in Needs you");
        assert!(history(&c,"other",&listed,2000)?.1.is_empty(),"another person's answers are never theirs");
        Ok(())
    }
    #[test]
    fn mc_attention_unknown_times_and_unrelated_commands_do_not_invent_samples() -> anyhow::Result<()> {
        let c=db()?;observe(&c,"a","a",&[Pending {at:None,..approval()}],"01",110)?;
        response(&c,"person","a","restart",&Response::Restart,200)?;
        response(&c,"person","a","answer",&answer(100),200)?;
        assert!(samples(&c,"person")?.is_empty());
        assert_eq!(history(&c,"person",&["a".into()].into_iter().collect(),1000)?.0.first().and_then(|r|r.at),None);Ok(())
    }
}
