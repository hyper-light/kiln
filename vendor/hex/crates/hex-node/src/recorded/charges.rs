//! Joint plan step 14 on macOS and Windows (AC-T7.4.1; `docs/design/learned-charge.md` C1 to C5, C8): the charge
//! learner of a node whose platform caps a managed microVM's CPU rather than weighs it. Once a window, the platform's
//! own ([`crate::recorded::Recover::charge_window`]), it reads each running VM's usage through its native owner,
//! learns its charge and booking (`hex_sim::charge`), and caps it at its water-filled share (`charge::Node::rates`):
//! macOS's managed control's rate, Windows' job's hard cap ([`crate::recorded::Recover::set_managed_rate`]). Every
//! VM so keeps at least its booking and the caps fit the fleet's CPU budget, as Linux's weights hold them (C4).
//! Neither platform can hold a VM's memory at a budget below its ceiling: macOS ends a process past its footprint
//! limit and Windows refuses a commit past a job's, and neither throttles at a level as `memory.high` does, so each
//! VM's budget is refused, typed, naming that (C5). Its capacity answers carry what it learned, as Linux's do (C8).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hex_api::{CapacityResources, Learned, LearnedUse, Refusal, RuntimeCost, Usage};
use hex_core::record::{BoxRecord, Phase};
use hex_sim::charge::{self, ChargeError, ClassKey, Cpu, Sample};
use hex_sim::model::Class;

/// A VM by its native scope: one VM's lifetime.
type Scope = [u8; 16];

/// One CPU in milli-CPU, the unit a vCPU and the machine's CPU are counted in.
const CPU_MILLI: u64 = 1_000;

/// What the learner reads and writes of a node's managed VMs: their native owner's in the node, a record's in tests.
pub(crate) trait Managed {
    fn usage(&self, record: &BoxRecord) -> Result<Usage, Refusal>;
    /// Caps the VM at `rate` milli-CPU.
    fn set_rate(&self, record: &BoxRecord, rate: u64) -> Result<(), Refusal>;
}

fn refused(why: ChargeError) -> Refusal {
    match why {
        ChargeError::Full { what, bound } => Refusal::Exhausted { what, bound },
        ChargeError::Awaits(needs) => Refusal::Awaits { needs },
        other => Refusal::Invalid {
            what: "a microVM's learned charge",
            why: other.to_string(),
        },
    }
}

/// Whether `record` is a managed microVM that runs now: made, with its native lifetime, its receipt and its controller,
/// and not ended.
pub(crate) fn running(record: &BoxRecord) -> bool {
    record.phase == Phase::Ready
        && record.native_scope.is_some()
        && record.spec.allocation.is_some()
        && record.init.is_some()
        && record.exit.is_none()
        && matches!(
            record.spec.class.class(),
            hex_api::Class::Shards | hex_api::Class::Firecracker
        )
}

/// One VM the learner holds.
struct Vm {
    class: Class,
    /// What its receipt booked: what the ledger holds of it.
    receipt: CapacityResources,
    /// Its VMM's CPU so far, in microseconds, and when it was read.
    last: (u64, Instant),
    /// The cap it runs under: its allocation's CPU until the learner first caps it.
    rate: u64,
}

/// The node's learner (module docs).
pub(crate) struct Learner {
    node: charge::Node<Scope>,
    vms: BTreeMap<Scope, Vm>,
    /// Why the last window failed: until one passes, the capacity answers carry nothing learned.
    fault: Option<Refusal>,
    /// Why no VM's memory budget is held on this platform (C5), typed.
    unheld: Refusal,
}

/// The most VMs a machine of `machine` could hold, each at least the least reservation placement takes.
fn population(machine: CapacityResources) -> Result<usize, Refusal> {
    let cpus = machine
        .cpu_milli
        .checked_div(u64::from(hex_api::MIN_CPU_MILLI));
    let memory = machine.mem_bytes.checked_div(hex_api::MIN_MEMORY);
    cpus.zip(memory)
        .and_then(|(cpus, memory)| usize::try_from(cpus.min(memory)).ok())
        .ok_or_else(|| Refusal::Invalid {
            what: "a microVM's learned charge",
            why: "the machine's population passes a usize".into(),
        })
}

/// A VM's vCPUs: its admitted guest's CPU in whole CPUs, rounded up.
fn vcpus(allocation: hex_api::Allocation) -> Result<u32, Refusal> {
    let guest = allocation.guest()?;
    u32::try_from(guest.cpu_milli.div_ceil(CPU_MILLI)).map_err(|_| Refusal::Invalid {
        what: "a microVM's learned charge",
        why: "its guest's vCPUs pass a u32".into(),
    })
}

impl Learner {
    /// A learner over `window` on a machine of `machine`.
    pub(crate) fn new(window: Duration, machine: CapacityResources) -> Result<Self, Refusal> {
        let bound = population(machine)?;
        Ok(Self {
            node: charge::Node::new(
                window,
                machine,
                charge::Bounds {
                    vms: bound,
                    classes: bound,
                },
            )
            .map_err(refused)?,
            vms: BTreeMap::new(),
            fault: None,
            unheld: Refusal::Platform {
                what: "memory_high",
                platform: std::env::consts::OS,
                why: "a managed VM's memory is held at its ceiling alone: the platform ends (macOS's footprint \
                      limit) or refuses (Windows' job commit limit) past a bound, and throttles at no level below it"
                    .to_owned(),
            },
        })
    }

    /// One window (C1 to C4) read at `now`: the managed VMs `running` now (their records), within the fleet's `budget`.
    /// A failure is kept until a window passes.
    pub(crate) fn window(
        &mut self,
        io: &impl Managed,
        running: &[BoxRecord],
        budget: CapacityResources,
        now: Instant,
    ) -> Result<(), Refusal> {
        let outcome = self.learn(io, running, budget, now);
        self.fault = outcome.as_ref().err().cloned();
        outcome
    }

    fn learn(
        &mut self,
        io: &impl Managed,
        running: &[BoxRecord],
        budget: CapacityResources,
        now: Instant,
    ) -> Result<(), Refusal> {
        let scope_of = |record: &BoxRecord| record.native_scope.as_ref().map(|s| *s.bytes());
        let gone: Vec<Scope> = self
            .vms
            .keys()
            .filter(|scope| !running.iter().any(|r| scope_of(r) == Some(**scope)))
            .copied()
            .collect();
        for scope in gone {
            self.node.end(&scope).map_err(refused)?;
            self.vms.remove(&scope);
        }
        // What each VM used of the cap it ran under in this window, for its next cap.
        let mut used = BTreeMap::new();
        for record in running {
            let Some(scope) = scope_of(record) else {
                continue;
            };
            let Some(allocation) = record.spec.allocation else {
                continue;
            };
            // A VM ending between the listing and its read is read no more; the next listing ends it.
            let Ok(usage) = io.usage(record) else {
                continue;
            };
            if let Some(vm) = self.vms.get_mut(&scope) {
                let (before, at) = vm.last;
                let cpu = Duration::from_micros(usage.cpu_usec.saturating_sub(before));
                let span = now.saturating_duration_since(at);
                self.node
                    .observe(
                        scope,
                        &Sample {
                            cpu: Cpu::Whole(cpu),
                            span,
                            mem_bytes: usage.memory_bytes,
                            disk_bytes: None,
                        },
                    )
                    .map_err(refused)?;
                let milli = cpu
                    .as_micros()
                    .checked_mul(u128::from(CPU_MILLI))
                    .and_then(|n| n.checked_div(span.as_micros().max(1)))
                    .and_then(|n| u64::try_from(n).ok())
                    .unwrap_or(u64::MAX);
                used.insert(scope, (milli, vm.rate));
                vm.last = (usage.cpu_usec, now);
                continue;
            }
            let class = match record.spec.class.class() {
                hex_api::Class::Shards => Class::Shards,
                hex_api::Class::Firecracker => Class::Firecracker,
                hex_api::Class::Process | hex_api::Class::Oci => continue,
            };
            let receipt = allocation.booking();
            self.node
                .track(
                    scope,
                    ClassKey::new(class, allocation.cost.id, allocation.resources),
                    0,
                    vcpus(allocation)?,
                    receipt,
                )
                .map_err(refused)?;
            self.vms.insert(
                scope,
                Vm {
                    class,
                    receipt,
                    last: (usage.cpu_usec, now),
                    rate: allocation.resources.cpu_milli,
                },
            );
        }
        self.node.set_budget(budget);
        self.node.settle().map_err(refused)?;
        let rates = self
            .node
            .rates(budget.cpu_milli, &used, u64::from(hex_api::MIN_CPU_MILLI))
            .map_err(refused)?;
        for (scope, rate) in rates {
            let Some(vm) = self.vms.get_mut(&scope) else {
                continue;
            };
            if vm.rate == rate {
                continue;
            }
            let record = running
                .iter()
                .find(|record| scope_of(record) == Some(scope))
                .ok_or_else(|| Refusal::Broken("a learned VM without its record".into()))?;
            io.set_rate(record, rate)?;
            vm.rate = rate;
        }
        Ok(())
    }

    /// The window's failure is `why`, which no read reached.
    pub(crate) fn failed(&mut self, why: Refusal) {
        self.fault = Some(why);
    }

    /// Why no VM's memory budget is held on this platform (C5).
    pub(crate) fn unheld(&self) -> &Refusal {
        &self.unheld
    }

    /// The cap each VM runs under now, by its lifetime.
    #[cfg(test)]
    fn rate(&self, scope: &Scope) -> Option<u64> {
        self.vms.get(scope).map(|vm| vm.rate)
    }

    /// What `class`'s capacity answer carries (C2, C8): the priors of `costs`, the class's learned use, and each of
    /// its running VMs' holdings, by their records' ids, those most past their receipts first. None after a failed
    /// window, so its VMs are booked whole.
    pub(crate) fn learned(
        &self,
        class: Class,
        costs: &[RuntimeCost],
        running: &[BoxRecord],
    ) -> Result<Option<Learned>, Refusal> {
        if self.fault.is_some() {
            return Ok(None);
        }
        let mut vms = Vec::new();
        for record in running {
            let Some(scope) = record.native_scope.as_ref().map(|s| *s.bytes()) else {
                continue;
            };
            if let Some(vm) = self.vms.get(&scope)
                && vm.class == class
            {
                vms.push((scope, &record.spec.id, vm.receipt));
            }
        }
        learned(&self.node, class, costs, vms, Vec::new()).map(Some)
    }
}

/// What a capacity answer for `class` carries of `node` (C2, C5, C8), shared by every OS's learner: the priors of
/// `costs` (at most one per profile and ceiling, none booked whole), the class's learned use and each of its `vms`'
/// holdings (by scope, box and receipt), those most past their receipts first (memory, then CPU, then disk), and the
/// class's `ended` VMs.
pub(crate) fn learned(
    node: &charge::Node<Scope>,
    class: Class,
    costs: &[RuntimeCost],
    vms: Vec<(Scope, &hex_api::BoxId, CapacityResources)>,
    ended: Vec<hex_api::LearnedEnd>,
) -> Result<Learned, Refusal> {
    let mut priors: Vec<hex_api::LearnedPrior> = Vec::new();
    for cost in costs {
        let ceiling = cost.allocate(CapacityResources::default())?.resources;
        let booking = node.prior(&ClassKey::new(class, cost.id, ceiling));
        let known = priors
            .iter()
            .any(|prior| prior.profile == cost.id && prior.ceiling == ceiling);
        if booking != ceiling && !known && priors.len() < hex_api::MAX_LEARNED_PRIORS {
            priors.push(hex_api::LearnedPrior {
                profile: cost.id,
                ceiling,
                booking,
            });
        }
    }
    let mut used = LearnedUse::default();
    let mut holdings = Vec::new();
    holdings
        .try_reserve_exact(vms.len())
        .map_err(|_| Refusal::Exhausted {
            what: "a class's learned holdings",
            bound: vms.len(),
        })?;
    for (scope, id, receipt) in vms {
        let holding = node
            .holding(&scope)
            .ok_or_else(|| Refusal::Broken("a learned VM without a holding".into()))?;
        used = used
            .checked_add(LearnedUse {
                booked: receipt,
                reservation: holding,
            })
            .ok_or_else(|| Refusal::Invalid {
                what: "a microVM's learned charge",
                why: "a class's learned use overflows".into(),
            })?;
        let past = (
            holding.mem_bytes.saturating_sub(receipt.mem_bytes),
            holding.cpu_milli.saturating_sub(receipt.cpu_milli),
            holding.disk_bytes.saturating_sub(receipt.disk_bytes),
        );
        holdings.push((
            past,
            hex_api::LearnedHolding {
                id: id.try_clone()?,
                holding,
            },
        ));
    }
    holdings.sort_by_key(|(past, _)| std::cmp::Reverse(*past));
    Ok(Learned {
        priors,
        used,
        holdings: holdings.into_iter().map(|(_, holding)| holding).collect(),
        ended,
    })
}

#[cfg(test)]
mod tests;
