//! The `oci` class where the host's kernel isn't Linux (docs/design/shards-guest.md §1, §5): one Shards
//! guest per node, booted on the node's first `oci` create, running Hex's own Linux node agent, unchanged
//! (CLAUDE.md §8). This runtime is a client of that node: it stages each box's root into the guest
//! ([`hex_client::guest::stage`]), rewrites the spec to the guest's paths ([`rewrite`]), and forwards every
//! call over [`hex_client::guest::Client`]. Shards' existing KVM/HVF/WHP adapters cover the supported
//! hosts; the public seam recognizes them, and the actual backend checks host capabilities at boot.

mod books;
mod cost;
mod dedicated;
mod family;
pub mod gpu;
mod gpu_pool;
pub mod held;
mod image;
mod leases;
mod managed;
#[cfg(target_os = "linux")]
pub(crate) use managed::wait_stopped;
pub use managed::{CapCycle, ManagedGuestTime, ManagedRate};
#[cfg(any(test, feature = "native-trusted-entry-measurement"))]
pub(crate) mod native_fixture;
mod observer;
mod physical;
mod pool;
pub mod rewrite;
mod route;
mod run_out;
pub mod selected;
mod served;
mod shared;
pub mod shares;

pub use image::{ImageClass, ImageWaits, NodeBooted, NodeVm, host_images};
pub use observer::config::{
    ENV_GPU_OBSERVER_FILE, ENV_GPU_OBSERVER_SHA256, GpuArtifact, GpuArtifactRole, GpuObserver,
    GpuObserverInput,
};

use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, ClassSpec, Proc, Program, Refusal, Rootfs, Runtime, Signal,
    Stdio, Usage,
};
use hex_client::guest::Link;
use hex_client::guest::shards::{ShardsLink, refusal};
use hex_client::guest::stage::{self, Item};
use hex_client::guest::{Client, LinkOwner, own_link};
use hex_core::stream::{self, files};

use shares::Share;

/// The existing evented connection owner, shared without an unchecked new Arc.
pub type EventedClient = triomphe::Arc<Client>;

pub use dedicated::ExecClient;

fn evented_owner(client: Client) -> Result<EventedClient, Refusal> {
    triomphe::Arc::try_new(client).map_err(|_| Refusal::Exhausted {
        what: "guest evented client owner",
        bound: std::mem::size_of::<Client>(),
    })
}

pub use cost::{
    CpuResponse, ENV_COST_EVIDENCE, ENV_COST_FILE, ENV_COST_SHA256, GuestInputs,
    cpu_response_section, cpu_response_split, load_runtime_cost, load_runtime_evidence,
    load_runtime_inputs,
};

/// The guest a node boots: its kernel and initramfs (whose `/init` is Hex's static `hex`), its vCPUs and
/// memory (the node's to give, §1), and the host folders it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestConfig {
    /// The `shards` binary that boots it (on macOS the signed build, run where it is installed).
    pub binary: PathBuf,
    /// Which form of the guest it boots: the one, never both.
    pub boot: Boot,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Enable Shards' existing shared Venus GPU; physical reservations still require mapping.
    pub gpu: bool,
    pub shares: Vec<Share>,
    /// Where the guest's relay stages each box's root: [`stage::STAGED`], the guest's tmpfs.
    pub staged: String,
}

/// How a node boots its guest, one form or the other (docs/design/node-under-shards-init.md §4), so that both can't
/// be configured at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// Shards' direct boot of Hex's own kernel and initramfs, whose `/init` is `hex`, the guest's PID 1
    /// (`HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`).
    Kernel { kernel: PathBuf, initrd: PathBuf },
    /// Hex's node image, pinned by digest, booted through Shards' image boot under shards-init with its nesting grant
    /// (`HEX_GUEST_IMAGE`).
    Image(String),
}

/// The environment variables a node reads its guest from.
pub const ENV_SHARDS_BIN: &str = "SHARDS_BIN";
pub const ENV_KERNEL: &str = "HEX_GUEST_KERNEL";
pub const ENV_INITRD: &str = "HEX_GUEST_INITRD";
pub const ENV_CPUS: &str = "HEX_GUEST_CPUS";
pub const ENV_MEMORY_MIB: &str = "HEX_GUEST_MEMORY_MIB";
/// Explicit shared Venus enable; absent preserves the CPU-only guest configuration.
pub const ENV_GPU: &str = "HEX_GUEST_GPU";
/// The node's guest under shards-init (docs/design/node-under-shards-init.md §4): an Agentfile image of Hex's own
/// init, pinned by digest, which Shards boots with its nesting grant. It is the other form of the guest to the kernel
/// and initramfs above, and a node boots one form.
pub const ENV_IMAGE: &str = "HEX_GUEST_IMAGE";

impl GuestConfig {
    /// The guest named by the environment ([`GuestConfig::from_vars`] over the process's).
    pub fn from_env() -> Result<Self, Refusal> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// The guest named by `var`: `SHARDS_BIN`, its form (`HEX_GUEST_KERNEL` and `HEX_GUEST_INITRD`, or
    /// `HEX_GUEST_IMAGE`), `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`, each needed (Shards is never taken from
    /// `PATH` unasked, and the guest's vCPUs and memory are the node's to give from its budget, §1, so none is
    /// guessed), and `HEX_GUEST_SHARES` (else [`shares::defaults`]). Every one missing is named in one refusal, read
    /// before anything boots, so an operator fixes them in one go. Shards refuses vCPUs or memory past the host's (its
    /// `Bounds`) when it boots.
    ///
    /// A guest under shards-init ([`ENV_IMAGE`]) boots through the image class and is reached through its run's
    /// agent-control endpoint (docs/design/node-under-shards-init.md §5). Named beside the kernel or the initramfs, it
    /// is refused naming both forms; asked a GPU, it awaits D234's `gpus=` ([`hex_api::Upstream::NestedRuntime`]).
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, Refusal> {
        let image = var(ENV_IMAGE);
        if image.is_some() {
            let direct: Vec<&str> = [ENV_KERNEL, ENV_INITRD]
                .into_iter()
                .filter(|name| var(name).is_some())
                .collect();
            if !direct.is_empty() {
                return Err(Refusal::Invalid {
                    what: "the oci class's guest",
                    why: format!(
                        "{ENV_IMAGE} and {} are both set: a node boots one form of its guest, the image under \
                         shards-init or the kernel and initramfs",
                        direct.join(", ")
                    ),
                });
            }
        }
        let needed: &[&str] = if image.is_some() {
            &[ENV_SHARDS_BIN, ENV_IMAGE, ENV_CPUS, ENV_MEMORY_MIB]
        } else {
            &[
                ENV_SHARDS_BIN,
                ENV_KERNEL,
                ENV_INITRD,
                ENV_CPUS,
                ENV_MEMORY_MIB,
            ]
        };
        let missing: Vec<&str> = needed
            .iter()
            .copied()
            .filter(|name| var(name).is_none_or(|v| v.is_empty()))
            .collect();
        if !missing.is_empty() {
            return Err(Refusal::Invalid {
                what: "the oci class's guest",
                why: format!(
                    "{} unset: the node boots its Linux guest from them",
                    missing.join(", ")
                ),
            });
        }
        let path = |name: &str| PathBuf::from(var(name).unwrap_or_default());
        let gpu = match var(ENV_GPU) {
            None => false,
            Some(value) if value == "false" => false,
            Some(value) if value == "true" => true,
            Some(_) => {
                return Err(Refusal::Invalid {
                    what: ENV_GPU,
                    why: "expected true or false".to_owned(),
                });
            }
        };
        let boot = match image {
            Some(image) => {
                if gpu {
                    return Err(Refusal::Awaits {
                        needs: hex_api::Upstream::NestedRuntime,
                    });
                }
                let image = image.into_string().map_err(|given| Refusal::Invalid {
                    what: ENV_IMAGE,
                    why: format!("{given:?} is not UTF-8, as an image's reference is"),
                })?;
                // Pinned by its digest (§4; the review's C-17), so a tag that moved never boots another image as the
                // node boots its guest again.
                hex_api::check_pinned(ENV_IMAGE, &image)?;
                Boot::Image(image)
            }
            None => Boot::Kernel {
                kernel: path(ENV_KERNEL),
                initrd: path(ENV_INITRD),
            },
        };
        Ok(Self {
            binary: path(ENV_SHARDS_BIN),
            boot,
            vcpus: number(&var, ENV_CPUS)?,
            memory_mib: number(&var, ENV_MEMORY_MIB)?,
            gpu,
            shares: shares::from_var(var(shares::ENV))?,
            staged: stage::STAGED.to_owned(),
        })
    }

    /// The spec Shards boots: the initramfs's `/init` is `hex`, which as PID 1 is the guest's init
    /// whatever words the kernel passes it (HEX-GUEST-12); its command line the serial console, a panic
    /// resetting the guest so the host sees it end, and `hex.shares=` naming each share's mode in tag
    /// order, none when there are none (HEX-GUEST-11). The guest is reached on the channel's three
    /// ports alone. An image's boot has none: it boots through Shards' image boot ([`ImageClass::boot_node`]).
    pub fn spec(&self) -> Result<shards_api::GuestSpec, Refusal> {
        let (kernel, initrd) = self.direct()?;
        Ok(shards_api::GuestSpec {
            kernel: kernel.to_path_buf(),
            init: shards_api::Init::Initrd(initrd.to_path_buf()),
            cmdline: self.cmdline(),
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            gpu: self.gpu,
            gpu_devices: Vec::new(),
            disks: Vec::new(),
            binary: Some(self.binary.clone()),
            ports: vec![stream::CONTROL_PORT, stream::STDIO_PORT, files::FILES_PORT],
            shares: self
                .shares
                .iter()
                .map(|s| shards_api::Share {
                    dir: s.dir.clone(),
                    read_only: s.read_only,
                })
                .collect(),
        })
    }

    /// The direct boot's kernel and initramfs; refused for an image, which has neither.
    pub fn direct(&self) -> Result<(&std::path::Path, &std::path::Path), Refusal> {
        match &self.boot {
            Boot::Kernel { kernel, initrd } => Ok((kernel, initrd)),
            Boot::Image(_) => Err(Refusal::Invalid {
                what: "the oci class's guest",
                why: "an image under shards-init boots through Shards' image boot, not its direct boot of a kernel and \
                      initramfs"
                    .to_owned(),
            }),
        }
    }
}

impl GuestConfig {
    fn cmdline_parts(&self, mut emit: impl FnMut(&str)) {
        // Shards' console (D261, `shards_api::CONSOLE`): virtio-console alone since Shards 35e2de91e (the 16550 gone),
        // the one every default line Shards boots with begins with. Named, since a line naming none leaves the kernel no
        // console and Shards keeps none of the boot's lines; taken from Shards, never a copy of Hex's, since one naming
        // another (`console=ttyS0`) left init no /dev/console (g121, 2026-11-15).
        // `panic=-1`: a negative timeout reboots at once on a panic (kernel-parameters.txt, `panic=`;
        // kernel/panic.c `panic_timeout`), so the VM ends and the host sees it rather than a guest that
        // hangs.
        emit(shards_api::CONSOLE);
        emit(" panic=-1");
        if !self.shares.is_empty() {
            emit(" ");
            emit(shares::CMDLINE);
            emit("=");
            let mut separated = false;
            for share in &self.shares {
                if separated {
                    emit(",");
                }
                emit(if share.read_only { "ro" } else { "rw" });
                separated = true;
            }
        }
    }

    fn cmdline(&self) -> String {
        let mut cmdline = String::new();
        self.cmdline_parts(|part| cmdline.push_str(part));
        cmdline
    }

    #[cfg(any(test, feature = "native-trusted-entry-measurement"))]
    pub(crate) fn try_spec(&self) -> Result<shards_api::GuestSpec, Refusal> {
        let exhausted = |bound| Refusal::Exhausted {
            what: "native fixture guest specification storage",
            bound,
        };
        if self.shares.len() > shares::MAX_SHARES {
            return Err(route::invalid(
                "native fixture exceeds the configured share bound",
            ));
        }
        let path = |path: &std::path::Path| -> Result<PathBuf, Refusal> {
            let mut copy = OsString::new();
            copy.try_reserve_exact(path.as_os_str().len())
                .map_err(|_| exhausted(path.as_os_str().len()))?;
            copy.push(path.as_os_str());
            Ok(PathBuf::from(copy))
        };
        let mut count = Some(0usize);
        self.cmdline_parts(|part| count = count.and_then(|count| count.checked_add(part.len())));
        let mut cmdline = String::new();
        let count = count.ok_or_else(|| exhausted(usize::MAX))?;
        cmdline
            .try_reserve_exact(count)
            .map_err(|_| exhausted(count))?;
        // The same immutable segment traversal was counted above. Every push
        // fits this reservation; no formatting/allocation fallback is used.
        self.cmdline_parts(|part| cmdline.push_str(part));
        let mut shares = Vec::new();
        shares
            .try_reserve_exact(self.shares.len())
            .map_err(|_| exhausted(shares::MAX_SHARES))?;
        for share in &self.shares {
            shares.push(shards_api::Share {
                dir: path(&share.dir)?,
                read_only: share.read_only,
            });
        }
        let allowed = [stream::CONTROL_PORT, stream::STDIO_PORT, files::FILES_PORT];
        let mut ports = Vec::new();
        ports
            .try_reserve_exact(allowed.len())
            .map_err(|_| exhausted(allowed.len()))?;
        ports.extend_from_slice(&allowed);
        let (kernel, initrd) = self.direct()?;
        Ok(shards_api::GuestSpec {
            kernel: path(kernel)?,
            init: shards_api::Init::Initrd(path(initrd)?),
            cmdline,
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            gpu: self.gpu,
            gpu_devices: Vec::new(),
            disks: Vec::new(),
            binary: Some(path(&self.binary)?),
            ports,
            shares,
        })
    }

    /// One existing fixed-VM cost/configuration check, shared by runtime
    /// registration and the measurement's typed preparation, without an owner.
    pub(crate) fn checked_runtime_allocation(
        &self,
        cost: hex_api::RuntimeCost,
    ) -> Result<hex_api::Allocation, Refusal> {
        cost.check()?;
        let configured = selected::resources(self)?;
        if cost.floor.cpu_milli != configured.cpu_milli
            || cost.ceiling.cpu_milli != configured.cpu_milli
            || cost.floor.mem_bytes != configured.mem_bytes
            || cost.ceiling.mem_bytes != configured.mem_bytes
        {
            let why = "CPU/memory floor and ceiling must equal the configured VM";
            let mut text = String::new();
            text.try_reserve_exact(why.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "Shards runtime cost refusal bytes",
                    bound: why.len(),
                })?;
            text.push_str(why);
            return Err(Refusal::Invalid {
                what: "Shards runtime cost",
                why: text,
            });
        }
        cost.allocate(hex_api::CapacityResources::default())
    }
}

fn number<T: std::str::FromStr>(
    var: &impl Fn(&str) -> Option<OsString>,
    name: &'static str,
) -> Result<T, Refusal>
where
    T::Err: std::fmt::Display,
{
    let text = var(name).unwrap_or_default();
    let text = text.to_string_lossy();
    text.parse::<T>().map_err(|e| Refusal::Invalid {
        what: name,
        why: format!("{text:?}: {e}"),
    })
}

/// The booted guest and the client of its node.
enum GuestHandle {
    Shared(Arc<dyn shards_api::Guest>),
    Managed(shards_api::ManagedGuest),
    /// The node's guest under shards-init: a node VM, the image class's run (joint plan step 39), whose end and stop
    /// are the class's.
    Node(hex_api::NodeRun),
}

impl GuestHandle {
    /// The Shards guest it is, where it is one: a node VM under shards-init is the image class's run instead.
    fn shards(&self) -> Option<&dyn shards_api::Guest> {
        match self {
            Self::Shared(guest) => Some(guest.as_ref()),
            Self::Managed(guest) => Some(guest),
            Self::Node(_) => None,
        }
    }

    /// The Shards guest `call` needs: a dedicated or shared guest's, never the node VM under shards-init, which no such
    /// call reaches, refused by name if one did.
    fn shards_for(&self, call: &'static str) -> Result<&dyn shards_api::Guest, Refusal> {
        self.shards().ok_or_else(|| {
            Refusal::Broken(format!(
                "{call}: the node's guest under shards-init is no Shards guest"
            ))
        })
    }
}

struct Owner {
    id: BoxId,
    class: Class,
    allocation: Option<hex_api::Allocation>,
    route: Option<hex_api::RouteReceipt>,
}

struct Booted {
    guest: GuestHandle,
    link: LinkOwner,
    client: Client,
    evented: Mutex<Option<EventedClient>>,
    owner: Option<Owner>,
    backing: Option<shared::Binding>,
}

fn booted_owner(booted: Booted) -> Result<triomphe::Arc<Booted>, Refusal> {
    triomphe::Arc::try_new(booted).map_err(|_| Refusal::Exhausted {
        what: "booted guest connection owner",
        // One such connection owner per already-admitted VM, not another VM grant.
        bound: 1,
    })
}

// This host's class, as the records name it (records.tsv, evidence alone; the review's C-9).
pub use hex_client::guest::shards::CLASS;

/// The managed plan of guest `spec` booted with `binary` on `socket`: the native plan where `images` declare its images
/// (its cost evidence's), else Shards' own plan with the runtime folders `paths`. Its guest is told no boot deadline, so
/// its relay keeps its own learned bounds (decision 9; the review's C-9). A node's guests prepare theirs here, and so
/// does the guest-cost producer (crates/hex-cli/examples/guest_cost), so it measures what a node runs (AC-T7.4.1).
pub fn managed_plan(
    spec: &shards_api::GuestSpec,
    binary: &std::path::Path,
    socket: &std::path::Path,
    paths: &shards_api::RuntimePaths,
    images: Option<shards_api::NativeImages>,
) -> Result<shards_api::PreparedBoot, Refusal> {
    let plan = if let Some(images) = images {
        shards_api::prepare_native_boot(
            spec,
            binary,
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
            binary,
            socket,
            shards_api::recorded_console_bytes(&spec.kernel),
        )
        .and_then(|plan| plan.with_runtime_paths(paths))
    };
    plan.map_err(|why| refusal("preparing admitted Shards scope", &why))
}

/// A managed guest booted from `plan` through `transaction`'s native owner, following its guest with no deadline (the
/// review's C-9): the one managed launch a node's guests take ([`managed::boot`]), public so the guest-cost producer
/// boots its guests as a node does (AC-T7.4.1).
pub fn boot_managed(
    transaction: &dyn crate::devices::Transaction,
    plan: shards_api::PreparedBoot,
) -> Result<shards_api::ManagedGuest, Refusal> {
    managed::boot(transaction, plan)
}

/// What tells a node's loop that bytes wait to be written on a connection it watches.
pub type Room = Arc<dyn Fn() + Send + Sync>;

/// What joins a guest's control connection, in the event form, to its node's loop: the node gives one
/// ([`GuestRuntime::join_loop`]), and every guest booted or taken back from then is joined through it.
pub trait Joins: Send + Sync {
    /// A token for a connection to come, and what tells the loop that bytes wait to be written on it.
    fn reserve(&self) -> Result<(u64, Room), Refusal>;
    /// Watches `pump` under `token` from now.
    fn join(&self, token: u64, pump: hex_client::guest::Pump) -> Result<(), Refusal>;
}

/// The `oci` class through a Shards guest.
pub struct GuestRuntime {
    guests: Arc<dyn shards_api::Guests>,
    config: GuestConfig,
    booted: Mutex<Option<triomphe::Arc<Booted>>>,
    /// The shared backing's guest overhead as its capacity answers measured it, by its generation (joint plan step 14,
    /// C7): none before the first.
    guest_overhead: Mutex<Option<(hex_api::BackingKey, shared::Seen)>>,
    /// The slowest boot to ready this node has seen, its guest's boots' W* (decision 9): none before its first.
    slowest: Mutex<Option<Duration>>,
    /// The digests learned for layer tarballs not named by theirs, as they crossed (§4a).
    digests: Mutex<Digests>,
    /// The Shards binary's identity when the running guest booted it.
    booted_binary: Mutex<Option<stage::Identity>>,
    /// The warm-up of a Shards binary that changed while the guest ran ([`GuestRuntime::warm_if_changed`]).
    warm: Mutex<Warm>,
    /// Where the guest it holds is recorded ([`GuestRuntime::recorded_at`]).
    record: Option<PathBuf>,
    /// The event form's client of the guest's node: its answers are read by the node's loop, so a call
    /// through it holds no thread while the guest answers.
    dedicated: Mutex<Vec<dedicated::Slot>>,
    gpu_catalog: Option<crate::devices::SharedCatalog>,
    books: OnceLock<Weak<dyn crate::devices::Books>>,
    runtime_cost: Option<hex_api::RuntimeCost>,
    native_images: Option<shards_api::NativeImages>,
    native_paths: Option<Arc<shards_api::RuntimePaths>>,
    shared_pool: Mutex<Option<hex_api::BackingKey>>,
    gpu_observer: Option<GpuObserver>,
    /// What joins each guest's evented connection to the node's loop ([`GuestRuntime::join_loop`]).
    joins: Mutex<Option<Arc<dyn Joins>>>,
    /// The Shards class on Agentfile images ([`GuestRuntime::with_images`]), its boot none until given.
    images: image::ImageClass,
    /// The lanes its guest under shards-init admits at once to its relay's sockets, as its node fitted them
    /// ([`GuestRuntime::set_lanes`]); none until given, and that guest is refused without them.
    lanes: OnceLock<u32>,
    /// The key it controls its guest under shards-init by (D252), made at that guest's first boot from the OS's
    /// random numbers: its run's endpoint takes it alone.
    control: Mutex<Option<Arc<shards_api::control::Identity>>>,
}

/// A throwaway guest booted on a Shards binary that changed while the node's guest ran, so the node's next boot
/// of it is warm: an upgrade's first boot pays the host's first-run checks of its new code, 1.5-5.3 s against a
/// warm boot's 0.17 s, once per binary and for every process after it
/// (bench/native/guest/boot-ready/RECORD.md). At most one is in flight.
#[derive(Default)]
struct Warm {
    /// The binary warmed, or being warmed.
    identity: Option<stage::Identity>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Its throwaway guest, as its thread boots and follows it ([`GuestRuntime::finish_warm`]).
    guest: Arc<Mutex<Throwaway>>,
}

/// A warm-up's throwaway guest, as the runtime's end finds it. Its readiness is followed by its VM's life past W*
/// (decision 9), so the runtime's end stops it rather than wait on a boot nobody needs: a stop ends the wait that follows
/// it (Shards' `Ended(Stopped)`).
#[derive(Default)]
enum Throwaway {
    /// Not booted yet.
    #[default]
    Booting,
    /// Booted, its readiness followed.
    Booted(Arc<dyn shards_api::Guest>),
    /// Stopped, by its warm-up once ready or by the runtime's end: one booted after that end is stopped at once.
    Stopped,
}

impl Throwaway {
    /// Stops the throwaway in `slot` once, whoever asks first: its warm-up once it is ready, or the runtime's end.
    fn stop(slot: &Mutex<Self>) {
        let Ok(mut held) = slot.lock() else {
            return;
        };
        if let Self::Booted(guest) = std::mem::replace(&mut *held, Self::Stopped) {
            let _ = guest.stop();
        }
    }
}

/// Boots a throwaway guest of `spec` on `guests` and follows it to ready as the node's own boot does (decision 9: within
/// `bound`, the slowest boot this node has seen, counted from the boot's start; past it the run-out told and the dial
/// following its VM by its life), then stops it, or at once where the runtime ended first (`slot`). Ready, so its VM
/// process ran too. A warm-up that fails leaves the next boot to pay the first run, as it would have.
fn warm_up(
    guests: &dyn shards_api::Guests,
    spec: &shards_api::GuestSpec,
    bound: Option<Duration>,
    slot: &Mutex<Throwaway>,
) {
    let began = Instant::now();
    let Ok(guest) = guests.boot(spec) else {
        return;
    };
    let guest: Arc<dyn shards_api::Guest> = guest.into();
    let booted = match slot.lock() {
        Ok(mut held) if matches!(*held, Throwaway::Booting) => {
            *held = Throwaway::Booted(Arc::clone(&guest));
            true
        }
        _ => false,
    };
    if !booted {
        let _ = guest.stop();
        return;
    }
    let link = ShardsLink::new(Arc::clone(&guest), bound).booted_at(began);
    let _ = link.dial(stream::CONTROL_PORT);
    Throwaway::stop(slot);
}

/// The Shards binary's identity, if it can be read (a binary that can't is the next boot's to refuse).
fn binary_identity(binary: &std::path::Path) -> Option<stage::Identity> {
    std::fs::metadata(binary).ok().map(|m| stage::identity(&m))
}

/// The digests learned for layer tarballs not named by theirs (docker save's `<id>/layer.tar`), each with the
/// identity its file had when it was read whole ([`stage::Identity`]): a create names such a layer by it
/// without reading it, while its file's identity is that one; one whose identity changed is hashed again
/// as it crosses. Bounded by the layers the guest's store can hold (its last budget's capacity), one entry
/// each; past it, the least recently used goes, and that layer is hashed again when it next crosses.
#[derive(Default)]
struct Digests {
    by_path: std::collections::BTreeMap<PathBuf, (stage::Identity, files::Digest, u64)>,
    by_use: std::collections::BTreeMap<u64, PathBuf>,
    used: u64,
    bound: usize,
}

impl Digests {
    /// The digests of `paths` this remembers, for files whose identity is the one each was learned with.
    fn known(&mut self, paths: &[PathBuf]) -> rewrite::Known {
        let mut known = rewrite::Known::new();
        for path in paths {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let Some((identity, digest, last)) = self.by_path.get_mut(path) else {
                continue;
            };
            if *identity != stage::identity(&meta) {
                continue;
            }
            self.used = self.used.saturating_add(1);
            self.by_use.remove(last);
            *last = self.used;
            self.by_use.insert(self.used, path.clone());
            known.insert(path.clone(), *digest);
        }
        known
    }

    /// Remembers what a stage learned, within the layers the guest's store can hold.
    fn learn(&mut self, staged: stage::Staged) {
        self.bound = usize::try_from(staged.capacity).unwrap_or(usize::MAX);
        for learned in staged.learned {
            self.used = self.used.saturating_add(1);
            if let Some((_, _, last)) = self.by_path.insert(
                learned.path.clone(),
                (learned.identity, learned.digest, self.used),
            ) {
                self.by_use.remove(&last);
            }
            self.by_use.insert(self.used, learned.path);
        }
        while self.by_path.len() > self.bound {
            let Some((_, path)) = self.by_use.pop_first() else {
                break;
            };
            self.by_path.remove(&path);
        }
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("the guest runtime's lock is poisoned".to_owned())
}

impl GuestRuntime {
    fn cpu_spec(&self) -> Result<shards_api::GuestSpec, Refusal> {
        let mut spec = self.config.spec()?;
        spec.gpu = false;
        Ok(spec)
    }
    /// The class over `guests`, booting `config`'s guest when first asked. A boot's first dial waits within
    /// [`Self::boot_bound`], and past it follows the guest ([`ShardsLink`]).
    pub fn new(guests: Arc<dyn shards_api::Guests>, config: GuestConfig) -> Self {
        let images = image::ImageClass::new(config.binary.clone());
        Self {
            guests,
            config,
            booted: Mutex::new(None),
            guest_overhead: Mutex::new(None),
            slowest: Mutex::new(None),
            digests: Mutex::new(Digests::default()),
            booted_binary: Mutex::new(None),
            warm: Mutex::new(Warm::default()),
            record: None,
            dedicated: Mutex::new(Vec::new()),
            gpu_catalog: None,
            books: OnceLock::new(),
            runtime_cost: None,
            native_images: None,
            native_paths: None,
            shared_pool: Mutex::new(None),
            gpu_observer: None,
            joins: Mutex::new(None),
            images,
            lanes: OnceLock::new(),
            control: Mutex::new(None),
        }
    }

    /// The lanes its guest under shards-init admits at once to its relay's sockets: what its node fitted to its
    /// descriptors with that guest's among them (hex-node's `serve::fit`), each a guest frame wide each way. Set once,
    /// as the node fits itself before that guest boots; set again, the same or refused.
    pub fn set_lanes(&self, lanes: u32) -> Result<(), Refusal> {
        match self.lanes.set(lanes) {
            Ok(()) => Ok(()),
            Err(again) if self.lanes.get() == Some(&again) => Ok(()),
            Err(_) => Err(Refusal::Broken(
                "the guest's lanes are fitted once, as its node starts".to_owned(),
            )),
        }
    }

    /// Records the guest it boots at `path` ([`held`]), and takes back the guest recorded there when the
    /// node starts ([`GuestRuntime::boot`]): a guest outlives its node's crash (its VM is a process of its
    /// own, reparented), so the next node holds it, and the boxes in it, rather than boot another beside it
    /// and leave it an orphan (native plan step 8, T-T7.1.2).
    pub fn recorded_at(mut self, path: PathBuf) -> Self {
        self.record = Some(path);
        self
    }

    /// Boots the node's guest now, so no create pays a boot, an upgrade's first least of all (HEX-GUEST-39): the
    /// node calls it when it starts, before it accepts a box ([`WithGuest::new`]). Where Shards has no backend
    /// there is no guest to boot, and the `oci` class is refused at create by name (§5).
    pub fn boot(&self) -> Result<(), Refusal> {
        if self.managed_shared() {
            return self.recover_backing();
        }
        if self.guests.backend().is_err() {
            return Ok(());
        }
        self.attach_recorded()?;
        self.guest().map(drop)
    }

    /// Takes back the guest a node before this one recorded, if there is one: attached, dialed and
    /// greeted as a boot's is, its boxes then the node's. One gone or ended is forgotten, and the node boots
    /// its own: Shards' take-back (D215, on every host) proves it by its process seen to have exited or its
    /// pid another start's. One that runs and won't answer is stopped first, so none is left beside the next.
    /// A guest that can't be told gone or taken back refuses the node by type, naming why, rather than boot
    /// another beside one that may run.
    fn attach_recorded(&self) -> Result<(), Refusal> {
        let Some(path) = &self.record else {
            return Ok(());
        };
        let Some(identity) = held::read(path)? else {
            return Ok(());
        };
        let guest: Arc<dyn shards_api::Guest> =
            match self.guests.attach(&identity, &self.cpu_spec()?) {
                Ok(guest) => guest.into(),
                Err(
                    shards_api::Refusal::Gone { .. }
                    | shards_api::Refusal::Ended(_)
                    | shards_api::Refusal::Process {
                        failure:
                            shards_api::ProcessFailure::Exited
                            | shards_api::ProcessFailure::DifferentStart { .. },
                        ..
                    },
                ) => {
                    return held::remove(path);
                }
                Err(why) => return Err(refusal("taking back the node's guest", &why)),
            };
        let link: LinkOwner = own_link(ShardsLink::new(Arc::clone(&guest), self.boot_bound()?))?;
        match Client::connect(triomphe::Arc::clone(&link)) {
            Ok(client) => {
                // The binary that booted it isn't known here: no warm-up is asked for it.
                let b = booted_owner(Booted {
                    guest: GuestHandle::Shared(guest),
                    link,
                    client,
                    evented: Mutex::new(None),
                    owner: None,
                    backing: None,
                })?;
                self.join(&b)?;
                *self.lock()? = Some(b);
                Ok(())
            }
            Err(_) => {
                guest.stop().map_err(|why| {
                    refusal("stopping a guest taken back that won't answer", &why)
                })?;
                held::remove(path)
            }
        }
    }

    /// The boxes this guest keeps a selected receipt for, by name: what the node's startup barrier
    /// joins to its retained records (AC-T7.4.1). Reads the receipt folder's names only.
    pub fn receipt_ids(&self) -> Result<Vec<BoxId>, Refusal> {
        self.selected_ids()
    }

    /// Joins the guest's control connection, in the event form, to the node's loop through `joins`, and
    /// every guest's booted or taken back after: a call through it is answered from the loop, holding no
    /// thread while the guest answers (native plan step 8 (1)). With no guest running, the next boot joins.
    pub fn join_loop(&self, joins: Arc<dyn Joins>) -> Result<(), Refusal> {
        *self.joins.lock().map_err(poisoned)? = Some(joins);
        match self.running()? {
            Some(b) => self.join(&b),
            None => Ok(()),
        }?;
        for id in self.selected_ids()? {
            if let Some(guest) = self.selected_running(&id)? {
                self.join(&guest)?;
            }
        }
        Ok(())
    }

    /// Opens booted guest `b`'s evented connection and joins it to the loop, where one is given: with no deadline, its
    /// connection follows its guest (the review's C-9).
    fn join(&self, b: &Booted) -> Result<(), Refusal> {
        let joins = self.joins.lock().map_err(poisoned)?.clone();
        let Some(joins) = joins else {
            return Ok(());
        };
        let (token, room) = joins.reserve()?;
        let (client, pump) = Client::connect_evented(triomphe::Arc::clone(&b.link), room)?;
        let client = evented_owner(client)?;
        *b.evented.lock().map_err(poisoned)? = Some(client);
        joins.join(token, pump)
    }

    /// The event form's client of the running guest, joined to the loop: one whose connection ended while
    /// its guest runs is joined again first. `None` with no guest running.
    pub fn evented(&self) -> Result<Option<EventedClient>, Refusal> {
        let Some(b) = self.running()? else {
            return Ok(None);
        };
        let held = b.evented.lock().map_err(poisoned)?.clone();
        if let Some(client) = held.filter(|c| !c.closed()) {
            return Ok(Some(client));
        }
        self.join(&b)?;
        Ok(b.evented.lock().map_err(poisoned)?.clone())
    }

    /// A program's own evented control connection, held until its start/end or cancellation.
    /// Windows' caller-owned sockets cannot be lent through another process. Keeping their offered exec on
    /// its own control connection makes cancellation close every early stream without affecting other programs.
    pub fn program_client(&self) -> Result<Option<EventedClient>, Refusal> {
        let Some(b) = self.running()? else {
            return Ok(None);
        };
        let joins = self
            .joins
            .lock()
            .map_err(poisoned)?
            .clone()
            .ok_or_else(|| {
                Refusal::Broken("the guest program has no node event loop".to_owned())
            })?;
        let (token, room) = joins.reserve()?;
        let (client, pump) = Client::connect_evented(triomphe::Arc::clone(&b.link), room)?;
        let client = evented_owner(client)?;
        joins.join(token, pump)?;
        Ok(Some(client))
    }

    /// Records `guest` as the one this node holds, where a record is kept: one whose identity Shards can't
    /// give leaves no record, so a node after a crash finds none to take back.
    fn record(&self, guest: &dyn shards_api::Guest) -> Result<(), Refusal> {
        let Some(path) = &self.record else {
            return Ok(());
        };
        match guest.identity() {
            Ok(identity) => held::write(path, &identity),
            Err(_) => held::remove(path),
        }
    }

    /// Where the Shards binary changed since the running guest booted it, boots one throwaway guest on it in the
    /// background, at Shards' least memory and with no shares, and stops it, so the node's next boot is warm
    /// (HEX-GUEST-40). The running guest is never booted again for it: that would end its boxes. At most one
    /// warm-up is in flight, and a binary is warmed once. Called on each create: a stat, no polling.
    fn warm_if_changed(&self) -> Result<(), Refusal> {
        // A profiled/native-owned runtime has no extra pool grant for a second
        // VM. Optional warming is skipped; the shared backing never pays twice.
        if self.runtime_cost.is_some() || self.uses_books() {
            return Ok(());
        }
        let Some(now) = binary_identity(&self.config.binary) else {
            return Ok(());
        };
        let booted = *self.booted_binary.lock().map_err(poisoned)?;
        if booted.is_none_or(|b| b == now) {
            return Ok(());
        }
        let mut warm = self.warm.lock().map_err(poisoned)?;
        if warm.identity == Some(now) || warm.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(());
        }
        let mut least = self.config.clone();
        least.vcpus = 1;
        least.memory_mib = shards_api::min_memory_mib(std::env::consts::ARCH);
        least.shares.clear();
        least.gpu = false;
        // Told no boot deadline, as the node's own guest is (the review's C-9).
        let spec = least.spec()?;
        let guests = Arc::clone(&self.guests);
        let bound = self.boot_bound()?;
        let slot = Arc::new(Mutex::new(Throwaway::Booting));
        let held = Arc::clone(&slot);
        let thread = std::thread::Builder::new()
            .name("hex-guest-warm".to_owned())
            .spawn(move || warm_up(guests.as_ref(), &spec, bound, &held))
            .map_err(|e| Refusal::os("starting a guest warm-up", e))?;
        warm.identity = Some(now);
        warm.thread = Some(thread);
        warm.guest = slot;
        Ok(())
    }

    /// Ends a warm-up in flight: its throwaway guest stopped, which ends its boot's wait, and its thread joined.
    fn finish_warm(&self) {
        let Ok((slot, thread)) = self
            .warm
            .lock()
            .map(|mut w| (Arc::clone(&w.guest), w.thread.take()))
        else {
            return;
        };
        Throwaway::stop(&slot);
        if let Some(t) = thread {
            let _ = t.join();
        }
    }

    /// The W* of the guest's boots (decision 9; the review's C-9): the slowest boot to ready this node has seen; none
    /// before its first, whose dial then follows its guest from its start.
    pub fn boot_bound(&self) -> Result<Option<Duration>, Refusal> {
        Ok(*self.slowest.lock().map_err(poisoned)?)
    }

    /// A boot to ready this node has seen, taking `took`: the bound rises with it, never falls.
    pub fn learn(&self, took: Duration) {
        if let Ok(mut slowest) = self.slowest.lock() {
            *slowest = Some(slowest.map_or(took, |slowest| slowest.max(took)));
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<triomphe::Arc<Booted>>>, Refusal> {
        self.booted.lock().map_err(poisoned)
    }

    /// The guest, booted now if it isn't, or booted again if it ended (every box in it ended with it). One boot, its
    /// first dial the boot's wait (decision 9; the review's C-9): within W*, the slowest boot to ready this node has
    /// seen, counted from the boot's start (HEX-GUEST-34); past it the run-out told once and the dial following the
    /// guest's VM by its life ([`ShardsLink`]); before any boot is seen, following from its start, told once as it
    /// starts. It is refused at the guest's end, at its VM's stall as Shards judges it, or by a refusal of its own,
    /// never at a bound, and never stopped and booted again. The guest is told no boot deadline: its relay keeps its
    /// own learned bounds.
    fn guest(&self) -> Result<triomphe::Arc<Booted>, Refusal> {
        if self.managed_shared() {
            return self.shared_running();
        }
        let mut booted = self.lock()?;
        if let Some(b) = booted.as_ref() {
            if self.still_runs(&b.guest)? {
                return Ok(triomphe::Arc::clone(b));
            }
            *booted = None;
        }
        if let Boot::Image(image) = &self.config.boot {
            let b = self.boot_image(image)?;
            *booted = Some(triomphe::Arc::clone(&b));
            return Ok(b);
        }
        // Refused by name before anything boots where Shards has no backend (§5).
        self.guests
            .backend()
            .map_err(|why| refusal("finding Shards' backend", &why))?;
        let binary = binary_identity(&self.config.binary);
        let bound = self.boot_bound()?;
        let began = Instant::now();
        // HEX-GUEST-34: a W* past any monotonic deadline is refused by type before anything boots.
        if let Some(bound) = bound
            && began.checked_add(bound).is_none()
        {
            return Err(Refusal::Invalid {
                what: "the guest's boot bound",
                why: format!("{bound:?} from now is past a monotonic deadline"),
            });
        }
        let guest: Arc<dyn shards_api::Guest> = self
            .guests
            .boot(&self.cpu_spec()?)
            .map_err(|why| refusal("booting the node's guest", &why))?
            .into();
        let link: LinkOwner =
            own_link(ShardsLink::new(Arc::clone(&guest), bound).booted_at(began))?;
        let client = match Client::connect(triomphe::Arc::clone(&link)) {
            Ok(client) => client,
            Err(why) => {
                // Refused at its end, its VM's stall, or by a refusal of its own: none is left beside the next boot.
                let _ = guest.stop();
                return Err(why);
            }
        };
        self.learn(began.elapsed());
        if let Err(why) = self.record(guest.as_ref()) {
            // Unrecorded, a crash would leave it an orphan: ended, and the boot refused.
            let _ = guest.stop();
            return Err(why);
        }
        *self.booted_binary.lock().map_err(poisoned)? = binary;
        let b = booted_owner(Booted {
            guest: GuestHandle::Shared(guest),
            link,
            client,
            evented: Mutex::new(None),
            owner: None,
            backing: None,
        })?;
        // The new guest's control connection joins the loop as the one before's did.
        self.join(&b)?;
        *booted = Some(triomphe::Arc::clone(&b));
        Ok(b)
    }

    /// Whether `guest` still runs: a Shards guest by its own word, the node VM under shards-init by its run's, which
    /// is reaped through its stop where it ended on its own, its end kept in its row. A node VM whose end can't be read
    /// or kept is refused, typed (the review's C-4): a guest booted beside it would leave its row Ready, locked and
    /// booked, so the next ask tries its end again first.
    fn still_runs(&self, guest: &GuestHandle) -> Result<bool, Refusal> {
        match guest {
            GuestHandle::Node(run) => self.images.node_runs(*run),
            other => Ok(other
                .shards()
                .is_some_and(|guest| matches!(guest.wait_for(Duration::ZERO), Ok(None)))),
        }
    }

    /// Boots the node's guest under shards-init through the image class, and connects to its node through its run's
    /// agent-control endpoint (docs/design/node-under-shards-init.md §4, §5; ROOT, 2026-11-13: one code path).
    /// - **Its run** is minted by the node's store, and it is charged: its receipt is the configured VM's allocation
    ///   under its cost profile, as the direct boot's guest is charged, so no node VM runs uncharged.
    /// - **The image class** boots it and follows it to its init's READY, its node and relay then ready.
    /// - **Its relay** is dialed through a lane of its run's endpoint, which takes this runtime's key alone, at most
    ///   the lanes its node fitted at once, each a guest frame wide each way; its node's hello is its answer.
    /// - **A node that won't greet it** has its run stopped, and the boot is refused in its words.
    fn boot_image(&self, image: &str) -> Result<triomphe::Arc<Booted>, Refusal> {
        let lanes = self.lanes.get().copied().ok_or_else(|| Refusal::Invalid {
            what: "the oci class's guest",
            why: "its node fitted no lanes for its guest under shards-init".to_owned(),
        })?;
        let cost = self.runtime_cost.ok_or_else(|| Refusal::Invalid {
            what: "the oci class's guest",
            why: "its guest under shards-init has no cost profile to charge it by".to_owned(),
        })?;
        let identity = self.control_identity()?;
        let vm = image::NodeVm {
            run: self.authority()?.mint_node_run()?,
            image: image.to_owned(),
            allocation: self.config.checked_runtime_allocation(cost)?,
            shares: self
                .config
                .shares
                .iter()
                .map(|share| shards_api::image::ImageShare {
                    host: share.dir.clone(),
                    at: share.at.clone(),
                    read_only: share.read_only,
                    // Its containers' programs may run from a share, as before Shards' D279 (image channel v41).
                    exec: true,
                    // Its run's own root alone (Shards' D277, image channel v37): Hex's init, the run's own command,
                    // mounts it, and the node image runs no member.
                    members: None,
                })
                .collect(),
            controller: identity.fingerprint(),
            lanes,
        };
        let booted = self.images.boot_node(&vm)?;
        let window = u32::try_from(stream::MAX_FRAME).map_err(|_| {
            Refusal::Broken("a guest frame is wider than a lane's window".to_owned())
        })?;
        let link: LinkOwner = match hex_client::guest::lanes::Lanes::dial(
            &identity,
            booted.control.vm,
            booted.control.address,
            shards_api::control::SocketAdmission { lanes, window },
        )
        .and_then(own_link)
        {
            Ok(link) => link,
            Err(why) => return Err(self.stopped_for(vm.run, why)),
        };
        let client = match Client::connect(triomphe::Arc::clone(&link)) {
            Ok(client) => client,
            Err(why) => return Err(self.stopped_for(vm.run, why)),
        };
        let b = match booted_owner(Booted {
            guest: GuestHandle::Node(vm.run),
            link,
            client,
            evented: Mutex::new(None),
            owner: None,
            backing: None,
        }) {
            Ok(b) => b,
            Err(why) => return Err(self.stopped_for(vm.run, why)),
        };
        // Its control connection joins the loop as a direct boot's does; a node VM whose connection can't join is
        // stopped, so no run is left beside the next boot's.
        if let Err(why) = self.join(&b) {
            return Err(self.stopped_for(vm.run, why));
        }
        Ok(b)
    }

    /// Each node VM's receipt as its record keeps it (joint plan step 39): its run, what its allocation books, where
    /// its run stands and what its guest is admitted at (V2), from its record owner's rows.
    fn node_vm_receipts(&self) -> Result<Vec<hex_api::NodeVmReceipt>, Refusal> {
        let records = self.authority()?.all_node_vms()?;
        let mut receipts = Vec::new();
        receipts
            .try_reserve_exact(records.len())
            .map_err(|_| Refusal::Exhausted {
                what: "node VM receipts",
                bound: records.len(),
            })?;
        for record in records {
            receipts.push(hex_api::NodeVmReceipt {
                run: record.run,
                booking: record.allocation.booking(),
                phase: record.phase,
                guest: record.allocation.guest()?,
            });
        }
        Ok(receipts)
    }

    /// Whether its guest is its node's node VM under shards-init (joint plan step 39): booted through the image class.
    pub(super) fn node_vm_form(&self) -> bool {
        matches!(self.config.boot, Boot::Image(_))
    }

    /// Gives `capacity`, its host's answer for its oci class, its node VM's route and receipts (joint plan step 39;
    /// ROOT, 2026-11-13, V2): one route, NodeVm, for a Linux image of its host's architecture with no GPU, charged
    /// nothing at the host as its node VM's receipt pays the host envelope once; and each node VM's receipt, its guest
    /// the room the scheduler holds that route's boxes to, which the node's ledger books while it boots and runs.
    pub(super) fn add_node_vm_offers(
        &self,
        capacity: &mut hex_api::Capacity,
    ) -> Result<(), Refusal> {
        capacity.check()?;
        if capacity.routes.is_empty() {
            capacity
                .device_routes
                .try_reserve_exact(capacity.devices.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "physical route eligibility",
                    bound: hex_api::MAX_DEVICES,
                })?;
            capacity.device_routes.resize(capacity.devices.len(), 0);
        }
        shared::push_offer(
            capacity,
            hex_api::RouteOffer {
                key: hex_api::RouteKey {
                    class: Class::Oci,
                    image: hex_api::ImagePlatform {
                        os: hex_api::ImageOs::Linux,
                        arch: route::host_platform()?.arch,
                    },
                    execution: hex_api::Execution::NodeVm,
                    gpu: hex_api::GpuEligibility::None,
                },
                cost: None,
                backing: None,
            },
            &[],
        )?;
        capacity.node_vms = self.node_vm_receipts()?;
        capacity.check()
    }

    /// `why` a node VM's boot failed past its READY, its run stopped first; where its stop fails too, both.
    fn stopped_for(&self, run: hex_api::NodeRun, why: Refusal) -> Refusal {
        match self.images.stop_node(run) {
            Ok(_) => why,
            Err(cleanup) => Refusal::Broken(format!("{why}; and stopping its node VM: {cleanup}")),
        }
    }

    /// This runtime's control key, made once from the OS's random numbers.
    fn control_identity(&self) -> Result<Arc<shards_api::control::Identity>, Refusal> {
        let mut kept = self.control.lock().map_err(poisoned)?;
        if let Some(identity) = kept.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity =
            Arc::new(
                shards_api::control::Identity::generate().map_err(|why| Refusal::Os {
                    op: "making the key the node controls its guest under shards-init by",
                    why: why.to_string(),
                })?,
            );
        *kept = Some(Arc::clone(&identity));
        Ok(identity)
    }

    /// The guest if it is booted: a call about a box needs no boot, as there's no box before one.
    fn running(&self) -> Result<Option<triomphe::Arc<Booted>>, Refusal> {
        Ok(self.lock()?.clone())
    }

    /// `call` on the guest that holds box `id`, `snapshot` the call's one read of its record (`books::Routed::Other`).
    fn with<T>(
        &self,
        snapshot: Option<&hex_core::record::BoxRecord>,
        id: &BoxId,
        call: impl FnOnce(&Booted) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let running = self.booted_for_record(snapshot, id)?;
        match running {
            Some(b) => call(&b),
            None => Err(Refusal::NoSuchBox(id.clone())),
        }
    }

    /// Stages `items` for `id`. A copy left by a box the guest's node no longer holds (a host that
    /// ended between a stage and its create) is removed and staged again; one of a box it holds is
    /// refused as [`Refusal::Exists`].
    fn stage(&self, b: &Booted, id: &BoxId, items: &[Item]) -> Result<stage::Staged, Refusal> {
        match stage::stage(b.link.as_ref(), id, items, &files::LIMITS) {
            Err(Refusal::Exists(_)) => match b.client.inspect(id) {
                Err(Refusal::NoSuchBox(_)) => {
                    stage::unstage(b.link.as_ref(), id)?;
                    stage::stage(b.link.as_ref(), id, items, &files::LIMITS)
                }
                Ok(_) => Err(Refusal::Exists(id.clone())),
                Err(why) => Err(why),
            },
            other => other,
        }
    }
}

impl Drop for GuestRuntime {
    fn drop(&mut self) {
        self.finish_warm();
        if let Ok(Some(b)) = self.booted.get_mut().map(Option::take) {
            match &b.guest {
                GuestHandle::Node(run) => self.images.stop_node_told(*run),
                other => {
                    if let Some(guest) = other.shards() {
                        let _ = guest.stop();
                    }
                }
            }
            if matches!(&b.guest, GuestHandle::Shared(_))
                && let Some(path) = &self.record
            {
                let _ = held::remove(path);
            }
        }
    }
}

/// AC-T7.4.1: configured vCPU/memory are ceilings, never host-room substitutes. Disk remains the
/// actual guest's backing writable filesystem; PID plus start fences a replacement guest's pool.
fn guest_capacity(
    mut capacity: hex_api::Capacity,
    config: &GuestConfig,
    fence: String,
) -> Result<hex_api::Capacity, Refusal> {
    capacity.check()?;
    let cpu = u64::from(config.vcpus)
        .checked_mul(1000)
        .ok_or_else(|| Refusal::Broken("guest CPU milli overflow".to_owned()))?;
    let memory = config
        .memory_mib
        .checked_mul(1 << 20)
        .ok_or_else(|| Refusal::Broken("guest memory bytes overflow".to_owned()))?;
    capacity.id = fence;
    capacity.total.cpu_milli = capacity.total.cpu_milli.min(cpu);
    capacity.total.mem_bytes = capacity.total.mem_bytes.min(memory);
    capacity.free.cpu_milli = capacity.free.cpu_milli.min(capacity.total.cpu_milli);
    capacity.free.mem_bytes = capacity.free.mem_bytes.min(capacity.total.mem_bytes);
    capacity.check()?;
    Ok(capacity)
}

impl Runtime for GuestRuntime {
    fn backing_usage(&self, key: &hex_api::BackingKey) -> Result<Usage, Refusal> {
        GuestRuntime::backing_usage(self, key)
    }
    fn initialize_backing(
        &self,
        grant: hex_api::BackingGrant,
    ) -> Result<hex_api::BackingOffer, Refusal> {
        if self.shared_key()? == Some(grant.key) {
            self.initialize_pool(grant)
        } else {
            self.initialize_gpu_pool(grant)
        }
    }

    fn release_backing(&self, key: hex_api::BackingKey) -> Result<hex_api::BackingOffer, Refusal> {
        if self.shared_key()? == Some(key) {
            if self.pool_record(key)?.phase == hex_api::BackingPhase::Released {
                self.acknowledge_shared_release(key)
            } else {
                self.release_pool(key)
            }
        } else {
            self.release_gpu_backing(key)
        }
    }
    fn capacity(&self, class: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        if class == Class::ShardsImage {
            return self.image_capacity().map(Some);
        }
        if !matches!(class, Class::Oci | Class::Shards) {
            return Err(Refusal::Invalid {
                what: "capacity class",
                why: "the guest runs only oci".to_owned(),
            });
        }
        if self.uses_books() && class == Class::Shards {
            return self.native_capacity().map(Some);
        }
        if self.managed_shared() {
            let mut capacity = self.authority()?.capacity()?;
            self.add_offers(&mut capacity, class)?;
            return Ok(Some(capacity));
        }
        let booted = self.guest()?;
        // A node whose guest is a node VM under shards-init answers its oci class on its host's plane, as a node whose
        // guest is shared does, its route its node VM's (joint plan step 39; ROOT, 2026-11-13, V2): never the guest's
        // own pool, which the scheduler would take for its whole node's.
        if class == Class::Oci && matches!(booted.guest, GuestHandle::Node(_)) {
            let mut capacity = self.authority()?.capacity()?;
            self.add_node_vm_offers(&mut capacity)?;
            return Ok(Some(capacity));
        }
        let capacity = booted.client.capacity(Class::Oci)?.ok_or_else(|| {
            Refusal::Broken("the guest node reported no resource pool".to_owned())
        })?;
        // A replacement guest's pool is fenced by its boot: a Shards guest's process and its start, the node VM under
        // shards-init's run.
        let fence = match &booted.guest {
            GuestHandle::Node(run) => format!(
                "node-vm:{}",
                run.bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
            other => {
                let identity = other
                    .shards_for("reading the guest pool identity")?
                    .identity()
                    .map_err(|why| refusal("reading the guest pool identity", &why))?;
                format!("guest:{}:{}", identity.pid, identity.started)
            }
        };
        Ok(Some(guest_capacity(capacity, &self.config, fence)?))
    }

    fn classes(&self) -> Vec<Class> {
        let mut classes = vec![Class::Oci];
        if self.uses_books() && self.runtime_cost.is_some() {
            classes.push(Class::Shards);
        }
        // P2.2: its image class, where its node boots image VMs (`ImageClass::serves`), which a create says otherwise.
        if self.uses_books() && self.images.serves().is_ok() {
            classes.push(Class::ShardsImage);
        }
        classes
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        route::guest_spec(&spec)?;
        if matches!(spec.class, ClassSpec::Process | ClassSpec::Firecracker(_)) {
            return Err(Refusal::Invalid {
                what: "class",
                why: "the node's guest runs the oci class; process boxes are the host's".to_owned(),
            });
        }
        if let ClassSpec::ShardsImage(_) = &spec.class {
            return self.images.create_image(spec);
        }
        // Refused as on any node, naming the field, before anything is mapped, staged or booted.
        if let ClassSpec::Oci(oci) | ClassSpec::Shards(oci) = &spec.class {
            rewrite::check(oci)?;
        }
        if self.owns_record(&spec) {
            return self.create_recorded(spec);
        }
        if !spec.devices.is_empty() || matches!(spec.class, ClassSpec::Shards(_)) {
            return Err(Refusal::Broken(
                "a dedicated guest needs the initialized native record authority".into(),
            ));
        }
        self.create_payload(spec)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.start_image(record),
            books::Routed::Other(snapshot) => {
                let snapshot = snapshot.as_ref();
                self.call_recorded(snapshot, id, true, || {
                    self.with(snapshot, id, |b| b.client.start(id))
                })
            }
        }
    }

    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let snapshot = match self.routed(id)? {
            books::Routed::Retired(record) => return Err(record.retired_refusal()?),
            // A program in an image VM runs in a member it names (`exec_member`).
            books::Routed::Image(_) => return Err(image::unnamed_member(id)),
            books::Routed::Other(snapshot) => snapshot,
        };
        let snapshot = snapshot.as_ref();
        self.call_recorded(snapshot, id, true, || {
            self.with(snapshot, id, |b| b.client.exec(id, program, stdio))
        })
    }

    fn run(
        &self,
        spec: BoxSpec,
        _program: Program,
        _stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_run()?;
        Err(Refusal::Invalid {
            what: "class",
            why: "the guest runs an explicit VM or OCI workload; process boxes are the host's"
                .to_owned(),
        })
    }

    fn member(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.member_image(record, member),
            books::Routed::Other(_) => Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members its caller drives"
                ),
            }),
        }
    }

    /// A program in an image VM's member (Shards' D243, on its run's exec lane); any other box's refused as the trait
    /// refuses it.
    fn exec_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self
                .images
                .exec_member_image(record, member, program, stdio),
            books::Routed::Other(_) => Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members a program runs in"
                ),
            }),
        }
    }

    /// Goal 2's item 3: an image VM's member's next MCP connection (D254); any other box's refused as the trait refuses
    /// it.
    fn mcp(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.mcp_image(record, member),
            books::Routed::Other(_) => Err(Refusal::Invalid {
                what: "mcp",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members with an MCP bridge"
                ),
            }),
        }
    }

    /// Kiln plan step 35's `/runs/control`: an image VM's agent-control link (D238, D254), as both its ends say it; any
    /// other box's refused as the trait refuses it.
    fn link(&self, id: &BoxId) -> Result<hex_api::ControlLink, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.link_image(record),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(_)) => Err(Refusal::Invalid {
                what: "link",
                why: format!(
                    "box {id} isn't a Shards image box: only its run has an agent-control link"
                ),
            }),
        }
    }

    /// Goal 2's item 2: a signal to an image VM's member alone, on its own lane; any other box's refused as the trait
    /// refuses it.
    fn signal_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        signal: Signal,
    ) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.signal_member_image(record, member, signal),
            books::Routed::Other(_) => Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members a signal reaches"
                ),
            }),
        }
    }

    /// Joint plan step 14 (C7): an image VM's guest told its room (D241); a Shards guest VM's awaits its room call, its
    /// guest's init having none from Hex; any other box's refused as the trait refuses it.
    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.room_image(record, room),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(record))
                if matches!(record.spec.class, ClassSpec::Shards(_)) =>
            {
                Err(Refusal::Awaits {
                    needs: hex_api::Upstream::GuestRoom,
                })
            }
            books::Routed::Other(Some(_)) => Err(Refusal::Invalid {
                what: "room",
                why: format!(
                    "box {id} isn't a Shards image box: only its guest holds itself within a room"
                ),
            }),
        }
    }

    /// Joint plan step 14 (C4): an image VM's vCPUs held to `rate` (D247); any other box's refused as the trait
    /// refuses it, a Shards guest VM's CPU being its host's cap.
    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.rate_image(record, rate),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(_)) => Err(Refusal::Invalid {
                what: "rate",
                why: format!(
                    "box {id} isn't a Shards image box: only its VM holds its vCPUs to a rate"
                ),
            }),
        }
    }

    /// Joint plan step 14 (C1): what an image VM costs its host (D241); any other box's refused as the trait refuses it.
    fn host_use(&self, id: &BoxId) -> Result<hex_api::HostUse, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.host_use_image(record),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(_)) => Err(Refusal::Invalid {
                what: "host_use",
                why: format!(
                    "box {id} runs no VM process of its own: only a Shards image VM's run host reads its cost"
                ),
            }),
        }
    }

    /// Kiln plan P5.5b: an image VM's pause, its class's ([`image::ImageClass`]); any other box's refused as the trait
    /// refuses it, naming its class: a Shards guest's oci box among them, its VM its node's.
    fn pause(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.pause_image(record),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(record)) => Err(Refusal::Pause {
                id: id.try_clone()?,
                op: "pause",
                cause: hex_api::PauseCause::Class(record.spec.class.class()),
            }),
        }
    }

    /// Kiln plan P5.5b: an image VM's resume, as its pause is routed.
    fn resume(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.resume_image(record),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(record)) => Err(Refusal::Pause {
                id: id.try_clone()?,
                op: "resume",
                cause: hex_api::PauseCause::Class(record.spec.class.class()),
            }),
        }
    }

    /// Shards' D270 slice 4: an image VM's member given a component, its class's ([`image::ImageClass`]); any other box's
    /// refused as the trait refuses it, naming its class: a Shards guest's oci box among them, its VM its node's.
    fn swap(
        &self,
        id: &BoxId,
        at: &str,
        digest: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        let record = self.swappable(id, "swap")?;
        self.images
            .swap_image(record, at, image::Change::Swap(digest), when)
    }

    /// Shards' D270 slice 4: an image VM's member given its image's own component back, as its swap is routed.
    fn restore(
        &self,
        id: &BoxId,
        at: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        let record = self.swappable(id, "restore")?;
        self.images
            .swap_image(record, at, image::Change::Restore, when)
    }

    /// Shards' D270 slice 4: an image VM's components, as its swap is routed.
    fn components(&self, id: &BoxId) -> Result<Vec<hex_api::ComponentAt>, Refusal> {
        let record = self.swappable(id, "components")?;
        self.images.components_image(record)
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.signal_image(record, signal),
            books::Routed::Other(snapshot) => {
                let snapshot = snapshot.as_ref();
                self.call_recorded(snapshot, id, true, || {
                    self.with(snapshot, id, |b| b.client.signal(id, signal))
                })
            }
        }
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let routed = self.routed(id)?;
        self.inspect_routed(routed, id)
    }

    /// An image box followed to its start's end as its class follows it (kiln's HexVms adapter); every other box as its
    /// inspect answers it, its one read of the node's records carried on.
    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        match self.routed(id)? {
            books::Routed::Image(record) => self.images.started_image(record),
            routed => self.inspect_routed(routed, id),
        }
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        let snapshot = match self.routed(id)? {
            books::Routed::Retired(record) => return Err(record.retired_refusal()?),
            books::Routed::Image(record) => return self.images.usage_image(record),
            books::Routed::Other(snapshot) => snapshot,
        };
        let snapshot = snapshot.as_ref();
        if let Some(usage) = self.native_usage(snapshot, id)? {
            return Ok(usage);
        }
        self.call_recorded(snapshot, id, false, || {
            self.with(snapshot, id, |b| b.client.usage(id))
        })
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        let mut boxes = match self.running()? {
            Some(b) => {
                let mut listed = b.client.list()?;
                for info in &mut listed {
                    self.project_info_in(&b, info)?;
                }
                listed
            }
            None => Vec::new(),
        };
        // Recovery attaches by each immutable receipt rather than by map/catalog order.
        let ids = self.selected_ids()?;
        for id in ids {
            if let Some(b) = self.selected_running(&id)? {
                let listed = b.client.list()?;
                if listed.iter().any(|info| info.id != id) {
                    return Err(Refusal::Broken(
                        "a dedicated guest reports another box".into(),
                    ));
                }
                boxes
                    .try_reserve_exact(listed.len())
                    .map_err(|_| Refusal::Exhausted {
                        what: "guest box listing",
                        bound: crate::MAX_BOXES,
                    })?;
                for info in listed {
                    boxes.push(self.project_info(&b, info)?);
                }
            }
        }
        let images = self.images.image_infos(image::Unread::Unlisted)?;
        // Each oci box its node retired with its node VM (A-6), from its record, as its inspect answers it.
        let retired = self.retired_infos()?;
        boxes
            .try_reserve_exact(images.len().saturating_add(retired.len()))
            .map_err(|_| Refusal::Exhausted {
                what: "guest box listing",
                bound: crate::MAX_BOXES,
            })?;
        boxes.extend(images);
        boxes.extend(retired);
        Ok(boxes)
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => self.delete_retired(&record),
            books::Routed::Image(record) => self.images.delete_image(record),
            books::Routed::Other(Some(record)) if self.owns_record(&record.spec) => {
                self.delete_recorded(&record, id)
            }
            books::Routed::Other(snapshot) => self.delete_payload(snapshot.as_ref(), id),
        }
    }

    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<hex_api::HostUse>, Refusal> {
        match self.routed(id)? {
            books::Routed::Image(record) => self.images.delete_image_used(record),
            books::Routed::Retired(_) | books::Routed::Other(_) => self.delete(id).map(|()| None),
        }
    }
}

impl GuestRuntime {
    /// Joint plan step 14 (D255; ROOT, 2026-11-13): asks the governor of managed box `record`'s VM to hold its vCPUs to
    /// `rate` milli-CPU, through its booted managed guest ([`managed::rate`]). A box whose guest this runtime holds
    /// through no managed guest of its own has no governor of its own to ask: its platform's cap is to hold it, told.
    pub fn managed_rate(
        &self,
        record: &hex_core::record::BoxRecord,
        rate: u64,
    ) -> Result<ManagedRate, Refusal> {
        let id = &record.spec.id;
        let Some(booted) = self.booted_for_record(Some(record), id)? else {
            return Err(Refusal::NoSuchBox(id.try_clone()?));
        };
        match &booted.guest {
            GuestHandle::Managed(guest) => managed::rate(guest, id, rate),
            GuestHandle::Shared(_) | GuestHandle::Node(_) => {
                Ok(ManagedRate::Platform(Refusal::Platform {
                    what: "a managed VM's governor",
                    platform: std::env::consts::OS,
                    why: "its guest is held through no managed guest of its own".to_owned(),
                }))
            }
        }
    }

    /// Joint plan step 14 (D260; ROOT, 2026-11-13): managed box `record`'s VM's vCPUs' guest time, as its VM process
    /// answers its use through its booted managed guest ([`managed::guest_time`]), each held to the VM's size: none apart
    /// for a box whose guest is held through no managed guest of its own, told why.
    pub fn managed_guest_time(
        &self,
        record: &hex_core::record::BoxRecord,
    ) -> Result<ManagedGuestTime, Refusal> {
        let id = &record.spec.id;
        let Some(booted) = self.booted_for_record(Some(record), id)? else {
            return Err(Refusal::NoSuchBox(id.try_clone()?));
        };
        match &booted.guest {
            GuestHandle::Managed(guest) => {
                let vcpus = record
                    .spec
                    .allocation
                    .ok_or_else(|| Refusal::Invalid {
                        what: "a managed VM's guest time",
                        why: format!("box {id} holds no allocation to size its vCPUs by"),
                    })?
                    .vm_size()?
                    .vcpus;
                managed::guest_time(guest, id, vcpus)
            }
            GuestHandle::Shared(_) | GuestHandle::Node(_) => Ok(ManagedGuestTime::Unheld(
                "its guest is held through no managed guest of its own".to_owned(),
            )),
        }
    }

    /// Joint plan step 14 (D255): the period managed box `record`'s VM's governor holds its rate over, as its VM process
    /// last answered: none before its first answer, and none for a box whose guest is held through no managed guest of
    /// its own.
    pub fn managed_rate_period(
        &self,
        record: &hex_core::record::BoxRecord,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        let id = &record.spec.id;
        let Some(booted) = self.booted_for_record(Some(record), id)? else {
            return Err(Refusal::NoSuchBox(id.try_clone()?));
        };
        Ok(match &booted.guest {
            GuestHandle::Managed(guest) => guest.rate_period(),
            GuestHandle::Shared(_) | GuestHandle::Node(_) => None,
        })
    }

    /// Joint plan step 39 (place 3): what node VM `run` costs its host (D241), its node's charge learner's read of it.
    pub fn node_vm_host_use(&self, run: hex_api::NodeRun) -> Result<hex_api::HostUse, Refusal> {
        self.images.host_use_node(run)
    }

    /// Joint plan step 39 (place 3): the period node VM `run`'s governor holds its rate over (D247), its run's own, as
    /// its last rate's answer said it: none before its first (Shards' image channel v31).
    pub fn node_vm_rate_period(
        &self,
        run: hex_api::NodeRun,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        self.images.rate_period_node(run)
    }

    /// Joint plan step 39 (place 3): holds node VM `run`'s vCPUs together to `rate` milli-CPU through its run's rate
    /// (D247), its node's charge learner's cap.
    pub fn node_vm_rate(
        &self,
        run: hex_api::NodeRun,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.images.rate_node(run, rate)
    }

    /// Joint plan step 39 (place 3): stops node VM `run`'s run, its node's charge learner's last resort (C5), its end kept
    /// in its row; the guest it was is booted again at the next ask, its run ended.
    pub fn stop_node_vm(&self, run: hex_api::NodeRun) -> Result<(), Refusal> {
        self.images.stop_node(run).map(|_| ())
    }

    /// Joint plan step 39: tells node VM `run`'s guest its room, `room` bytes past what it holds (D188), its node's charge
    /// learner's word, as an image box's guest is told.
    pub fn node_vm_room(&self, run: hex_api::NodeRun, room: i64) -> Result<(), Refusal> {
        self.images.room_node(run, Some(room)).map(|_| ())
    }

    /// Joint plan step 14 (C1): the period running image box `id`'s VM's governor holds its rate over, its run's own as
    /// its VM said it with its last rate's answer (D191, D247's `ImageGuest::rate_period`; Shards' image channel v31),
    /// none before its first: the window the node's charge learner caps it over. Any other box's refused as
    /// `Runtime::rate` refuses it.
    pub fn rate_period(&self, id: &BoxId) -> Result<Option<std::time::Duration>, Refusal> {
        match self.routed(id)? {
            books::Routed::Retired(record) => Err(record.retired_refusal()?),
            books::Routed::Image(record) => self.images.rate_period_image(record),
            books::Routed::Other(None) => Err(Refusal::NoSuchBox(id.try_clone()?)),
            books::Routed::Other(Some(_)) => Err(Refusal::Invalid {
                what: "rate_period",
                why: format!(
                    "box {id} isn't a Shards image box: only its VM's governor holds a rate over a period"
                ),
            }),
        }
    }

    /// Box `id` as its route says it is, its record read once by the caller.
    fn inspect_routed(&self, routed: books::Routed, id: &BoxId) -> Result<BoxInfo, Refusal> {
        match routed {
            // A retired box says why from its record, with no backing or VM to ask (joint plan step 14).
            books::Routed::Retired(record) => record.retired_info(),
            books::Routed::Image(record) => self.images.inspect_image(record),
            books::Routed::Other(snapshot) => {
                let snapshot = snapshot.as_ref();
                self.call_recorded(snapshot, id, false, || {
                    self.with(snapshot, id, |b| {
                        self.project_info(b, b.client.inspect(id)?)
                    })
                })
            }
        }
    }

    /// What a node VM's guest refusing room for a box its host's pool admitted is told as (V2; ROOT, 2026-11-13: a
    /// basis to fix, never a retry): the guest's refusal, with the host's pool's numbers beside the guest's own.
    fn guest_room_refusal(
        refusal: Refusal,
        pool: hex_api::CapacityResources,
        booked: hex_api::CapacityResources,
        free: Option<hex_api::CapacityResources>,
    ) -> Refusal {
        let guest = free.map_or_else(
            || "its own room unread".to_owned(),
            |free| {
                format!(
                    "{} milli-CPU and {} bytes free by its own answer",
                    free.cpu_milli, free.mem_bytes
                )
            },
        );
        Refusal::Invalid {
            what: "a node VM's guest's room",
            why: format!(
                "its guest refused a box its host's pool admitted: the pool holds {} milli-CPU and {} bytes \
                 for its node VM's boxes, {} milli-CPU and {} bytes booked with this one; its guest has \
                 {guest}, and says: {refusal}",
                pool.cpu_milli, pool.mem_bytes, booked.cpu_milli, booked.mem_bytes
            ),
        }
    }

    /// Box `refused`'s create refusal, its spec routed `NodeVm`, as [`GuestRuntime::guest_room_refusal`] tells it
    /// where its guest refused it for room (Full or Exhausted): the pool its Ready node VM's guest is, what that pool
    /// books of its node VM's oci boxes with this one (their caps, as each record keeps them), and what the guest says
    /// is free. Any other refusal is the guest's as it gave it.
    fn told_guest_refusal(&self, b: &Booted, refused: &BoxSpec, refusal: Refusal) -> Refusal {
        if !matches!(refusal, Refusal::Full(_) | Refusal::Exhausted { .. }) {
            return refusal;
        }
        match self.node_vm_pool(refused) {
            Ok((pool, booked)) => {
                let free = b
                    .client
                    .capacity(Class::Oci)
                    .ok()
                    .flatten()
                    .map(|capacity| capacity.free);
                Self::guest_room_refusal(refusal, pool, booked, free)
            }
            Err(_) => refusal,
        }
    }

    /// Its node VM's pool as its host's scheduler holds it (V2): its Ready node VM's guest, and what the pool books of
    /// its node VM's oci boxes with `refused` (each one's caps as its record keeps them).
    fn node_vm_pool(
        &self,
        refused: &BoxSpec,
    ) -> Result<(hex_api::CapacityResources, hex_api::CapacityResources), Refusal> {
        let books = self.authority()?;
        let pool = books
            .all_node_vms()?
            .into_iter()
            .find(|row| row.phase == hex_api::NodeVmPhase::Ready && row.exit.is_none())
            .map(|row| row.allocation.guest())
            .transpose()?
            .unwrap_or_default();
        let caps = |spec: &BoxSpec| hex_api::CapacityResources {
            cpu_milli: spec.bounds.cpu_max_milli.map_or(0, u64::from),
            mem_bytes: spec.bounds.memory_max.unwrap_or(0),
            disk_bytes: 0,
        };
        let mut booked = caps(refused);
        for record in books.records()?.into_values().flatten() {
            let hosted = record.exit.is_none()
                && record.spec.id != refused.id
                && record
                    .spec
                    .route
                    .is_some_and(|route| route.key.execution == hex_api::Execution::NodeVm);
            if hosted {
                booked = booked.checked_add(caps(&record.spec)).unwrap_or(booked);
            }
        }
        Ok((pool, booked))
    }

    fn create_payload(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        if !spec.devices.is_empty() || matches!(spec.class, ClassSpec::Shards(_)) {
            return Err(Refusal::Broken(
                "dedicated launch requires its canonical transaction".into(),
            ));
        }
        if self.managed_shared() {
            return self.create_shared(spec);
        }
        // Its route, kept to tell a refusal of what its host's pool admitted (V2): the guest sees none.
        let routed = if spec
            .route
            .is_some_and(|route| route.key.execution == hex_api::Execution::NodeVm)
        {
            Some(spec.clone())
        } else {
            None
        };
        let rewritten = self.rewrite_spec(spec)?;
        let b = self.guest()?;
        self.warm_if_changed()?;
        self.create_in(&b, rewritten)
            .map_err(|refusal| match &routed {
                Some(spec) => self.told_guest_refusal(&b, spec, refusal),
                None => refusal,
            })
    }

    /// Deletes box `id` from the guest that holds it, `snapshot` the call's one read of its record.
    fn delete_payload(
        &self,
        snapshot: Option<&hex_core::record::BoxRecord>,
        id: &BoxId,
    ) -> Result<(), Refusal> {
        let Some(b) = self.booted_for_record(snapshot, id)? else {
            return if self.managed_shared() {
                Ok(())
            } else {
                Err(Refusal::NoSuchBox(id.try_clone()?))
            };
        };
        b.client.delete(id)?;
        match stage::unstage(b.link.as_ref(), id) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(why) => Err(why),
        }
    }

    /// The boxes the node's running guest holds under names no readable record of `records` carries: orphans
    /// only a create whose record was torn can have left (T-T7.1.2). None while no guest runs here. A guest
    /// that can't answer is its refusal, and proves no box gone.
    pub fn orphans(&self, records: &crate::devices::Records) -> Result<Vec<BoxId>, Refusal> {
        let Some(b) = self.running()? else {
            return Ok(Vec::new());
        };
        let listed = b.client.list()?;
        let mut orphans = Vec::new();
        orphans
            .try_reserve_exact(listed.len())
            .map_err(|_| Refusal::Exhausted {
                what: "orphaned guest boxes",
                bound: listed.len(),
            })?;
        orphans.extend(
            listed
                .into_iter()
                .map(|info| info.id)
                .filter(|id| !matches!(records.get(id), Some(Ok(_)))),
        );
        Ok(orphans)
    }

    /// Deletes orphan `id` ([`GuestRuntime::orphans`]) through the running guest, then its staged copy: a box
    /// the guest still holds keeps its root. One gone already is deleted, and so is any with no guest running
    /// here to hold it.
    pub fn delete_orphan(&self, id: &BoxId) -> Result<(), Refusal> {
        let Some(b) = self.running()? else {
            return Ok(());
        };
        match b.client.delete(id) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => {}
            Err(why) => return Err(why),
        }
        match stage::unstage(b.link.as_ref(), id) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(why) => Err(why),
        }
    }

    fn rewrite_spec(&self, mut spec: BoxSpec) -> Result<rewrite::Rewritten, Refusal> {
        // The guest's native runtime executes the same OCI payload. The backend-explicit
        // class belongs to the host endpoint and is retained by its durable BoxRecord.
        // Host allocation/overhead is retained only by the canonical host record;
        // the inner native OCI runtime enforces the unchanged workload bounds.
        spec.allocation = None;
        spec.route = None;
        spec.class = match spec.class {
            ClassSpec::Shards(oci) => ClassSpec::Oci(oci),
            other => other,
        };
        let known = match &spec.class {
            ClassSpec::Oci(oci) => match &oci.rootfs {
                Rootfs::Layers(paths) => self.digests.lock().map_err(poisoned)?.known(paths),
                Rootfs::Dir(_) => rewrite::Known::new(),
            },
            _ => rewrite::Known::new(),
        };
        rewrite::rewrite(spec, &self.config.shares, &self.config.staged, &known)
    }

    fn create_in(&self, b: &Booted, rewritten: rewrite::Rewritten) -> Result<BoxInfo, Refusal> {
        let id = rewritten.spec.id.try_clone()?;
        // An id the guest's node already holds is refused as the node refuses it, before anything is
        // staged: staging first would send a box's whole root only to be refused (CLAUDE.md §3).
        match b.client.inspect(&id) {
            Ok(_) => return Err(Refusal::Exists(id)),
            Err(Refusal::NoSuchBox(_)) => {}
            Err(why) => return Err(why),
        }
        let staged = self.stage(b, &id, &rewritten.items)?;
        self.digests.lock().map_err(poisoned)?.learn(staged);
        let info = b.client.create(rewritten.spec).inspect_err(|_| {
            // The copy goes with the create that failed; a removal that fails leaves it for the
            // next stage of this id, which removes it (above).
            let _ = stage::unstage(b.link.as_ref(), &id);
        })?;
        self.project_info(b, info)
    }
}

/// Which of a [`WithGuest`]'s runtimes takes a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The host's own runtime.
    Host,
    /// The guest's.
    Guest,
}

/// The routing by class of §1: `process` boxes the host's, every `oci` box the guest's, for a host that
/// runs no `oci` class of its own (macOS).
pub fn by_class<P>(_host: &P, class: &ClassSpec) -> Result<Side, Refusal> {
    Ok(match class {
        ClassSpec::Process => Side::Host,
        ClassSpec::Oci(_)
        | ClassSpec::Bundle(_)
        | ClassSpec::Shards(_)
        | ClassSpec::ShardsImage(_) => Side::Guest,
        ClassSpec::Firecracker(_) => {
            return Err(Refusal::Invalid {
                what: "runtime backend",
                why: "Firecracker requires its initialized backend endpoint".into(),
            });
        }
    })
}

/// A node's runtime where Linux images are its guest's: the host's own runtime (`MacRuntime`,
/// `WinRuntime`) takes what its `route` says it runs, the guest the rest, one box id naming one box across
/// both (CLAUDE.md §8). macOS routes by class ([`by_class`]); Windows runs its own images through HCS and
/// sends only Linux images to the guest (`WinRuntime::oci_side`).
pub struct WithGuest<P> {
    process: P,
    oci: GuestRuntime,
    route: fn(&P, &ClassSpec) -> Result<Side, Refusal>,
}

impl<P: Runtime> WithGuest<P> {
    /// The configured backend route, shared with recovery; it never selects a device.
    pub fn route(&self, class: &ClassSpec) -> Result<Side, Refusal> {
        if matches!(class, ClassSpec::Shards(_) | ClassSpec::ShardsImage(_)) {
            return Ok(Side::Guest);
        }
        (self.route)(&self.process, class)
    }

    /// Makers and recovery resolve the same immutable host receipt before any
    /// native operation or method turn. A layer path is not image OS evidence.
    pub fn route_for(&self, spec: &BoxSpec) -> Result<Side, Refusal> {
        let host = route::host_platform()?;
        if let Some(side) = route::committed_side(spec, host)? {
            if side == Side::Host && !self.process.classes().contains(&spec.backend_class()?) {
                return Err(route::invalid(
                    "the committed native backend has not been initialized",
                ));
            }
            return Ok(side);
        }
        if spec.class.class() == Class::Oci
            && host.os != hex_api::ImageOs::Macos
            && self.process.classes().contains(&Class::Oci)
        {
            return Err(route::invalid(
                "legacy OCI is ambiguous between native and Linux guest execution",
            ));
        }
        self.route(&spec.class)
    }
    pub fn bind_books(&self, books: Weak<dyn crate::devices::Books>) -> Result<(), Refusal> {
        self.oci.bind_books(books)
    }

    /// The host's own runtime, which runs the `process` class: a node's recovery takes its boxes back
    /// through it (native plan step 8).
    pub fn host(&self) -> &P {
        &self.process
    }

    /// The guest's runtime, which runs the `oci` class: a node's recovery asks it which boxes it still holds.
    pub fn guest(&self) -> &GuestRuntime {
        &self.oci
    }

    /// Construct the route before the existing Arc<Recorded<WithGuest<P>>> is
    /// available. This performs no boot: register that SAME owner's Weak Books
    /// and initialized cost/provider before dedicated admission or recovery.
    pub fn configured(
        process: P,
        oci: GuestRuntime,
        route: fn(&P, &ClassSpec) -> Result<Side, Refusal>,
    ) -> Self {
        Self {
            process,
            oci,
            route,
        }
    }

    /// The node's runtime, routed by class ([`by_class`]), its guest booted before it returns, so it accepts
    /// no box before that boot ([`GuestRuntime::boot`]); a guest that can't boot refuses the node by type.
    pub fn new(process: P, oci: GuestRuntime) -> Result<Self, Refusal> {
        Self::routing(process, oci, by_class)
    }

    /// Routed by `route`, which a box it refuses is refused by; its guest booted before it returns, as
    /// [`WithGuest::new`]'s is.
    pub fn routing(
        process: P,
        oci: GuestRuntime,
        route: fn(&P, &ClassSpec) -> Result<Side, Refusal>,
    ) -> Result<Self, Refusal> {
        oci.boot()?;
        Ok(Self::configured(process, oci, route))
    }

    /// The runtime that makes a box of `spec`, as its `route` says. An id is one box's across both classes: the other
    /// side holding it refuses the box as [`Refusal::Exists`].
    fn maker(&self, spec: &BoxSpec) -> Result<&dyn Runtime, Refusal> {
        let (this, other): (&dyn Runtime, &dyn Runtime) = match self.route_for(spec)? {
            Side::Host => (&self.process, &self.oci),
            Side::Guest => (&self.oci, &self.process),
        };
        if self.oci.uses_books() {
            return Ok(this);
        }
        match other.inspect(&spec.id) {
            Ok(_) => Err(Refusal::Exists(spec.id.clone())),
            Err(Refusal::NoSuchBox(_)) => Ok(this),
            Err(why) => Err(why),
        }
    }

    /// The runtime holding `id`, or [`Refusal::NoSuchBox`]: the host's, asked first as it answers
    /// without a guest, then the guest's.
    fn holder(&self, id: &BoxId) -> Result<&dyn Runtime, Refusal> {
        if self.oci.uses_books() {
            let record = self
                .oci
                .recorded_box(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            return Ok(match self.route_for(&record.spec)? {
                Side::Host => &self.process,
                Side::Guest => &self.oci,
            });
        }
        match self.process.inspect(id) {
            Ok(_) => Ok(&self.process),
            Err(Refusal::NoSuchBox(_)) => match self.oci.inspect(id) {
                Ok(_) => Ok(&self.oci),
                Err(why) => Err(why),
            },
            Err(why) => Err(why),
        }
    }
}

impl<P: Runtime> Runtime for WithGuest<P> {
    fn backing_usage(&self, key: &hex_api::BackingKey) -> Result<Usage, Refusal> {
        self.oci.backing_usage(key)
    }
    fn initialize_backing(
        &self,
        grant: hex_api::BackingGrant,
    ) -> Result<hex_api::BackingOffer, Refusal> {
        self.oci.initialize_backing(grant)
    }

    fn release_backing(&self, key: hex_api::BackingKey) -> Result<hex_api::BackingOffer, Refusal> {
        self.oci.release_backing(key)
    }
    fn capacity(&self, class: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        match class {
            // A host whose process class answers no pool of its own (macOS's) answers it on its host's plane, the one
            // its oci answer is on, with its native process route (V2; ROOT, 2026-11-13): so its process boxes keep a
            // route beside its guest's, as Windows' own answer keeps one.
            Class::Process => match self.process.capacity(class)? {
                Some(report) => Ok(Some(report)),
                None if self.oci.uses_books() => {
                    let mut capacity = self.oci.authority()?.capacity()?;
                    crate::devices::native_routes(
                        &mut capacity,
                        Class::Process,
                        route::host_platform()?,
                    )?;
                    Ok(Some(capacity))
                }
                None => Ok(None),
            },
            Class::Oci => {
                // Windows runs its own OCI images as well as guest Linux images. Its initialized
                // native pool (including exact device holders) must not disappear behind the guest.
                if self.process.classes().contains(&Class::Oci)
                    && let Some(mut native) = self.process.capacity(class)?
                {
                    if self.oci.managed_shared() {
                        if !native.devices.is_empty() {
                            let actual = self.oci.authority()?.capacity()?;
                            actual.check()?;
                            crate::devices::normalize_capacity_devices(
                                &mut native.devices,
                                &actual.devices,
                            )?;
                        }
                        self.oci.add_offers(&mut native, class)?;
                    } else if self.oci.node_vm_form() {
                        // Its Linux images go to its node VM, beside its own (Windows'): its route and receipts
                        // with them (joint plan step 39, V2), so its scheduler sees the node VM.
                        self.oci.add_node_vm_offers(&mut native)?;
                    }
                    return Ok(Some(native));
                }
                self.oci.capacity(class)
            }
            Class::Shards | Class::ShardsImage => self.oci.capacity(class),
            Class::Firecracker => self.process.capacity(class),
        }
    }

    fn classes(&self) -> Vec<Class> {
        let mut classes = vec![Class::Process, Class::Oci];
        let guest = self.oci.classes();
        for class in [Class::Shards, Class::ShardsImage] {
            if guest.contains(&class) {
                classes.push(class);
            }
        }
        classes
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        spec.check()?;
        self.maker(&spec)?.create(spec)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.holder(id)?.start(id)
    }

    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.holder(id)?.exec(id, program, stdio)
    }

    /// A run's box is a `process` box ([`BoxSpec::check_run`]), made where a create of its spec would be.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        spec.check()?;
        spec.check_run()?;
        self.maker(&spec)?.run(spec, program, stdio)
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.holder(id)?.signal(id, signal)
    }

    fn member(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        self.holder(id)?.member(id, member)
    }

    fn mcp(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        self.holder(id)?.mcp(id, member)
    }

    fn exec_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.holder(id)?.exec_member(id, member, program, stdio)
    }

    fn link(&self, id: &BoxId) -> Result<hex_api::ControlLink, Refusal> {
        self.holder(id)?.link(id)
    }

    fn signal_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        signal: Signal,
    ) -> Result<(), Refusal> {
        self.holder(id)?.signal_member(id, member, signal)
    }

    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        self.holder(id)?.room(id, room)
    }

    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.holder(id)?.rate(id, rate)
    }

    fn host_use(&self, id: &BoxId) -> Result<hex_api::HostUse, Refusal> {
        self.holder(id)?.host_use(id)
    }

    /// The runtime holding the box pauses it (kiln plan P5.5b), as it answers `host_use`: an image VM's its guest
    /// runtime's class, a process box's its host's, which pauses none.
    fn pause(&self, id: &BoxId) -> Result<(), Refusal> {
        self.holder(id)?.pause(id)
    }

    fn resume(&self, id: &BoxId) -> Result<(), Refusal> {
        self.holder(id)?.resume(id)
    }

    /// The runtime holding the box swaps its member's component (Shards' D270 slice 4), as it pauses it: an image VM's
    /// its guest runtime's class, a process box's its host's, which has none.
    fn swap(
        &self,
        id: &BoxId,
        at: &str,
        digest: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        self.holder(id)?.swap(id, at, digest, when)
    }

    fn restore(
        &self,
        id: &BoxId,
        at: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        self.holder(id)?.restore(id, at, when)
    }

    fn components(&self, id: &BoxId) -> Result<Vec<hex_api::ComponentAt>, Refusal> {
        self.holder(id)?.components(id)
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.holder(id)?.inspect(id)
    }

    /// The runtime holding the box answers it, as it answers `host_use` (the review's B-6): an image VM's start
    /// followed to its end, never the trait's inspect at once.
    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.holder(id)?.started(id)
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.holder(id)?.usage(id)
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        let mut all = self.process.list()?;
        all.extend(self.oci.list()?);
        Ok(all)
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.holder(id)?.delete(id)
    }

    /// The runtime holding the box deletes it and answers its VM's final use (the review's B-6), never the trait's
    /// plain delete answering none, read as a run that never started.
    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<hex_api::HostUse>, Refusal> {
        self.holder(id)?.delete_with_host_use(id)
    }
}

#[cfg(test)]
mod tests;
