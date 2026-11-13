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
mod managed;
#[cfg(any(test, feature = "native-trusted-entry-measurement"))]
pub(crate) mod native_fixture;
mod observer;
mod physical;
mod pool;
pub mod rewrite;
mod route;
pub mod selected;
mod shared;
pub mod shares;

pub use image::{ImageWaits, host_images};
pub use observer::config::{
    ENV_GPU_OBSERVER_FILE, ENV_GPU_OBSERVER_SHA256, GpuArtifact, GpuArtifactRole, GpuObserver,
    GpuObserverInput,
};

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, ClassSpec, Proc, Program, Refusal, Rootfs, Runtime, Signal,
    Stdio, Usage,
};
#[cfg(test)]
use hex_client::guest::Link;
use hex_client::guest::shards::{ShardsLink, refusal};
use hex_client::guest::stage::{self, Item};
use hex_client::guest::{Client, LinkOwner, own_link};
use hex_core::stream::{self, files};

use shares::Share;

/// The existing evented connection owner, shared without an unchecked new Arc.
pub type EventedClient = triomphe::Arc<Client>;

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
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Enable Shards' existing shared Venus GPU; physical reservations still require mapping.
    pub gpu: bool,
    pub shares: Vec<Share>,
    /// Where the guest's relay stages each box's root: [`stage::STAGED`], the guest's tmpfs.
    pub staged: String,
}

/// The environment variables a node reads its guest from.
pub const ENV_SHARDS_BIN: &str = "SHARDS_BIN";
pub const ENV_KERNEL: &str = "HEX_GUEST_KERNEL";
pub const ENV_INITRD: &str = "HEX_GUEST_INITRD";
pub const ENV_CPUS: &str = "HEX_GUEST_CPUS";
pub const ENV_MEMORY_MIB: &str = "HEX_GUEST_MEMORY_MIB";
/// Explicit shared Venus enable; absent preserves the CPU-only guest configuration.
pub const ENV_GPU: &str = "HEX_GUEST_GPU";

impl GuestConfig {
    /// The guest named by the environment ([`GuestConfig::from_vars`] over the process's).
    pub fn from_env() -> Result<Self, Refusal> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// The guest named by `var`: `SHARDS_BIN`, `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`,
    /// `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`, each needed (Shards is never taken from `PATH`
    /// unasked, and the guest's vCPUs and memory are the node's to give from its budget, §1, so none is
    /// guessed), and `HEX_GUEST_SHARES` (else [`shares::defaults`]). Every one missing is named in one
    /// refusal, read before anything boots, so an operator fixes them in one go. Shards refuses vCPUs or
    /// memory past the host's (its `Bounds`) when it boots.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, Refusal> {
        let needed = [
            ENV_SHARDS_BIN,
            ENV_KERNEL,
            ENV_INITRD,
            ENV_CPUS,
            ENV_MEMORY_MIB,
        ];
        let missing: Vec<&str> = needed
            .into_iter()
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
        Ok(Self {
            binary: path(ENV_SHARDS_BIN),
            kernel: path(ENV_KERNEL),
            initrd: path(ENV_INITRD),
            vcpus: number(&var, ENV_CPUS)?,
            memory_mib: number(&var, ENV_MEMORY_MIB)?,
            gpu: match var(ENV_GPU) {
                None => false,
                Some(value) if value == "false" => false,
                Some(value) if value == "true" => true,
                Some(_) => {
                    return Err(Refusal::Invalid {
                        what: ENV_GPU,
                        why: "expected true or false".to_owned(),
                    });
                }
            },
            shares: shares::from_var(var(shares::ENV))?,
            staged: stage::STAGED.to_owned(),
        })
    }

    /// The spec Shards boots: the initramfs's `/init` is `hex`, which as PID 1 is the guest's init
    /// whatever words the kernel passes it (HEX-GUEST-12); its command line the serial console, a panic
    /// resetting the guest so the host sees it end, and `hex.shares=` naming each share's mode in tag
    /// order, none when there are none (HEX-GUEST-11). The guest is reached on the channel's three
    /// ports alone.
    pub fn spec(&self) -> shards_api::GuestSpec {
        shards_api::GuestSpec {
            kernel: self.kernel.clone(),
            init: shards_api::Init::Initrd(self.initrd.clone()),
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
        }
    }
}

impl GuestConfig {
    fn cmdline_parts(&self, mut emit: impl FnMut(&str)) {
        // `panic=-1`: a negative timeout reboots at once on a panic (kernel-parameters.txt, `panic=`;
        // kernel/panic.c `panic_timeout`), so the VM ends and the host sees it rather than a guest that
        // hangs.
        emit("console=ttyS0 panic=-1");
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
        Ok(shards_api::GuestSpec {
            kernel: path(&self.kernel)?,
            init: shards_api::Init::Initrd(path(&self.initrd)?),
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
}

impl std::ops::Deref for GuestHandle {
    type Target = dyn shards_api::Guest;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Shared(guest) => guest.as_ref(),
            Self::Managed(guest) => guest,
        }
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

// The guest's records live beside the link whose first dial their boot record bounds.
pub use hex_client::guest::shards::{CLASS, recorded, recorded_boot, seed_in, slowest_in};

/// The first dial's bound before this node has seen a boot ([`GuestRuntime::first_dial_bound`] with nothing
/// learned): the managed helper's control patience at the node's start, and the bound the macOS CPU
/// response's producer measures up to (hex-cli's example `macos_cpu_response`, AC-T7.4.1).
pub fn recorded_dial_bound() -> Result<Duration, Refusal> {
    dial_bound(Duration::ZERO)
}

/// Twice (the margin) the slowest of the record's boot and `seen`, the slowest boot this node has seen.
fn dial_bound(seen: Duration) -> Result<Duration, Refusal> {
    Ok(seen.max(recorded_boot()?).saturating_mul(2))
}

/// The word that tells a Shards guest its boot's deadline on its kernel command line (AC-T7.4.1), as a Firecracker
/// guest is told its own (`hex.firecracker-ready-ms`): `hex.ready-ms=` and what is left of the boot's bound as it
/// boots, in whole milliseconds rounded up, so the guest, counting from its kernel's start, ends no sooner than its
/// host. Guest init hands it to the relay, whose calls to its node before the guest is ready take it as their
/// caller's (hex-cli's `guest`); a guest told none keeps their learned bounds.
pub const READY_MS: &str = "hex.ready-ms";

/// Tells the guest booted with `cmdline` that `left` is what remains of its boot's deadline ([`READY_MS`]), within the
/// kernel's command-line bound; refused where nothing is left, the word is there already, or init's arguments (`--`)
/// begin, past which no word is the kernel's. Public for the guest-cost producer, which boots its guests as the node
/// does (AC-T7.4.1).
pub fn tell_ready(cmdline: &mut String, left: Duration) -> Result<(), Refusal> {
    let invalid = |why: &str| Refusal::Invalid {
        what: "the guest's boot deadline",
        why: why.to_owned(),
    };
    let millis = left.as_nanos().div_ceil(1_000_000);
    if millis == 0 {
        return Err(invalid("nothing of it is left to boot in"));
    }
    if cmdline
        .split_whitespace()
        .any(|word| word == "--" || word.starts_with(READY_MS))
    {
        return Err(invalid(
            "the command line names it already, or passes init its arguments",
        ));
    }
    let word = format!(" {READY_MS}={millis}");
    let exhausted = Refusal::Exhausted {
        what: "guest kernel command line",
        bound: gpu::CMDLINE_BYTES,
    };
    if cmdline
        .len()
        .checked_add(word.len())
        .is_none_or(|len| len >= gpu::CMDLINE_BYTES)
    {
        return Err(exhausted);
    }
    cmdline
        .try_reserve_exact(word.len())
        .map_err(|_| exhausted)?;
    cmdline.push_str(&word);
    Ok(())
}

/// Boots of one guest: the first, and one more where the first hung alive past its bound, as a hung
/// boot has another guest to try before its caller is refused.
const BOOTS: usize = 2;

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
    /// The slowest boot to ready this node has seen, the learned half of the first dial's bound.
    slowest: Mutex<Duration>,
    /// Guests stopped for hanging alive past the bound, never ready.
    retired: AtomicU64,
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
    native_paths: Option<shards_api::RuntimePaths>,
    shared_pool: Mutex<Option<hex_api::BackingKey>>,
    gpu_observer: Option<GpuObserver>,
    /// What joins each guest's evented connection to the node's loop ([`GuestRuntime::join_loop`]).
    joins: Mutex<Option<Arc<dyn Joins>>>,
    /// Shards' image boot, for the Shards class on Agentfile images ([`GuestRuntime::with_images`]).
    image: Option<image::ImageBoot>,
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
    fn cpu_spec(&self) -> shards_api::GuestSpec {
        let mut spec = self.config.spec();
        spec.gpu = false;
        spec
    }
    /// The class over `guests`, booting `config`'s guest when first asked. A boot's first dial waits at
    /// most [`Self::first_dial_bound`] for the guest to listen ([`ShardsLink`]).
    pub fn new(guests: Arc<dyn shards_api::Guests>, config: GuestConfig) -> Self {
        Self {
            guests,
            config,
            booted: Mutex::new(None),
            slowest: Mutex::new(Duration::ZERO),
            retired: AtomicU64::new(0),
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
            image: None,
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
    /// its own; one that runs and won't answer is stopped first, so none is left beside the next. A guest
    /// that can't be told gone or taken back refuses the node by type, naming why, rather than boot
    /// another beside one that may run.
    fn attach_recorded(&self) -> Result<(), Refusal> {
        let Some(path) = &self.record else {
            return Ok(());
        };
        let Some(identity) = held::read(path)? else {
            return Ok(());
        };
        let guest: Arc<dyn shards_api::Guest> =
            match self.guests.attach(&identity, &self.cpu_spec()) {
                Ok(guest) => guest.into(),
                Err(shards_api::Refusal::Gone { .. } | shards_api::Refusal::Ended(_)) => {
                    return held::remove(path);
                }
                Err(why) => return Err(refusal("taking back the node's guest", &why)),
            };
        let link: LinkOwner = own_link(ShardsLink::new(
            Arc::clone(&guest),
            self.first_dial_bound()?,
        ))?;
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

    /// Opens booted guest `b`'s evented connection and joins it to the loop, where one is given.
    fn join(&self, b: &Booted) -> Result<(), Refusal> {
        self.join_with_deadline(b, None)
    }

    fn join_before(&self, b: &Booted, budget: managed::Budget) -> Result<(), Refusal> {
        budget.remaining()?;
        self.join_with_deadline(b, Some(budget.deadline()))?;
        budget.remaining()?;
        Ok(())
    }

    fn join_with_deadline(&self, b: &Booted, deadline: Option<Instant>) -> Result<(), Refusal> {
        let joins = self.joins.lock().map_err(poisoned)?.clone();
        let Some(joins) = joins else {
            return Ok(());
        };
        let (token, room) = joins.reserve()?;
        let (client, pump) = match deadline {
            Some(at) => Client::connect_evented_before(triomphe::Arc::clone(&b.link), room, at)?,
            None => Client::connect_evented(triomphe::Arc::clone(&b.link), room)?,
        };
        let client = evented_owner(client)?;
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(Refusal::os(
                "joining the guest within its original create budget",
                std::io::ErrorKind::TimedOut,
            ));
        }
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
        let mut spec = least.spec();
        let guests = Arc::clone(&self.guests);
        let bound = self.first_dial_bound()?;
        tell_ready(&mut spec.cmdline, bound)?;
        let thread = std::thread::Builder::new()
            .name("hex-guest-warm".to_owned())
            .spawn(move || {
                // Ready, so its VM process ran too; then gone. A warm-up that fails leaves the next boot to
                // pay the first run, as it would have.
                if let Ok(guest) = guests.boot(&spec) {
                    let _ = guest.connect(stream::CONTROL_PORT, bound);
                    let _ = guest.stop();
                }
            })
            .map_err(|e| Refusal::os("starting a guest warm-up", e))?;
        warm.identity = Some(now);
        warm.thread = Some(thread);
        Ok(())
    }

    /// Waits for a warm-up in flight to end: its boot and dial are bounded by the first dial's bound.
    fn finish_warm(&self) {
        let thread = self.warm.lock().ok().and_then(|mut w| w.thread.take());
        if let Some(t) = thread {
            let _ = t.join();
        }
    }

    /// How long a boot may take to be ready (the lead's rule for learned waits): twice (the margin) the slowest of
    /// the record's and this node's own, so it only rises; before any boot here, twice the record's.
    pub fn first_dial_bound(&self) -> Result<Duration, Refusal> {
        dial_bound(*self.slowest.lock().map_err(poisoned)?)
    }

    /// A boot to ready this node has seen, taking `took`: the bound rises with it, never falls.
    pub fn learn(&self, took: Duration) {
        if let Ok(mut slowest) = self.slowest.lock() {
            *slowest = (*slowest).max(took);
        }
    }

    /// Guests stopped because they stayed alive past the bound without being ready.
    pub fn retired(&self) -> u64 {
        self.retired.load(Ordering::Relaxed)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<triomphe::Arc<Booted>>>, Refusal> {
        self.booted.lock().map_err(poisoned)
    }

    /// The guest, booted now if it isn't, or booted again if it ended (every box in it ended with it).
    fn guest(&self) -> Result<triomphe::Arc<Booted>, Refusal> {
        if self.managed_shared() {
            return self.shared_running();
        }
        let mut booted = self.lock()?;
        if let Some(b) = booted.as_ref() {
            match b.guest.wait_for(Duration::ZERO) {
                Ok(None) => return Ok(triomphe::Arc::clone(b)),
                Ok(Some(_)) | Err(_) => *booted = None,
            }
        }
        // Refused by name before anything boots where Shards has no backend (§5).
        self.guests
            .backend()
            .map_err(|why| refusal("finding Shards' backend", &why))?;
        let binary = binary_identity(&self.config.binary);
        let mut hung = Vec::with_capacity(BOOTS);
        for _ in 0..BOOTS {
            let bound = self.first_dial_bound()?;
            let began = Instant::now();
            // HEX-GUEST-34: the boot and its first dial spend one deadline: a boot that spends it is refused
            // by type before a dial, never left a zero patience, whose answer Shards awaits without a bound.
            let ready_by = began.checked_add(bound).ok_or_else(|| Refusal::Invalid {
                what: "the guest's boot bound",
                why: format!("{bound:?} from now is past a monotonic deadline"),
            })?;
            // AC-T7.4.1: the guest is told what is left of it, which its relay's hello to its node takes.
            let mut spec = self.cpu_spec();
            tell_ready(
                &mut spec.cmdline,
                ready_by.saturating_duration_since(Instant::now()),
            )?;
            let guest: Arc<dyn shards_api::Guest> = self
                .guests
                .boot(&spec)
                .map_err(|why| refusal("booting the node's guest", &why))?
                .into();
            let link: LinkOwner = own_link(ShardsLink::new(Arc::clone(&guest), bound))?;
            match Client::connect_before(triomphe::Arc::clone(&link), ready_by) {
                Ok(client) => {
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
                    return Ok(b);
                }
                Err(why) => {
                    // Alive and not ready within the bound: a hang, retired for another boot. Ended,
                    // or refused otherwise, the boot's failure is its caller's.
                    let alive = matches!(guest.wait_for(Duration::ZERO), Ok(None));
                    let _ = guest.stop();
                    if !alive {
                        return Err(why);
                    }
                    self.retired.fetch_add(1, Ordering::Relaxed);
                    hung.push(format!("not ready within {bound:?}: {why}"));
                }
            }
        }
        Err(Refusal::Os {
            op: "booting the node's guest",
            why: format!(
                "{BOOTS} guests booted, each alive and not ready within twice the slowest boot to \
                 ready seen (bench/native/guest/boot-ready/RECORD.md): {}",
                hung.join("; ")
            ),
        })
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
            let _ = b.guest.stop();
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
    identity: &shards_api::Identity,
) -> Result<hex_api::Capacity, Refusal> {
    capacity.check()?;
    let cpu = u64::from(config.vcpus)
        .checked_mul(1000)
        .ok_or_else(|| Refusal::Broken("guest CPU milli overflow".to_owned()))?;
    let memory = config
        .memory_mib
        .checked_mul(1 << 20)
        .ok_or_else(|| Refusal::Broken("guest memory bytes overflow".to_owned()))?;
    capacity.id = format!("guest:{}:{}", identity.pid, identity.started);
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
        let capacity = booted.client.capacity(Class::Oci)?.ok_or_else(|| {
            Refusal::Broken("the guest node reported no resource pool".to_owned())
        })?;
        let identity = booted
            .guest
            .identity()
            .map_err(|why| refusal("reading the guest pool identity", &why))?;
        let capacity = guest_capacity(capacity, &self.config, &identity)?;
        Ok(Some(capacity))
    }

    fn classes(&self) -> Vec<Class> {
        if self.uses_books() && self.runtime_cost.is_some() {
            vec![Class::Oci, Class::Shards]
        } else {
            vec![Class::Oci]
        }
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
            return self.create_image(spec);
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
            books::Routed::Image(record) => self.start_image(record),
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
            // A member's program is the agent-control endpoint's (joint plan step 11).
            books::Routed::Image(_) => {
                return Err(Refusal::Awaits {
                    needs: hex_api::Upstream::MemberExec,
                });
            }
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
            books::Routed::Image(record) => self.member_image(record, member),
            books::Routed::Other(_) => Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members its caller drives"
                ),
            }),
        }
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Image(record) => self.signal_image(record, signal),
            books::Routed::Other(snapshot) => {
                let snapshot = snapshot.as_ref();
                self.call_recorded(snapshot, id, true, || {
                    self.with(snapshot, id, |b| b.client.signal(id, signal))
                })
            }
        }
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        match self.routed(id)? {
            books::Routed::Image(record) => self.inspect_image(record),
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

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        let snapshot = match self.routed(id)? {
            books::Routed::Image(record) => return self.usage_image(record),
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
        let images = self.image_infos()?;
        boxes
            .try_reserve_exact(images.len())
            .map_err(|_| Refusal::Exhausted {
                what: "guest box listing",
                bound: crate::MAX_BOXES,
            })?;
        boxes.extend(images);
        Ok(boxes)
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.routed(id)? {
            books::Routed::Image(record) => self.delete_image(record),
            books::Routed::Other(Some(record)) if self.owns_record(&record.spec) => {
                self.delete_recorded(&record, id)
            }
            books::Routed::Other(snapshot) => self.delete_payload(snapshot.as_ref(), id),
        }
    }
}

impl GuestRuntime {
    fn create_payload(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        if !spec.devices.is_empty() || matches!(spec.class, ClassSpec::Shards(_)) {
            return Err(Refusal::Broken(
                "dedicated launch requires its canonical transaction".into(),
            ));
        }
        if self.managed_shared() {
            return self.create_shared(spec);
        }
        let rewritten = self.rewrite_spec(spec)?;
        let b = self.guest()?;
        self.warm_if_changed()?;
        self.create_in(&b, rewritten)
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
            Class::Process => self.process.capacity(class),
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
                    }
                    return Ok(Some(native));
                }
                self.oci.capacity(class)
            }
            Class::Shards => self.oci.capacity(class),
            Class::Firecracker => self.process.capacity(class),
        }
    }

    fn classes(&self) -> Vec<Class> {
        if self.oci.classes().contains(&Class::Shards) {
            vec![Class::Process, Class::Oci, Class::Shards]
        } else {
            vec![Class::Process, Class::Oci]
        }
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

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.holder(id)?.inspect(id)
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
}

#[cfg(test)]
mod tests;
