//! The machine's microVMs through Hex (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17; #73 part 7): the
//! `Sandbox` seam over Hex's runtime (`hex_api::Runtime`), each launch a Shards image box of its own, its harness
//! member driven. Not yet: every launch is refused.

use std::sync::Arc;

use futures::future::BoxFuture;
use hex_api::Runtime;
use kiln_ci::place::holder::BoxName;
use kiln_seams::sandbox::{Holds, Launch, Refusal, Run, Sandbox};

/// What a box of Hex's holds its run to.
pub const HOLDS: Holds = Holds { memory: false, processes: false, weights: false, no_egress: true, egress_by_host: true, shares: true, signals: false, credentials: false };

/// The machine's microVMs as Hex holds them.
pub struct HexVms {
    /// Hex's runtime, shared as Hex hands it out (`hex_node::images::ImageRuntime::new` answers an `Arc`).
    _runtime: Arc<dyn Runtime>,
    _names: Box<dyn Fn() -> BoxName + Send + Sync>,
}

impl HexVms {
    /// Over Hex's runtime `runtime`, naming each launch's box with `names`.
    pub fn new(runtime: Arc<dyn Runtime>, names: impl Fn() -> BoxName + Send + Sync + 'static) -> Self {
        Self { _runtime: runtime, _names: Box::new(names) }
    }
}

impl Sandbox for HexVms {
    fn launch<'a>(&'a self, _launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async { Err(Refusal::Unsupported("a run through Hex, not yet".into())) })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["microvm"]
    }

    fn holds(&self) -> Holds {
        HOLDS
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hex_api::sim::{SIM_IMAGE, SimRuntime};
    use hex_api::{BoxId, Destination, Grant, Member, MemberKind, Ports, Proto, Runtime as _};
    use kiln_ci::place::holder::BoxName;
    use kiln_seams::sandbox::{Boundary, Egress, Exit, Launch, Limits, Output, Refusal, Run, Sandbox as _, Signal};

    use super::HexVms;

    /// The sandbox over Hex's simulator, its boxes numbered as a runner process of epoch 7 numbers those of its runs
    /// that reserve nothing.
    fn vms() -> (Arc<SimRuntime>, HexVms) {
        let sim = Arc::new(SimRuntime::new());
        let n = AtomicU64::new(0);
        let vms = HexVms::new(Arc::<SimRuntime>::clone(&sim), move || BoxName::Unreserved { epoch: 7, n: n.fetch_add(1, Ordering::Relaxed) });
        (sim, vms)
    }

    /// Claude Code's member of the simulator's image, resuming a conversation, in its project's folder.
    fn launch(egress: Egress) -> Launch {
        Launch { limits: Limits::NONE, egress, folder: Some("/work/project".into()), boundary: Boundary::MicroVm { image: SIM_IMAGE.to_owned() }, ..Launch::of("claude", &["--resume", "c1"]) }
    }

    /// Its output and its ends, to its last word.
    async fn drain(run: &dyn Run) -> (Vec<u8>, Vec<Exit>) {
        let (mut out, mut ends) = (Vec::new(), Vec::new());
        while let Some(said) = run.next().await {
            match said {
                Output::Stdout(b) => out.extend(b),
                Output::Stderr(_) => {}
                Output::Exited(e) => ends.push(e),
            }
        }
        (out, ends)
    }

    /// #73 part 7 slice 1 (plan step 17): a launch is a Shards image box of Hex's, named as placement names it, its
    /// harness member driven: what it's given on stdin comes back on its stdout, its end once its stdin closes is
    /// its own, said once, and by then its box is gone. Its destination by name is granted by TCP to its harness
    /// member: Hex's simulator at c7603b0b takes it (`unheld` awaits UDP and IPv6 alone, no longer every egress), so
    /// an IPv6 destination is what's refused, `Awaits` as `Unsupported`; egress to whatever its image grants is
    /// refused too; neither leaves a box.
    #[test]
    fn s17_p7_a_members_run_echoes_its_stdin_ends_and_its_box_goes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            let run = vms.launch(launch(Egress::Only(vec!["API.anthropic.com:443".into(), "api.anthropic.com:443".into()]))).await?;
            assert_eq!(run.id().as_deref(), Some("kiln-z7-0"));
            let id = BoxId::new("kiln-z7-0")?;
            let harness = Member { kind: MemberKind::Harness, name: "claude".into() };
            let to = Destination::Name("api.anthropic.com".into());
            assert_eq!(sim.inspect(&id)?.grants, [Grant::Egress { member: harness, to, ports: Ports { proto: Proto::Tcp, first: 443, last: 443 } }], "one grant, by TCP to its port");
            run.stdin(b"one\n".to_vec()).await?;
            run.stdin(b"two\n".to_vec()).await?;
            run.close_stdin();
            let (out, ends) = drain(run.as_ref()).await;
            assert_eq!(String::from_utf8_lossy(&out), "one\ntwo\n");
            assert_eq!(ends, [Exit::Code(0)]);
            assert!(matches!(sim.inspect(&id), Err(hex_api::Refusal::NoSuchBox(_))), "its box is gone once its end is said");
            assert_eq!(run.stdin(b"late".to_vec()).await, Err(Refusal::Ended));
            assert_eq!(run.next().await, None);
            let any = vms.launch(launch(Egress::Any)).await.err();
            assert!(matches!(any, Some(Refusal::Unsupported(_))), "{any:?}");
            let v6 = vms.launch(launch(Egress::Only(vec!["[2001:db8::7]:443".into()]))).await.err();
            assert!(matches!(&v6, Some(Refusal::Unsupported(why)) if why.contains("IPv6")), "{v6:?}");
            let portless = vms.launch(launch(Egress::Only(vec!["api.anthropic.com".into()]))).await.err();
            assert!(matches!(portless, Some(Refusal::Invalid { what: "its egress", .. })), "{portless:?}");
            assert_eq!(sim.list()?, [], "no box is left of a refused launch");
            anyhow::Ok(())
        })
    }

    /// #73 part 7 slice 1: a signal Hex doesn't deliver yet (`MemberSignal`) is refused `Unsupported`, the run going
    /// on; a kill deletes its box, its end `Killed`, said once its box is gone; a run let go of takes its box with it.
    #[test]
    fn s17_p7_a_kill_and_a_drop_delete_the_box() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            assert!(matches!(run.signal(Signal::Interrupt), Err(Refusal::Unsupported(_))));
            run.kill();
            let (_, ends) = drain(run.as_ref()).await;
            assert_eq!(ends, [Exit::Killed]);
            assert_eq!(sim.list()?, [], "its box is gone once its end is said");
            assert_eq!(run.signal(Signal::Terminate), Err(Refusal::Ended));
            let dropped = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            assert_eq!(sim.list()?.len(), 1);
            drop(dropped);
            // Its delete runs on the blocking pool: its box goes, and stays gone.
            for _ in 0..1000 {
                if sim.list()?.is_empty() {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(sim.list()?, [], "a run let go of takes its box with it");
            anyhow::Ok(())
        })
    }
}
