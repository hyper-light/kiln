//! Owned Reach work, plan 01a199e1b177752e84dad5dc990e6a15: a completed MCP call
//! is not proof that its remote shell exited. Only transport lifecycle evidence ends work.
use std::sync::{Arc, Weak, atomic::{AtomicBool, Ordering}};
use parking_lot::Mutex;
use crate::harness::{host::Host, session::Session};
use super::local::{Ev, Req, RunReq};

pub const PREFIX: &str = "reach-owned-";

pub struct Run {
    pub id: String,
    pub agent: String,
    host: Weak<Host>,
    session: Weak<Session>,
    launched: AtomicBool,
    cancelled: AtomicBool,
    confirmed_exit: AtomicBool,
    stop: async_channel::Sender<()>,
    outcome: Mutex<Option<Result<(), String>>>,
    changed: event_listener::Event,
}

impl Run {
    pub fn new(host: &Arc<Host>, session: &Arc<Session>) -> (Arc<Self>, async_channel::Receiver<()>) {
        let (stop, recv) = async_channel::unbounded();
        (Arc::new(Self { id: format!("{PREFIX}{}",crate::util::new_id()), agent: session.id.clone(), host: Arc::downgrade(host), session: Arc::downgrade(session), launched: AtomicBool::new(false), cancelled: AtomicBool::new(false), confirmed_exit:AtomicBool::new(false), stop, outcome: Mutex::new(None), changed: event_listener::Event::new() }), recv)
    }
    pub fn launching(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Acquire) { return Err("owned run was stopped before launch".into()); }
        self.launched.store(true, Ordering::Release); Ok(())
    }
    pub fn stop(&self) { self.cancelled.store(true, Ordering::Release); let _=self.stop.try_send(()); }
    pub fn confirm_exit(&self){self.confirmed_exit.store(true,Ordering::Release);}
    pub async fn wait(&self) -> Result<(), String> {
        loop {
            let changed=self.changed.listen();
            if let Some(outcome)=self.outcome.lock().clone(){return outcome;}
            changed.await;
        }
    }
    async fn see(&self, event: &Ev) {
        let terminal=match event {
            Ev::Exit{error:None,..}=>Some(Ok(())),
            Ev::Exit{error:Some(error),..}=>Some(if self.confirmed_exit.swap(false,Ordering::AcqRel) || !self.launched.load(Ordering::Acquire){Ok(())}else{Err(error.clone())}),
            Ev::Error(error)=>Some(if self.launched.load(Ordering::Acquire){Err(error.clone())}else{Ok(())}),
            _=>None,
        };
        if let (Some(host),Some(session))=(self.host.upgrade(),self.session.upgrade()) {
            let update={
                let mut core=session.core.lock();
                if let Some(task)=core.owned_reach.get_mut(&self.id){
                    match event{
                        Ev::Started{runner,..}=>{task.status="running".into();task.started=crate::util::now_ms();task.summary=Some(format!("Running on {runner}"));},
                        Ev::Exit{code,error,..}=>{if terminal.as_ref().is_some_and(Result::is_ok){task.status=if *code==Some(0){"completed"}else{"failed"}.into();task.ended=Some(crate::util::now_ms());task.exit_code = *code;}task.summary=error.clone();},
                        Ev::Error(error)=>{if terminal.as_ref().is_some_and(Result::is_ok){task.status="failed".into();task.ended=Some(crate::util::now_ms());}task.summary=Some(error.clone());},
                        _=>{},
                    }
                }
                core.sync_owned_reach();
                if matches!(event,Ev::Started{..}|Ev::Exit{..}|Ev::Error(_)){core.touch_state();true}else{false}
            };
            if update && let Err(error)=host.publish_owned(&session).await{tracing::warn!(agent=%self.agent,%error,"owned Reach work could not be recorded");}
        }
        if let Some(terminal)=terminal{
            let confirmed=terminal.is_ok();
            *self.outcome.lock()=Some(terminal);self.changed.notify(usize::MAX);
            if confirmed && let Some(session)=self.session.upgrade(){session.owned_reach.lock().remove(&self.id);}
        }
    }
}

/// A closed command bridge is not completion of the exit observer. In particular,
/// the transport can drop its command receiver while the final event is being persisted.
async fn finish_owned_work<W,B>(work:W,bridge:B)
where W:std::future::Future<Output=()>,B:std::future::Future<Output=()>{
    futures::pin_mut!(work,bridge);
    if let futures::future::Either::Right(((),remaining))=futures::future::select(work,bridge).await{remaining.await;}
}

/// A bound run keeps its controller and exit observer even after a detached CLI client leaves.
/// No human/unowned run is registered or stopped through this path.
pub async fn run(host: Option<Arc<Host>>, handle:&super::node::Handle, me:&str, req:RunReq, ev:async_channel::Sender<Ev>, ctl:async_channel::Receiver<Req>, reuse:bool){
    let Some(agent)=req.asker.as_deref() else{
        if reuse{super::reuse::run(handle,me,req,ev,ctl).await;}else{super::local::run(handle,me,req,ev,ctl).await;}return;
    };
    let Some(host)=host else{let _=ev.send(Ev::Error("the authenticated agent has no owned-work tracker on this runner".into())).await;return;};
    let (owned,stop)=match host.begin_owned_reach(agent,&req).await{Ok(run)=>run,Err(error)=>{let _=ev.send(Ev::Error(error)).await;return;}};
    let (events,seen)=async_channel::unbounded();let (control,commands)=async_channel::unbounded();
    let execute=async{
        if reuse{super::reuse::run_owned(handle,me,req,events,commands,Some(owned.clone())).await;}else{super::local::run_owned(handle,me,req,events,commands,Some(owned.clone())).await;}
    };
    let forward=async{
        while let Ok(event)=seen.recv().await{owned.see(&event).await;let _=ev.send(event).await;}
        let missing=owned.outcome.lock().is_none();
        if missing{owned.see(&Ev::Error("lost confirmation of the owned run's exit".into())).await;}
    };
    let bridge=async{
        let mut client=true;
        loop{
            use futures::FutureExt as _;
            let receiving=client;
            let next=async{if receiving{ctl.recv().await}else{futures::future::pending().await}}.fuse();
            let stopping=stop.recv().fuse();futures::pin_mut!(next,stopping);
            let command=futures::select!{request=next=>match request{Ok(request)=>Some(request),Err(_)=>{client=false;None}},signal=stopping=>match signal{Ok(())=>Some(Req::Signal(super::wire::Signal::Kill)),Err(_)=>return}};
            if let Some(command)=command && control.send(command).await.is_err(){return;}
        }
    };
    let work=async{futures::join!(execute,forward);};
    finish_owned_work(work,bridge).await;
}

#[cfg(test)]
mod tests{
    use super::*;
    use crate::harness::{model::Status,session::Spec};
    fn open(dir:&std::path::Path,id:&str)->anyhow::Result<(Arc<Host>,Arc<Session>,async_channel::Receiver<(String,crate::harness::core::Update)>)>{
        let(tx,rx)=async_channel::unbounded();
        let host=Host::new(tx,Some(dir.join("harness.json")),Default::default());
        let session=host.open(Some(id.into()),Spec{kind:"claude".into(),program:Some(dir.join("no-native-provider").to_string_lossy().into_owned()),cwd:Some(dir.to_string_lossy().into_owned()),..Default::default()})?;
        session.core.lock().state.status=Status::Idle;
        Ok((host,session,rx))
    }
    fn request(agent:&str)->RunReq{RunReq{asker:Some(agent.into()),command:"a simulated remote script".into(),detach:true,..Default::default()}}

    #[test]
    fn mc_attention_owned_reach_lives_past_main_turn_until_actual_exit()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-live")?;
            let(run,_stop)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;
            run.launching().map_err(anyhow::Error::msg)?;
            run.see(&Ev::Started{runner:"capable-runner".into(),pid:7,cwd:"repo".into()}).await;
            {let mut core=session.core.lock();core.end_harness_work();core.state.status=Status::Idle;core.state.tasks.clear();}
            let info=session.info();assert_eq!(info.owned_work_known,Some(true));assert_eq!(info.tasks_running,1);assert_eq!(info.left_behind,1);assert!(info.subagents.is_empty());
            session.core.lock().state.tasks.clear();
            host.publish_owned(&session).await.map_err(anyhow::Error::msg)?;
            assert!(session.snapshot().iter().any(|update|matches!(update,crate::harness::core::Update::State{state} if state.tasks.iter().any(|task|task.id==run.id && task.live()))));
            let saved:serde_json::Value=serde_json::from_reader(std::fs::File::open(dir.path().join("harness.json"))?)?;
            assert_eq!(saved.pointer("/0/owned_reach/0/id").and_then(serde_json::Value::as_str),Some(run.id.as_str()));
            run.see(&Ev::Exit{code:Some(0),error:None,lost:0}).await;
            assert_eq!(session.info().tasks_running,0);assert_eq!(session.info().left_behind,0);
            assert!(run.wait().await.is_ok());host.close(&session.id);Ok(())
        })
    }

    #[test]
    fn mc_attention_archive_fences_new_owned_launches_and_waits_for_confirmed_exit()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use futures::FutureExt as _;
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-fence")?;
            let(run,stop)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
            session.core.lock().state.tasks.clear(); // A native refresh must not lose ownership.
            let archive=host.archive(&session.id);
            let answer=async{
                stop.recv().await?;
                assert!(host.get(&session.id).is_some(),"a requested kill is not an exit confirmation");
                let req=request(&session.id);let pending=host.begin_owned_reach(&session.id,&req).fuse();futures::pin_mut!(pending);
                assert!(futures::poll!(pending.as_mut()).is_pending(),"Archive holds launch admission");
                run.see(&Ev::Exit{code:None,error:None,lost:0}).await;
                Ok::<_,anyhow::Error>(())
            };
            let(result,answer)=futures::join!(archive,answer);answer?;result.map_err(anyhow::Error::msg)?;
            assert!(host.get(&session.id).is_none());
            assert!(host.begin_owned_reach(&session.id,&request(&session.id)).await.is_err(),"no launch can follow an archive");Ok(())
        })
    }

    #[test]
    fn mc_attention_unknown_remote_exit_keeps_retryable_archive_and_live_task()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-unknown")?;
            let(run,_)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
            run.see(&Ev::Error("remote exit could not be confirmed".into())).await;
            session.core.lock().state.tasks.clear();
            assert_eq!(session.info().tasks_running,1);
            assert!(host.archive(&session.id).await.is_err());assert!(host.get(&session.id).is_some());
            assert!(host.begin_owned_reach(&session.id,&request(&session.id)).await.is_err(),"pending archive refuses a new launch");
            run.see(&Ev::Exit{code:None,error:None,lost:0}).await;
            host.archive(&session.id).await.map_err(anyhow::Error::msg)?;assert!(host.get(&session.id).is_none());Ok(())
        })
    }

    #[test]
    fn mc_attention_restart_retains_owned_work_without_inventing_a_controller()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-restart")?;
            let(run,_)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
            let(tx,_updates2)=async_channel::unbounded();let restarted=Host::new(tx,Some(dir.path().join("harness.json")),Default::default());restarted.restore();
            let restored=restarted.get(&session.id).ok_or_else(||anyhow::anyhow!("missing restored owned work"))?;
            assert_eq!(restored.info().owned_work_known,Some(false));assert_eq!(restored.info().tasks_running,1);
            assert!(!restored.core.lock().state.tasks.iter().filter(|task|task.id==run.id).any(|task|task.stoppable));
            assert!(restarted.archive(&session.id).await.is_err(),"unconfirmed prior work cannot become an archived success");
            assert!(!dir.path().join("archived").join(format!("{}.json",session.id)).exists(),"unknown ownership must be refused before archive intent");
            host.close(&session.id);restarted.close(&session.id);Ok(())
        })
    }

    #[test]
    fn mc_attention_confirmed_prelaunch_refusal_does_not_leave_phantom_work()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-refused")?;
            let(run,_)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;
            run.see(&Ev::Error("no runner satisfies the declared needs".into())).await;
            assert_eq!(session.info().tasks_running,0);assert!(run.wait().await.is_ok());
            host.archive(&session.id).await.map_err(anyhow::Error::msg)?;Ok(())
        })
    }

    #[test]
    fn mc_attention_launched_refusal_is_terminal_only_with_transport_confirmation()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-confirmed-refusal")?;
            let(run,_)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
            // The same words are deliberately used for both: evidence comes from the
            // transport, never a substring guessed from an error message.
            run.see(&Ev::Exit{code:None,error:Some("run unavailable".into()),lost:0}).await;
            assert_eq!(session.info().tasks_running,1);assert!(run.wait().await.is_err());
            run.confirm_exit();
            run.see(&Ev::Exit{code:None,error:Some("run unavailable".into()),lost:0}).await;
            assert_eq!(session.info().tasks_running,0);assert!(run.wait().await.is_ok());
            host.archive(&session.id).await.map_err(anyhow::Error::msg)?;Ok(())
        })
    }

    #[test]
    fn mc_attention_closed_command_bridge_still_publishes_the_actual_exit()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use futures::FutureExt as _;
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-bridge")?;
            let(run,_stop)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
            let(control,commands)=async_channel::unbounded::<Req>();
            let(closed,closed_rx)=futures::channel::oneshot::channel();
            let(bridge_done,bridge_done_rx)=futures::channel::oneshot::channel();
            let(release,release_rx)=futures::channel::oneshot::channel();
            let observing=run.clone();
            let work=async move{
                drop(commands);let _=closed.send(());
                let _=release_rx.await;
                observing.see(&Ev::Exit{code:Some(0),error:None,lost:0}).await;
            };
            let bridge=async move{let _=closed_rx.await;assert!(control.send(Req::Signal(super::super::wire::Signal::Kill)).await.is_err());let _=bridge_done.send(());};
            let deliver=async{
                bridge_done_rx.await?;assert!(run.wait().now_or_never().is_none());
                release.send(()).map_err(|_|anyhow::anyhow!("the exit observer was cancelled when its command bridge closed"))?;Ok::<_,anyhow::Error>(())
            };
            let((),delivered)=futures::join!(finish_owned_work(work,bridge),deliver);delivered?;
            assert_eq!(session.info().tasks_running,0);
            assert!(run.wait().now_or_never().is_some_and(|outcome|outcome.is_ok()));
            host.archive(&session.id).await.map_err(anyhow::Error::msg)?;Ok(())
        })
    }
    #[test]
    fn mc_attention_ordinary_close_and_let_go_signal_the_owned_controller()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use futures::FutureExt as _;
            for let_go in [false,true]{
                let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),if let_go{"owned-let-go"}else{"owned-close"})?;
                let(run,stop)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;run.launching().map_err(anyhow::Error::msg)?;
                if let_go{assert!(host.let_go(&session.id));}else{assert!(host.close(&session.id));}
                assert!(stop.recv().now_or_never().is_some_and(|result|result.is_ok()),"dropping the owner must signal its detached remote controller");
            }Ok(())
        })
    }
    #[test]
    fn mc_attention_bad_local_cwd_is_a_confirmed_prelaunch_refusal()->anyhow::Result<()>{
        crate::rt::block_on(async{
            let dir=tempfile::tempdir()?;let(host,session,_updates)=open(dir.path(),"owned-bad-cwd")?;
            let(run,_)=host.begin_owned_reach(&session.id,&request(&session.id)).await.map_err(anyhow::Error::msg)?;
            let(events,_)=async_channel::unbounded();let(_control,commands)=async_channel::unbounded();
            let result=super::super::local::run_here(super::super::wire::RunSpec{cwd:Some(dir.path().join("missing-folder").to_string_lossy().into_owned()),command:"must not launch".into(),..Default::default()},false,&events,commands,Some(&run)).await;
            let error=result.err().ok_or_else(||anyhow::anyhow!("invalid local cwd was accepted"))?;
            assert!(!run.launched.load(Ordering::Acquire));
            run.see(&Ev::Error(error.to_string())).await;
            assert_eq!(session.info().tasks_running,0);assert!(run.wait().await.is_ok());
            host.archive(&session.id).await.map_err(anyhow::Error::msg)?;Ok(())
        })
    }
}
