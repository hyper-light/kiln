//! Targeted handler regressions with real in-memory handoff records and Instances metadata.

use super::*;
use crate::harness::mail;

fn refusal(error: Refusal) -> anyhow::Error { anyhow::anyhow!("{error:?}") }

fn hello(machine: &str) -> crate::proto::Hello {
    crate::proto::Hello { protocol:crate::proto::VERSION,name:machine.into(),labels:Vec::new(),slots:1,hostname:machine.into(),os:"linux".into(),arch:"aarch64".into(),version:"transfer-fixture".into(),ephemeral:false,dedicated_job:None,running:Vec::new(),terminal:false }
}

fn join(hub: &Hub, certificate: PeerId, machine: &str, instance: &str, at: i64) -> Result<Principal, Refusal> {
    hub.instances.update(|instances| {
        instances.announce(certificate,crate::proto::InstanceHello { id:instance.into(),created_at:at,pid:1 });
        instances.join(certificate,machine,hello(machine),at);
        instances.set_features(certificate,vec![crate::proto::feature::MAIL_TRANSFER.into()]);
    });
    principal(hub,certificate)
}

fn manifest(attempt: &str, session: &str, source_epoch: u64) -> anyhow::Result<TransferManifest> {
    Ok(TransferManifest { attempt:attempt.into(),session:session.into(),source_epoch,destination_epoch:source_epoch.checked_add(1).ok_or_else(||anyhow::anyhow!("fixture epoch overflow"))?,count:1,sha256:crate::util::sha256_hex(b"exact bounded fixture export") })
}

fn capacity(epoch: u64) -> super::super::admission::Capacity {
    super::super::admission::Capacity { epoch,generation:1,retry_generation:Some(1),learning:super::super::admission::Learning::Observed,target:1,admitted:1,bytes:1,peers:1,eligible_peer:None,feedback_generation:None,depth:None }
}

async fn commit(hub: &Hub, manifest: TransferManifest, source: Principal, destination: Principal) -> anyhow::Result<Record> {
    let offered=handoffs::offer(&hub.store,manifest.clone(),source,destination.clone()).await.map_err(refusal)?;
    assert_eq!(offered.phase,Phase::Offered);
    assert_eq!(handoffs::prepare(&hub.store,manifest.clone(),destination.clone()).await.map_err(refusal)?.phase,Phase::Prepared);
    let committed=handoffs::adopt(&hub.store,manifest,destination).await.map_err(refusal)?;
    assert_eq!(committed.phase,Phase::Committed);
    Ok(committed)
}

#[test]
fn replaying_an_older_committed_handoff_cannot_reassign_the_latest_owner() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub=crate::hub::tests::scratch_hub().await?;
        let result: anyhow::Result<()>=async {
            let session="transfer-old-replay";
            // These fixed PeerIds model certificate identity; process identity comes from Hello.
            let (a,b,c)=(0xaa_u64,0xbb_u64,0xcc_u64);
            let source=join(&hub,a,"machine-a","instance-a",1).map_err(refusal)?;
            let middle=join(&hub,b,"machine-b","instance-b",2).map_err(refusal)?;
            let destination=join(&hub,c,"machine-c","instance-c",3).map_err(refusal)?;
            let original=mail::Publish { owner:"owner".into(),sender:"sender".into(),recipient:session.into(),id:"immutable-original".into(),command:serde_json::json!({"type":"send","id":"immutable-original","text":"exact retained original 日本語"}),provenance:None,status:None };
            let publication=mail::publish(&hub.store,original.clone(),u64::MAX).await?;
            let delivery=mail::claim_at(&hub.store,publication.revision,1,1,"fixture-source".into(),super::super::state_view::FRAME).await?.ok_or_else(||anyhow::anyhow!("fixture original was not claimed"))?;
            let first=commit(&hub,manifest("A-B",session,1)?,source.clone(),middle.clone()).await?;
            let latest=commit(&hub,manifest("B-C",session,2)?,middle.clone(),destination.clone()).await?;
            terminal(&hub,&latest).await.map_err(refusal)?;
            let holder=hub.harness.holder(session).ok_or_else(||anyhow::anyhow!("current destination holder was not set"))?;
            assert_eq!(hub.instances.load().owner(&Owned::Agent(session.into())),Some(c));
            for (actor,input) in [
                (source.clone(),TransferUp::Offer { manifest:first.manifest.clone(),target:middle.machine.clone(),instance:middle.instance.clone() }),
                (middle.clone(),TransferUp::Ready { manifest:first.manifest.clone() }),
                (middle.clone(),TransferUp::Adopt { manifest:first.manifest.clone() }),
                (source.clone(),TransferUp::Abort { manifest:first.manifest.clone() }),
            ] {
                apply(&hub,actor,input).await.map_err(refusal)?;
                assert_eq!(hub.instances.load().owner(&Owned::Agent(session.into())),Some(c),"an old attempt receipt cannot move current routing backwards");
                assert_eq!(hub.harness.holder(session).as_deref(),Some(holder.as_str()));
                assert_eq!(handoffs::latest(&hub.store,session.into()).await.map_err(refusal)?,Some(latest.clone()));
                assert!(dispatch_ready(&hub,session,&capacity(3)).await.map_err(refusal)?);
            }
            let replay=mail::replay_at(&hub.store,delivery.token.revision,3,"fixture-destination".into(),super::super::state_view::FRAME).await?.ok_or_else(||anyhow::anyhow!("latest destination cannot reconcile unchanged token"))?;
            assert_eq!(replay.token,delivery.token);assert_eq!(replay.publication,original);
            Ok(())
        }.await;
        hub.shutdown.cancel();
        result
    })
}

#[test]
fn same_certificate_with_a_new_instance_cannot_pass_stored_transfer_principal_gates() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub=crate::hub::tests::scratch_hub().await?;
        let result: anyhow::Result<()>=async {
            let session="transfer-instance-fence";let (a,b)=(0xdd_u64,0xee_u64);
            let source=join(&hub,a,"machine-source","source-instance",1).map_err(refusal)?;
            let original=join(&hub,b,"machine-destination","destination-original",2).map_err(refusal)?;
            let committed=commit(&hub,manifest("instance-handoff",session,1)?,source,original.clone()).await?;
            hub.instances.update(|instances|instances.own(Owned::Agent(session.into()),b));
            assert!(allows_claim(&hub,b,session,Some(2)).await);assert!(allows_claim(&hub,b,session,None).await);
            assert!(allows_generic_adoption(&hub,b,session,true).await);assert!(dispatch_ready(&hub,session,&capacity(2)).await.map_err(refusal)?);
            assert!(!allows_claim(&hub,b,session,Some(1)).await,"current identity still needs the committed epoch");
            hub.instances.update(|instances|instances.connected(b));
            assert!(!hub.instances.load().current_connection(b));
            assert!(!allows_claim(&hub,b,session,Some(2)).await,"a certificate reconnect cannot reuse old Hello authority");
            assert!(!allows_generic_adoption(&hub,b,session,true).await);
            assert!(!dispatch_ready(&hub,session,&capacity(2)).await.map_err(refusal)?);
            assert_eq!(send(&hub,&original,TransferDown::Committed { manifest:committed.manifest.clone() }).await,Err(Refusal::Unavailable));
            // A real Hello for the same process is a positive reconnect control.
            assert_eq!(join(&hub,b,"machine-destination","destination-original",2).map_err(refusal)?,original);
            assert!(hub.instances.load().current_connection(b));
            assert!(allows_claim(&hub,b,session,Some(2)).await);assert!(allows_generic_adoption(&hub,b,session,true).await);assert!(dispatch_ready(&hub,session,&capacity(2)).await.map_err(refusal)?);
            send(&hub,&original,TransferDown::Committed { manifest:committed.manifest.clone() }).await.map_err(refusal)?;
            hub.instances.update(|instances|instances.connected(b));
            let restarted=join(&hub,b,"machine-destination","destination-restarted",3).map_err(refusal)?;
            assert_eq!(restarted.peer,original.peer);assert_eq!(restarted.machine,original.machine);assert_ne!(restarted.instance,original.instance);
            assert!(hub.instances.load().current_connection(b));assert_eq!(hub.instances.load().owner(&Owned::Agent(session.into())),Some(b));
            assert!(!allows_claim(&hub,b,session,Some(2)).await,"same certificate and valid epoch are insufficient after instance change");
            assert!(!allows_claim(&hub,b,session,None).await,"inventory also requires the stored instance");
            assert!(!allows_generic_adoption(&hub,b,session,true).await);
            assert!(!dispatch_ready(&hub,session,&capacity(2)).await.map_err(refusal)?);
            assert_eq!(send(&hub,&original,TransferDown::Committed { manifest:committed.manifest.clone() }).await,Err(Refusal::Stale));
            assert_eq!(handoffs::latest(&hub.store,session.into()).await.map_err(refusal)?,Some(committed),"new process metadata cannot rewrite durable authenticated binding");
            Ok(())
        }.await;
        hub.shutdown.cancel();
        result
    })
}

/// 2026-11-12: adacachy's runner started again as a new process, and the sessions a committed
/// hand-over had bound to its old process were never claimable: their Opens dropped, their mail
/// refused as a stale owner's, their streams started over without end. The process that
/// superseded the binding's, holding the session past the binding's epoch, claims it and gets
/// its mail; at the binding's epoch, or with no epoch, it doesn't (the instance fence stands),
/// and while the old process is connected no other process does.
#[test]
fn superseded_binding_lets_its_successor_past_the_epoch_claim() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub=crate::hub::tests::scratch_hub().await?;
        let result: anyhow::Result<()>=async {
            let session="transfer-superseded-binding";let (a,b,c)=(0x1a_u64,0x1b_u64,0x1c_u64);
            let source=join(&hub,a,"machine-source","source-instance",1).map_err(refusal)?;
            let bound=join(&hub,b,"machine-held","held-old",2).map_err(refusal)?;
            commit(&hub,manifest("superseded-binding",session,4)?,source,bound).await?;
            // A second process of the machine while the bound one is connected: no claim.
            join(&hub,c,"machine-held","held-other",3).map_err(refusal)?;
            assert!(!allows_claim(&hub,c,session,Some(6)).await,"the bound process is connected: no other process claims");
            // The bound process ends; its machine's runner starts again.
            hub.instances.update(|instances|{instances.lost(b);instances.lost(c);});
            join(&hub,b,"machine-held","held-new",4).map_err(refusal)?;
            hub.instances.update(|instances|instances.own(Owned::Agent(session.into()),b));
            assert!(!allows_claim(&hub,b,session,Some(5)).await,"at the binding's epoch the fence stands");
            assert!(!allows_claim(&hub,b,session,None).await,"an inventory carries no epoch: the fence stands");
            assert!(!dispatch_ready(&hub,session,&capacity(5)).await.map_err(refusal)?);
            assert!(allows_claim(&hub,b,session,Some(6)).await,"past the binding's epoch its successor claims it");
            assert!(dispatch_ready(&hub,session,&capacity(6)).await.map_err(refusal)?,"and its mail goes to it");
            Ok(())
        }.await;
        hub.shutdown.cancel();
        result
    })
}

/// 2026-11-12, Amphora: a hand-over its source's process prepared and then left (that process
/// superseded) stood prepared for good, its session unclaimable and its mail held. It commits for
/// the destination that prepared it and is connected; with that destination gone too it aborts.
#[test]
fn unfinished_handover_of_a_superseded_source_settles() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub=crate::hub::tests::scratch_hub().await?;
        let result: anyhow::Result<()>=async {
            let (a,b)=(0x2a_u64,0x2b_u64);
            let source=join(&hub,a,"machine-one","one-old",1).map_err(refusal)?;
            let destination=join(&hub,b,"machine-one","one-next",2).map_err(refusal)?;
            let committed="transfer-prepared-live";
            let m=manifest("prepared-live",committed,0)?;
            handoffs::offer(&hub.store,m.clone(),source.clone(),destination.clone()).await.map_err(refusal)?;
            handoffs::prepare(&hub.store,m,destination.clone()).await.map_err(refusal)?;
            let aborted="transfer-offered-gone";
            let n=manifest("offered-gone",aborted,0)?;
            handoffs::offer(&hub.store,n,source,destination).await.map_err(refusal)?;
            // While the source is connected nothing moves.
            assert!(!allows_claim(&hub,b,committed,Some(1)).await);
            assert_eq!(handoffs::latest(&hub.store,committed.into()).await.map_err(refusal)?.map(|r|r.phase),Some(Phase::Prepared));
            // The source's process ends and its runner starts again.
            hub.instances.update(|instances|{instances.lost(a);});
            join(&hub,a,"machine-one","one-again",3).map_err(refusal)?;
            hub.instances.update(|instances|instances.own(Owned::Agent(committed.into()),b));
            assert!(allows_claim(&hub,b,committed,Some(1)).await,"the prepared destination claims it at its epoch");
            assert_eq!(handoffs::latest(&hub.store,committed.into()).await.map_err(refusal)?.map(|r|r.phase),Some(Phase::Committed));
            assert!(dispatch_ready(&hub,committed,&capacity(1)).await.map_err(refusal)?);
            // Offered only (never prepared): aborted, the session back with its source's epoch.
            assert!(dispatch_ready(&hub,aborted,&capacity(0)).await.map_err(refusal)?);
            assert_eq!(handoffs::latest(&hub.store,aborted.into()).await.map_err(refusal)?.map(|r|r.phase),Some(Phase::Aborted));
            Ok(())
        }.await;
        hub.shutdown.cancel();
        result
    })
}
