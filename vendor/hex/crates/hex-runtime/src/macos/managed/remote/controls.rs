//! Source-only controls: no helper/controller spawn, signal, native scope or GPU call.
use super::*;
use hex_api::{BackingKey, BackingPhase, CapacityResources, Execution, RuntimeCost};
use hex_core::record::{PoolRecord, RECORD_VERSION};
use std::num::NonZeroU64;

pub(super) fn helper(directory: &Path) -> Arc<Helper> {
    helper_with(directory, Duration::from_secs(1))
}
/// The same helper, each request bounded by `rpc`.
fn helper_with(directory: &Path, rpc: Duration) -> Arc<Helper> {
    // The helper's endpoint and receipt parents are private to its UID (0700).
    std::fs::set_permissions(
        directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    Helper::new(
        ManagedHelper::new(
            directory.join("hex"),
            directory.join("s"),
            directory.join("r"),
            rpc,
        )
        .unwrap(),
        protocol::CpuControl {
            cost_id: [3; 32],
            response: Duration::from_nanos(1),
        },
        hex_core::watch::Rates {
            bytes: NonZeroU64::new(1).unwrap(),
            processes: NonZeroU64::new(1).unwrap(),
        },
        Duration::from_nanos(1),
    )
    .unwrap()
}
pub(super) fn pool() -> PoolRecord {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    let cost = RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    PoolRecord {
        version: RECORD_VERSION,
        key: BackingKey {
            pool: [5; 32],
            generation: [7; 16],
        },
        execution: Execution::ShardsShared,
        cost,
        allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
        devices: Vec::new(),
        phase: BackingPhase::Draining,
        init: Some(ProcId {
            pid: 11,
            start_ticks: 13,
        }),
        exit: Some(Exit::Code(17)),
    }
}
pub(super) fn completed(record: &PoolRecord) -> protocol::completed::Snapshot {
    let helper = ProcId {
        pid: 19,
        start_ticks: 23,
    };
    protocol::completed::Snapshot {
        helper,
        owners: vec![protocol::completed::Completed {
            key: protocol::Key::from_record(NativeRecord::Pool(record), helper).unwrap(),
            controller: record.init,
            status: record.exit.unwrap(),
            vsock: b"/reused-vsock".to_vec(),
        }],
    }
}

/// AC-T7.4.1: completed native facts permit cleanup but never a fresh helper while the final SAME
/// canonical charge remains Draining. Changed birth, generation, commitment and status stay held.
#[test]
fn completed_cleanup_does_not_refund_or_restart_a_draining_canonical_owner() {
    let mut record = pool();
    record.check().unwrap();
    let snapshot = completed(&record);
    let records = crate::devices::Records::new();
    assert!(check_completed_metadata(&snapshot, &records, std::slice::from_ref(&record)).is_ok());
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    record.phase = BackingPhase::Released;
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_ok());
    record.exit = Some(Exit::Code(29));
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    assert!(check_completed_metadata(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    record.exit = Some(Exit::Code(17));
    record.init.as_mut().unwrap().start_ticks += 1;
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    record.init = snapshot.owners[0].controller;
    record.cost.id[0] += 1;
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    record = pool();
    record.key.generation = [31; 16];
    assert!(check_completed_metadata(&snapshot, &records, std::slice::from_ref(&record)).is_err());
    record.phase = BackingPhase::Prepared;
    record.allocation = None;
    record.init = None;
    record.exit = None;
    record.check().unwrap();
    // A fresh unsubmitted generation follows immutable Store replacement, not PGID/path reuse.
    assert!(check_restart(&snapshot, &records, std::slice::from_ref(&record)).is_ok());
    assert!(check_restart(&snapshot, &records, &[]).is_ok());
}

struct ReadOnlyBooks {
    helper: Arc<Helper>,
    reads: std::sync::atomic::AtomicUsize,
}
impl crate::devices::Books for ReadOnlyBooks {
    fn records(&self) -> Result<crate::devices::Records, Refusal> {
        assert!(
            self.helper.process.try_lock().is_ok(),
            "Store read under helper mutex"
        );
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(crate::devices::Records::new())
    }
    fn pool_records(&self) -> Result<Vec<PoolRecord>, Refusal> {
        assert!(
            self.helper.process.try_lock().is_ok(),
            "Pool read under helper mutex"
        );
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn prepare_pool(
        &self,
        _: [u8; 32],
        _: Execution,
        _: RuntimeCost,
        _: &[hex_api::DeviceId],
        _: (&[u8; 32], &[hex_api::DeviceId], Option<&PoolRecord>),
        _: &mut crate::devices::PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        panic!("restart fence must not prepare a backing")
    }
    fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
        panic!("restart fence must not probe")
    }
    fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        panic!("restart fence must not probe")
    }
    fn with_transaction(
        &self,
        _: &hex_api::BoxId,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        panic!("restart fence must not reenter a transaction")
    }
    fn with_handoff_transaction(
        &self,
        _: &hex_api::BoxId,
        _: &BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        panic!("restart fence must not reenter a handoff transaction")
    }
    fn with_pool_transaction(
        &self,
        _: &BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        panic!("restart fence must not reenter a pool transaction")
    }
}
/// AC-T7.4.1: SAME weak authority is idempotent, never replaceable, and gone means uncertainty;
/// counted reads happen outside the helper's process mutex with no native/capacity/turn callback.
#[test]
fn weak_restart_fence_reads_only_the_same_live_canonical_owner() {
    let directory = tempfile::tempdir().unwrap();
    let helper = helper(directory.path());
    let owner = std::sync::Arc::new(ReadOnlyBooks {
        helper: Arc::clone(&helper),
        reads: 0.into(),
    });
    let books: std::sync::Arc<dyn crate::devices::Books> = owner.clone();
    let weak = std::sync::Arc::downgrade(&books);
    helper.bind_books(weak.clone()).unwrap();
    helper.bind_books(weak).unwrap();
    let foreign: std::sync::Arc<dyn crate::devices::Books> = std::sync::Arc::new(ReadOnlyBooks {
        helper: Arc::clone(&helper),
        reads: 0.into(),
    });
    assert!(
        helper
            .bind_books(std::sync::Arc::downgrade(&foreign))
            .is_err()
    );
    helper.restart_fence(&completed(&pool())).unwrap();
    assert_eq!(owner.reads.load(Ordering::SeqCst), 2);
    assert!(matches!(
        *helper.process.lock().unwrap(),
        HelperProcess::Idle
    ));
    drop(books);
    drop(owner);
    assert!(helper.restart_fence(&completed(&pool())).is_err());
}

/// Where a helper's close lands against the node's next read of its peer PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Read {
    Before,
    After,
}

/// A helper's end of its connection, closed once the helper has answered, at the node's next read
/// of its peer PID: the close the helper makes after each answer, wherever it lands.
struct CloseAfterAnswer {
    at: Read,
    answered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    end: std::sync::Arc<Mutex<Option<OwnedFd>>>,
}

thread_local! {
    static NEMESIS: std::cell::RefCell<Option<CloseAfterAnswer>> = const { std::cell::RefCell::new(None) };
}

/// Runs before and after each node read of a helper connection's peer PID (`super::peer_pid`), on
/// the reading thread. Once the helper has answered, it closes the helper's end at its point, and the
/// node's socket is then disconnected: the end held the last reference.
pub(super) fn around_peer_read(fd: BorrowedFd<'_>, now: Read) {
    NEMESIS.with(|nemesis| {
        let nemesis = nemesis.borrow();
        let Some(close) = nemesis.as_ref() else {
            return;
        };
        if close.at != now || !close.answered.load(Ordering::SeqCst) {
            return;
        }
        if let Some(end) = close.end.lock().unwrap().take() {
            drop(end);
            // A program another test starts while this one makes its pair (macOS's socketpair takes
            // no close-on-exec flag, and std's spawn passes every descriptor not marked) holds a copy
            // of the end until it ends: the close has landed once the node's socket is disconnected.
            let deadline = Instant::now() + Duration::from_secs(60);
            while !matches!(sys::peer_pid(fd), Err(sys::PeerPidFailure::Native(error)) if error.raw_os_error() == Some(libc::ENOTCONN))
            {
                assert!(
                    Instant::now() < deadline,
                    "the helper's end closed but the node's socket is still connected"
                );
                std::thread::park_timeout(Duration::from_millis(10));
            }
        }
    });
}

/// AC-T7.4.1 (a reply's peer check): the helper closes each connection once it has answered (each
/// request's, and its startup socket after the first Prepare), whenever its scheduler runs it. Do: a
/// helper (this test's process, the live incarnation its key names) answers a request on a private
/// connection, and its end closes once it has answered: before the node's next read of the peer, and
/// right after it. Expect: the node takes the answer both times; the close is no refusal wherever it
/// lands (non-vacuity: the end closed, and the node's socket was disconnected).
#[test]
fn a_whole_reply_holds_wherever_the_helpers_close_lands_against_the_nodes_peer_reads() {
    for at in [Read::Before, Read::After] {
        let dir = tempfile::tempdir().unwrap();
        let helper = helper_with(dir.path(), Duration::from_secs(60));
        let pid = rustix::process::getpid().as_raw_nonzero().get();
        let own = ProcId {
            pid: u32::try_from(pid).unwrap(),
            start_ticks: sys::rusage(pid).unwrap().unwrap().started,
        };
        let key = protocol::Key::from_record(NativeRecord::Pool(&pool()), own).unwrap();
        let (node, end) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        for fd in [&node, &end] {
            crate::macos::private_descriptor(fd, true).unwrap();
        }
        let answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let end = std::sync::Arc::new(Mutex::new(Some(end)));
        NEMESIS.with(|nemesis| {
            *nemesis.borrow_mut() = Some(CloseAfterAnswer {
                at,
                answered: std::sync::Arc::clone(&answered),
                end: std::sync::Arc::clone(&end),
            });
        });
        let answering = std::thread::spawn({
            let (answered, end) = (
                std::sync::Arc::clone(&answered),
                std::sync::Arc::clone(&end),
            );
            move || {
                let slot = end.lock().unwrap();
                let fd = slot.as_ref().unwrap().as_fd();
                let until = Some(Instant::now() + Duration::from_secs(60));
                let (bytes, fds) = frame::receive(fd, protocol::timed::MAX_MESSAGE, until).unwrap();
                let (_, id, _, request) = protocol::timed::read(&bytes, fds.len()).unwrap();
                let answer = protocol::Answer::Done {
                    key: request.key().try_clone().unwrap(),
                };
                let bytes = protocol::encode_answer(VERSION, id, &answer).unwrap();
                answered.store(true, Ordering::SeqCst);
                frame::send(fd, &bytes, &[], protocol::MAX_MESSAGE, until).unwrap();
            }
        });
        let result = helper.rpc_on(
            &protocol::Request::Control {
                key,
                action: protocol::Action::Inspect,
                controller: None,
            },
            &[],
            None,
            Some(node.as_fd()),
        );
        answering.join().unwrap();
        NEMESIS.with(|nemesis| nemesis.borrow_mut().take());
        assert!(
            end.lock().unwrap().is_none(),
            "{at:?}: the helper's end never closed after its answer"
        );
        match result.map_err(CallFailure::refusal) {
            Ok((protocol::Answer::Done { key }, descriptors)) => {
                assert_eq!(key.helper, own);
                assert!(descriptors.is_empty());
            }
            Ok((other, _)) => panic!("{at:?}: the helper's answer changed: {other:?}"),
            Err(refusal) => {
                panic!("{at:?}: a whole answer was refused for the close after it: {refusal:?}")
            }
        }
    }
}
