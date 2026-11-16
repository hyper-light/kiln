//! The Shards class's driver (macOS HVF, Linux KVM): its guest launched by the configured `shards`, as a
//! node launches it (on Linux in a cgroup of its own) but told no deadline, so it keeps its learned bounds, with
//! runtime folders of the run's own; followed to its readiness, its agent's answer (the relay's token and the
//! node's hello, as `GuestRuntime`'s first dial is; past the class's W* its run-out told once, never ended at it,
//! decision 9); its VMM read through its OS (its CPU and its guest's part
//! of it, its footprint and its guest RAM's part of it, what its folders hold), and its own report read from
//! its agent's `oci` capacity.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::{CapacityResources, Class};
use hex_client::guest::{Client, own_link, shards::ShardsLink};
use hex_runtime::guest::GuestConfig;
use shards_api::Guests as _;

use super::guest::guest_range;
use super::run::walk;
use super::seam::{Driver, Envelope, Files, Holding, Launched, Lines, Probe, Use};

pub struct Shards {
    config: GuestConfig,
    guests: shards_api::ConfiguredShards,
    cache: PathBuf,
    home: PathBuf,
    /// The run's own folder of runtime folders, the wrappers' among them.
    folder: PathBuf,
    /// Where its VMMs run, made at the first launch.
    places: Option<Places>,
}

/// A launched guest: its VM, where its VMM runs, and, once it is ready, its agent's client.
pub struct Guest {
    vm: Arc<dyn shards_api::Guest>,
    place: Place,
    client: Option<Client>,
}

/// Where a Linux run's VMMs run: each in a cgroup of its own, as the node runs each VM's, beneath this
/// process's own cgroup, which must hold the memory controller to hand down (run guest_cost in a cgroup
/// delegated to it, as the node's subtree is: `systemd-run --user --scope -p Delegate=yes ...`). Every
/// process in it moves into a leaf (`producer`) first: a cgroup with processes of its own hands no
/// controller down (cgroup v2's no-internal-process rule).
#[cfg(target_os = "linux")]
struct Places {
    root: PathBuf,
    wrappers: PathBuf,
    next: u64,
}

/// One VMM's cgroup, and the wrapper the API launches for it: a script that joins the cgroup, then runs the
/// configured `shards` in its place, so all the VMM holds is charged there from its start.
#[cfg(target_os = "linux")]
struct Place {
    cgroup: PathBuf,
    wrapper: PathBuf,
}

#[cfg(target_os = "macos")]
struct Places;

#[cfg(target_os = "macos")]
struct Place;

#[cfg(target_os = "linux")]
impl Places {
    fn delegated(runtime: &Path) -> Result<Self, String> {
        let root = hex_runtime::linux::cgroup::own_cgroup().map_err(|e| e.to_string())?;
        let read = |name: &str| {
            std::fs::read_to_string(root.join(name))
                .map_err(|e| format!("reading {}: {e}", root.join(name).display()))
        };
        if !read("cgroup.controllers")?
            .split_whitespace()
            .any(|controller| controller == "memory")
        {
            return Err(format!(
                "{} holds no memory controller to give each VMM a cgroup of its own, as the node does: run guest_cost in a cgroup delegated to it (systemd-run --user --scope -p Delegate=yes <guest_cost> ...)",
                root.display()
            ));
        }
        let leaf = root.join("producer");
        match std::fs::create_dir(&leaf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("making {}: {e}", leaf.display())),
        }
        for pid in read("cgroup.procs")?.lines() {
            match std::fs::write(leaf.join("cgroup.procs"), pid) {
                Ok(()) => {}
                // A process that has ended since the listing.
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {}
                Err(e) => return Err(format!("moving {pid} into {}: {e}", leaf.display())),
            }
        }
        std::fs::write(root.join("cgroup.subtree_control"), "+memory").map_err(|e| {
            format!(
                "handing the memory controller down from {}: {e}",
                root.display()
            )
        })?;
        let wrappers = runtime.join("wrappers");
        std::fs::create_dir_all(&wrappers)
            .map_err(|e| format!("making {}: {e}", wrappers.display()))?;
        Ok(Self {
            root,
            wrappers,
            next: 0,
        })
    }

    /// A new VMM's place: its cgroup, and the wrapper that joins it and runs `binary`, set as `spec`'s.
    fn place(&mut self, binary: &Path, spec: &mut shards_api::GuestSpec) -> Result<Place, String> {
        let name = format!("vm-{}", self.next);
        self.next = self.next.saturating_add(1);
        let quoted = |path: &Path| {
            path.to_str()
                .filter(|text| !text.contains('\''))
                .map(|text| format!("'{text}'"))
                .ok_or_else(|| format!("{} can't be quoted for sh", path.display()))
        };
        let place = Place {
            cgroup: self.root.join(&name),
            wrapper: self.wrappers.join(&name),
        };
        let script = format!(
            "#!/bin/sh\necho $$ > {} || exit 125\nexec {} \"$@\"\n",
            quoted(&place.cgroup.join("cgroup.procs"))?,
            quoted(binary)?
        );
        std::fs::create_dir(&place.cgroup)
            .map_err(|e| format!("making {}: {e}", place.cgroup.display()))?;
        std::fs::write(&place.wrapper, script)
            .and_then(|()| {
                std::fs::set_permissions(&place.wrapper, std::fs::Permissions::from_mode(0o700))
            })
            .map_err(|e| format!("writing {}: {e}", place.wrapper.display()))?;
        spec.binary = Some(place.wrapper.clone());
        Ok(place)
    }
}

/// A VMM's cgroup to `populated 0` once its guest has ended: its host's own exit, measuring (decision 9: within W*, the
/// slowest this run has seen, its answer; past it the run-out told once and the wait following the cgroup's processes,
/// never killed at a bound; no record seeds it).
#[cfg(target_os = "linux")]
static EMPTIED: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("guest_cost.emptied");

#[cfg(target_os = "linux")]
impl Place {
    /// Its VMM ended, its cgroup and wrapper go, once its cgroup holds no process. A guest's stop answers at its run's
    /// end, and its VMM's host exits on its own after it (Shards at 66113379), so the cgroup empties by itself: followed
    /// on its own events (`cgroup.events`) to its emptying ([`EMPTIED`]). Never an rmdir of a cgroup that holds a
    /// process.
    fn remove(&self) -> Result<(), String> {
        let cgroup = hex_runtime::linux::cgroup::BoxCgroup::at(self.cgroup.clone());
        let failed = |e: hex_api::Refusal| format!("removing {}: {e}", self.cgroup.display());
        hex_testkit::waits::follow::within(
            &EMPTIED,
            "the VMM's cgroup emptying after its guest's end",
            || hex_testkit::waits::follow::Party::Live,
            |by| {
                let left = by.map_or(Duration::MAX, |at| {
                    at.saturating_duration_since(Instant::now())
                });
                match cgroup.wait_empty(left) {
                    Ok(true) => Some(Ok(())),
                    Ok(false) => None,
                    Err(e) => Some(Err(failed(e))),
                }
            },
        )
        .map_err(|unmet| format!("removing {}: {unmet}", self.cgroup.display()))??;
        cgroup.remove().map_err(failed)?;
        std::fs::remove_file(&self.wrapper)
            .map_err(|e| format!("removing {}: {e}", self.wrapper.display()))
    }
}

#[cfg(target_os = "macos")]
impl Places {
    fn delegated(_: &Path) -> Result<Self, String> {
        Ok(Self)
    }

    /// The Mac's managed control holds a VM's process group, not a cgroup: the VMM runs where launched.
    fn place(&mut self, _: &Path, _: &mut shards_api::GuestSpec) -> Result<Place, String> {
        Ok(Place)
    }
}

#[cfg(target_os = "macos")]
impl Place {
    fn remove(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Shards {
    /// The guest the environment configures, as `hex node` reads it (`GuestConfig::from_env`).
    pub fn from_env(out: &Path) -> Result<Self, String> {
        Self::configured(GuestConfig::from_env().map_err(|e| e.to_string())?, out)
    }

    /// The `config` guest, launched with `out/runtime/{cache,home}` as its runtime folders (made private).
    pub fn configured(config: GuestConfig, out: &Path) -> Result<Self, String> {
        let runtime = out.join("runtime");
        let (cache, home) = (runtime.join("cache"), runtime.join("home"));
        for dir in [&runtime, &cache, &home] {
            std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("making {} private: {e}", dir.display()))?;
        }
        let absolute = |dir: &Path| std::path::absolute(dir).map_err(|e| e.to_string());
        let (cache, home) = (absolute(&cache)?, absolute(&home)?);
        let guests = shards_api::Shards::configured(cache.clone(), home.clone())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            guests,
            cache,
            home,
            folder: absolute(&runtime)?,
            places: None,
        })
    }

    fn ram(&self) -> Result<u64, String> {
        super::guest::ram(&self.config)
    }

    /// Stops `vm` and follows it to its end ([`super::guest::follow_end`]).
    fn stop(vm: &dyn shards_api::Guest) -> Result<(), String> {
        vm.stop().map_err(|e| format!("stopping the guest: {e}"))?;
        super::guest::follow_end(vm)
    }

    /// The probes of a launched guest's VMM: its pid, its guest's RAM, where it runs, and its folders (the
    /// runtime ones and its socket's, which the API made for it alone).
    fn probes(
        &self,
        vm: &dyn shards_api::Guest,
        place: &Place,
    ) -> Result<(VmmCpu, VmmHolds), String> {
        let identity = vm
            .identity()
            .map_err(|e| format!("naming the guest's VMM: {e}"))?;
        let pid = i32::try_from(identity.pid).map_err(|e| e.to_string())?;
        let socket = identity
            .vsock
            .parent()
            .ok_or("the guest's socket has no folder")?
            .to_owned();
        probes(
            pid,
            self.ram()?,
            place,
            vec![self.cache.clone(), self.home.clone(), socket],
        )
    }
}

impl Driver for Shards {
    type Guest = Guest;
    type Cpu = VmmCpu;
    type Holds = VmmHolds;

    fn class(&self) -> String {
        hex_runtime::guest::CLASS.to_owned()
    }

    fn describe(&self) -> Result<Lines, String> {
        let mut lines = super::guest::describe(&self.config)?;
        #[cfg(target_os = "macos")]
        lines.push((
            "host".to_owned(),
            hex_runtime::macos::machine_model().map_err(|e| e.to_string())?,
        ));
        #[cfg(target_os = "linux")]
        {
            let name = rustix::system::uname();
            lines.push((
                "host".to_owned(),
                format!(
                    "{} {}",
                    name.machine().to_string_lossy(),
                    name.release().to_string_lossy()
                ),
            ));
        }
        Ok(lines)
    }

    fn envelope(&self) -> Result<Envelope, String> {
        super::guest::envelope(&self.config)
    }

    fn runtime(&self) -> &Path {
        &self.home
    }

    fn sources(&self) -> Lines {
        let disk = format!(
            "the allocated bytes (st_blocks x 512, a hard-linked file once) of every path under SHARDS_CACHE {}, SHARDS_HOME {} and the guest's socket folder (its vsock's, which the API makes for it alone)",
            self.cache.display(),
            self.home.display()
        );
        vec![
            ("cpu".to_owned(), cpu_source()),
            ("memory".to_owned(), memory_source()),
            ("disk".to_owned(), disk),
        ]
    }

    fn held(&self) -> Result<Files, String> {
        walk(&[self.cache.clone(), self.home.clone()])
    }

    fn launch(&mut self) -> Result<Launched<Self>, String> {
        let mut spec = self.config.spec().map_err(|e| e.to_string())?;
        let places = match &mut self.places {
            Some(places) => places,
            None => self.places.insert(Places::delegated(&self.folder)?),
        };
        let place = places.place(&self.config.binary, &mut spec)?;
        let vm: Arc<dyn shards_api::Guest> = match self.guests.boot(&spec) {
            Ok(vm) => Arc::from(vm),
            Err(e) => {
                let why = format!("launching the guest: {e}");
                let removed = place.remove();
                return Err(match removed {
                    Ok(()) => why,
                    Err(remove) => format!("{why}; then {remove}"),
                });
            }
        };
        match self.probes(vm.as_ref(), &place) {
            Ok((cpu, holds)) => Ok((
                Guest {
                    vm,
                    place,
                    client: None,
                },
                cpu,
                holds,
            )),
            // Whatever kept it from being read, the guest is stopped first: none is left running.
            Err(why) => {
                let stopped = Self::stop(vm.as_ref()).and_then(|()| place.remove());
                Err(match stopped {
                    Ok(()) => why,
                    Err(stop) => format!("{why}; then {stop}"),
                })
            }
        }
    }

    fn ready(&mut self, guest: &mut Guest) -> Result<(), String> {
        // Followed to its answer: its link's dials past their own W* tell the run-out once and follow the guest's VM by
        // its life, with no bound of the record's, and its hello waits on its connection (decision 9; the review's C-9).
        let client = own_link(ShardsLink::new(Arc::clone(&guest.vm), None))
            .and_then(Client::connect)
            .map_err(|why| format!("reaching the guest's agent: {why}"))?;
        guest.client = Some(client);
        Ok(())
    }

    fn idle(&mut self, guest: &Guest, until: Instant) -> Result<(), String> {
        // The hold is a wait on the guest's end, until `until`: it must not come.
        match guest
            .vm
            .wait_for(until.saturating_duration_since(Instant::now()))
        {
            Ok(None) => Ok(()),
            Ok(Some(end)) => Err(format!("the guest ended in its idle hold: {end:?}")),
            Err(e) => Err(format!("holding the guest: {e}")),
        }
    }

    fn report(&mut self, guest: &Guest) -> Result<CapacityResources, String> {
        let client = guest
            .client
            .as_ref()
            .ok_or("the guest's agent never answered")?;
        hex_api::Runtime::capacity(client, Class::Oci)
            .map_err(|e| format!("reading the guest agent's capacity: {e}"))?
            .map(|capacity| capacity.free)
            .ok_or_else(|| "the guest's agent reports no oci capacity".to_owned())
    }

    fn tear_down(&mut self, guest: Guest) -> Result<(), String> {
        let Guest { vm, place, client } = guest;
        drop(client);
        Self::stop(vm.as_ref())?;
        place.remove()
    }
}

/// The bytes of the entries (`(start, end, bytes)`, in address order) that tile `range`, which must be
/// mapped whole: each entry in it starts where the one before ends, from its start to its end.
fn within(
    entries: impl Iterator<Item = (u64, u64, u64)>,
    range: (u64, u64),
) -> Result<u64, String> {
    let (mut at, mut bytes) = (range.0, 0u64);
    for (start, end, held) in entries {
        if end <= range.0 || start >= range.1 {
            continue;
        }
        if start != at || end > range.1 {
            return Err(format!(
                "the guest's RAM {:#x}-{:#x} is no longer mapped whole (an entry {start:#x}-{end:#x})",
                range.0, range.1
            ));
        }
        at = end;
        bytes = bytes.saturating_add(held);
    }
    if at != range.1 {
        return Err(format!(
            "the guest's RAM {:#x}-{:#x} is mapped only to {at:#x}",
            range.0, range.1
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn cpu_source() -> String {
    "the VMM's proc_pid_rusage ri_user_time + ri_system_time, less its vCPU threads' (named vcpuN by Shards, crates/vmm/src/vm/runtime.rs; each read by proc_pidinfo PROC_PIDTHREADINFO), the threads XNU counts its guest's time on (Shards PM M288): a lower bound of its own CPU, which leaves out the vCPU threads' exit handling until Shards reports each vCPU's guest exec time (hv_vcpu_get_exec_time)".to_owned()
}

#[cfg(target_os = "linux")]
fn cpu_source() -> String {
    format!(
        "utime + stime less guest_time of the VMM's /proc/<pid>/stat (fields 14, 15 and 43; proc(5): utime counts its guest's time too), in clock ticks of 1/{} s: a window's figure is within 2 ticks of its own CPU, which the census maximum absorbs",
        rustix::param::clock_ticks_per_second()
    )
}

#[cfg(target_os = "macos")]
fn memory_source() -> String {
    format!(
        "in production's state, the guest ready and its node agent running (the readings from its readiness on): the VMM's proc_pid_rusage ri_phys_footprint, less its guest RAM's pages resident and compressed (proc_pidinfo PROC_PIDREGIONINFO over the entries of the one run as long as its RAM of untagged entries it may read and write, outside any submap, PROC_PIDREGIONPATHINFO naming no file for any), in this host's {}-byte pages; ri_lifetime_max_phys_footprint is in the samples for the record",
        rustix::param::page_size()
    )
}

#[cfg(target_os = "linux")]
fn memory_source() -> String {
    "in production's state, the guest ready and its node agent running (the readings from its readiness on): the VMM's cgroup, its own from its start as the node gives each VM one (a wrapper joins it, then runs SHARDS_BIN), its non-file memory (memory.stat anon + kernel: the VMM's own and the kernel's for it, KVM's page tables among them) less its guest RAM's touched pages (Anonymous of the one run of private anonymous mappings as long as its RAM in /proc/<pid>/smaps); its page cache (memory.stat file: the images it read, shared by every VM that reads them and reclaimable) recorded apart, not charged".to_owned()
}

/// A launched guest's VMM's CPU, as macOS reads it: its own (`proc_pid_rusage`) and its vCPU threads'
/// (`proc_pidinfo`).
#[cfg(target_os = "macos")]
pub struct VmmCpu {
    pid: i32,
    timebase: hex_runtime::macos::sys::Timebase,
    handles: Vec<u64>,
}

/// What a launched guest's VMM holds, as macOS reads it: its footprint, its guest RAM's pages
/// (`PROC_PIDREGIONINFO`, which counts an entry's pages one by one, so a reading takes as long as its RAM is
/// large), and its folders.
#[cfg(target_os = "macos")]
pub struct VmmHolds {
    pid: i32,
    page: u64,
    ram: u64,
    guest: Option<(u64, u64)>,
    folders: Vec<PathBuf>,
}

/// The probes of the VMM `pid`, whose guest has `ram` bytes of RAM and whose folders are `folders`.
#[cfg(target_os = "macos")]
fn probes(
    pid: i32,
    ram: u64,
    _: &Place,
    folders: Vec<PathBuf>,
) -> Result<(VmmCpu, VmmHolds), String> {
    use hex_runtime::macos::sys;
    // Room for every thread a task may have, so a thread list is whole.
    let most = sys::sysctl_number(c"kern.num_taskthreads")
        .map_err(|e| format!("reading kern.num_taskthreads: {e}"))?;
    Ok((
        VmmCpu {
            pid,
            timebase: sys::Timebase::get().map_err(|e| e.to_string())?,
            handles: vec![0; usize::try_from(most).map_err(|e| e.to_string())?],
        },
        VmmHolds {
            pid,
            page: u64::try_from(rustix::param::page_size()).map_err(|e| e.to_string())?,
            ram,
            guest: None,
            folders,
        },
    ))
}

#[cfg(target_os = "macos")]
fn ended() -> String {
    "the guest's VMM has ended".to_owned()
}

#[cfg(target_os = "macos")]
impl Probe for VmmCpu {
    type Reading = Use;

    fn read(&mut self) -> Result<Use, String> {
        use hex_runtime::macos::sys;
        let threads = sys::threads(self.pid, &mut self.handles)
            .map_err(|e| format!("reading the VMM's threads: {e}"))?
            .ok_or_else(ended)?;
        let vcpu = |name: &[u8]| {
            name.strip_prefix(b"vcpu")
                .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
        };
        let (mut guest_cpu, mut guest_threads) = (Duration::ZERO, 0u32);
        for thread in threads.iter().filter(|thread| vcpu(&thread.name)) {
            guest_cpu = guest_cpu.saturating_add(thread.cpu);
            guest_threads = guest_threads.saturating_add(1);
        }
        let usage = sys::rusage(self.pid)
            .map_err(|e| format!("reading the VMM's use: {e}"))?
            .filter(|usage| !usage.ended)
            .ok_or_else(ended)?;
        Ok(Use {
            cpu: Duration::from_nanos(self.timebase.nanos(usage.cpu)),
            guest_cpu,
            guest_threads: Some(guest_threads),
        })
    }
}

#[cfg(target_os = "macos")]
impl VmmHolds {
    /// The VMM's map, entry by entry (`PROC_PIDREGIONINFO`), each with whether it may be the guest's RAM:
    /// memory the VMM may read and write that no allocator tagged and no submap holds, as Shards' guest RAM is
    /// (`reserve_ram`'s anonymous mmap, which XNU shows as `shared memory` once the hypervisor maps it too),
    /// and as reservations of address space alone, which may never be read, are not.
    fn entries(&self) -> Result<Vec<(u64, u64, bool)>, String> {
        let mut entries = Vec::new();
        let mut at = 0u64;
        while let Some(entry) = hex_runtime::macos::sys::region(self.pid, at)
            .map_err(|e| format!("reading the VMM's memory map: {e}"))?
        {
            let end = entry
                .address
                .checked_add(entry.size)
                .filter(|end| *end > at)
                .ok_or("the VMM's memory map doesn't advance")?;
            // VM_PROT_READ | VM_PROT_WRITE (mach/vm_prot.h).
            let usable = entry.max_protection & 3 == 3;
            entries.push((
                entry.address,
                end,
                entry.tag == 0 && !entry.submap && usable,
            ));
            at = end;
        }
        Ok(entries)
    }

    /// The guest's RAM, where the VMM maps it now: the one run [`guest_range`] finds in its map, each of
    /// whose entries maps no file (`PROC_PIDREGIONPATHINFO`, read for those entries alone).
    fn find_guest(&self) -> Result<Option<(u64, u64)>, String> {
        let entries = self.entries()?;
        let Some(range) = guest_range(&entries, self.ram)? else {
            return Ok(None);
        };
        for &(start, _, _) in entries
            .iter()
            .filter(|(start, end, _)| *start >= range.0 && *end <= range.1)
        {
            let entry = hex_runtime::macos::sys::region_with_path(self.pid, start)
                .map_err(|e| format!("reading the VMM's memory map: {e}"))?
                .ok_or("the VMM's memory map ended inside its guest's RAM")?;
            if entry.file != Some(false) {
                return Ok(None);
            }
        }
        Ok(Some(range))
    }

    /// The bytes of the guest's RAM the VMM holds, resident or compressed.
    fn guest_bytes(&self, range: (u64, u64)) -> Result<u64, String> {
        let mut entries = Vec::new();
        let mut at = range.0;
        while at < range.1 {
            let Some(entry) = hex_runtime::macos::sys::region(self.pid, at)
                .map_err(|e| format!("reading the VMM's memory map: {e}"))?
            else {
                break;
            };
            let end = entry
                .address
                .checked_add(entry.size)
                .filter(|end| *end > at)
                .ok_or("the VMM's memory map doesn't advance")?;
            let pages = entry.resident.saturating_add(entry.swapped_out);
            entries.push((entry.address, end, pages.saturating_mul(self.page)));
            at = end;
        }
        within(entries.into_iter(), range)
    }
}

#[cfg(target_os = "macos")]
impl Probe for VmmHolds {
    type Reading = Holding;

    fn read(&mut self) -> Result<Holding, String> {
        let usage = hex_runtime::macos::sys::rusage(self.pid)
            .map_err(|e| format!("reading the VMM's use: {e}"))?
            .filter(|usage| !usage.ended)
            .ok_or_else(ended)?;
        if self.guest.is_none() {
            self.guest = self.find_guest()?;
        }
        let guest_memory = self
            .guest
            .map(|range| self.guest_bytes(range))
            .transpose()?;
        Ok(Holding {
            footprint: usage.footprint,
            file_pages: None,
            peak_footprint: Some(usage.peak),
            guest_memory,
            files: walk(&self.folders)?,
        })
    }

    /// The map read whole again: the guest's RAM is still the one run found, so no reading took another
    /// run for it.
    fn verify(&mut self) -> Result<(), String> {
        let now = self.find_guest()?;
        if now != self.guest {
            return Err(format!(
                "the guest's RAM was read at {:x?} and is now at {now:x?}",
                self.guest
            ));
        }
        Ok(())
    }
}

/// A launched guest's VMM's CPU, as Linux reads it: its own and its guest's (`/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
pub struct VmmCpu {
    pid: i32,
    hz: u64,
}

/// What a launched guest's VMM holds, as Linux reads it: what its cgroup is charged (`memory.stat`: its
/// non-file memory, `anon` and `kernel`, and its page cache, `file`, apart), its guest RAM mapping's touched
/// pages (`/proc/<pid>/smaps`, which walks each mapping's pages, so a reading takes as long as its RAM is
/// large), and its folders.
#[cfg(target_os = "linux")]
pub struct VmmHolds {
    pid: i32,
    ram: u64,
    cgroup: PathBuf,
    guest: Option<(u64, u64)>,
    folders: Vec<PathBuf>,
}

/// The probes of the VMM `pid`, whose guest has `ram` bytes of RAM, which runs in `place`'s cgroup, and
/// whose folders are `folders`.
#[cfg(target_os = "linux")]
fn probes(
    pid: i32,
    ram: u64,
    place: &Place,
    folders: Vec<PathBuf>,
) -> Result<(VmmCpu, VmmHolds), String> {
    Ok((
        VmmCpu {
            pid,
            hz: rustix::param::clock_ticks_per_second().max(1),
        },
        VmmHolds {
            pid,
            ram,
            cgroup: place.cgroup.clone(),
            guest: None,
            folders,
        },
    ))
}

/// A cgroup's charge as its `memory.stat` keeps it (cgroup-v2.rst, "memory.stat"; one `name bytes` a line):
/// its non-file memory, `anon` and `kernel` (the kernel's own for it: stacks, page tables, KVM's among them,
/// slab), and its page cache, `file`.
#[cfg(target_os = "linux")]
fn charged(cgroup: &Path) -> Result<(u64, u64), String> {
    let path = cgroup.join("memory.stat");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let field = |name: &str| -> Result<u64, String> {
        text.lines()
            .find_map(|line| {
                line.split_once(' ')
                    .filter(|(key, _)| *key == name)
                    .map(|(_, value)| value)
            })
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| format!("{} names no {name} count", path.display()))
    };
    Ok((
        field("anon")?
            .checked_add(field("kernel")?)
            .ok_or("a cgroup's charge overflows")?,
        field("file")?,
    ))
}

#[cfg(target_os = "linux")]
fn read_proc(pid: i32, name: &str) -> Result<String, String> {
    std::fs::read_to_string(format!("/proc/{pid}/{name}"))
        .map_err(|e| format!("reading the VMM's /proc/{pid}/{name}: {e}"))
}

/// One mapping of `/proc/<pid>/smaps`: where it is, whether it is private and anonymous (no path), and its
/// anonymous bytes resident (as the cgroup's `anon` counts them).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapping {
    start: u64,
    end: u64,
    anonymous: bool,
    held: u64,
}

/// The mappings of `/proc/<pid>/smaps` text (proc(5)): each a header `start-end perms offset dev inode
/// [path]` and its fields, of which `Anonymous` (in kB) is counted.
#[cfg(target_os = "linux")]
fn mappings(text: &str) -> Result<Vec<Mapping>, String> {
    let mut out: Vec<Mapping> = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if let Some((start, end)) = first.split_once('-')
            && !first.ends_with(':')
        {
            let hex = |text: &str| {
                u64::from_str_radix(text, 16).map_err(|_| format!("smaps names no address: {line}"))
            };
            let perms = words
                .next()
                .ok_or_else(|| format!("smaps names no access: {line}"))?;
            let path = words.nth(3);
            out.push(Mapping {
                start: hex(start)?,
                end: hex(end)?,
                anonymous: path.is_none() && perms.ends_with('p'),
                held: 0,
            });
            continue;
        }
        if first == "Anonymous:" {
            let kb: u64 = words
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("smaps' {first} isn't a count: {line}"))?;
            let mapping = out
                .last_mut()
                .ok_or("smaps has a field before its first mapping")?;
            mapping.held = mapping.held.saturating_add(kb.saturating_mul(1024));
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
impl VmmCpu {
    fn ticks(&self, ticks: u64) -> Result<Duration, String> {
        u64::try_from(u128::from(ticks) * 1_000_000_000 / u128::from(self.hz))
            .map(Duration::from_nanos)
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "linux")]
impl Probe for VmmCpu {
    type Reading = Use;

    fn read(&mut self) -> Result<Use, String> {
        // proc(5): after the command's closing parenthesis, the state is the 3rd field, utime and stime the
        // 14th and 15th, guest_time the 43rd.
        let stat = read_proc(self.pid, "stat")?;
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.split_whitespace().collect())
            .ok_or("the VMM's stat has no command")?;
        if matches!(fields.first().copied(), Some("Z" | "X") | None) {
            return Err("the guest's VMM has ended".to_owned());
        }
        let field = |at: usize| -> Result<u64, String> {
            fields
                .get(at)
                .and_then(|field| field.parse().ok())
                .ok_or_else(|| format!("the VMM's stat field {} isn't a count", at + 3))
        };
        Ok(Use {
            cpu: self.ticks(field(11)?.saturating_add(field(12)?))?,
            guest_cpu: self.ticks(field(40)?)?,
            guest_threads: None,
        })
    }
}

#[cfg(target_os = "linux")]
impl Probe for VmmHolds {
    type Reading = Holding;

    fn read(&mut self) -> Result<Holding, String> {
        let maps = mappings(&read_proc(self.pid, "smaps")?)?;
        // Found again at each reading: once found, the same run, so no reading takes another for it.
        let entries: Vec<(u64, u64, bool)> =
            maps.iter().map(|m| (m.start, m.end, m.anonymous)).collect();
        let found = guest_range(&entries, self.ram)?;
        match (self.guest, found) {
            (None, found) => self.guest = found,
            (Some(was), Some(now)) if was == now => {}
            (Some(was), now) => {
                return Err(format!(
                    "the guest's RAM was read at {was:x?} and is now at {now:x?}"
                ));
            }
        }
        let guest_memory = self
            .guest
            .map(|range| within(maps.iter().map(|m| (m.start, m.end, m.held)), range))
            .transpose()?;
        let (footprint, file) = charged(&self.cgroup)?;
        Ok(Holding {
            footprint,
            file_pages: Some(file),
            peak_footprint: None,
            guest_memory,
            files: walk(&self.folders)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// A stand-in VMM's start into its place, a child of the test's own, measuring (decision 9: no record seeds it).
    #[cfg(target_os = "linux")]
    static STARTED: hex_core::patience::Patience =
        hex_core::patience::Patience::measuring("test.guest_cost.started");

    /// A place for a stand-in VMM under a cgroup of the test's own, as a launch makes one (`Places::place`), and the
    /// stand-in in it: a shell that joins the place's cgroup as a launch's wrapper does, then holds on its stdin, so it
    /// outlives its guest's stop until the test closes it. None where the test's cgroup takes no child or no process
    /// moves into one (a cgroup not delegated to it), said as a skip.
    #[cfg(target_os = "linux")]
    fn a_place_held(test: &str, dir: &Path) -> Option<(PathBuf, Place, std::process::Child)> {
        let own = hex_runtime::linux::cgroup::own_cgroup().unwrap();
        // A parent of each test's own: the tests run at once in one process.
        let parent = own.join(format!("hex-{test}-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir(&parent) {
            let why = format!("{} takes no child: {e}", own.display());
            hex_testkit::skip(test, "a cgroup delegated to the test", &why).unwrap();
            return None;
        }
        let place = Place {
            cgroup: parent.join("vm-0"),
            wrapper: dir.join("vm-0"),
        };
        std::fs::create_dir(&place.cgroup).unwrap();
        std::fs::write(&place.wrapper, "#!/bin/sh\n").unwrap();
        let mut vmm = std::process::Command::new("/bin/sh")
            .args(["-c", "echo $$ > \"$1/cgroup.procs\" && exec cat", "sh"])
            .arg(&place.cgroup)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let cgroup = hex_runtime::linux::cgroup::BoxCgroup::at(place.cgroup.clone());
        let mut ended = None;
        // A look: the cgroup tells no event as a process joins it. Its party, the stand-in, ends the wait in the fact.
        hex_testkit::waits::follow::until(
            &STARTED,
            "the stand-in VMM in its place or ended",
            || hex_testkit::waits::follow::Party::Live,
            || {
                ended = vmm.try_wait().unwrap();
                ended.is_some() || cgroup.populated().unwrap()
            },
        )
        .unwrap();
        if let Some(status) = ended {
            let _ = std::fs::remove_dir(&place.cgroup);
            let _ = std::fs::remove_dir(&parent);
            let why = format!(
                "no process moves into {} ({status})",
                place.cgroup.display()
            );
            hex_testkit::skip(test, "a cgroup delegated to the test", &why).unwrap();
            return None;
        }
        Some((parent, place, vmm))
    }

    fn config(dir: &Path) -> GuestConfig {
        for (name, bytes) in [
            ("shards", b"vmm".as_slice()),
            ("kernel", b"kernel".as_slice()),
            ("initrd", b"initrd".as_slice()),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        GuestConfig {
            binary: dir.join("shards"),
            boot: hex_runtime::guest::Boot::Kernel {
                kernel: dir.join("kernel"),
                initrd: dir.join("initrd"),
            },
            vcpus: 2,
            memory_mib: 512,
            gpu: false,
            shares: Vec::new(),
            staged: hex_client::guest::stage::STAGED.to_owned(),
        }
    }

    /// AC-T7.4.1 (decision 9; the review's C-9): a VMM whose host ends on its own after its guest's stop is followed to
    /// its end, never killed at a bound: at Shards 66113379 a guest's stop answers at its run's end and its VMM's host
    /// exits on its own after. Do give a stand-in VMM a place as a launch does, its process in the place's cgroup, let it
    /// end on its own as its guest's stop is done (its stdin closed), and remove the place. Expect the place gone and the
    /// stand-in's own end, no signal's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_vmm_that_ends_after_its_stop_is_waited_for_before_its_place_goes() {
        let dir = tempfile::tempdir().unwrap();
        let test = "a_vmm_that_ends_after_its_stop_is_waited_for_before_its_place_goes";
        let Some((parent, place, mut vmm)) = a_place_held(test, dir.path()) else {
            return;
        };
        drop(vmm.stdin.take());
        let got = place.remove();
        let (cgroup_left, wrapper_left) = (place.cgroup.exists(), place.wrapper.exists());
        let ended = vmm.try_wait().unwrap();
        let _ = vmm.kill();
        let _ = vmm.wait();
        let _ = std::fs::remove_dir(&place.cgroup);
        let _ = std::fs::remove_dir(&parent);
        assert_eq!(got, Ok(()));
        assert!(!cgroup_left && !wrapper_left, "the place is left");
        assert_eq!(ended.and_then(|status| status.code()), Some(0), "{ended:?}");
    }

    /// AC-T7.4.1: the driver's envelope is its configured guest's (vCPUs as milli-CPUs and threads, MiB as
    /// bytes, no disk), its runtime folders are made private under the run's, and it names each image by
    /// path and SHA-256.
    #[test]
    fn the_driver_measures_its_configured_guest() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Shards::configured(config(dir.path()), &dir.path().join("out")).unwrap();
        let envelope = driver.envelope().unwrap();
        assert_eq!(
            envelope.resources,
            CapacityResources {
                cpu_milli: 2000,
                mem_bytes: 512 << 20,
                disk_bytes: 0,
            }
        );
        assert_eq!(envelope.vcpus, 2);
        let home = std::fs::metadata(driver.runtime()).unwrap();
        assert_eq!(home.permissions().mode() & 0o777, 0o700);
        let lines = driver.describe().unwrap();
        let kernel = lines
            .iter()
            .find(|(name, _)| name == "HEX_GUEST_KERNEL")
            .unwrap();
        let expected = crate::run::hex(&Sha256::digest(b"kernel"));
        assert!(
            kernel.1.ends_with(&format!("sha256={expected}")),
            "{kernel:?}"
        );
        assert!(
            lines
                .iter()
                .any(|(name, value)| name == "HEX_GUEST_MEMORY_MIB" && value == "512")
        );
    }

    /// AC-T7.4.1: the guest's RAM among a VMM's map entries: expect the one run of anonymous entries, each
    /// where the one before ends, as long as its RAM (XNU's 128 MiB entries; Linux's split by advice), a
    /// neighbour that abuts it or a file's entry in it never taken in, none while it isn't mapped, and two
    /// such runs refused.
    #[test]
    fn the_guest_ram_is_the_one_anonymous_run_as_long_as_it() {
        const M: u64 = 1 << 20;
        let ram = 512 * M;
        let base = 0x1_0000_0000;
        let chunks: Vec<(u64, u64, bool)> = (0..4)
            .map(|n| (base + n * 128 * M, base + (n + 1) * 128 * M, true))
            .collect();
        let mut entries = vec![(base - M, base, true)];
        entries.extend(chunks.iter().copied());
        entries.push((base + ram, base + ram + M, true));
        assert_eq!(
            guest_range(&entries, ram).unwrap(),
            Some((base, base + ram))
        );
        assert_eq!(guest_range(&entries[..3], ram).unwrap(), None);
        let mut filed = entries.clone();
        filed[2].2 = false;
        assert_eq!(guest_range(&filed, ram).unwrap(), None);
        let mut twice = entries.clone();
        twice.push((base + 4 * ram, base + 5 * ram, true));
        assert!(guest_range(&twice, ram).is_err());

        let held: Vec<(u64, u64, u64)> = entries.iter().map(|&(s, e, _)| (s, e, 3)).collect();
        assert_eq!(
            within(held.iter().copied(), (base, base + ram)).unwrap(),
            12
        );
        let mut holed = held.clone();
        holed.remove(2);
        assert!(within(holed.into_iter(), (base, base + ram)).is_err());
    }

    /// AC-T7.4.1: smaps text as proc(5) gives it: expect each mapping's range, whether it is private and
    /// anonymous, and its Anonymous bytes (its swapped ones not: the cgroup's anon counts resident pages).
    #[cfg(target_os = "linux")]
    #[test]
    fn smaps_reads_each_mapping_and_its_anonymous_bytes() {
        let text = "\
55d0c0000000-55d0c0021000 r--p 00000000 08:01 1234                       /usr/bin/shards-vm
Rss:                  12 kB
Anonymous:             4 kB
Swap:                  0 kB
VmFlags: rd mr mw me dw sd
7f0000000000-7f0020000000 rw-p 00000000 00:00 0
Rss:               30720 kB
Anonymous:         30720 kB
Swap:                  8 kB
VmFlags: rd wr mr mw me ac sd hg
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
Anonymous:            16 kB
Swap:                  0 kB
";
        let maps = mappings(text).unwrap();
        assert_eq!(maps.len(), 3);
        assert_eq!(
            maps[1],
            Mapping {
                start: 0x7f00_0000_0000,
                end: 0x7f00_2000_0000,
                anonymous: true,
                held: 30720 * 1024,
            }
        );
        assert!(!maps[0].anonymous && !maps[2].anonymous);
        assert_eq!(maps[2].held, 16 * 1024);
    }

    /// AC-T7.4.1: a real busy process read as a VMM is: expect its CPU to grow from nothing, its footprint
    /// read (on Linux its cgroup's charge, here a folder that holds what a cgroup's memory files do), no RAM
    /// of a guest told apart in it (none is as long as the one asked for), and its folder's bytes listed.
    #[test]
    fn a_process_is_read_as_a_vmm_is() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("held"), vec![1u8; 64 * 1024]).unwrap();
        #[cfg(target_os = "linux")]
        let place = {
            let cgroup = dir.path().join("cgroup");
            std::fs::create_dir(&cgroup).unwrap();
            std::fs::write(
                cgroup.join("memory.stat"),
                "anon 100\nfile 456\nkernel 23\nshmem 0\n",
            )
            .unwrap();
            Place {
                cgroup,
                wrapper: dir.path().join("wrapper"),
            }
        };
        #[cfg(target_os = "macos")]
        let place = Place;
        let mut busy = std::process::Command::new("/usr/bin/yes")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(busy.id()).unwrap();
        let (mut cpu, mut holds) =
            probes(pid, u64::MAX, &place, vec![dir.path().join("held")]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut used = cpu.read().unwrap();
        while used.cpu.is_zero() {
            assert!(Instant::now() < deadline, "the process never used CPU");
            std::thread::park_timeout(Duration::from_millis(1));
            used = cpu.read().unwrap();
        }
        let read = holds.read().unwrap();
        holds.verify().unwrap();
        let _ = busy.kill();
        let _ = busy.wait();
        assert!(read.footprint > 0, "{read:?}");
        #[cfg(target_os = "linux")]
        assert_eq!((read.footprint, read.file_pages), (123, Some(456)));
        assert_eq!(read.guest_memory, None);
        assert!(used.guest_cpu <= used.cpu, "{used:?}");
        let held = read
            .files
            .iter()
            .find(|held| held.path.ends_with("held"))
            .unwrap();
        assert!(held.bytes >= 64 * 1024, "{held:?}");
    }

    /// AC-T7.4.1 (ROOT: the shared images' page cache is no one VM's cost; original): two guests' VMM
    /// cgroups as the Linux probe reads them, alike but that the first read the guest's images cold, its
    /// cgroup charged their 20 MiB of page cache, and the second found them cached: expect the same memory
    /// charged for both, the census's charge no matter which guest read the images first.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cold_first_guest_is_charged_as_a_warm_one() {
        use crate::run::{Readings, Tick, measured};
        use crate::seam::Use;
        const M: u64 = 1 << 20;
        let (guest, anon, kernel) = (160 * M, 166 * M, 2 * M);
        let charged = |file: u64| {
            let dir = tempfile::tempdir().unwrap();
            let cgroup = dir.path().join("cgroup");
            std::fs::create_dir(&cgroup).unwrap();
            let current = anon + kernel + file;
            std::fs::write(cgroup.join("memory.current"), format!("{current}\n")).unwrap();
            std::fs::write(cgroup.join("memory.peak"), format!("{current}\n")).unwrap();
            std::fs::write(
                cgroup.join("memory.stat"),
                format!("anon {anon}\nfile {file}\nkernel {kernel}\nshmem 0\n"),
            )
            .unwrap();
            let place = Place {
                cgroup,
                wrapper: dir.path().join("wrapper"),
            };
            let pid = i32::try_from(std::process::id()).unwrap();
            let (_, mut holds) = probes(pid, u64::MAX, &place, Vec::new()).unwrap();
            let mut held = holds.read().unwrap();
            // This process maps no guest: its touched pages, as a VMM's guest RAM mapping would show them.
            held.guest_memory = Some(guest);
            let at = Duration::from_millis(100);
            let readings = Readings {
                cpu: vec![Tick {
                    at,
                    reading: Use {
                        cpu: Duration::ZERO,
                        guest_cpu: Duration::ZERO,
                        guest_threads: None,
                    },
                }],
                holdings: vec![Tick { at, reading: held }],
                own: None,
            };
            measured(
                Duration::ZERO,
                &readings,
                CapacityResources::default(),
                at,
                1,
                &Vec::new(),
                &Vec::new(),
            )
            .unwrap()
            .mem_bytes
        };
        let (cold, warm) = (charged(20 * M), charged(0));
        assert_eq!(cold, warm, "cold {cold}, warm {warm}");
        assert_eq!(warm, anon + kernel - guest);
    }

    /// AC-T7.4.1 (light smoke, few boots): produce a pair over the configured Shards guest with SEEDABLE
    /// two, through the same census, evidence and output the measurement uses: expect it written and loaded
    /// as a node loads it, its envelope the configured guest's, its cost carrying no VMM overhead and no guest
    /// allowance, two guests in its evidence, and its VMM's own memory in its evidence. Needs SHARDS_BIN, HEX_GUEST_KERNEL, HEX_GUEST_INITRD, HEX_GUEST_CPUS and
    /// HEX_GUEST_MEMORY_MIB, GUEST_COST_SMOKE_OUT (a folder under ~/.cache), on a Mac the window's
    /// CPU-response row file, GUEST_COST_SMOKE_ROW and GUEST_COST_SMOKE_ROW_SHA256, and on Linux a cgroup
    /// delegated to the test (`systemd-run --user --scope -p Delegate=yes <the test binary> ...`).
    #[test]
    #[ignore = "boots two real guests: run with --ignored where a Shards guest is configured"]
    fn two_guests_make_a_pair_the_node_loads() {
        let out =
            PathBuf::from(std::env::var_os("GUEST_COST_SMOKE_OUT").expect("GUEST_COST_SMOKE_OUT"));
        let mut driver = Shards::from_env(&out).unwrap();
        let envelope = driver.envelope().unwrap().resources;
        let (windows, prefix) = crate::windows(&crate::smoke_args(&out)).unwrap();
        let made = super::super::run::produce(
            &mut driver,
            &super::super::run::Plan { seedable: 2 },
            &out,
            &windows,
            &prefix,
        )
        .unwrap();
        let (cost, _, rows) = hex_runtime::guest::load_runtime_evidence(
            &made.cost_file,
            made.cost_sha256,
            &made.evidence_file,
        )
        .unwrap();
        assert_eq!(rows.is_empty(), prefix.is_empty());
        assert_eq!((cost.floor, cost.ceiling), (envelope, envelope));
        // The cost carries no VMM overhead and no guest allowance: a node learns both from its windows (joint plan
        // step 14; ROOT, 2026-11-13, by decision 9's rule; fa865284b). What the census measured of them is evidence
        // alone, its overhead line's.
        let none = hex_api::CapacityResources::default();
        assert_eq!(
            (cost.overhead, cost.guest_overhead),
            (none, none),
            "{cost:?}"
        );
        let evidence = std::fs::read(&made.evidence_file).unwrap();
        let text = String::from_utf8_lossy(&evidence[prefix.len()..]).into_owned();
        assert_eq!(text.lines().filter(|l| l.starts_with("guest ")).count(), 2);
        let own_memory: u64 = text
            .lines()
            .find_map(|l| l.strip_prefix("overhead "))
            .and_then(|l| {
                l.split_whitespace()
                    .find_map(|w| w.strip_prefix("mem_bytes="))
            })
            .and_then(|bytes| bytes.parse().ok())
            .unwrap_or_else(|| panic!("no overhead line naming its memory: {text}"));
        assert!(own_memory > 0, "{text}");
        println!("{}\n{text}", made.summary);
    }
}
