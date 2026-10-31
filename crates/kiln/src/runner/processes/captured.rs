//! U08: a captured command lends its opaque custody to the existing native Group. Its
//! command task is admitted before either blocking reservation or launch, so dropping the
//! caller only signals that owner. Physical retirement runs inline in that same task.

use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;

use super::{Execution, Group};

/// The existing bounded fs share, without a fallback thread or another blocking pool. The
/// closure owns every input before submission; a canceled receiver never cancels accepted work.
/// Bridge the runtime leaf's result to a runtime-independent receiver, as the mirror lock does.
pub(super) async fn blocking<T: Send + 'static>(operation: impl FnOnce() -> T + Send + 'static) -> anyhow::Result<T> {
    let (send, receive) = futures::channel::oneshot::channel();
    let completion = hyper_rt::blocking::run(crate::rt::SHARE_FS, move || { let _ = send.send(operation()); })?;
    drop(completion);
    receive.await.context("captured command blocking operation ended without a result")
}

fn native(execution: &Execution) -> anyhow::Result<()> {
    if execution.parent.is_some() { return Err(crate::proc::CustodyRefusal::InheritedExecution.into()); }
    Ok(())
}

/// Existing reconciliation before an external File is acquired. This is not a new resource
/// allocation, private admission namespace, process controller or independently inferred grant.
pub(crate) async fn reconcile(execution: Arc<Execution>) -> anyhow::Result<()> {
    native(&execution)?;
    let admission = execution.admission.clone();
    let paths = admission.clone();
    let _host = blocking(move || crate::reach::leases::host_guard(&paths, true)).await??;
    super::reconcile_host(&admission).await?;
    blocking(move || {
        crate::reach::leases::ensure_host_clear(&admission)?;
        super::ensure_no_orphans(&admission)
    }).await??;
    Ok(())
}

/// One native command reservation within the original Execution. The immutable kernel identity
/// and the SAME opaque custody are enrolled before initialization or command launch. A task
/// that owns this reservation is already admitted; it alone performs the existing retirement.
#[derive(Clone)]
struct PreparedCapture {
    execution: Arc<Execution>,
    group: Arc<Group>,
    kernel: crate::reach::leases::Containment,
    stop: crate::rt::Shutdown,
}

impl Execution {
    fn prepare_capture(self: &Arc<Self>, custody: Box<dyn Send>, stop: crate::rt::Shutdown) -> anyhow::Result<PreparedCapture> {
        native(self)?;
        // This check is after the caller acquired its SAME external File. A preceding
        // startup check alone races an old owner dying while this operation waited on File.
        crate::reach::leases::ensure_host_clear(&self.admission)?;
        super::ensure_no_orphans(&self.admission)?;
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced && !stop.is_cancelled(), "captured command admission is closed");
        let host = self.host(&mut state)?;
        let gate = self.gate(&mut state)?;
        // No native helper starts until the custody below is enrolled. The existing Group
        // records its physical identity durably before initialize() can launch its gate.
        let group = Group::create((&self.admission, &self.work), &self.job, host, gate, self._attempt, None, false)?;
        let kernel = group.kernel.clone().context("captured command reservation has no native containment")?;
        {
            let mut held = group.ownership.lock();
            held.as_mut().context("captured command reservation lost its ownership")?._custody = Some(custody);
        }
        *group.captured_stop.lock() = Some(stop.clone());
        group.prepared.store(false, Ordering::Release);
        state.groups.retain(|g| !g.finished.is_cancelled());
        state.groups.push(Arc::clone(&group));
        Ok(PreparedCapture { execution: Arc::clone(self), group, kernel, stop })
    }
}

impl PreparedCapture {
    fn launch(&self, command: Command, until: Instant) -> anyhow::Result<crate::proc::Duplex> {
        anyhow::ensure!(self.execution.accepts_work() && !self.stop.is_cancelled() && Instant::now() < until, "captured command stopped before launch");
        self.group.initialize()?;
        let command = super::command_environment(command)?;
        anyhow::ensure!(self.execution.accepts_work() && !self.stop.is_cancelled() && Instant::now() < until, "captured command stopped before launch");
        let duplex = self.kernel.spawn(command)?;
        self.group.pgid.store(i32::try_from(duplex.pid).unwrap_or(0), Ordering::Release);
        Ok(duplex)
    }

    async fn capture(&self, command: Command, until: Instant) -> anyhow::Result<Output> {
        let launch = self.clone();
        // This closure can outlive the caller. The pre-admitted task and Group still own
        // custody while it is queued or inside an uninterruptible native spawn. It must
        // settle before emptiness can count: no accepted late launch may follow the proof.
        let launching = blocking(move || launch.launch(command, until)).fuse();
        let stop = self.stop.cancelled().fuse();
        let deadline = crate::rt::sleep(until.saturating_duration_since(Instant::now())).fuse();
        futures::pin_mut!(launching, stop, deadline);
        let duplex = futures::select_biased! {
            result = launching => result??,
            _ = stop => {
                let kernel = self.kernel.clone();
                let _ = blocking(move || kernel.kill()).await;
                // A kill request is not proof. The accepted closure may still start after
                // it, so settle that SAME launch before the inline final kill/empty proof.
                let _ = launching.await;
                return Err(anyhow::anyhow!("captured command stopped while launching"));
            },
            _ = deadline => {
                self.stop.cancel();
                let kernel = self.kernel.clone();
                let _ = blocking(move || kernel.kill()).await;
                let _ = launching.await;
                return Err(anyhow::anyhow!("captured command deadline expired while launching"));
            },
        };
        duplex.stdin.close();
        anyhow::ensure!(!self.stop.is_cancelled(), "captured command stopped");
        let remaining = until.saturating_duration_since(Instant::now());
        let collect = crate::rt::timeout(remaining, async move {
            let (stdout, stderr, code) = futures::join!(super::collect(duplex.stdout), super::collect(duplex.stderr), duplex.exit);
            Ok::<_, anyhow::Error>(Output { status: super::status(code.ok().flatten()), stdout, stderr })
        }).fuse();
        let stop = self.stop.cancelled().fuse();
        futures::pin_mut!(collect, stop);
        futures::select_biased! {
            _ = stop => Err(anyhow::anyhow!("captured command stopped")),
            result = collect => result.map_err(|_| anyhow::anyhow!("captured command deadline expired"))?,
        }
    }

    async fn retire(self) {
        self.stop.cancel();
        let held = self.group.ownership.lock().take();
        match held {
            Some(held) => Group::retire_native(self.kernel, held, self.group.finished.clone(), None, self.execution.admission.clone(), true).await,
            // Another path cannot normally take a captured reservation. If it has, this is
            // still a wait for the SAME exact native proof, never a second inferred finish.
            None => self.group.finished.cancelled().await,
        }
    }
}

struct RequestStop(crate::rt::Shutdown);
impl Drop for RequestStop { fn drop(&mut self) { self.0.cancel(); } }

/// The waiter has no abort authority over the physical owner task. Refused task admission
/// drops only unsubmitted input; after acceptance, cancellation requests its one Stop signal.
pub(crate) async fn output<H: Send + 'static>(command: Command, timeout: Duration, custody: H, execution: Arc<Execution>) -> anyhow::Result<Output> {
    native(&execution)?;
    let until = Instant::now().checked_add(timeout).context("captured command deadline is out of range")?;
    let stop = crate::rt::Shutdown::new();
    let _request = RequestStop(stop.clone());
    super::within(None, || crate::rt::spawn(async move {
        let prepared = blocking(move || execution.prepare_capture(Box::new(custody), stop)).await??;
        // Always retire this SAME reservation after a failed apply/pipe/task outcome. The
        // Group and custody remain retained through unknown physical cleanup or pool refusal.
        let result = std::panic::AssertUnwindSafe(prepared.capture(command, until)).catch_unwind().await;
        prepared.retire().await;
        match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!("captured command task failed")) }
    })).await.context("captured command owner was not admitted or ended without a result")?
}
