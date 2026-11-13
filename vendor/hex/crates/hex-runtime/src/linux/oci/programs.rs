//! The device programs a node agent holds (goal 5 extended, step 1): decision 2's sharing, with the
//! agent as the holder that outlives its containers. Each policy's program is loaded once and kept open
//! ([`devshare::Programs`]), so a create attaches it with no load (research f60). On a root node the
//! default policy's program is attached once, at the node's start, to the root its oci boxes alone live
//! under, with `BPF_F_ALLOW_OVERRIDE`: a box with the default policy attaches nothing, and a box with
//! another attaches its own, which then decides for it in the root's place (f40-f42). A rootless node
//! loads nothing: loading needs CAP_BPF in the initial user namespace, so its containers are held to
//! their device lists by construction, as before (`run::attach_device_program`).

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rustix::fs::{Mode, OFlags};

use crate::linux::cgroup::BoxCgroup;
use crate::linux::oci::devshare::{self, Give};
use crate::linux::oci::resources::{self, Insn, Plan};
use crate::linux::oci::{OciError, sys};

/// What a holder asks the kernel of device programs ([`Programs::at_root_on`], [`program_for_on`]): the device programs
/// a cgroup carries, a loaded program by its id, its identity and its translation, a load, and an attach that
/// replaces. The kernel's own ([`Kernel`]), or a test's, so what a holder keeps on a kernel that shows a program's
/// translation as it is, withholds it or rewrites it is shown without root.
pub trait DevicePrograms {
    /// A loaded program, open.
    type Program;
    /// The device programs `cgroup` carries itself ([`sys::attached_device_programs`]).
    fn attached(&self, cgroup: BorrowedFd<'_>) -> Result<sys::Attached, OciError>;
    /// Loaded program `id`, open, if there is one.
    fn by_id(&self, id: u32) -> Result<Option<Self::Program>, OciError>;
    /// What the kernel says `program` is.
    fn identity(&self, program: &Self::Program) -> Result<sys::ProgramIdentity, OciError>;
    /// `program`'s translated instructions, `len` bytes of them, where the kernel dumps them.
    fn translation(&self, program: &Self::Program, len: u32) -> Result<Option<Vec<u8>>, OciError>;
    /// `insns` loaded as a cgroup device program.
    fn load(&self, insns: &[Insn]) -> Result<Self::Program, OciError>;
    /// `program` attached to `cgroup` with `BPF_F_ALLOW_OVERRIDE`, replacing what it carries.
    fn attach_override(
        &self,
        cgroup: BorrowedFd<'_>,
        program: &Self::Program,
    ) -> Result<(), OciError>;
}

/// The kernel's device programs (`bpf(2)`, through [`sys`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct Kernel;

impl DevicePrograms for Kernel {
    type Program = OwnedFd;
    fn attached(&self, cgroup: BorrowedFd<'_>) -> Result<sys::Attached, OciError> {
        sys::attached_device_programs(cgroup)
    }
    fn by_id(&self, id: u32) -> Result<Option<OwnedFd>, OciError> {
        sys::program_by_id(id)
    }
    fn identity(&self, program: &OwnedFd) -> Result<sys::ProgramIdentity, OciError> {
        sys::program_identity(program.as_fd())
    }
    fn translation(&self, program: &OwnedFd, len: u32) -> Result<Option<Vec<u8>>, OciError> {
        sys::program_instructions(program.as_fd(), len)
    }
    fn load(&self, insns: &[Insn]) -> Result<OwnedFd, OciError> {
        sys::load_device_program(insns)
    }
    fn attach_override(&self, cgroup: BorrowedFd<'_>, program: &OwnedFd) -> Result<(), OciError> {
        sys::attach_device_program(cgroup, program.as_fd(), sys::Attach::Override)
    }
}

/// What a kernel shows of a device program it loaded ([`shown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Its translation, the program itself byte for byte ([`devshare::is_policy`]): a loaded program is proven by it
    /// (fedora's kernel, research f55).
    Itself,
    /// No translation: a blinded program, to a caller without raw-dump rights (`kernel.kptr_restrict` 2, f51-f52).
    Withheld,
    /// A translation other than the program: a kernel that rewrites it.
    Rewritten,
}

/// What `kernel` shows of a device program it loaded: a fresh copy of the default policy's. Only where it shows the
/// program itself does [`Programs::at_root_on`] keep a root's program and [`program_for_on`] take a hinted one: the
/// translation is their proof, a tag none (f54). Elsewhere neither proves anything, and a holder loads and attaches a
/// copy of its own, the root's program replaced by one attach, a hinted one passed over, never the policy (f55).
pub fn shown<K: DevicePrograms>(kernel: &K) -> Result<Shown, OciError> {
    let default = resources::default_program()?;
    let copy = kernel.load(&default)?;
    let identity = kernel.identity(&copy)?;
    Ok(match kernel.translation(&copy, identity.xlated_len)? {
        None => Shown::Withheld,
        Some(translation) if devshare::is_policy(&identity, Some(&translation), &default) => {
            Shown::Itself
        }
        Some(_) => Shown::Rewritten,
    })
}

/// What a node's device programs have done since it opened them: what tests and the bench read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Programs loaded for boxes; the root's is [`Counts::root_attached`]'s.
    pub loaded: u64,
    /// Programs attached to boxes' cgroups.
    pub attached: u64,
    /// Programs closed past the bound.
    pub closed: u64,
    /// Programs held open now.
    pub held: usize,
    /// Whether the node's start loaded the default policy's program and attached it to the oci boxes'
    /// root: `false` where the root carried it already (an agent restarted over its boxes), and on a
    /// rootless node.
    pub root_attached: bool,
}

/// A node agent's device programs: those it holds open, one per policy, and the policy whose program its
/// oci boxes' root carries.
#[derive(Debug)]
pub struct Programs {
    held: Mutex<devshare::Programs<Arc<OwnedFd>>>,
    /// The default policy's instructions, when the oci boxes' root carries their program.
    root: Option<Vec<Insn>>,
    root_attached: bool,
    loaded: AtomicU64,
    attached: AtomicU64,
    closed: AtomicU64,
}

impl Programs {
    /// A rootless node's: no program at its root, and none loaded.
    pub fn rootless(bound: usize) -> Self {
        Self::holding(bound, None, false)
    }

    /// A root node's: the default policy's program on `root`, the root its oci boxes alone live under,
    /// attached with `BPF_F_ALLOW_OVERRIDE` unless the root carries it already, and at most `bound`
    /// programs held for boxes. Which program the root carries is asked once, here, since the query
    /// takes the kernel's `cgroup_mutex` (research f61). One program attached with OVERRIDE whose
    /// translation is the default's is kept, so an agent restarted over its boxes leaves their root's
    /// program as it was. Anything else is replaced by one attach (include/uapi/linux/bpf.h: an OVERRIDE
    /// attach "will release old program and attach the new one"), or, where the kernel won't (programs
    /// attached with other flags, which Hex never attaches there), refused by name.
    pub fn at_root(root: &Path, bound: usize) -> Result<Self, OciError> {
        Self::at_root_on(&Kernel, root, bound)
    }

    /// [`Programs::at_root`] over `kernel`'s device programs.
    pub fn at_root_on<K: DevicePrograms>(
        kernel: &K,
        root: &Path,
        bound: usize,
    ) -> Result<Self, OciError> {
        let default = resources::default_program()?;
        let dir = rustix::fs::open(
            root,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| {
            OciError::os(
                "opening the oci boxes' root",
                format!("{}: {e}", root.display()),
            )
        })?;
        let there = kernel.attached(dir.as_fd())?;
        let kept = there.count == 1
            && there.flags == sys::BPF_F_ALLOW_OVERRIDE
            && there
                .ids
                .first()
                .is_some_and(|id| program_for_on(kernel, *id, &default).is_some());
        if !kept {
            let prog = kernel.load(&default)?;
            kernel.attach_override(dir.as_fd(), &prog).map_err(|e| {
                OciError::os(
                    "attaching the default device program to the oci boxes' root",
                    format!(
                        "{}: {e} (it holds {} program(s), attached with flags {:#x})",
                        root.display(),
                        there.count,
                        there.flags
                    ),
                )
            })?;
            // `prog` closes here: the root's attachment holds the program (research f53).
        }
        Ok(Self::holding(bound, Some(default), !kept))
    }

    fn holding(bound: usize, root: Option<Vec<Insn>>, root_attached: bool) -> Self {
        Self {
            held: Mutex::new(devshare::Programs::new(bound)),
            root,
            root_attached,
            loaded: AtomicU64::new(0),
            attached: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        }
    }

    /// The program `plan` asks a box's own cgroup to carry on a create through the seam
    /// ([`devshare::give`]): `None` for the root's policy (the root's program decides) or, with no root
    /// program, for a policy that allows every device; else the policy's program. A container in a user
    /// namespace of its own is held to its device list by construction and refused an allow-by-default
    /// list with denials, as before ([`Plan::check_in_userns`]); under the root's program it carries the
    /// allow-all program, so the root's default doesn't decide for it in construction's place. What a
    /// cgroup handed on must carry already for a box to be given it (goal 5 extended, step 6).
    pub fn wants(&self, plan: &Plan, in_userns: bool) -> Result<Option<Vec<Insn>>, OciError> {
        let policy = if in_userns {
            plan.check_in_userns()?;
            None
        } else {
            plan.devices.as_deref()
        };
        Ok(match devshare::give(policy, self.root.as_deref()) {
            Give::Nothing => None,
            Give::Program(insns) => Some(insns),
        })
    }

    /// Gives box cgroup `cg` the device program `plan` asks for ([`Programs::wants`]): the program held
    /// for the policy, loaded first only when none is.
    pub fn attach(&self, cg: &BoxCgroup, plan: &Plan, in_userns: bool) -> Result<(), OciError> {
        match self.wants(plan, in_userns)? {
            Some(insns) => self.attach_program(cg, insns),
            None => Ok(()),
        }
    }

    /// Attaches the program held for `insns` to box cgroup `cg`, loading it first only when none is.
    pub fn attach_program(&self, cg: &BoxCgroup, insns: Vec<Insn>) -> Result<(), OciError> {
        let prog = self.program(insns)?;
        let dir = cg
            .open_dir()
            .map_err(|e| OciError::os("opening a box's cgroup", e))?;
        sys::attach_device_program(dir.as_fd(), prog.as_fd(), sys::Attach::Multi)?;
        self.attached.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The program held for `insns`, loaded and held now if none is. It's loaded with the lock held, so a
    /// policy many creates ask for at once is loaded once; a policy's first create is the only one that
    /// waits on the verifier, and the others wait for it only that once.
    fn program(&self, insns: Vec<Insn>) -> Result<Arc<OwnedFd>, OciError> {
        let mut held = self.held.lock().map_err(|_| {
            OciError::Broken("the node's device programs' lock was poisoned".to_owned())
        })?;
        if let Some(prog) = held.get(&insns) {
            return Ok(Arc::clone(prog));
        }
        let prog = Arc::new(sys::load_device_program(&insns)?);
        self.loaded.fetch_add(1, Ordering::Relaxed);
        // What the bound puts out closes as it drops, once no create still attaching holds it; a program
        // attached to a box lives on there (research f53).
        for _ in held.put(insns, Arc::clone(&prog)) {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(prog)
    }

    pub fn counts(&self) -> Result<Counts, OciError> {
        let held = self
            .held
            .lock()
            .map_err(|_| {
                OciError::Broken("the node's device programs' lock was poisoned".to_owned())
            })?
            .len();
        Ok(Counts {
            loaded: self.loaded.load(Ordering::Relaxed),
            attached: self.attached.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
            held,
            root_attached: self.root_attached,
        })
    }
}

/// Loaded program `id`, open, if it's the program for `insns`: a cgroup device program whose translated
/// instructions are `insns` byte for byte ([`devshare::is_policy`]). The open descriptor holds it from
/// that check to its attach, so an id the kernel gives another program meanwhile (ids are reused,
/// research f53) can't be what's attached. One gone, or one that can't be opened or read, is `None`.
pub fn program_for(id: u32, insns: &[Insn]) -> Option<OwnedFd> {
    program_for_on(&Kernel, id, insns)
}

/// [`program_for`] over `kernel`'s device programs.
pub fn program_for_on<K: DevicePrograms>(
    kernel: &K,
    id: u32,
    insns: &[Insn],
) -> Option<K::Program> {
    let prog = kernel.by_id(id).ok()??;
    let identity = kernel.identity(&prog).ok()?;
    let translation = kernel.translation(&prog, identity.xlated_len).ok()?;
    if devshare::is_policy(&identity, translation.as_deref(), insns) {
        Some(prog)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    /// A kernel of device programs: programs by id, and the one program a cgroup carries, attached with OVERRIDE.
    struct Sim {
        shows: Shown,
        programs: RefCell<BTreeMap<u32, Vec<Insn>>>,
        next: Cell<u32>,
        root: Cell<Option<u32>>,
        attaches: Cell<u32>,
    }

    impl Sim {
        fn new(shows: Shown) -> Self {
            Self {
                shows,
                programs: RefCell::new(BTreeMap::new()),
                next: Cell::new(1),
                root: Cell::new(None),
                attaches: Cell::new(0),
            }
        }
    }

    impl DevicePrograms for Sim {
        type Program = u32;
        fn attached(&self, _: BorrowedFd<'_>) -> Result<sys::Attached, OciError> {
            Ok(match self.root.get() {
                Some(id) => sys::Attached {
                    flags: sys::BPF_F_ALLOW_OVERRIDE,
                    count: 1,
                    ids: vec![id],
                },
                None => sys::Attached::default(),
            })
        }
        fn by_id(&self, id: u32) -> Result<Option<u32>, OciError> {
            Ok(if self.programs.borrow().contains_key(&id) {
                Some(id)
            } else {
                None
            })
        }
        fn identity(&self, program: &u32) -> Result<sys::ProgramIdentity, OciError> {
            let len = self.programs.borrow().get(program).map_or(0, Vec::len);
            let extra = usize::from(self.shows == Shown::Rewritten);
            Ok(sys::ProgramIdentity {
                prog_type: sys::BPF_PROG_TYPE_CGROUP_DEVICE,
                id: *program,
                xlated_len: u32::try_from((len + extra) * 8).unwrap(),
                ..sys::ProgramIdentity::default()
            })
        }
        fn translation(&self, program: &u32, _: u32) -> Result<Option<Vec<u8>>, OciError> {
            let bytes = |insns: &Vec<Insn>| insns.iter().flat_map(Insn::bytes).collect::<Vec<u8>>();
            Ok(match self.shows {
                Shown::Itself => self.programs.borrow().get(program).map(bytes),
                Shown::Withheld => None,
                Shown::Rewritten => self.programs.borrow().get(program).map(|insns| {
                    let mut rewritten = bytes(insns);
                    rewritten.extend([0; 8]);
                    rewritten
                }),
            })
        }
        fn load(&self, insns: &[Insn]) -> Result<u32, OciError> {
            let id = self.next.get();
            self.next.set(id + 1);
            self.programs.borrow_mut().insert(id, insns.to_vec());
            Ok(id)
        }
        fn attach_override(&self, _: BorrowedFd<'_>, program: &u32) -> Result<(), OciError> {
            self.root.set(Some(*program));
            self.attaches.set(self.attaches.get() + 1);
            Ok(())
        }
    }

    /// The lead's root lab of 2026-11-11 (oci_devices.rs:210 at main 2dc8295f on 00aad8da, kernel 7.3.3-1-cachyos,
    /// `kernel.kptr_restrict` 2): a second holder of the node's kind keeps the oci boxes' root's default exactly where
    /// the kernel proves a loaded program by its translation ([`shown`]), and elsewhere replaces it by one attach, the
    /// root's policy the default's either way. The original at d6847be8 expected the keep on every kernel, as
    /// oci_devices did, and failed over one that withholds. Do: over kernels that show a loaded program as itself,
    /// withhold it, or rewrite it, open a holder at an empty root, then a second at the same root. Expect: each kernel
    /// shown as it is; the first attached the default; the second kept it (no attach, the same program) where the
    /// kernel shows the program itself, and replaced it by one attach elsewhere; the root's program the default's.
    #[test]
    fn a_second_holder_keeps_the_roots_default_where_the_kernel_proves_it() {
        let default = resources::default_program().unwrap();
        for shows in [Shown::Itself, Shown::Withheld, Shown::Rewritten] {
            let root = tempfile::tempdir().unwrap();
            let kernel = Sim::new(shows);
            assert_eq!(shown(&kernel).unwrap(), shows);
            let node = Programs::at_root_on(&kernel, root.path(), 1).unwrap();
            assert!(
                node.counts().unwrap().root_attached,
                "{shows:?}: the node's start attached the default"
            );
            let first = kernel.root.get();
            let held = Programs::at_root_on(&kernel, root.path(), 1).unwrap();
            let proves = shows == Shown::Itself;
            assert_eq!(held.counts().unwrap().root_attached, !proves, "{shows:?}");
            assert_eq!(
                kernel.attaches.get(),
                if proves { 1 } else { 2 },
                "{shows:?}"
            );
            assert_eq!(kernel.root.get() == first, proves, "{shows:?}");
            let carried = kernel.root.get().unwrap();
            assert_eq!(
                kernel.programs.borrow().get(&carried),
                Some(&default),
                "{shows:?}: the root's policy"
            );
        }
    }
}
