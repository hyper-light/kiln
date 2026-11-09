//! Durable per-person attention (plan 01a199e1b177752e84dad5dc990e6a15).
use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use kiln_ci::attention::UseKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub sort: String,
    pub auto_park_days: Option<u32>,
    pub archive_after_days: u32,
    /// A parked chat is archived `archive_after_days` after it was parked. On until a person turns it off
    /// (the person's decision, 2026-11-08: "Just turn it on"); the plan's default is a week.
    pub auto_archive: bool,
    pub park_later_minutes: u32,
    pub park_evening_hour: u8,
    pub park_morning_hour: u8,
}
impl Default for Settings {
    fn default() -> Self { Self { sort: "learned".into(), auto_park_days: None, archive_after_days: 7, auto_archive: true, park_later_minutes: 60, park_evening_hour: 18, park_morning_hour: 9 } }
}

/// Milliseconds in a day: a unit conversion for the day counts a person sets.
pub const DAY_MS: i64 = 86_400_000;

/// Turned on, automatic parking starts at this many days of quiet: the plan's default (step 8), which the person
/// changes as they like. While it's off nothing of it is kept, so each snapshot says it (`settings_defaults`).
pub const AUTO_PARK_DAYS_WHEN_ON: u32 = 3;

/// When an automatic park ends (plan 01a199e1b177752e84dad5dc990e6a15, step 8), `days` (its `auto_park_days`) after the
/// quiet that parked it. With automatic archive on, at the archive: `archive_after_days` after it was parked, when it is
/// archived, or comes back instead if something of its own runs then or another person used it since. With it off, after
/// as long again as the quiet that parked it, when it comes back. Either way the park ends, and the chat isn't parked again
/// until `days` of quiet pass from its return (`idle_since`).
pub fn automatic_park_ends(settings: &Settings, days: u32, parked_at: i64) -> i64 {
    let span = if settings.auto_archive { settings.archive_after_days } else { days };
    parked_at.saturating_add(i64::from(span).saturating_mul(DAY_MS))
}

/// The exact current request the person deferred. Supplied request times are identity
/// components, never compared with a different machine's clock (attention plan step 6).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all="snake_case")]
pub enum DeferredKind{Approval,Held,Stop}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeferredRequest{pub agent:String,pub kind:DeferredKind,pub id:String,pub at:i64}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Placement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parked_until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parked_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_seen: Option<i64>,
    /// The requests a park defers: those the person saw when they parked it. Any other wakes it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deferred_requests: Vec<DeferredRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// kiln parked it, not the person (step 8): a person's Unpark of it is a correction.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
    /// The person brought back a chat kiln had parked automatically (step 8), as `reason` says.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub correction: bool,
    /// When its last park ended, whatever ended it: the quiet that parks it automatically counts from then (step 8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub woke_at: Option<i64>,
    pub revision: i64,
}

impl Placement {
    /// Parked until `until` from `at`, deferring `deferred`, saying `reason`; `automatic` when kiln parked it.
    pub fn parked(&self, at: i64, until: i64, deferred: Vec<DeferredRequest>, reason: String, automatic: bool) -> Self {
        Self { pinned: None, parked_at: Some(at), parked_until: Some(until), deferred_requests: deferred, reason: Some(reason), automatic, correction: false, ..self.clone() }
    }
    /// Its park ended at `at`, saying `reason` (a person's Unpark of kiln's own park is a `correction`).
    pub fn woke(&self, at: i64, reason: String, correction: bool) -> Self {
        Self { parked_at: None, parked_until: None, deferred_requests: Vec::new(), reason: Some(reason), automatic: false, correction, woke_at: Some(at), ..self.clone() }
    }
}

pub fn settings(c: &Connection, principal: &str) -> anyhow::Result<Settings> {
    let json: Option<String> = c.query_row("SELECT value FROM attention_settings WHERE principal = ?1", [principal], |r| r.get(0)).optional()?;
    json.map(|j| serde_json::from_str(&j).map_err(Into::into)).unwrap_or_else(|| Ok(Settings::default()))
}
pub fn save_settings(c: &Connection, principal: &str, s: &Settings) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(s.sort.as_str(), "learned" | "yours"), "sort is learned or yours");
    anyhow::ensure!(s.auto_park_days != Some(0) && s.archive_after_days > 0, "days must be positive");
    anyhow::ensure!(s.park_later_minutes > 0 && s.park_evening_hour < 24 && s.park_morning_hour < 24, "park minutes must be positive and hours must be between 0 and 23");
    c.execute("INSERT INTO attention_settings(principal,value) VALUES(?1,?2) ON CONFLICT(principal) DO UPDATE SET value=excluded.value", params![principal, serde_json::to_string(s)?])?;
    Ok(())
}
pub fn placements(c: &Connection, principal: &str) -> anyhow::Result<BTreeMap<String, Placement>> {
    let mut q = c.prepare("SELECT agent,value,revision FROM attention_placement WHERE principal=?1")?;
    let rows: Vec<(String,String,i64)> = q.query_map([principal], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    rows.into_iter().map(|(id,j,revision)| { let mut p: Placement=serde_json::from_str(&j)?; p.revision=revision; Ok((id,p)) }).collect()
}
pub fn placement(c: &Connection, principal: &str, agent: &str) -> anyhow::Result<Placement> {
    let row: Option<(String,i64)> = c.query_row("SELECT value,revision FROM attention_placement WHERE principal=?1 AND agent=?2", params![principal,agent], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    match row { None=>Ok(Placement::default()),Some((j,r))=>{let mut p:Placement=serde_json::from_str(&j)?;p.revision=r;Ok(p)} }
}
/// Compare-and-swap, including Undo: a stale device cannot undo a newer choice.
pub fn put(c: &Connection, principal: &str, agent: &str, before: &Placement, after: &Placement, undoable: bool) -> anyhow::Result<Placement> {
    let mut next=after.clone();
    next.revision=before.revision.checked_add(1).ok_or_else(|| anyhow::anyhow!("placement revision exhausted"))?;
    let previous=if undoable {Some(serde_json::to_string(before)?)} else {None};
    let json=serde_json::to_string(&next)?;
    let changed=if before.revision==0 {
        c.execute("INSERT OR IGNORE INTO attention_placement(principal,agent,value,previous,revision) VALUES(?1,?2,?3,?4,?5)",params![principal,agent,json,previous,next.revision])?
    } else {
        c.execute("UPDATE attention_placement SET value=?3,previous=?4,revision=?5 WHERE principal=?1 AND agent=?2 AND revision=?6",params![principal,agent,json,previous,next.revision,before.revision])?
    };
    anyhow::ensure!(changed==1,"placement changed on another device; refresh before trying again");
    Ok(next)
}
pub fn undo(c: &Connection, principal:&str, agent:&str, revision:i64, now:i64)->anyhow::Result<Placement>{
    let current=placement(c,principal,agent)?;
    anyhow::ensure!(current.revision==revision,"a newer choice replaced this action");
    let previous:Option<String>=c.query_row("SELECT previous FROM attention_placement WHERE principal=?1 AND agent=?2",params![principal,agent],|r|r.get(0)).optional()?.flatten();
    let mut before:Placement=serde_json::from_str(&previous.ok_or_else(||anyhow::anyhow!("this action has no Undo"))?)?;
    if before.parked_until.is_some_and(|t|t<=now){before.parked_until=None;before.parked_at=None;before.deferred_requests.clear();before.automatic=false;}
    put(c,principal,agent,&current,&before,false)
}
pub fn record(c:&Connection,principal:&str,agent:&str,kind:&str,key:&str,at:i64)->anyhow::Result<bool>{record_edge(c,principal,agent,kind,key,at,None)}

/// Which end of a turn a turn use is (step 1): when it started or when it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Edge{Start,End}
impl Edge{fn word(self)->&'static str{match self{Self::Start=>"start",Self::End=>"end"}}}

/// A use, kept once by its key; a turn's (`turn`, `subagent_turn`) says which end of it it is.
pub fn record_edge(c:&Connection,principal:&str,agent:&str,kind:&str,key:&str,at:i64,edge:Option<Edge>)->anyhow::Result<bool>{
    anyhow::ensure!(at>=0,"activity time cannot be negative");
    let _:UseKind=serde_json::from_value(serde_json::Value::String(kind.into()))?;
    Ok(c.execute("INSERT OR IGNORE INTO attention_uses(principal,agent,kind,key,at,edge) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,agent,kind,key,at,edge.map(Edge::word)])?>0)
}
/// Every use recorded for `principal` (and kiln's own, the agents' turns), in time order (the tests' view of the table).
#[cfg(test)]
pub fn uses(c:&Connection,principal:&str)->anyhow::Result<Vec<kiln_ci::attention::Use>>{
    let mut q=c.prepare("SELECT agent,kind,at FROM attention_uses WHERE principal=?1 OR principal='' ORDER BY at,agent,kind,key")?;
    let rows:Vec<(String,String,i64)>=q.query_map([principal],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
    rows.into_iter().map(|(agent,kind,at)|Ok(kiln_ci::attention::Use{agent,kind:serde_json::from_value(serde_json::Value::String(kind))?,at})).collect()
}

/// A use as the `uses` API gives it (step 1): its agent, kind and time, and for a turn which end it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UseRow{pub agent:String,pub kind:UseKind,pub at:i64,#[serde(skip_serializing_if="Option::is_none")]pub edge:Option<Edge>}
/// The person's uses and their agents' own turns, oldest first.
pub fn use_rows(c:&Connection,principal:&str)->anyhow::Result<Vec<UseRow>>{
    let mut q=c.prepare("SELECT agent,kind,at,edge FROM attention_uses WHERE principal=?1 OR principal='' ORDER BY at,agent,kind,key")?;
    let rows:Vec<(String,String,i64,Option<String>)>=q.query_map([principal],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<_,_>>()?;
    rows.into_iter().map(|(agent,kind,at,edge)|Ok(UseRow{agent,kind:serde_json::from_value(serde_json::Value::String(kind))?,at,edge:edge.map(|e|serde_json::from_value(serde_json::Value::String(e))).transpose()?})).collect()
}

/// Since when nothing has happened between `principal` and `agent` (step 8): its latest use, theirs or its own turns',
/// or the end of its last park when that came later. None without any history: no evidence of its age.
pub fn idle_since(c:&Connection,principal:&str,agent:&str,placement:&Placement)->anyhow::Result<Option<i64>>{
    let last:Option<i64>=c.query_row("SELECT MAX(at) FROM attention_uses WHERE agent=?1 AND (principal=?2 OR principal='')",params![agent,principal],|r|r.get(0))?;
    Ok(last.map(|at|placement.woke_at.map_or(at,|woke|at.max(woke))))
}
/// Notifications, links and search are deliberate choices without a list position.
pub fn direct_choice(c:&Connection,person:&str,agent:&str,key:&str,candidates:&[String],at:i64)->anyhow::Result<bool>{
    if !candidates.iter().any(|id|id==agent){return Ok(false);}
    Ok(c.execute("INSERT OR IGNORE INTO attention_choices(principal,key,at,chosen,candidates,source) VALUES(?1,?2,?3,?4,?5,'direct')",params![person,key,at,agent,serde_json::to_string(candidates)?])?>0)
}
/// Every choice recorded for `principal`, in time order (the tests' view of the table).
#[cfg(test)]
pub fn choices(c:&Connection,principal:&str)->anyhow::Result<Vec<kiln_ci::attention::Choice>>{
    let mut q=c.prepare("SELECT at,chosen,candidates,propensity,correction,evidence FROM attention_choices WHERE principal=?1 ORDER BY at,key")?;
    type Row=(i64,String,String,Option<f64>,Option<String>,Option<String>);
    let rows:Vec<Row>=q.query_map([principal],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?.collect::<Result<_,_>>()?;
    rows.into_iter().map(|(at,chosen,j,propensity,correction,evidence)|Ok(kiln_ci::attention::Choice{at,chosen,candidates:serde_json::from_str(&j)?,propensity,correction:correction.map(|j|serde_json::from_str(&j)).transpose()?,evidence:evidence.map(|e|serde_json::from_value(serde_json::Value::String(e))).transpose()?.unwrap_or_default()})).collect()
}

#[cfg(test)]
mod tests{
    use super::*;
    fn db()->anyhow::Result<Connection>{let c=Connection::open_in_memory()?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;c.execute_batch(include_str!("../../store/migrations/20261108-0101_w21_attention_examination.sql"))?;Ok(c)}
    #[test]
    fn mc_attention_people_and_undo_are_isolated()->anyhow::Result<()>{
        let c=db()?;let before=placement(&c,"a","chat")?;
        let p=put(&c,"a","chat",&before,&Placement{pinned:Some(12),..before.clone()},true)?;
        assert!(placement(&c,"b","chat")?.pinned.is_none());
        let parked=put(&c,"a","chat",&p,&Placement{pinned:None,parked_at:Some(15),parked_until:Some(99),..p.clone()},true)?;
        assert!(undo(&c,"a","chat",p.revision,16).is_err());
        assert_eq!(undo(&c,"a","chat",parked.revision,16)?.pinned,Some(12));Ok(())
    }
    #[test]
    fn mc_attention_undo_retains_exact_deferred_requests_until_the_park_expires()->anyhow::Result<()>{
        let c=db()?;let base=placement(&c,"person","chat")?;
        let request=DeferredRequest{agent:"chat".into(),kind:DeferredKind::Approval,id:"rpc:1".into(),at:1000};
        let parked=put(&c,"person","chat",&base,&base.parked(10,100,vec![request.clone()],"You parked it".into(),false),true)?;
        let active=put(&c,"person","chat",&parked,&parked.woke(20,"You brought it back".into(),false),true)?;
        let restored=undo(&c,"person","chat",active.revision,50)?;assert_eq!(restored.deferred_requests,vec![request]);
        let active=put(&c,"person","chat",&restored,&restored.woke(60,"You brought it back".into(),false),true)?;
        let expired=undo(&c,"person","chat",active.revision,101)?;assert!(expired.parked_until.is_none());assert!(expired.deferred_requests.is_empty());Ok(())
    }
    #[test]
    fn mc_attention_direct_navigation_is_a_choice_without_position_weighting()->anyhow::Result<()>{
        let c=db()?;let available=vec!["a".into(),"b".into()];
        assert!(direct_choice(&c,"person","b","notification",&available,100)?);
        assert!(!direct_choice(&c,"person","b","notification",&available,200)?);
        assert!(!direct_choice(&c,"person","inaccessible","bad-link",&available,200)?);
        let rows=choices(&c,"person")?;assert_eq!(rows.len(),1);assert_eq!(rows.first().map(|r|r.at),Some(100));assert!(rows.first().is_some_and(|r|r.propensity.is_none() && r.correction.is_none()));
        let source:String=c.query_row("SELECT source FROM attention_choices WHERE key='notification'",[],|r|r.get(0))?;assert_eq!(source,"direct");Ok(())
    }
    #[test]
    fn mc_attention_retries_and_noise_do_not_add_interest()->anyhow::Result<()>{let c=db()?;assert!(record(&c,"a","chat","message","m1",10)?);assert!(!record(&c,"a","chat","message","m1",11)?);assert!(record(&c,"a","chat","status","poll",12).is_err());assert_eq!(uses(&c,"a")?.len(),1);assert!(uses(&c,"b")?.is_empty());Ok(())}
    #[test]
    fn mc_attention_personal_placement_survives_restart_without_expiry()->anyhow::Result<()>{
        let dir=crate::os::home_dir().join(".cache/kiln-attention-tests");std::fs::create_dir_all(&dir)?;
        let path=dir.join(format!("{}.db",crate::util::new_id()));
        {
            let c=Connection::open(&path)?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;
            c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;
            let before=placement(&c,"person","chat")?;
            put(&c,"person","chat",&before,&Placement{pinned:Some(1),updated_seen:Some(2),..before.clone()},true)?;
            record(&c,"person","chat","message","original",3)?;
        }
        {
            let c=Connection::open(&path)?;let p=placement(&c,"person","chat")?;
            assert_eq!(p.pinned,Some(1));assert_eq!(p.updated_seen,Some(2));assert_eq!(uses(&c,"person")?.len(),1);
            assert_eq!(placement(&c,"other","chat")?,Placement::default());
        }
        std::fs::remove_file(path)?;Ok(())
    }
    #[test]
    fn mc_attention_park_times_are_settings_and_validate()->anyhow::Result<()>{
        let c=db()?;let mut s=Settings::default();
        assert_eq!((s.park_later_minutes,s.park_evening_hour,s.park_morning_hour),(60,18,9),"the plan's park menu: later today, this evening, tomorrow morning");
        s.park_later_minutes=0;assert!(save_settings(&c,"a",&s).is_err());
        s.park_later_minutes=90;s.park_evening_hour=24;assert!(save_settings(&c,"a",&s).is_err());
        s.park_evening_hour=19;s.park_morning_hour=8;save_settings(&c,"a",&s)?;assert_eq!(settings(&c,"a")?,s);Ok(())
    }
    #[test]
    fn mc_attention_settings_are_declared_and_not_notice_ttl()->anyhow::Result<()>{let c=db()?;assert_eq!(settings(&c,"a")?,Settings::default());let mut s=Settings{auto_park_days:Some(3),..Settings::default()};save_settings(&c,"a",&s)?;assert!(settings(&c,"b")?.auto_park_days.is_none());s.archive_after_days=0;assert!(save_settings(&c,"a",&s).is_err());Ok(())}
    /// The person's decision (2026-11-08, "Just turn it on"): parked chats are archived after a week unless a person turns it off.
    #[test]
    fn mc_attention_automatic_archive_is_on_by_default_after_a_week(){
        let s=Settings::default();assert!(s.auto_archive);assert_eq!(s.archive_after_days,7);assert_eq!(s.auto_park_days,None,"automatic parking stays off until turned on");
    }
    /// Step 8: an automatic park always ends; with archive on at the archive, with it off after as long as the quiet that made it.
    #[test]
    fn mc_attention_an_automatic_park_ends_at_the_archive_or_after_its_own_quiet(){
        let on=Settings{auto_archive:true,archive_after_days:7,..Settings::default()};
        let off=Settings{auto_archive:false,..on.clone()};
        assert_eq!(automatic_park_ends(&on,3,1_000),1_000+7*DAY_MS);
        assert_eq!(automatic_park_ends(&off,3,1_000),1_000+3*DAY_MS);
    }
}

/// The whole family is durable before its first process is stopped. One savepoint: the store's call is
/// already in its writer's transaction (`Store::call`), and a refused member leaves nothing of the group.
#[cfg(test)]
pub fn begin_archive(c:&mut Connection,root:&str,members:&[(String,String,String)],at:i64)->anyhow::Result<()>{begin_archive_checked(c,root,members,at,|_|Ok(()))}
pub fn begin_archive_checked(c:&mut Connection,root:&str,members:&[(String,String,String)],at:i64,check:impl FnOnce(&Connection)->anyhow::Result<()>)->anyhow::Result<()>{
    let tx=c.savepoint()?;check(&tx)?;
    let ids:Vec<&str>=members.iter().map(|(id,_,_)|id.as_str()).collect();
    let existing:Option<String>=tx.query_row("SELECT members FROM attention_archive_groups WHERE root=?1",[root],|r|r.get(0)).optional()?;
    if let Some(existing)=existing{
        let old:Vec<String>=serde_json::from_str(&existing)?;
        anyhow::ensure!(old.iter().map(String::as_str).eq(ids.iter().copied()),"archive membership changed; finish its existing operation first");
    }else{
        for(id,owner,payload)in members{
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attention_archive WHERE agent=?1)",[id],|r|r.get(0))?;
            anyhow::ensure!(!exists,"{id} already belongs to another archive");
            tx.execute("INSERT INTO attention_archive(agent,principal,at,value,status) VALUES(?1,?2,?3,?4,'archiving')",params![id,owner,at,payload])?;
        }
        tx.execute("INSERT INTO attention_archive_groups(root,members,status) VALUES(?1,?2,'archiving')",params![root,serde_json::to_string(&ids)?])?;
    }
    tx.commit()?;Ok(())
}

pub fn archive_members(c:&Connection,root:&str)->anyhow::Result<Option<(Vec<String>,String)>>{
    let row:Option<(String,String)>=c.query_row("SELECT members,status FROM attention_archive_groups WHERE root=?1",[root],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(j,status)|Ok((serde_json::from_str(&j)?,status))).transpose()
}

/// Individual restored rows stay recoverable until the whole group's restore is acknowledged.
pub fn finish_restore(c:&mut Connection,root:&str)->anyhow::Result<()>{
    let tx=c.savepoint()?;
    let members=archive_members(&tx,root)?.ok_or_else(||anyhow::anyhow!("archive group disappeared"))?.0;
    for id in members{tx.execute("DELETE FROM attention_archive WHERE agent=?1",[id])?;}
    tx.execute("DELETE FROM attention_archive_groups WHERE root=?1",[root])?;
    tx.commit()?;Ok(())
}

#[cfg(test)]
mod archive_group_tests{
    use super::*;
    fn db()->anyhow::Result<Connection>{let c=Connection::open_in_memory()?;c.execute_batch("CREATE TABLE audit2(id INTEGER PRIMARY KEY);")?;c.execute_batch(include_str!("../../store/migrations/20261108-0100_w21_attention.sql"))?;c.execute_batch(include_str!("../../store/migrations/20261108-0101_w21_attention_examination.sql"))?;Ok(c)}
    fn members()->Vec<(String,String,String)>{vec![("sub".into(),"person".into(),"sub-payload".into()),("parent".into(),"person".into(),"parent-payload".into())]}
    #[test]
    fn mc_attention_partial_archive_and_restore_keep_the_complete_family()->anyhow::Result<()>{
        let mut c=db()?;begin_archive(&mut c,"parent",&members(),1)?;
        c.execute("UPDATE attention_archive SET status='archived' WHERE agent='sub'",[])?;
        assert_eq!(archive_members(&c,"parent")?.map(|(ids,_)|ids),Some(vec!["sub".into(),"parent".into()]));
        let n:i64=c.query_row("SELECT COUNT(*) FROM attention_archive",[],|r|r.get(0))?;assert_eq!(n,2);
        // A failed parent stop or partial Restore retains every payload for an idempotent retry.
        c.execute("UPDATE attention_archive_groups SET status='restoring' WHERE root='parent'",[])?;
        assert_eq!(archive_members(&c,"parent")?.map(|(_,status)|status).as_deref(),Some("restoring"));
        assert_eq!(c.query_row("SELECT value FROM attention_archive WHERE agent='parent'",[],|r|r.get::<_,String>(0))?,"parent-payload");
        finish_restore(&mut c,"parent")?;assert!(archive_members(&c,"parent")?.is_none());
        assert_eq!(c.query_row("SELECT COUNT(*) FROM attention_archive",[],|r|r.get::<_,i64>(0))?,0);Ok(())
    }
    #[test]
    fn mc_attention_archive_group_conflict_changes_nothing_and_preserves_independent_archives()->anyhow::Result<()>{
        let mut c=db()?;begin_archive(&mut c,"independent",&[("independent".into(),"person".into(),"own-payload".into())],1)?;
        let conflict=vec![("new-sub".into(),"person".into(),"new".into()),("independent".into(),"person".into(),"overwritten".into())];
        assert!(begin_archive(&mut c,"other",&conflict,2).is_err());
        assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM attention_archive WHERE agent='new-sub')",[],|r|r.get::<_,bool>(0))?);
        assert!(archive_members(&c,"other")?.is_none());
        begin_archive(&mut c,"parent",&members(),3)?;begin_archive(&mut c,"parent",&members(),4)?;finish_restore(&mut c,"parent")?;
        assert_eq!(c.query_row("SELECT value FROM attention_archive WHERE agent='independent'",[],|r|r.get::<_,String>(0))?,"own-payload");Ok(())
    }
}
