//! A selected box's VM route and lifetime. Durable box records remain the only
//! device-holder authority. One short-lock route table holds per-box reservations;
//! no table or state lock is held over file, guest, channel or callback operations.

use super::selected::{self, Phase, Record};
use super::*;
use crate::devices::Transaction;
use hex_core::record::{BoxRecord, OwnerKey, OwnerRef};

pub(super) struct Guest {
    pub booted: triomphe::Arc<Booted>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{Bounds, BoxState};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct Controlled {
        ended: AtomicBool,
        released: AtomicBool,
        stops: AtomicUsize,
        wait: Box<dyn Fn() + Send + Sync>,
    }

    impl shards_api::Guest for Controlled {
        fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
            Ok(self.ended.load(Ordering::SeqCst))
        }
        fn terminal_scope(&self) -> Result<Option<shards_api::TerminalScope>, shards_api::Refusal> {
            Ok(self.ended.load(Ordering::SeqCst).then(|| {
                if self.released.load(Ordering::SeqCst) {
                    shards_api::TerminalScope::Released
                } else {
                    shards_api::TerminalScope::Process
                }
            }))
        }
        fn connect(
            &self,
            port: u32,
            _: Duration,
        ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
            Err(shards_api::Refusal::Channel {
                port,
                why: "this model relay is deliberately unavailable".into(),
            })
        }
        fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
            Ok(shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/model/private.sock".into(),
            })
        }
        fn connect_before(
            &self,
            port: u32,
            at: std::time::Instant,
        ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
            let left = at
                .checked_duration_since(std::time::Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or(shards_api::Refusal::Connect {
                    port,
                    failure: shards_api::HandshakeError::Expired {
                        phase: shards_api::HandshakePhase::Dial,
                    },
                })?;
            let stream = self.connect(port, left)?;
            if std::time::Instant::now() >= at {
                return Err(shards_api::Refusal::Connect {
                    port,
                    failure: shards_api::HandshakeError::Expired {
                        phase: shards_api::HandshakePhase::ClearTimeouts,
                    },
                });
            }
            Ok(stream)
        }

        fn stop(&self) -> Result<(), shards_api::Refusal> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
            (self.wait)();
            Ok(if self.ended.load(Ordering::SeqCst) {
                Some(shards_api::End::Stopped)
            } else {
                None
            })
        }
    }

    impl shards_api::ManagedProcess for Controlled {
        fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
            static ID: std::sync::LazyLock<shards_api::Identity> =
                std::sync::LazyLock::new(|| shards_api::Identity {
                    pid: 42,
                    started: 7,
                    vsock: "/model/private.sock".into(),
                });
            Ok(&ID)
        }
        fn stop(&self) -> Result<(), shards_api::Refusal> {
            shards_api::Guest::stop(self)
        }
        fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            Ok(
                shards_api::Guest::terminal_scope(self)?.map(|scope| shards_api::ManagedEnd {
                    status: shards_api::ManagedStatus::Code(0),
                    scope,
                }),
            )
        }
        fn wait_for(
            &self,
            _: Duration,
        ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            (self.wait)();
            Ok(shards_api::ManagedProcess::terminal(self)?
                .filter(|end| end.scope == shards_api::TerminalScope::Released))
        }
    }

    struct Cleanup {
        record: BoxRecord,
        process: triomphe::Arc<Controlled>,
        fail_finalize: AtomicBool,
    }
    impl Transaction for Cleanup {
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            Err(Refusal::Broken(
                "selected-box cleanup owns no pool scope".into(),
            ))
        }
        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            panic!("box cleanup must not inspect unrelated pools")
        }
        fn read_pool(
            &self,
            _: &hex_api::BackingKey,
        ) -> Result<Option<hex_core::record::PoolRecord>, Refusal> {
            panic!("box cleanup must not read a pool")
        }
        fn put_pool(&self, _: &hex_core::record::PoolRecord) -> Result<(), Refusal> {
            panic!("box cleanup must not write a pool")
        }
        fn grant_pool(
            &self,
            _: &hex_api::BackingGrant,
        ) -> Result<hex_core::record::PoolRecord, Refusal> {
            panic!("box cleanup must not grant a pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("box cleanup must not remove a pool")
        }
        fn records(&self) -> Result<crate::devices::Records, Refusal> {
            panic!("cleanup has no inventory effect")
        }
        fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok((self.record.spec.id == *id).then(|| self.record.clone()))
        }
        fn put(&self, _: &BoxRecord) -> Result<(), Refusal> {
            panic!("outer owner persists")
        }
        fn remove(&self, _: &BoxId) -> Result<(), Refusal> {
            panic!("outer owner removes")
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            if self.fail_finalize.load(Ordering::SeqCst) {
                return Err(super::super::books::uncertain(&self.record.spec.id)?);
            }
            Ok(())
        }
        fn managed_usage(&self) -> Result<Usage, Refusal> {
            Err(Refusal::Broken(
                "this cleanup model has no native counters".into(),
            ))
        }
        fn launch_managed(
            &self,
            _: &crate::devices::ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("cleanup cannot launch")
        }
        fn attach_managed(
            &self,
            identity: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            if let Some(identity) = identity {
                assert_eq!(
                    identity,
                    shards_api::ManagedProcess::identity(self.process.as_ref()).unwrap()
                );
            }
            Ok(shards_api::managed_receipt(self.process.clone()))
        }
    }

    fn model_record(runtime: &GuestRuntime, id: &BoxId) -> BoxRecord {
        let mut spec = super::super::tests::oci(hex_api::Rootfs::Dir("/rootfs".into()), Vec::new());
        let ClassSpec::Oci(payload) = spec.class else {
            panic!("model OCI")
        };
        spec.id = id.clone();
        spec.class = ClassSpec::Shards(payload);
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        let floor = runtime.dedicated_resources().unwrap();
        let cost = hex_api::RuntimeCost {
            id: [1; 32],
            floor,
            ceiling: floor,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
        };
        spec.allocation = Some(
            cost.allocate(hex_api::CapacityResources {
                cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
                mem_bytes: hex_api::MIN_MEMORY,
                disk_bytes: 0,
            })
            .unwrap(),
        );
        let mut record = BoxRecord::new(spec, hex_core::record::Phase::Deleting, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([7; 16]));
        record.init = Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        });
        record
    }

    fn runtime(path: &std::path::Path) -> Arc<GuestRuntime> {
        Arc::new(
            GuestRuntime::new(
                Arc::new(shards_api::sim::SimGuests::new(|_| {
                    shards_api::End::PowerOff
                })),
                GuestConfig {
                    binary: "/model/shards".into(),
                    kernel: "/model/kernel".into(),
                    initrd: "/model/initrd".into(),
                    vcpus: 1,
                    memory_mib: 256,
                    gpu: false,
                    shares: Vec::new(),
                    staged: "/model/staged".into(),
                },
            )
            .recorded_at(path.join("guest")),
        )
    }

    fn record(id: &BoxId) -> Record {
        Record {
            id: id.clone(),
            devices: Vec::new(),
            class: Class::Shards,
            scope: Some(hex_core::record::NativeScope::from_bytes([7; 16])),
            route: None,
            physical: Vec::new(),
            report: None,
            readiness: None,
            attempt: None,
            vcpus: 1,
            memory_mib: 256,
            phase: Phase::Running(shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/model/private.sock".into(),
            }),
        }
    }

    fn info(id: &BoxId) -> BoxInfo {
        BoxInfo {
            published: Vec::new(),
            grants: Vec::new(),
            id: id.clone(),
            class: Class::Oci,
            allocation: None,
            route: None,
            state: BoxState::Empty,
            bounds: Bounds::default(),
            adopted: false,
            membership: None,
            exit: None,
            abandoned: 0,
            last_resort: None,
        }
    }

    fn install(
        runtime: &GuestRuntime,
        id: &BoxId,
        guest: Arc<dyn shards_api::Guest>,
    ) -> triomphe::Arc<Booted> {
        let (link, client) = super::super::tests::model_connection();
        let booted = booted_owner(Booted {
            guest: GuestHandle::Shared(guest),
            link,
            client,
            evented: Mutex::new(None),
            owner: Some(Owner {
                id: id.clone(),
                class: Class::Shards,
                allocation: None,
                route: None,
            }),
            backing: None,
        })
        .unwrap();
        let receipt = record(id);
        std::fs::create_dir_all(runtime.selected_dir().unwrap()).unwrap();
        selected::write(&runtime.selected_path(id).unwrap(), &receipt).unwrap();
        runtime
            .reserve(id, Operation::Creating)
            .unwrap()
            .publish(Some(Guest {
                booted: triomphe::Arc::clone(&booted),
            }))
            .unwrap();
        booted
    }

    /// AC-T7.4.1: an actual selected-route wait may reenter a sibling and CPU route;
    /// the same box is excluded by a reservation, without a held table mutex.
    #[test]
    fn selected_route_callback_cannot_block_independent_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-a").unwrap();
        let sibling = BoxId::new("selected-b").unwrap();
        let weak = Arc::downgrade(&runtime);
        let asked = id.clone();
        let guest = Arc::new(Controlled {
            ended: AtomicBool::new(false),
            released: AtomicBool::new(false),
            stops: AtomicUsize::new(0),
            wait: Box::new(move || {
                let runtime = weak.upgrade().unwrap();
                assert!(matches!(
                    runtime.selected_running(&asked),
                    Err(Refusal::Exhausted { bound: 1, .. })
                ));
                assert!(runtime.selected_running(&sibling).unwrap().is_none());
                assert!(runtime.running().unwrap().is_none());
            }),
        });
        let expected = install(&runtime, &id, guest);
        assert!(triomphe::Arc::ptr_eq(
            &runtime.selected_running(&id).unwrap().unwrap(),
            &expected
        ));
    }

    /// AC-T7.4.1: a selected route succeeds even when the unrelated shared CPU
    /// guest lock is broken; no eager fallback may override the selected result.
    #[test]
    fn selected_route_does_not_consult_unrelated_cpu_guest() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-only").unwrap();
        install(
            &runtime,
            &id,
            Arc::new(Controlled {
                ended: AtomicBool::new(false),
                released: AtomicBool::new(false),
                stops: AtomicUsize::new(0),
                wait: Box::new(|| {}),
            }),
        );
        let poisoned = Arc::clone(&runtime);
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.booted.lock().unwrap();
                panic!("controlled CPU lock poison");
            })
            .join()
            .is_err()
        );
        let snapshot = runtime.recorded_box(&id).unwrap();
        assert_eq!(runtime.with(snapshot.as_ref(), &id, |_| Ok(17)), Ok(17));
    }

    /// AC-T7.4.1: stop accepted with no terminal witness keeps the exact receipt;
    /// a later positive terminal result permits deletion without a relay call.
    #[test]
    fn selected_stop_retains_ownership_until_terminal_proof() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-stop").unwrap();
        let guest = triomphe::Arc::try_new(Controlled {
            ended: AtomicBool::new(false),
            released: AtomicBool::new(false),
            stops: AtomicUsize::new(0),
            wait: Box::new(|| {}),
        })
        .unwrap();
        std::fs::create_dir_all(runtime.selected_dir().unwrap()).unwrap();
        selected::write(&runtime.selected_path(&id).unwrap(), &record(&id)).unwrap();
        let transaction = Cleanup {
            record: model_record(&runtime, &id),
            process: guest.clone(),
            fail_finalize: AtomicBool::new(true),
        };
        assert!(runtime.delete_selected(&id, &transaction).is_err());
        assert_eq!(guest.stops.load(Ordering::SeqCst), 1);
        assert!(matches!(
            selected::read(&runtime.selected_path(&id).unwrap())
                .unwrap()
                .unwrap()
                .phase,
            Phase::Stopping(_)
        ));
        assert!(
            shards_api::ManagedProcess::terminal(guest.as_ref())
                .unwrap()
                .is_none()
        );
        guest.ended.store(true, Ordering::SeqCst);
        assert!(
            runtime.delete_selected(&id, &transaction).is_err(),
            "controller exit leaves helpers uncertain"
        );
        guest.released.store(true, Ordering::SeqCst);
        assert_eq!(runtime.delete_selected(&id, &transaction), Ok(()));
        let stops = guest.stops.load(Ordering::SeqCst);
        assert_eq!(runtime.delete_selected(&id, &transaction), Ok(()));
        assert_eq!(
            guest.stops.load(Ordering::SeqCst),
            stops,
            "durable Released never stops a name-reused native scope"
        );
        assert!(runtime.finalize_selected(&id, &transaction).is_err());
        assert!(matches!(
            selected::read(&runtime.selected_path(&id).unwrap())
                .unwrap()
                .unwrap()
                .phase,
            Phase::Released {
                identity: Some(_),
                status: shards_api::ManagedStatus::Code(0),
            }
        ));
        transaction.fail_finalize.store(false, Ordering::SeqCst);
        runtime.finalize_selected(&id, &transaction).unwrap();
        assert!(
            selected::read(&runtime.selected_path(&id).unwrap())
                .unwrap()
                .is_some()
        );
        runtime.forget_selected(&id).unwrap();
        assert!(
            selected::read(&runtime.selected_path(&id).unwrap())
                .unwrap()
                .is_none()
        );
    }

    /// AC-T7.4.1: normalize only the exact dedicated box's inner OCI result.
    #[test]
    fn selected_host_class_preserves_backend_and_rejects_foreign_box() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-class").unwrap();
        let booted = install(
            &runtime,
            &id,
            Arc::new(Controlled {
                ended: AtomicBool::new(false),
                released: AtomicBool::new(false),
                stops: AtomicUsize::new(0),
                wait: Box::new(|| {}),
            }),
        );
        assert_eq!(booted.host_info(info(&id)).unwrap().class, Class::Shards);
        let allocation = model_record(&runtime, &id).spec.allocation;
        let route = Some(hex_api::RouteReceipt {
            key: hex_api::RouteKey {
                class: Class::Shards,
                image: hex_api::ImagePlatform {
                    os: hex_api::ImageOs::Linux,
                    arch: super::super::route::host_platform().unwrap().arch,
                },
                execution: hex_api::Execution::ShardsDedicated,
                gpu: hex_api::GpuEligibility::None,
            },
            backing: None,
        });
        let (link, client) = super::super::tests::model_connection();
        let projected = Booted {
            guest: GuestHandle::Shared(Arc::new(Controlled {
                ended: AtomicBool::new(false),
                released: AtomicBool::new(false),
                stops: AtomicUsize::new(0),
                wait: Box::new(|| {}),
            })),
            link,
            client,
            evented: Mutex::new(None),
            owner: Some(Owner {
                id: id.clone(),
                class: Class::Shards,
                allocation,
                route,
            }),
            backing: None,
        };
        let mut exited = info(&id);
        exited.state = BoxState::Empty;
        exited.exit = Some(hex_api::Exit::Code(0));
        let outer = projected.host_info(exited).unwrap();
        assert_eq!(outer.allocation, allocation);
        assert_eq!(outer.route, route);
        let mut forged = info(&id);
        forged.allocation = allocation;
        assert!(
            projected.host_info(forged).is_err(),
            "inner OCI cannot mint a host VM allocation"
        );
        let mut forged = info(&id);
        forged.route = route;
        assert!(
            projected.host_info(forged).is_err(),
            "inner OCI cannot mint a host execution route"
        );
        assert!(
            booted
                .host_info(info(&BoxId::new("foreign-box").unwrap()))
                .is_err()
        );
    }
    /// AC-T7.4.1: old allocation and actual VM units survive a new initialized
    /// admission profile; recovery cannot resize the old VM to today's config.
    #[test]
    fn retained_allocation_survives_current_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("retained-cost").unwrap();
        let record = model_record(&runtime, &id);
        let receipt = Record::retained(&record).unwrap();
        receipt.matches_record(&record).unwrap();
        let mut changed = runtime.config.clone();
        changed.vcpus = 2;
        assert!(Record::intent(&record, &changed).is_err());
        receipt.matches_record(&record).unwrap();
        let mut forged = record.spec.clone();
        forged.allocation.as_mut().unwrap().resources.mem_bytes = 1;
        assert!(receipt.matches(&forged).is_err());
    }

    /// AC-T7.4.1: a reused BoxId/socket/PID cannot transplant another boot's
    /// cleanup authority, including legacy records with no native incarnation.
    #[test]
    fn selected_scope_must_match_canonical_before_any_native_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-scope").unwrap();
        let process = triomphe::Arc::try_new(Controlled {
            ended: AtomicBool::new(true),
            released: AtomicBool::new(true),
            stops: AtomicUsize::new(0),
            wait: Box::new(|| {}),
        })
        .unwrap();
        std::fs::create_dir_all(runtime.selected_dir().unwrap()).unwrap();
        let path = runtime.selected_path(&id).unwrap();
        for (canonical_scope, receipt_scope) in [
            (Some([8; 16]), Some([7; 16])),
            (None, Some([7; 16])),
            (Some([7; 16]), None),
            (None, None),
        ] {
            let mut canonical = model_record(&runtime, &id);
            canonical.native_scope = canonical_scope.map(hex_core::record::NativeScope::from_bytes);
            let mut receipt = record(&id);
            receipt.scope = receipt_scope.map(hex_core::record::NativeScope::from_bytes);
            selected::write(&path, &receipt).unwrap();
            assert!(receipt.matches_record(&canonical).is_err());
            let transaction = Cleanup {
                record: canonical,
                process: process.clone(),
                fail_finalize: AtomicBool::new(false),
            };
            assert!(runtime.delete_selected(&id, &transaction).is_err());
            assert!(runtime.finalize_selected(&id, &transaction).is_err());
            assert_eq!(selected::read(&path).unwrap(), Some(receipt));
        }
        assert_eq!(process.stops.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: an unsuccessful method restores the same ready value; malformed
    /// slot ownership is refused before the next native method can acquire it.
    #[test]
    fn selected_slot_keeps_one_value_across_failed_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let id = BoxId::new("selected-slot").unwrap();
        let booted = install(
            &runtime,
            &id,
            Arc::new(Controlled {
                ended: AtomicBool::new(false),
                released: AtomicBool::new(false),
                stops: AtomicUsize::new(0),
                wait: Box::new(|| {}),
            }),
        );
        {
            let reservation = runtime.reserve(&id, Operation::Loading).unwrap();
            assert!(runtime.reserve(&id, Operation::Stopping).is_err());
            assert!(triomphe::Arc::ptr_eq(
                &reservation.previous.as_ref().unwrap().booted,
                &booted
            ));
            // Dropping the failed method restores this exact previous value.
        }
        let reservation = runtime.reserve(&id, Operation::Stopping).unwrap();
        assert!(triomphe::Arc::ptr_eq(
            &reservation.previous.as_ref().unwrap().booted,
            &booted
        ));
        drop(reservation);
        let mut routes = runtime.dedicated.lock().unwrap();
        let slot = routes
            .iter_mut()
            .find(|slot| slot.key.as_ref() == OwnerRef::Box(&id))
            .unwrap();
        slot.route = Route::Busy(Operation::Loading);
        assert!(slot.take(Operation::Creating).is_err());
        assert!(slot.guest.is_some(), "invalid slot must not drop its owner");
        slot.route = Route::Ready;
    }

    /// AC-T7.4.1: a backing and box use the same bounded route storage, with
    /// typed owner keys and no table lock retained by either native operation.
    #[test]
    fn pool_process_reservation_excludes_only_its_own_canonical_owner() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let pool = [7; 32];
        let other = [8; 32];
        let id = BoxId::new("pool-sibling").unwrap();
        let pending = runtime
            .reserve_owner(OwnerRef::Pool(&pool), Operation::Creating)
            .unwrap();
        assert!(
            runtime
                .reserve_owner(OwnerRef::Pool(&pool), Operation::Loading)
                .is_err()
        );
        let sibling = runtime
            .reserve_owner(OwnerRef::Pool(&other), Operation::Loading)
            .unwrap();
        let boxed = runtime.reserve(&id, Operation::Creating).unwrap();
        assert_eq!(runtime.dedicated.lock().unwrap().len(), 3);
        drop((pending, sibling, boxed));
        assert!(runtime.dedicated.lock().unwrap().is_empty());
    }

    /// One bound owner of a selected GPU create: its canonical Creating record,
    /// its provider's current LUID, and a launcher that only counts.
    struct Preboot {
        record: BoxRecord,
        luid: Option<[u8; 8]>,
        queries: AtomicUsize,
        launches: AtomicUsize,
    }
    impl Transaction for Preboot {
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            panic!("a selected create owns no pool")
        }
        fn records(&self) -> Result<crate::devices::Records, Refusal> {
            panic!("a selected create reads only its own record")
        }
        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            panic!("a selected create reads no pool")
        }
        fn read_pool(
            &self,
            _: &hex_api::BackingKey,
        ) -> Result<Option<hex_core::record::PoolRecord>, Refusal> {
            panic!("a selected create reads no pool")
        }
        fn put_pool(&self, _: &hex_core::record::PoolRecord) -> Result<(), Refusal> {
            panic!("a selected create writes no pool")
        }
        fn grant_pool(
            &self,
            _: &hex_api::BackingGrant,
        ) -> Result<hex_core::record::PoolRecord, Refusal> {
            panic!("a selected create grants no pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("a selected create removes no pool")
        }
        fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok((self.record.spec.id == *id).then(|| self.record.clone()))
        }
        fn put(&self, _: &BoxRecord) -> Result<(), Refusal> {
            panic!("outer owner persists")
        }
        fn remove(&self, _: &BoxId) -> Result<(), Refusal> {
            panic!("outer owner removes")
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            panic!("no native scope was published")
        }
        fn managed_usage(&self) -> Result<Usage, Refusal> {
            panic!("no native counters exist")
        }
        fn launch_managed(
            &self,
            plan: &crate::devices::ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            assert!(matches!(plan, crate::devices::ManagedLaunch::Shards(_)));
            self.launches.fetch_add(1, Ordering::SeqCst);
            Err(Refusal::Broken(
                "this model starts no native process".into(),
            ))
        }
        fn attach_managed(
            &self,
            _: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("a fresh create attaches nothing")
        }
    }
    impl crate::devices::Books for Preboot {
        fn records(&self) -> Result<crate::devices::Records, Refusal> {
            panic!("a selected create reads only its own record")
        }
        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            panic!("a selected create reads no pool")
        }
        fn prepare_pool(
            &self,
            _: [u8; 32],
            _: hex_api::Execution,
            _: hex_api::RuntimeCost,
            _: &[hex_api::DeviceId],
            _: (
                &[u8; 32],
                &[hex_api::DeviceId],
                Option<&hex_core::record::PoolRecord>,
            ),
            _: &mut crate::devices::PoolPreparationCheck<'_>,
        ) -> Result<hex_core::record::PoolRecord, Refusal> {
            panic!("a selected create prepares no pool")
        }
        fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
            panic!("a selected create reads no capacity")
        }
        fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
            panic!("a selected create reads no inventory")
        }
        fn physical_luid(&self, id: hex_api::DeviceId) -> Result<[u8; 8], Refusal> {
            assert_eq!(self.record.spec.devices, [id]);
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.luid
                .ok_or_else(|| Refusal::Broken("the model provider reads no current LUID".into()))
        }
        fn with_transaction(
            &self,
            _: &BoxId,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("the caller already holds the canonical turn")
        }
        fn with_pool_transaction(
            &self,
            _: &hex_api::BackingKey,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("a selected create enters no pool")
        }
        fn with_handoff_transaction(
            &self,
            _: &BoxId,
            _: &hex_api::BackingKey,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("a selected create enters no handoff")
        }
    }

    /// AC-T7.4.1: create a selected GPU box configured by a current-boot LUID
    /// while the SAME provider reports a stale, an unknown or the matching LUID
    /// for its stable claim; expect the launcher reached only on the match.
    /// Windows assigns LUIDs per boot, so this runs on every target.
    #[test]
    fn selected_gpu_create_joins_current_luid_before_native_launch() {
        for current in [Some(10u64.to_le_bytes()), None, Some(9u64.to_le_bytes())] {
            let dir = super::super::tests::socket_fixture();
            let (config, input, allocation) =
                super::super::observer::config::tests::fixture(dir.path());
            let catalog = crate::devices::Catalog::parse(
                b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tluid:0000000000000009\t\n",
            )
            .unwrap();
            let runtime = GuestRuntime::new(
                Arc::new(shards_api::sim::SimGuests::new(|_| {
                    shards_api::End::PowerOff
                })),
                config,
            )
            .recorded_at(dir.path().join("g"))
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
            // A tarball root, which every host stages; Windows refuses a directory as a Linux root.
            let mut spec = super::super::tests::oci(
                hex_api::Rootfs::Dir(dir.path().join("rootfs.tar")),
                Vec::new(),
            );
            let ClassSpec::Oci(payload) = spec.class else {
                panic!("model OCI")
            };
            spec.class = ClassSpec::Shards(payload);
            spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
            spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
            spec.devices = vec![hex_api::DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key: 256,
            }];
            spec.allocation = Some(allocation);
            let mut record =
                BoxRecord::new(spec.clone(), hex_core::record::Phase::Creating, Vec::new());
            record.native_scope = Some(hex_core::record::NativeScope::from_bytes([8; 16]));
            let owner = Arc::new(Preboot {
                record,
                luid: current,
                queries: AtomicUsize::new(0),
                launches: AtomicUsize::new(0),
            });
            let books: Arc<dyn crate::devices::Books> = owner.clone();
            runtime.bind_books(Arc::downgrade(&books)).unwrap();
            let budget = managed::Budget::new(runtime.first_dial_bound().unwrap()).unwrap();
            let result = runtime.create_selected(spec.clone(), owner.as_ref(), budget);
            let allowed = current == Some(9u64.to_le_bytes());
            assert_eq!(
                owner.launches.load(Ordering::SeqCst),
                usize::from(allowed),
                "stale or unknown LUID reached the selected native launcher: {result:?}"
            );
            assert_eq!(
                owner.queries.load(Ordering::SeqCst),
                1,
                "the SAME initialized provider must establish the preboot join: {result:?}"
            );
            let intent = selected::read(&runtime.selected_path(&spec.id).unwrap()).unwrap();
            if allowed {
                // The model launcher refuses after the join: no VM or readiness.
                assert_eq!(
                    result.err(),
                    Some(Refusal::Broken(
                        "this model starts no native process".into()
                    ))
                );
                assert!(matches!(
                    intent.map(|intent| intent.phase),
                    Some(Phase::Intent)
                ));
            } else {
                assert_eq!(
                    result.err(),
                    Some(Refusal::Lifecycle {
                        id: spec.id.clone(),
                        cause: hex_api::LifecycleCause::Uncertain,
                    })
                );
                assert!(
                    intent.is_none(),
                    "a refused join leaves no local launch Intent"
                );
            }
            assert!(runtime.dedicated.lock().unwrap().is_empty());
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Loading,
    Creating,
    Stopping,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Loading => "same-box guest recovery in flight",
            Self::Creating => "same-box guest create in flight",
            Self::Stopping => "same-box guest stop in flight",
        }
    }
}

enum Route {
    Busy(Operation),
    Ready,
}

pub(super) struct Slot {
    key: OwnerKey,
    route: Route,
    // Ready owns exactly one value here; Busy owns none because its Reservation
    // holds the prior value. Keeping both in this same fallibly reserved slot
    // avoids another heap/refcount owner for a large Ready enum payload.
    guest: Option<Guest>,
}

impl Slot {
    fn take(&mut self, operation: Operation) -> Result<Guest, Refusal> {
        match (&self.route, self.guest.is_some()) {
            (Route::Ready, true) => {
                let guest = self
                    .guest
                    .take()
                    .ok_or_else(|| invalid("ready guest route lost its value"))?;
                self.route = Route::Busy(operation);
                Ok(guest)
            }
            (Route::Busy(existing), false) => Err(Refusal::Exhausted {
                what: existing.name(),
                bound: 1,
            }),
            _ => Err(invalid("guest route state and ownership disagree")),
        }
    }

    fn check_reserved(&self) -> Result<(), Refusal> {
        if !matches!(self.route, Route::Busy(_)) || self.guest.is_some() {
            return Err(invalid("guest reservation lost its exclusive slot"));
        }
        Ok(())
    }
}

/// Owning a reservation excludes a second same-ID operation without keeping a
/// mutex across callbacks. A failed operation restores its last known route;
/// its on-disk receipt still decides whether physical absence is proved.
pub(super) struct Reservation<'a> {
    runtime: &'a GuestRuntime,
    key: OwnerKey,
    pub previous: Option<Guest>,
    finished: bool,
}

impl Reservation<'_> {
    pub fn publish(mut self, next: Option<Guest>) -> Result<(), Refusal> {
        self.restore(next)?;
        self.finished = true;
        Ok(())
    }

    fn restore(&self, next: Option<Guest>) -> Result<(), Refusal> {
        if let Some(next) = &next {
            let matches = match self.key.as_ref() {
                OwnerRef::Box(id) => {
                    next.booted
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.id == *id)
                        && next.booted.backing.is_none()
                }
                OwnerRef::Pool(pool) => {
                    next.booted.owner.is_none()
                        && next
                            .booted
                            .backing
                            .is_some_and(|backing| backing.key.pool == *pool)
                }
            };
            if !matches {
                return Err(invalid(
                    "guest route publication names another canonical owner",
                ));
            }
        }
        let mut routes = self.runtime.dedicated.lock().map_err(poisoned)?;
        let index = routes
            .iter()
            .position(|slot| slot.key == self.key)
            .ok_or_else(|| invalid("guest reservation lost its route"))?;
        if let Some(next) = next {
            let slot = routes
                .get_mut(index)
                .ok_or_else(|| invalid("guest route index changed"))?;
            slot.check_reserved()?;
            slot.guest = Some(next);
            slot.route = Route::Ready;
        } else {
            routes
                .get(index)
                .ok_or_else(|| invalid("guest route index changed"))?
                .check_reserved()?;
            routes.swap_remove(index);
        }
        Ok(())
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A failed operation restores its last owned route even after a test panic;
        // this destructor performs no native effect or durable receipt removal.
        let mut routes = self
            .runtime
            .dedicated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = routes.iter().position(|slot| slot.key == self.key)
            && let Some(slot) = routes.get_mut(index)
            && slot.check_reserved().is_ok()
        {
            if let Some(previous) = self.previous.take() {
                slot.guest = Some(previous);
                slot.route = Route::Ready;
            } else {
                routes.swap_remove(index);
            }
        }
    }
}

pub(super) fn copy_path(path: &std::path::Path) -> Result<PathBuf, Refusal> {
    let mut text = OsString::new();
    text.try_reserve_exact(path.as_os_str().len())
        .map_err(|_| Refusal::Exhausted {
            what: "native guest path bytes",
            bound: path.as_os_str().len(),
        })?;
    text.push(path.as_os_str());
    Ok(PathBuf::from(text))
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid {
        what: "dedicated GPU guest",
        why: why.into(),
    }
}

impl Owner {
    fn project(&self, mut info: BoxInfo) -> Result<BoxInfo, Refusal> {
        self.project_in(&mut info)?;
        Ok(info)
    }

    fn project_in(&self, info: &mut BoxInfo) -> Result<(), Refusal> {
        if info.id != self.id
            || info.class != Class::Oci
            || info.allocation.is_some()
            || info.route.is_some()
        {
            return Err(invalid(
                "dedicated guest returned a foreign box, workload class or native charge",
            ));
        }
        info.class = self.class;
        info.allocation = self.allocation;
        info.route = self.route;
        Ok(())
    }
}

impl Booted {
    pub(super) fn host_info(&self, info: BoxInfo) -> Result<BoxInfo, Refusal> {
        match &self.owner {
            Some(owner) => owner.project(info),
            None => Ok(info),
        }
    }
}

impl GuestRuntime {
    pub(super) fn project_info(&self, booted: &Booted, info: BoxInfo) -> Result<BoxInfo, Refusal> {
        if booted.owner.is_some() || !self.uses_books() {
            return booted.host_info(info);
        }
        let mut info = info;
        self.project_info_in(booted, &mut info)?;
        Ok(info)
    }

    pub(super) fn project_info_in(
        &self,
        booted: &Booted,
        info: &mut BoxInfo,
    ) -> Result<(), Refusal> {
        if let Some(owner) = &booted.owner {
            return owner.project_in(info);
        }
        if !self.uses_books() {
            return Ok(());
        }
        let Some(record) = self.recorded_box(&info.id)? else {
            // A shared guest may also list a foreign entry. Its host receipt is
            // unknown here; never attach another box's class/route/charge to it.
            return Ok(());
        };
        route::guest_spec(&record.spec)?;
        if self.owns_record(&record.spec) {
            return Err(super::books::uncertain(&info.id)?);
        }
        let owner = Owner {
            class: record.spec.class.class(),
            id: record.spec.id,
            allocation: record.spec.allocation,
            route: record.spec.route,
        };
        owner.project_in(info)
    }

    pub(super) fn booted_for(&self, id: &BoxId) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        self.booted_for_record(self.recorded_box(id)?.as_ref(), id)
    }

    /// [`GuestRuntime::booted_for`], `snapshot` the call's one read of `id`'s record (`books::Routed::Other`).
    pub(super) fn booted_for_record(
        &self,
        snapshot: Option<&hex_core::record::BoxRecord>,
        id: &BoxId,
    ) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        let dedicated = if self.uses_books() {
            snapshot.is_some_and(|record| self.owns_record(&record.spec))
        } else if self.record.is_some() {
            selected::read(&self.selected_path(id)?)?.is_some()
        } else {
            false
        };
        if dedicated {
            // Forward already owns its canonical method turn. No recursive enter;
            // recovery/create installs the native route before accepting forwards.
            self.cached_selected(id)
        } else if self.managed_shared() {
            let record = snapshot.ok_or(super::books::uncertain(id)?)?;
            self.shared_for_record(record)
        } else {
            self.running()
        }
    }

    pub fn with_gpu_catalog(mut self, catalog: crate::devices::SharedCatalog) -> Self {
        self.gpu_catalog = Some(catalog);
        self
    }

    /// The exact pair already held by the configured probe provider. Native
    /// launch borrows the same validated values from its prepared plan.
    pub fn with_runtime_paths(mut self, paths: shards_api::RuntimePaths) -> Self {
        self.native_paths = Some(paths);
        self
    }

    /// Charge this configured VM envelope before create; the box's payload ceiling
    /// is within it. Parent/VMM overhead still needs its separately measured charge.
    pub fn dedicated_resources(&self) -> Result<hex_api::CapacityResources, Refusal> {
        selected::resources(&self.config)
    }

    pub(super) fn reserve(
        &self,
        id: &BoxId,
        operation: Operation,
    ) -> Result<Reservation<'_>, Refusal> {
        self.reserve_owner(OwnerRef::Box(id), operation)
    }

    /// Box and Pool native controllers share this one process/client route table.
    /// Its key excludes concurrent methods; the Store alone owns grants/claims.
    pub(super) fn reserve_owner(
        &self,
        key: OwnerRef<'_>,
        operation: Operation,
    ) -> Result<Reservation<'_>, Refusal> {
        let owned = key.try_owned()?;
        let mut routes = self.dedicated.lock().map_err(poisoned)?;
        let previous = if let Some(slot) = routes.iter_mut().find(|slot| slot.key.as_ref() == key) {
            Some(slot.take(operation)?)
        } else {
            if routes.len() >= crate::MAX_BOXES {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            routes.try_reserve(1).map_err(|_| Refusal::Exhausted {
                what: "guest routes",
                bound: crate::MAX_BOXES,
            })?;
            routes.push(Slot {
                key: key.try_owned()?,
                route: Route::Busy(operation),
                guest: None,
            });
            None
        };
        drop(routes);
        Ok(Reservation {
            runtime: self,
            key: owned,
            previous,
            finished: false,
        })
    }

    fn selected_dir(&self) -> Result<PathBuf, Refusal> {
        let record = self
            .record
            .as_ref()
            .ok_or_else(|| invalid("dedicated guests require durable receipts"))?;
        let mut text = OsString::new();
        text.try_reserve_exact(
            record
                .as_os_str()
                .len()
                .checked_add(".gpu".len())
                .ok_or_else(|| invalid("receipt directory length overflows"))?,
        )
        .map_err(|_| invalid("receipt directory storage unavailable"))?;
        text.push(record.as_os_str());
        text.push(".gpu");
        Ok(PathBuf::from(text))
    }

    pub(super) fn selected_path(&self, id: &BoxId) -> Result<PathBuf, Refusal> {
        let mut path = self.selected_dir()?;
        path.try_reserve(
            id.as_str()
                .len()
                .checked_add(1)
                .ok_or_else(|| invalid("receipt path length overflows"))?,
        )
        .map_err(|_| invalid("receipt path storage unavailable"))?;
        path.push(id.as_str());
        Ok(path)
    }

    fn selected_socket(&self, id: &BoxId) -> Result<PathBuf, Refusal> {
        let mut dir = self.selected_dir()?;
        let suffix = "-sockets";
        let mut text = OsString::new();
        text.try_reserve_exact(
            dir.as_os_str()
                .len()
                .checked_add(suffix.len())
                .ok_or_else(|| invalid("socket directory length overflows"))?,
        )
        .map_err(|_| invalid("socket directory storage unavailable"))?;
        text.push(dir.as_os_str());
        text.push(suffix);
        dir = PathBuf::from(text);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .map_err(|e| Refusal::os("creating private selected socket directory", e))?;
        held::sync_folder(&dir).map_err(|e| Refusal::os("syncing selected socket directory", e))?;
        dir.try_reserve(
            id.as_str()
                .len()
                .checked_add(1)
                .ok_or_else(|| invalid("socket path length overflows"))?,
        )
        .map_err(|_| invalid("socket path storage unavailable"))?;
        dir.push(id.as_str());
        Ok(dir)
    }

    pub(super) fn selected_ids(&self) -> Result<Vec<BoxId>, Refusal> {
        if self.record.is_none() {
            return Ok(Vec::new());
        }
        let entries = match std::fs::read_dir(self.selected_dir()?) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Refusal::os("listing selected guest receipts", e)),
        };
        let mut ids = Vec::new();
        for (seen, entry) in entries.enumerate() {
            // A durable receipt and at most one replacement temporary per box.
            if seen
                >= crate::MAX_BOXES
                    .checked_mul(2)
                    .ok_or(Refusal::Full(crate::MAX_BOXES))?
            {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            let entry =
                entry.map_err(|e| Refusal::os("reading selected guest receipt entry", e))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("receipt filename is not UTF-8"))?;
            if name.starts_with('.') && name.ends_with(".tmp") {
                continue;
            }
            let id = BoxId::new(name)?;
            if ids.len() >= crate::MAX_BOXES {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            ids.try_reserve(1)
                .map_err(|_| Refusal::Full(crate::MAX_BOXES))?;
            ids.push(id);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    fn selected_spec(&self, receipt: &Record) -> Result<shards_api::GuestSpec, Refusal> {
        self.check_native_binding(receipt)?;
        self.receipt_spec(receipt)
    }

    fn check_selected_observer(&self, receipt: &Record) -> Result<(), Refusal> {
        if receipt.physical.is_empty() {
            return Ok(());
        }
        let observer = self
            .gpu_observer
            .as_ref()
            .ok_or(super::books::uncertain(&receipt.id)?)?;
        let readiness = receipt.checked_work_ready()?;
        if readiness.configuration != observer.identity()
            || readiness.max_queues != observer.queues()
        {
            return Err(super::books::uncertain(&receipt.id)?);
        }
        receipt
            .attempt
            .as_ref()
            .ok_or(super::books::uncertain(&receipt.id)?)?
            .check_config(observer, receipt.physical.len())
    }

    fn check_native_binding(&self, receipt: &Record) -> Result<(), Refusal> {
        if receipt.devices.is_empty() {
            return Ok(());
        }
        if self.catalog_physical(&receipt.devices)? != receipt.physical {
            return Err(invalid(
                "configured physical binding changed since guest launch",
            ));
        }
        Ok(())
    }

    fn receipt_spec(&self, receipt: &Record) -> Result<shards_api::GuestSpec, Refusal> {
        let mut spec = self.config.spec();
        spec.gpu = !receipt.devices.is_empty();
        spec.vcpus = receipt.vcpus;
        spec.memory_mib = receipt.memory_mib;
        if receipt.devices.is_empty() {
            return Ok(spec);
        }
        spec.gpu_devices
            .try_reserve_exact(receipt.physical.len())
            .map_err(|_| Refusal::Exhausted {
                what: "selected Shards GPU identities",
                bound: receipt.physical.len(),
            })?;
        spec.gpu_devices.extend_from_slice(&receipt.physical);
        if receipt.physical.len() != receipt.devices.len() {
            return Err(invalid("receipt lost a physical selector"));
        }
        gpu::append_authority(&mut spec.cmdline, &receipt.id, &receipt.devices)?;
        Ok(spec)
    }

    pub(super) fn physical_for(
        &self,
        devices: &[hex_api::DeviceId],
    ) -> Result<Vec<shards_api::PhysicalGpu>, Refusal> {
        if devices.is_empty() {
            return Ok(Vec::new());
        }
        self.catalog_physical(devices)
    }

    pub fn validate_binding(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        if matches!(spec.class, ClassSpec::ShardsImage(_)) {
            return self.recover_image(spec);
        }
        if spec.devices.is_empty() && !matches!(spec.class, ClassSpec::Shards(_)) {
            return Ok(());
        }
        let receipt =
            selected::read(&self.selected_path(&spec.id)?)?.ok_or(Refusal::Lifecycle {
                id: spec.id.try_clone()?,
                cause: hex_api::LifecycleCause::Uncertain,
            })?;
        receipt.matches(spec)?;
        self.selected_spec(&receipt)?;
        Ok(())
    }

    fn booted(
        &self,
        guest: shards_api::ManagedGuest,
        receipt: &Record,
        record: &BoxRecord,
        budget: managed::Budget,
    ) -> Result<triomphe::Arc<Booted>, Refusal> {
        let owner = Owner {
            id: receipt.id.try_clone()?,
            class: receipt.class,
            allocation: record.spec.allocation,
            route: record.spec.route,
        };
        self.connect_managed(guest, Some(owner), None, budget)
    }

    pub(super) fn connect_managed(
        &self,
        guest: shards_api::ManagedGuest,
        owner: Option<Owner>,
        backing: Option<shared::Binding>,
        budget: managed::Budget,
    ) -> Result<triomphe::Arc<Booted>, Refusal> {
        let link: LinkOwner = own_link(ShardsLink::managed(
            guest
                .try_clone()
                .map_err(|why| refusal("retaining managed guest connection", &why))?,
            // HEX-GUEST-33: a later dial without its caller's deadline spends the whole learned bound,
            // not what this create had left; the create's own dials carry its deadline.
            budget.remaining().and_then(|_| self.first_dial_bound())?,
        ))?;
        let client = Client::connect_before(triomphe::Arc::clone(&link), budget.deadline())?;
        let booted = booted_owner(Booted {
            guest: GuestHandle::Managed(guest),
            link,
            client,
            evented: Mutex::new(None),
            owner,
            backing,
        })?;
        budget.remaining()?;
        self.join_before(&booted, budget)?;
        Ok(booted)
    }

    fn cached_selected(&self, id: &BoxId) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        let mut reservation = self.reserve(id, Operation::Loading)?;
        let Some(known) = reservation.previous.take() else {
            reservation.publish(None)?;
            return if self.uses_books() {
                Err(super::books::uncertain(id)?)
            } else {
                Ok(None)
            };
        };
        let booted = triomphe::Arc::clone(&known.booted);
        reservation.previous = Some(known);
        let terminal = booted
            .guest
            .wait_for(Duration::ZERO)
            .map_err(|why| refusal("observing selected guest lifetime", &why))?;
        let scope = booted
            .guest
            .terminal_scope()
            .map_err(|why| refusal("observing selected native scope", &why))?;
        if terminal.is_some() || scope.is_some() {
            if scope == Some(shards_api::TerminalScope::Released) {
                let known = reservation.previous.take();
                reservation.publish(known)?;
                return Ok(None);
            }
            return Err(super::books::uncertain(id)?);
        }
        // The durable VM receipt is authoritative. It was already reread by the
        // old binding check; retain only the actual native/client owner in RAM.
        let receipt =
            selected::read(&self.selected_path(id)?)?.ok_or(super::books::uncertain(id)?)?;
        if !matches!(receipt.phase, Phase::Running(_)) {
            return Err(super::books::uncertain(id)?);
        }
        if let Some(report) = receipt.checked_report()? {
            self.check_native_report(
                &receipt.devices,
                &receipt.physical,
                receipt.scope.ok_or(super::books::uncertain(id)?)?,
                report,
            )?;
        }
        self.check_selected_observer(&receipt)?;
        if self.uses_books() {
            let record = self.recorded_box(id)?.ok_or(super::books::uncertain(id)?)?;
            receipt.matches_record(&record)?;
            self.selected_spec(&receipt)?;
            let identity = booted
                .guest
                .identity()
                .map_err(|why| refusal("validating selected route identity", &why))?;
            managed::same_init(&record, &identity)?;
            if booted.owner.as_ref().is_none_or(|owner| {
                owner.allocation != record.spec.allocation
                    || owner.class != record.spec.class.class()
                    || owner.route != record.spec.route
            }) {
                return Err(super::books::uncertain(id)?);
            }
        }
        let known = reservation.previous.take();
        reservation.publish(known)?;
        Ok(Some(booted))
    }

    fn has_selected_route(&self, id: &BoxId) -> Result<bool, Refusal> {
        Ok(self
            .dedicated
            .lock()
            .map_err(poisoned)?
            .iter()
            .any(|slot| slot.key.as_ref() == OwnerRef::Box(id)))
    }

    pub(super) fn selected_running(
        &self,
        id: &BoxId,
    ) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        if !self.uses_books() {
            return self.cached_selected(id);
        }
        let record = self.recorded_box(id)?.ok_or(super::books::uncertain(id)?)?;
        let keys = self.turn_keys(&record.spec.devices)?;
        let books = self.books.get().ok_or(super::books::uncertain(id)?)?;
        crate::devices::with_transaction(books, id, &keys, |transaction| {
            let current = transaction.read(id)?.ok_or(super::books::uncertain(id)?)?;
            if current.spec != record.spec
                || current.init != record.init
                || current.native_scope != record.native_scope
            {
                return Err(super::books::uncertain(id)?);
            }
            self.selected_running_in(id, transaction, &current)
        })
    }

    pub fn recover_managed(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        if matches!(spec.class, ClassSpec::ShardsImage(_)) {
            return self.recover_image(spec);
        }
        if !self.owns_record(spec) {
            return Ok(());
        }
        self.validate_binding(spec)?;
        self.selected_running(&spec.id)?
            .ok_or(super::books::uncertain(&spec.id)?)?;
        Ok(())
    }

    pub(super) fn selected_running_in(
        &self,
        id: &BoxId,
        transaction: &dyn Transaction,
        record: &BoxRecord,
    ) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        if self.has_selected_route(id)? {
            return self.cached_selected(id);
        }
        let reservation = self.reserve(id, Operation::Loading)?;
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let receipt =
            selected::read(&self.selected_path(id)?)?.ok_or(super::books::uncertain(id)?)?;
        receipt.matches_record(record)?;
        let identity = receipt.identity()?;
        managed::same_init(record, identity)?;
        if !matches!(receipt.phase, Phase::Running(_)) {
            return Err(super::books::uncertain(id)?);
        }
        let spec = self.selected_spec(&receipt)?;
        if let Some(report) = receipt.checked_report()? {
            self.check_native_report(
                &receipt.devices,
                &receipt.physical,
                receipt.scope.ok_or(super::books::uncertain(id)?)?,
                report,
            )?;
        }
        let plan = self.managed_plan(&spec, &identity.vsock, budget)?;
        self.check_selected_observer(&receipt)?;
        let guest = managed::attach(transaction, plan, identity, budget)?;
        let booted = self.booted(guest, &receipt, record, budget)?;
        reservation.publish(Some(Guest {
            booted: triomphe::Arc::clone(&booted),
        }))?;
        Ok(Some(booted))
    }

    /// The managed plan of `spec` on `socket`, its guest told what is left of the create's `budget`, its boot's deadline
    /// (AC-T7.4.1, [`super::READY_MS`]); an attach's plan launches nothing, so its guest hears nothing of it.
    pub(super) fn managed_plan(
        &self,
        spec: &shards_api::GuestSpec,
        socket: &std::path::Path,
        budget: managed::Budget,
    ) -> Result<shards_api::PreparedBoot, Refusal> {
        let mut spec = spec.clone();
        super::tell_ready(&mut spec.cmdline, budget.remaining()?)?;
        let spec = &spec;
        let paths = self.native_paths.as_ref().ok_or_else(|| Refusal::Invalid {
            what: "managed Shards runtime paths",
            why: "the node's configured probe and native launch require the same explicit paths"
                .into(),
        })?;
        let plan = if let Some(images) = self.native_images {
            shards_api::prepare_native_boot(
                spec,
                &self.config.binary,
                socket,
                shards_api::NativeBoot {
                    paths,
                    images,
                    console_kept: shards_api::recorded_console_bytes_at(&spec.kernel, paths),
                },
            )
        } else {
            shards_api::prepare_boot(
                spec,
                &self.config.binary,
                socket,
                shards_api::recorded_console_bytes(&spec.kernel),
            )
            .and_then(|plan| plan.with_runtime_paths(paths))
        };
        plan.map_err(|why| refusal("preparing admitted Shards scope", &why))
    }

    pub(super) fn prepare_selected_create(&self, id: &BoxId) -> Result<(), Refusal> {
        if self.has_selected_route(id)? {
            return Err(super::books::uncertain(id)?);
        }
        let path = self.selected_path(id)?;
        if let Some(receipt) = selected::read(&path)? {
            if receipt.id != *id || !matches!(receipt.phase, Phase::Released { .. }) {
                return Err(super::books::uncertain(id)?);
            }
            // The caller proved the sole canonical record absent under its turn.
            // This is a completed removal whose final receipt unlink was interrupted.
            selected::remove(&path)?;
        }
        Ok(())
    }

    /// Read-only proof for the canonical handoff guard's unsubmitted
    /// Creating/Deleting reservation. This is neither native absence nor launch
    /// permission. Once the source reaches Released, a missing Intent alone
    /// cannot exclude a submitted Box scope and this proof always refuses.
    pub fn handoff_unsubmitted(
        &self,
        record: &BoxRecord,
        source: &hex_core::record::PoolRecord,
    ) -> Result<(), Refusal> {
        use hex_api::{BackingPhase, Execution};
        let id = &record.spec.id;
        if !matches!(
            record.phase,
            hex_core::record::Phase::Creating | hex_core::record::Phase::Deleting
        ) || record.init.is_some()
            || record.run.is_some()
            || record.exit.is_some()
            || record.native_scope.is_none()
            || record.spec.route_execution() != Some(Execution::ShardsDedicated)
            || record.spec.route.and_then(|route| route.backing) != Some(source.key)
            || source.execution != Execution::ShardsDedicated
            || !matches!(source.phase, BackingPhase::Live | BackingPhase::Draining)
        {
            return Err(super::books::uncertain(id)?);
        }
        self.check_allocation(&record.spec)?;
        selected::Record::intent(record, &self.config)?;
        self.preflight_backing(source)?;
        record
            .spec
            .route
            .ok_or(super::books::uncertain(id)?)?
            .check_backing_parts(
                record.spec.allocation.map(|allocation| allocation.cost),
                source.key,
                source.cost,
                &source.devices,
                &record.spec.devices,
            )?;
        route::guest_spec(&record.spec)?;
        record.native_commitment()?;
        self.catalog_physical(&record.spec.devices)?;
        if self.has_selected_route(id)? {
            return Err(super::books::uncertain(id)?);
        }
        // The caller retains the canonical Box/full-source guard across both
        // reads; no native/Store callback, route allocation or table mutation.
        if selected::read(&self.selected_path(id)?)?.is_some() {
            return Err(super::books::uncertain(id)?);
        }
        Ok(())
    }

    pub(super) fn create_selected(
        &self,
        spec: BoxSpec,
        transaction: &dyn Transaction,
        budget: managed::Budget,
    ) -> Result<BoxInfo, Refusal> {
        budget.remaining()?;
        self.check_allocation(&spec)?;
        let reservation = self.reserve(&spec.id, Operation::Creating)?;
        let path = self.selected_path(&spec.id)?;
        if reservation.previous.is_some() || selected::read(&path)?.is_some() {
            return Err(Refusal::Exists(spec.id));
        }
        // Store's first fresh Creating put persists this incarnation. The local
        // pre-put shell cannot supply it, and a reusable BoxId socket is no proof.
        let canonical = transaction
            .read(&spec.id)?
            .ok_or(super::books::uncertain(&spec.id)?)?;
        if canonical.phase != hex_core::record::Phase::Creating
            || canonical.spec != spec
            || canonical.init.is_some()
        {
            return Err(super::books::uncertain(&spec.id)?);
        }
        let mut receipt = Record::intent(&canonical, &self.config)?;
        receipt.physical = self.physical_for(&receipt.devices)?;
        if !receipt.physical.is_empty() {
            let observer = self
                .gpu_observer
                .as_ref()
                .ok_or(super::books::uncertain(&receipt.id)?)?;
            observer.verify(&self.config)?;
            observer.check_allocation(self.configured_allocation()?)?;
            observer.output_bytes(receipt.physical.len())?;
        }
        let guest_spec = self.selected_spec(&receipt)?;
        let rewritten = self.rewrite_spec(spec)?;
        let dir = self.selected_dir()?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| Refusal::os("creating selected guest receipt directory", e))?;
        if let Some(parent) = dir.parent() {
            held::sync_folder(parent)
                .map_err(|e| Refusal::os("syncing selected guest receipt directory", e))?;
        }
        let socket = self.selected_socket(&receipt.id)?;
        let mut plan = self.managed_plan(&guest_spec, &socket, budget)?;
        if !receipt.physical.is_empty() {
            self.check_current_luids(
                &receipt.devices,
                &receipt.physical,
                super::books::uncertain(&receipt.id)?,
            )?;
            let scope = receipt.scope.ok_or(super::books::uncertain(&receipt.id)?)?;
            plan = plan
                .with_gpu_report(*scope.bytes(), budget.deadline())
                .map_err(|why| refusal("preparing selected renderer report", &why))?;
        }
        budget.remaining()?;
        selected::write(&path, &receipt)?;
        let guest = managed::boot(transaction, plan, budget)?;
        let identity = shards_api::Guest::identity(&guest)
            .map_err(|why| refusal("recording selected guest identity", &why))?;
        let record = transaction
            .read(&receipt.id)?
            .ok_or(super::books::uncertain(&receipt.id)?)?;
        managed::same_init(&record, &identity)?;
        receipt.matches_record(&record)?;
        if !receipt.physical.is_empty() {
            let report = guest.take_gpu_report(budget.remaining()?).map_err(|why| {
                refusal(
                    "observing the selected renderer in its original scope",
                    &why,
                )
            })?;
            receipt.report = Some(report);
            let report = receipt
                .checked_report()?
                .ok_or(super::books::uncertain(&receipt.id)?)?;
            self.check_native_report(
                &receipt.devices,
                &receipt.physical,
                receipt.scope.ok_or(super::books::uncertain(&receipt.id)?)?,
                report,
            )?;
        }
        receipt.phase = Phase::Running(identity);
        selected::write(&path, &receipt)?;
        let booted = self.booted(guest, &receipt, &record, budget)?;
        if !receipt.physical.is_empty() {
            let observer = self
                .gpu_observer
                .as_ref()
                .ok_or(super::books::uncertain(&receipt.id)?)?;
            super::observer::observe_selected(observer, &booted, &mut receipt, &path, budget)?;
            self.check_selected_observer(&receipt)?;
        }
        budget.remaining()?;
        let created = self.create_in(&booted, rewritten);
        reservation.publish(Some(Guest { booted }))?;
        created
    }

    pub(super) fn delete_selected(
        &self,
        id: &BoxId,
        transaction: &dyn Transaction,
    ) -> Result<(), Refusal> {
        let reservation = self.reserve(id, Operation::Stopping)?;
        let path = self.selected_path(id)?;
        let receipt = selected::read(&path)?;
        let record = transaction.read(id)?.ok_or(super::books::uncertain(id)?)?;
        if record.native_scope.is_none() {
            return Err(super::books::uncertain(id)?);
        }
        let identity = match receipt.as_ref() {
            Some(receipt) => {
                receipt.matches_record(&record)?;
                match &receipt.phase {
                    Phase::Running(identity) | Phase::Stopping(identity) => {
                        managed::same_init(&record, identity)?;
                        Some(identity)
                    }
                    Phase::Released {
                        identity: Some(identity),
                        ..
                    } => {
                        managed::same_init(&record, identity)?;
                        reservation.publish(None)?;
                        return Ok(());
                    }
                    Phase::Released { identity: None, .. } if record.init.is_none() => {
                        reservation.publish(None)?;
                        return Ok(());
                    }
                    Phase::Released { identity: None, .. } => {
                        return Err(super::books::uncertain(id)?);
                    }
                    Phase::Intent => None,
                }
            }
            None if reservation.previous.is_some() => return Err(super::books::uncertain(id)?),
            None => None,
        };
        if let Some(identity) = identity {
            let mut stopping = selected::read(&path)?.ok_or(super::books::uncertain(id)?)?;
            stopping.phase = Phase::Stopping(shards_api::Identity {
                pid: identity.pid,
                started: identity.started,
                vsock: copy_path(&identity.vsock)?,
            });
            selected::write(&path, &stopping)?;
        }
        // The native transaction names the original scope even before publication.
        // PID exit/Gone/observer error never substitutes for complete scope release.
        let terminal = managed::stop(transaction, id, identity, self.first_dial_bound()?)?;
        let released_identity = identity
            .map(|identity| -> Result<_, Refusal> {
                Ok(shards_api::Identity {
                    pid: identity.pid,
                    started: identity.started,
                    vsock: copy_path(&identity.vsock)?,
                })
            })
            .transpose()?;
        let mut released = match receipt {
            Some(receipt) => receipt,
            None => {
                // No effect can precede Intent. Recover a pre-intent failure only
                // after the native owner positively proved the original scope empty.
                let mut receipt = Record::retained(&record)?;
                receipt.physical = self.catalog_physical(&receipt.devices)?;
                receipt
            }
        };
        released.phase = Phase::Released {
            identity: released_identity,
            status: terminal.status,
        };
        selected::write(&path, &released)?;
        reservation.publish(None)
    }

    pub(super) fn finalize_selected(
        &self,
        id: &BoxId,
        transaction: &dyn Transaction,
    ) -> Result<(), Refusal> {
        let receipt =
            selected::read(&self.selected_path(id)?)?.ok_or(super::books::uncertain(id)?)?;
        let record = transaction.read(id)?.ok_or(super::books::uncertain(id)?)?;
        receipt.matches_record(&record)?;
        let Phase::Released { identity, .. } = &receipt.phase else {
            return Err(super::books::uncertain(id)?);
        };
        match identity {
            Some(identity) => managed::same_init(&record, identity)?,
            None if record.init.is_none() => {}
            None => return Err(super::books::uncertain(id)?),
        }
        transaction.finalize_managed(identity.as_ref())
    }

    pub(super) fn forget_selected(&self, id: &BoxId) -> Result<(), Refusal> {
        selected::remove(&self.selected_path(id)?)
    }

    pub fn evented_for(&self, id: &BoxId) -> Result<Option<EventedClient>, Refusal> {
        let booted = self.booted_for(id)?;
        let Some(booted) = booted else {
            return Ok(None);
        };
        let cached = booted.evented.lock().map_err(poisoned)?.clone();
        if let Some(client) = cached.filter(|client| !client.closed()) {
            return Ok(Some(client));
        }
        self.join(&booted)?;
        Ok(booted.evented.lock().map_err(poisoned)?.clone())
    }

    pub fn program_client_for(&self, id: &BoxId) -> Result<Option<EventedClient>, Refusal> {
        let booted = self.booted_for(id)?;
        let Some(booted) = booted else {
            return Ok(None);
        };
        let joins = self
            .joins
            .lock()
            .map_err(poisoned)?
            .clone()
            .ok_or_else(|| invalid("guest program has no node event loop"))?;
        let (token, room) = joins.reserve()?;
        let (client, pump) = Client::connect_evented(triomphe::Arc::clone(&booted.link), room)?;
        let client = evented_owner(client)?;
        joins.join(token, pump)?;
        Ok(Some(client))
    }
}
