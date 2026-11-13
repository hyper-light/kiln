//! AC-T7.4.1: managed API controls using an in-memory native receipt and the
//! actual API's pipes, output drain and observer.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shards_api::{
    End, Guest as _, GuestSpec, Identity, Init, LaunchError, LaunchIo, ManagedEnd, ManagedGuest,
    ManagedLauncher, ManagedProcess, ManagedReceipt, ManagedStatus, PreparedBoot, ProcessFailure,
    ProcessOperation, Refusal, TerminalScope, attach_managed, boot_managed, managed_receipt,
    prepare_boot,
};

// One managed attempt's bound, as the node gives `managed::Budget` before any boot (`first_dial_bound`): the
// guest's recorded boot to ready (decision 9: no margin). Each wait is for a fact the attempt makes within itself, its
// observer's entry or its end once published, and returns at that event.
fn guard() -> Duration {
    hex_runtime::guest::recorded_dial_bound().unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Launch { receipt: u32, vsock: PathBuf },
    Attach { receipt: u32, requested: Identity },
    WaitEntered { receipt: u32, patience: Duration },
    WaitReturned { receipt: u32 },
    ObserverError,
    Released,
    Stop { receipt: u32 },
    OutputClosed,
    ReceiptDropped { receipt: u32 },
    ForcedCleanup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Observation {
    Pending,
    Released(ManagedEnd),
    Uncertain(Refusal),
}

struct State {
    observation: Observation,
    events: Vec<Event>,
    output: Option<LaunchIo>,
}

struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

impl Control {
    fn event(&self, event: Event) {
        lock(&self.state).events.push(event);
        self.changed.notify_all();
    }

    fn publish(&self, observation: Observation) {
        let mut state = lock(&self.state);
        state.events.push(match observation {
            Observation::Pending => panic!("only native facts are published"),
            Observation::Released(_) => Event::Released,
            Observation::Uncertain(_) => Event::ObserverError,
        });
        state.observation = observation;
        self.changed.notify_all();
    }

    fn close_output(&self) {
        let output = lock(&self.state).output.take();
        if output.is_some() {
            drop(output);
            self.event(Event::OutputClosed);
        }
    }

    fn await_event(&self, expected: &Event) -> bool {
        let deadline = Instant::now().checked_add(guard()).unwrap();
        let mut state = lock(&self.state);
        while !state.events.contains(expected) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

struct Receipt {
    key: u32,
    identity: Result<Identity, Refusal>,
    control: Arc<Control>,
}

impl ManagedProcess for Receipt {
    fn identity(&self) -> Result<&Identity, Refusal> {
        self.identity.as_ref().map_err(Clone::clone)
    }

    fn wait_for(&self, patience: Duration) -> Result<Option<ManagedEnd>, Refusal> {
        let deadline = Instant::now().checked_add(patience);
        let mut state = lock(&self.control.state);
        state.events.push(Event::WaitEntered {
            receipt: self.key,
            patience,
        });
        self.control.changed.notify_all();
        loop {
            let result = match &state.observation {
                Observation::Pending => None,
                Observation::Released(end) => Some(Ok(Some(*end))),
                Observation::Uncertain(why) => Some(Err(why.clone())),
            };
            if let Some(result) = result {
                state.events.push(Event::WaitReturned { receipt: self.key });
                self.control.changed.notify_all();
                return result;
            }
            state = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    self.control.changed.wait_timeout(state, left)
                }
                None => {
                    state = self
                        .control
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
            }
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        }
    }

    fn stop(&self) -> Result<(), Refusal> {
        self.control.event(Event::Stop { receipt: self.key });
        self.control.close_output();
        // The observation remains uncertain until a separate native fact arrives.
        Ok(())
    }

    fn terminal(&self) -> Result<Option<ManagedEnd>, Refusal> {
        match &lock(&self.control.state).observation {
            Observation::Pending => Ok(None),
            Observation::Released(end) => Ok(Some(*end)),
            Observation::Uncertain(why) => Err(why.clone()),
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        self.control
            .event(Event::ReceiptDropped { receipt: self.key });
    }
}

struct Launcher {
    receipt: Option<triomphe::Arc<Receipt>>,
    control: Arc<Control>,
}

impl ManagedLauncher for Launcher {
    fn launch(&mut self, plan: &PreparedBoot, io: LaunchIo) -> Result<ManagedReceipt, Refusal> {
        let receipt = self.receipt.take().unwrap();
        let mut state = lock(&self.control.state);
        state.events.push(Event::Launch {
            receipt: receipt.key,
            vsock: plan.vsock().into(),
        });
        state.output = Some(io);
        Ok(managed_receipt(receipt))
    }

    fn attach(&mut self, identity: &Identity) -> Result<ManagedReceipt, Refusal> {
        let receipt = self.receipt.take().unwrap();
        self.control.event(Event::Attach {
            receipt: receipt.key,
            requested: identity.clone(),
        });
        Ok(managed_receipt(receipt))
    }
}

#[derive(Debug)]
struct Cleanup {
    events: Vec<Event>,
    observation: Observation,
    output_closed: bool,
    receipt_dropped: bool,
}

struct Harness {
    key: u32,
    control: Arc<Control>,
    launcher: Launcher,
    guest: Option<ManagedGuest>,
}

impl Harness {
    fn new(key: u32, identity: Result<Identity, Refusal>) -> Self {
        let control = Arc::new(Control {
            state: Mutex::new(State {
                observation: Observation::Pending,
                events: Vec::new(),
                output: None,
            }),
            changed: Condvar::new(),
        });
        let receipt = triomphe::Arc::try_new(Receipt {
            key,
            identity,
            control: control.clone(),
        })
        .unwrap();
        Self {
            key,
            launcher: Launcher {
                receipt: Some(receipt),
                control: control.clone(),
            },
            control,
            guest: None,
        }
    }

    fn boot(&mut self, plan: PreparedBoot) -> Result<(), Refusal> {
        self.guest = Some(boot_managed(plan, &mut self.launcher)?);
        Ok(())
    }

    fn cleanup(&mut self) -> Cleanup {
        // Every early error/assertion can wake a pending API watcher. This event
        // makes forced cleanup fail the successful case's oracle.
        if self.guest.is_some()
            && matches!(lock(&self.control.state).observation, Observation::Pending)
        {
            self.control.event(Event::ForcedCleanup);
            self.control.publish(released(ManagedStatus::Unknown));
        }
        drop(self.guest.take());
        drop(self.launcher.receipt.take());
        self.control.close_output();
        let receipt_dropped = self
            .control
            .await_event(&Event::ReceiptDropped { receipt: self.key });
        let state = lock(&self.control.state);
        Cleanup {
            events: state.events.clone(),
            observation: state.observation.clone(),
            output_closed: state.output.is_none(),
            receipt_dropped: receipt_dropped
                && state
                    .events
                    .iter()
                    .filter(|event| **event == Event::ReceiptDropped { receipt: self.key })
                    .count()
                    == 1,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let cleanup = self.cleanup();
        // Diagnostic I/O must also remain safe while another assertion unwinds.
        let _ = writeln!(
            std::io::stderr().lock(),
            "managed_api_cleanup receipt={} {cleanup:?}",
            self.key
        );
    }
}

fn prepared() -> PreparedBoot {
    // Inert names only. The pure API prepares arguments without opening them.
    let root = if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    };
    let spec = GuestSpec {
        kernel: "not-opened-kernel".into(),
        init: Init::Initrd("not-opened-initrd".into()),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        gpu: false,
        gpu_devices: Vec::new(),
        disks: Vec::new(),
        shares: Vec::new(),
        ports: Vec::new(),
        binary: None,
    };
    prepare_boot(
        &spec,
        &root.join("not-started"),
        &root.join("managed-api-v"),
        0,
    )
    .unwrap()
}

fn identity(plan: &PreparedBoot) -> Identity {
    Identity {
        pid: 41,
        started: 7,
        vsock: plan.vsock().into(),
    }
}

fn released(status: ManagedStatus) -> Observation {
    Observation::Released(ManagedEnd {
        status,
        scope: TerminalScope::Released,
    })
}

fn stops(cleanup: &Cleanup) -> Vec<u32> {
    cleanup
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Stop { receipt } => Some(*receipt),
            _ => None,
        })
        .collect()
}

fn clean(cleanup: &Cleanup) {
    assert!(
        cleanup.output_closed && cleanup.receipt_dropped,
        "{cleanup:?}"
    );
    assert!(
        !cleanup.events.contains(&Event::ForcedCleanup),
        "{cleanup:?}"
    );
}

/// AC-T7.4.1: refusal cleans exactly the launched receipt, including an unpublished
/// controller. Immutable fake keys are independent of the claimed process ID.
#[test]
fn launch_identity_refusal_stops_the_returned_native_owner() {
    let missing = Refusal::Scope(ProcessFailure::InvalidStartRecord);
    for (key, claimed, expected) in [
        (1, Err(missing.clone()), missing),
        (
            2,
            {
                let mut wrong = identity(&prepared());
                wrong.vsock.set_file_name("foreign-v");
                Ok(wrong)
            },
            Refusal::Launch(LaunchError::Observer),
        ),
    ] {
        let plan = prepared();
        let endpoint = plan.vsock().to_owned();
        let mut harness = Harness::new(key, claimed);
        let result = harness.boot(plan);
        let cleanup = harness.cleanup();
        clean(&cleanup);
        assert_eq!(result, Err(expected));
        assert_eq!(stops(&cleanup), vec![key]);
        assert!(cleanup.events.contains(&Event::Launch {
            receipt: key,
            vsock: endpoint
        }));
        assert!(
            !cleanup
                .events
                .iter()
                .any(|event| matches!(event, Event::WaitEntered { .. }))
        );
        assert_eq!(cleanup.observation, Observation::Pending);
    }
}

/// AC-T7.4.1: mismatching attach refuses ownership of the foreign receipt.
#[test]
fn attach_identity_mismatch_never_stops_the_foreign_owner() {
    let plan = prepared();
    let expected = identity(&plan);
    let foreign = Identity {
        started: expected.started + 1,
        ..expected.clone()
    };
    let mut harness = Harness::new(3, Ok(foreign));
    let result = attach_managed(plan, &expected, &mut harness.launcher).map(|guest| {
        harness.guest = Some(guest);
    });
    let cleanup = harness.cleanup();
    clean(&cleanup);
    assert_eq!(result, Err(Refusal::Launch(LaunchError::Observer)));
    assert!(stops(&cleanup).is_empty());
    assert!(cleanup.events.contains(&Event::Attach {
        receipt: 3,
        requested: expected
    }));
    assert!(
        !cleanup
            .events
            .iter()
            .any(|event| matches!(event, Event::WaitEntered { .. }))
    );
    assert_eq!(cleanup.observation, Observation::Pending);
}

/// AC-T7.4.1: native observer uncertainty stays typed through API presentation
/// and last-drop cleanup still refers to that same native receipt.
#[test]
fn observer_uncertainty_remains_typed_and_stops_the_same_owner() {
    let plan = prepared();
    let mut harness = Harness::new(4, Ok(identity(&plan)));
    harness.boot(plan).unwrap();
    let entered = harness.control.await_event(&Event::WaitEntered {
        receipt: 4,
        patience: Duration::MAX,
    });
    let why = Refusal::Scope(ProcessFailure::Os {
        operation: ProcessOperation::QueryScope,
        code: 5,
    });
    harness.control.publish(Observation::Uncertain(why.clone()));
    let guest = harness.guest.as_ref().unwrap();
    let exited = guest.process_exited();
    let scope = guest.terminal_scope();
    let result = guest.wait_for(guard());
    let repeated = guest.wait_for(Duration::ZERO);
    let cleanup = harness.cleanup();
    clean(&cleanup);
    assert!(entered);
    assert_eq!(exited, Err(why.clone()));
    assert_eq!(scope, Err(why.clone()));
    assert_eq!(result, Err(why.clone()));
    assert_eq!(repeated, Err(Refusal::Launch(LaunchError::Observer)));
    assert_eq!(cleanup.observation, Observation::Uncertain(why));
    assert_eq!(stops(&cleanup), vec![4]);
    assert!(cleanup.events.contains(&Event::WaitReturned { receipt: 4 }));
    assert!(!cleanup.events.contains(&Event::Released));
}

/// AC-T7.4.1: real API pipes retain the prepared byte tail, decode it and consume
/// their end marker even while native stdout/stderr copies remain open.
#[test]
fn terminal_output_is_bounded_drained_and_releases_native_receipt_refs() {
    let plan = prepared();
    let keep = plan.output_bytes();
    let mut harness = Harness::new(5, Ok(identity(&plan)));
    harness.boot(plan).unwrap();
    let entered = harness.control.await_event(&Event::WaitEntered {
        receipt: 5,
        patience: Duration::MAX,
    });
    let prefix = vec![b'x'; keep + 1];
    let suffix = b"stderr-tail:\xff:complete";
    let written = {
        let mut state = lock(&harness.control.state);
        let output = state.output.as_mut().unwrap();
        output
            .stdout
            .write_all(&prefix)
            .and_then(|()| output.stderr.write_all(suffix))
    };
    harness.control.publish(released(ManagedStatus::Code(17)));
    let guest = harness.guest.as_ref().unwrap();
    let result = guest.wait_for(guard());
    let exited = guest.process_exited();
    let scope = guest.terminal_scope();
    let writers_still_open = lock(&harness.control.state).output.is_some();
    let cleanup = harness.cleanup();
    clean(&cleanup);
    assert!(entered && written.is_ok(), "write={written:?}");
    let mut all = prefix;
    all.extend_from_slice(suffix);
    let expected = String::from_utf8_lossy(&all[all.len() - keep..]);
    assert_eq!(
        result,
        Ok(Some(End::Failed(format!(
            "native guest Code(17): {expected}"
        ))))
    );
    assert_eq!(exited, Ok(true));
    assert_eq!(scope, Ok(Some(TerminalScope::Released)));
    assert!(writers_still_open);
    assert!(stops(&cleanup).is_empty());
    assert!(cleanup.events.contains(&Event::OutputClosed));
    assert!(cleanup.events.contains(&Event::WaitReturned { receipt: 5 }));
}
