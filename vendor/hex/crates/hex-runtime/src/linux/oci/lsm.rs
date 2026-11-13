//! SELinux mount labels and AppArmor profiles (goal 6, AC-T7.1.4): `linux.mountLabel` on every filesystem a
//! container's mounts make, `process.apparmorProfile` on the process's `execve`. Where the host's kernel runs
//! the module, a label it can't apply is refused by name. Where the kernel doesn't run the module at all, the
//! [`Policy`] decides: by default the label is skipped and the create warns, naming it, as crun runs such a
//! container (the lead's goal-17 bar, 2026-11-06); `HEX_OCI_LSM=strict` refuses it by name instead. The decision
//! is docs/design/runtime.md, "SELinux mount labels and AppArmor profiles"; its sources are
//! docs/research/runtime/goal6-lsm.md. The rules are pure and tested; the system calls sit beside them.

use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd};

use rustix::fs::{Mode, OFlags, XattrFlags};

use crate::linux::oci::{Failure, OciError};

/// How a filesystem takes the mount label (crun's `get_mount_label_how`, research §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Labelling {
    /// The `context` parameter of its filesystem context.
    Context,
    /// The `security.selinux` xattr on its root: mqueue, whose files the `context` option doesn't reach.
    Xattr,
    /// None: proc and sysfs show the kernel's own objects, and cgroup2 is mounted unlabelled, as crun mounts it.
    None,
}

/// How a filesystem of type `fstype` takes the mount label.
pub(crate) fn labelling(fstype: &str) -> Labelling {
    match fstype {
        "proc" | "sysfs" | "cgroup" | "cgroup2" => Labelling::None,
        "mqueue" => Labelling::Xattr,
        _ => Labelling::Context,
    }
}

/// The environment entry by which the command line's or a node's owner picks the [`Policy`]: `strict`, or
/// `skip` (the default where it is unset). Each binary reads it at its edge and hands the policy in.
pub const ENV_POLICY: &str = "HEX_OCI_LSM";

/// What a create does with a label whose module the host's kernel doesn't run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Run the container without it, and warn, naming it, as crun does.
    #[default]
    Skip,
    /// Refuse the create by name, for an owner who wants fail-closed.
    Strict,
}

impl Policy {
    /// The policy [`ENV_POLICY`]'s words ask for: unset or `skip` is [`Policy::Skip`], `strict` is
    /// [`Policy::Strict`], and anything else is refused by name.
    pub fn parse(asked: Option<&str>) -> Result<Self, OciError> {
        match asked {
            None | Some("skip") => Ok(Self::Skip),
            Some("strict") => Ok(Self::Strict),
            Some(other) => Err(OciError::Config(format!(
                "{ENV_POLICY}={other:?} names no LSM policy: skip or strict"
            ))),
        }
    }

    /// The policy this process's environment asks for ([`Policy::parse`]); a value that isn't Unicode is
    /// refused by name.
    pub fn from_env() -> Result<Self, OciError> {
        match std::env::var_os(ENV_POLICY) {
            None => Self::parse(None),
            Some(v) => Self::parse(Some(v.to_str().ok_or_else(|| {
                OciError::Config(format!("{ENV_POLICY} isn't Unicode: skip or strict"))
            })?)),
        }
    }
}

/// Whether the host's kernel runs a module, as [`selinux_module`] and [`apparmor_module`] find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Module {
    Active,
    /// Not run, and why that is known.
    Absent(String),
}

/// What a create does with `field` set to `value` where the kernel's `module` is as found: nothing more where
/// it runs (the label is applied, or refused by name if it can't be); where it doesn't, the warning to give
/// under [`Policy::Skip`], or the refusal by name under [`Policy::Strict`].
pub(crate) fn decide(
    field: &str,
    value: &str,
    module: &Module,
    policy: Policy,
) -> Result<Option<String>, OciError> {
    match (module, policy) {
        (Module::Active, _) => Ok(None),
        (Module::Absent(why), Policy::Skip) => Ok(Some(format!(
            "{field} ({value:?}) is skipped: {why}. The container runs without it, as crun runs one; \
             {ENV_POLICY}=strict refuses such a container instead"
        ))),
        (Module::Absent(why), Policy::Strict) => Err(OciError::Config(format!(
            "{field} is set ({value:?}) but {why}, and {ENV_POLICY}=strict refuses rather than run the \
             container without it"
        ))),
    }
}

/// What the exec attribute is written: `exec <profile>`, or `stack <profile>` where no_new_privs is set and
/// the process is already confined, the one change AppArmor allows it then (research §2).
pub(crate) fn apparmor_command(profile: &str, no_new_privileges: bool, confined: bool) -> Vec<u8> {
    let verb = if no_new_privileges && confined {
        "stack"
    } else {
        "exec"
    };
    format!("{verb} {profile}").into_bytes()
}

/// Whether `/sys/module/apparmor/parameters/enabled`'s contents say AppArmor is on: `Y`, as crun and runc
/// read it (research §3).
pub(crate) fn apparmor_enabled(parameter: &[u8]) -> bool {
    parameter.first() == Some(&b'Y')
}

/// Whether the AppArmor `current` attribute names a profile: anything but exactly `unconfined`, optionally
/// followed by a newline or a space (crun's `is_current_process_confined`, `src/libcrun/utils.c:1068-1105`).
pub(crate) fn confined(current: &[u8]) -> bool {
    match current.strip_prefix(b"unconfined") {
        Some(rest) => !matches!(rest.first(), None | Some(b'\n') | Some(b' ')),
        None => true,
    }
}

/// The AppArmor parameter crun and runc read for whether the module is on.
const APPARMOR_ENABLED: &str = "/sys/module/apparmor/parameters/enabled";

/// The scoped AppArmor attributes, and the legacy ones a kernel without them has (runc's `changeOnExec`).
const SCOPED: &CStr = c"/proc/thread-self/attr/apparmor";
const SCOPED_EXEC: &CStr = c"/proc/thread-self/attr/apparmor/exec";
const SCOPED_CURRENT: &CStr = c"/proc/thread-self/attr/apparmor/current";
const LEGACY_EXEC: &CStr = c"/proc/thread-self/attr/exec";
const LEGACY_CURRENT: &CStr = c"/proc/thread-self/attr/current";

/// The longest `current` attribute read: `unconfined` and one byte after it decide (see [`confined`]).
const CURRENT_BYTES: usize = 11;

/// A process's AppArmor profile, made ready before a clone: the attribute to write and what to write there.
/// [`ProfileOnExec::apply`] then makes system calls alone, so the child of a vfork-style clone may run it.
#[derive(Debug)]
pub(crate) struct ProfileOnExec {
    attr: &'static CStr,
    command: Vec<u8>,
}

/// What the kernel answers about AppArmor through `/proc` alone, so the answer holds in a container's init as
/// on the host, `/sys` mounted there or not: a kernel built with AppArmor has the scoped attributes (v6.19
/// `fs/proc/base.c:2915-2916`), and their `current` reads `EINVAL` where AppArmor isn't active, no module
/// answering for it (`security/security.c:3764-3774`). A kernel without them (before 5.1) is asked through the
/// parameter crun and runc read (research §3). Where it runs: the attribute to write, and whether this process
/// is already confined.
fn apparmor_probe() -> Result<Result<(&'static CStr, bool), String>, OciError> {
    let scoped = rustix::fs::access(SCOPED, rustix::fs::Access::EXISTS).is_ok();
    let (attr, current) = if scoped {
        (SCOPED_EXEC, SCOPED_CURRENT)
    } else {
        let on = match std::fs::read(APPARMOR_ENABLED) {
            Ok(bytes) => apparmor_enabled(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(OciError::os("reading the AppArmor parameter", e)),
        };
        if !on {
            return Ok(Err(format!(
                "AppArmor isn't enabled on this host ({APPARMOR_ENABLED} doesn't read Y)"
            )));
        }
        (LEGACY_EXEC, LEGACY_CURRENT)
    };
    let fd = open_proc(current, OFlags::RDONLY)
        .map_err(|e| OciError::os("opening the AppArmor current attribute", e))?;
    let mut buf = [0u8; CURRENT_BYTES];
    let n = match rustix::io::read(&fd, &mut buf) {
        Ok(n) => n,
        Err(rustix::io::Errno::INVAL) if scoped => {
            return Ok(Err(
                "AppArmor isn't enabled on this host (its current attribute reads EINVAL: the module isn't active)"
                    .to_owned(),
            ));
        }
        Err(e) => return Err(OciError::os("reading the AppArmor current attribute", e)),
    };
    Ok(Ok((attr, confined(buf.get(..n).unwrap_or_default()))))
}

/// Whether the host's kernel runs AppArmor ([`apparmor_probe`]).
pub(crate) fn apparmor_module() -> Result<Module, OciError> {
    Ok(match apparmor_probe()? {
        Ok(_) => Module::Active,
        Err(why) => Module::Absent(why),
    })
}

impl ProfileOnExec {
    /// `profile` made ready where AppArmor runs, `None` where it doesn't: the create found that first and,
    /// under [`Policy::Strict`], refused ([`decide`]), so a process becoming without it here is one its
    /// create warned of.
    pub(crate) fn of(profile: &str, no_new_privileges: bool) -> Result<Option<Self>, OciError> {
        Ok(apparmor_probe()?.ok().map(|(attr, confined)| Self {
            attr,
            command: apparmor_command(profile, no_new_privileges, confined),
        }))
    }

    /// Writes the exec attribute, so the profile goes on at the process's next `execve`. Its caller does
    /// this before the uid changes, while the task still owns its `/proc` files (research §2).
    pub(crate) fn apply(&self) -> Result<(), Failure> {
        let fd = open_proc(self.attr, OFlags::WRONLY)
            .map_err(Failure::of("opening the AppArmor exec attribute"))?;
        let written = rustix::io::write(&fd, &self.command).map_err(|e| match e {
            // AppArmor's answer for a profile it doesn't have (runc apparmor_linux.go:42-45).
            rustix::io::Errno::NOENT => Failure {
                what: "applying the AppArmor profile: it isn't loaded",
                errno: e,
            },
            e => Failure {
                what: "writing the AppArmor exec attribute",
                errno: e,
            },
        })?;
        if written != self.command.len() {
            return Err(Failure {
                what: "writing the AppArmor exec attribute: a short write",
                errno: rustix::io::Errno::IO,
            });
        }
        Ok(())
    }
}

/// Opens a `/proc` file and checks it is on procfs, so a container's own file at that path can't stand in
/// (crun's `check_proc_super_magic`, research §2).
fn open_proc(path: &CStr, flags: OFlags) -> rustix::io::Result<rustix::fd::OwnedFd> {
    let fd = rustix::fs::open(path, flags | OFlags::CLOEXEC, Mode::empty())?;
    if rustix::fs::fstatfs(&fd)?.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(rustix::io::Errno::XDEV);
    }
    Ok(fd)
}

/// The magic number of selinuxfs (`include/uapi/linux/magic.h`), mounted only where SELinux runs.
const SELINUX_MAGIC: i128 = 0xf97c_ff8c;

/// Whether the kernel runs SELinux, asked without a label: a tmpfs's context takes SELinux's `seclabel` flag,
/// which SELinux claims and ignores (v6.19 `security/selinux/hooks.c:2851`, `:1016-1018`), only where SELinux
/// runs; elsewhere no module claims it and tmpfs refuses it as unknown (`fs/fs_context.c:137-148`). The
/// `context` key itself can't tell, as an invalid label is refused there too (`:1059`). A rootless agent may
/// make no filesystem in its own namespace (fsopen(2): `EPERM` without CAP_SYS_ADMIN there), so it reads
/// whether selinuxfs is mounted at `/sys/fs/selinux` instead.
pub(crate) fn selinux_module() -> Result<Module, OciError> {
    let fs = match rustix::mount::fsopen("tmpfs", rustix::mount::FsOpenFlags::FSOPEN_CLOEXEC) {
        Ok(fs) => fs,
        Err(rustix::io::Errno::PERM) => {
            return Ok(match rustix::fs::statfs("/sys/fs/selinux") {
                Ok(st) if i128::from(st.f_type) & 0xffff_ffff == SELINUX_MAGIC => Module::Active,
                _ => Module::Absent(
                    "SELinux isn't active here (this unprivileged process can't ask the kernel, and no \
                     selinuxfs is mounted at /sys/fs/selinux)"
                        .to_owned(),
                ),
            });
        }
        Err(e) => return Err(OciError::os("fsopen", e)),
    };
    Ok(
        match rustix::mount::fsconfig_set_flag(fs.as_fd(), "seclabel") {
            Ok(()) => Module::Active,
            Err(rustix::io::Errno::INVAL) => Module::Absent(
                "SELinux isn't active here (no module claims a filesystem's seclabel option)"
                    .to_owned(),
            ),
            Err(e) => return Err(OciError::os("asking the kernel whether SELinux runs", e)),
        },
    )
}

/// Whether the host's kernel runs SELinux, asked as a create asks it ([`selinux_module`]), so a caller that
/// checks what a create did reads the same answer.
pub fn selinux_runs() -> Result<bool, OciError> {
    Ok(selinux_module()? == Module::Active)
}

/// Whether the host's kernel runs AppArmor, asked as a create asks it ([`apparmor_module`]).
pub fn apparmor_runs() -> Result<bool, OciError> {
    Ok(apparmor_module()? == Module::Active)
}

/// Asks the kernel for a tmpfs labelled `label` and drops it, where SELinux runs: a label it doesn't know is
/// refused by name here, before the create makes anything. A rootless agent can't ask (as [`selinux_module`]);
/// its container's init then sets the label on each filesystem it makes, or is refused by name there.
pub(crate) fn check_mount_label(label: &str) -> Result<(), OciError> {
    let fs = match rustix::mount::fsopen("tmpfs", rustix::mount::FsOpenFlags::FSOPEN_CLOEXEC) {
        Ok(fs) => fs,
        Err(rustix::io::Errno::PERM) => return Ok(()),
        Err(e) => return Err(OciError::os("fsopen", e)),
    };
    rustix::mount::fsconfig_set_string(fs.as_fd(), "context", label)
        .map_err(|e| refused_label(label, "tmpfs", e))?;
    rustix::mount::fsconfig_create(fs.as_fd()).map_err(|e| refused_label(label, "tmpfs", e))?;
    Ok(())
}

/// What a create does with its spec's labels on this host under `policy` ([`decide`]): the warnings to give
/// for labels skipped, or a refusal by name. Where SELinux runs, the mount label is checked with the kernel.
pub(crate) fn admit(spec: &super::spec::Spec, policy: Policy) -> Result<Vec<String>, OciError> {
    let mut warnings = Vec::new();
    if let Some(label) = spec.mount_label() {
        let module = selinux_module()?;
        if module == Module::Active {
            check_mount_label(label)?;
        }
        warnings.extend(decide("linux.mountLabel", label, &module, policy)?);
    }
    if let Some(profile) = spec.process.as_ref().and_then(|p| p.apparmor_profile()) {
        warnings.extend(decide(
            "process.apparmorProfile",
            profile,
            &apparmor_module()?,
            policy,
        )?);
    }
    Ok(warnings)
}

/// The refusal of a mount label the kernel wouldn't take as the context of a `fstype`.
pub(crate) fn refused_label(label: &str, fstype: &str, e: rustix::io::Errno) -> OciError {
    OciError::Config(format!(
        "linux.mountLabel is set ({label:?}) but the kernel, which runs SELinux, refused it as a {fstype}'s \
         context ({e}): the label isn't valid here, and Hex refuses rather than run the container without it"
    ))
}

/// Labels a filesystem by the `security.selinux` xattr on its root ([`Labelling::Xattr`]): `mount` is its
/// detached mount, its root opened to set the xattr, which a path-only descriptor can't.
pub(crate) fn label_root(mount: BorrowedFd<'_>, label: &str) -> Result<(), OciError> {
    let root = rustix::fs::openat(
        mount,
        c".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        OciError::os(
            "linux.mountLabel: opening a filesystem's root to label it",
            e,
        )
    })?;
    rustix::fs::fsetxattr(
        &root,
        "security.selinux",
        label.as_bytes(),
        XattrFlags::empty(),
    )
    .map_err(|e| {
        OciError::os(
            "linux.mountLabel: labelling a filesystem's root (security.selinux)",
            e,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC-T7.1.4 (linux_mount_label): ask how each filesystem takes the label, expect crun's answer
    // (src/libcrun/linux.c:3008-3018, :2038): proc, sysfs and cgroup none, mqueue by xattr, the rest by context.
    #[test]
    fn each_filesystem_takes_the_mount_label_as_crun_gives_it() {
        for fstype in ["proc", "sysfs", "cgroup", "cgroup2"] {
            assert_eq!(labelling(fstype), Labelling::None, "{fstype}");
        }
        assert_eq!(labelling("mqueue"), Labelling::Xattr);
        for fstype in ["tmpfs", "devpts", "overlay", "ramfs"] {
            assert_eq!(labelling(fstype), Labelling::Context, "{fstype}");
        }
    }

    // AC-T7.1.4 (all 28 tests crun passes, the lead's goal-17 bar): read HEX_OCI_LSM's words, expect unset and
    // `skip` to skip, `strict` to refuse, and any other word refused by name rather than read as either.
    #[test]
    fn the_policy_is_skip_unless_strict_is_asked_and_other_words_are_refused() {
        assert_eq!(Policy::parse(None).unwrap(), Policy::Skip);
        assert_eq!(Policy::parse(Some("skip")).unwrap(), Policy::Skip);
        assert_eq!(Policy::parse(Some("strict")).unwrap(), Policy::Strict);
        let e = Policy::parse(Some("Strict")).unwrap_err();
        assert!(
            matches!(&e, OciError::Config(why) if why.contains(ENV_POLICY)),
            "{e:?}"
        );
    }

    // AC-T7.1.4 and CLAUDE.md §3: decide for a label whose module runs, and for one whose module doesn't under
    // each policy. Expect nothing more where it runs; where it doesn't, a warning naming the field and value under
    // skip (never a silent drop), and a refusal naming them under strict.
    #[test]
    fn an_absent_module_is_warned_of_under_skip_and_refused_under_strict() {
        let absent = Module::Absent("AppArmor isn't enabled on this host".to_owned());
        for policy in [Policy::Skip, Policy::Strict] {
            assert_eq!(
                decide("process.apparmorProfile", "acme", &Module::Active, policy).unwrap(),
                None
            );
        }
        let warning = decide("process.apparmorProfile", "acme", &absent, Policy::Skip)
            .unwrap()
            .expect("a skipped label is warned of");
        assert!(
            warning.contains("process.apparmorProfile") && warning.contains("acme"),
            "{warning}"
        );
        assert!(warning.contains("AppArmor isn't enabled"), "{warning}");
        let e = decide("process.apparmorProfile", "acme", &absent, Policy::Strict).unwrap_err();
        assert!(
            matches!(&e, OciError::Config(why) if why.contains("process.apparmorProfile") && why.contains("acme")),
            "{e:?}"
        );
    }

    // AC-T7.1.4 (linux_process_apparmor_profile): make the exec attribute's text, expect `exec <profile>`, and
    // `stack <profile>` only where no_new_privs is set on a confined process (src/libcrun/utils.c:1121-1124).
    #[test]
    fn the_profile_is_stacked_only_under_no_new_privs_when_confined() {
        assert_eq!(apparmor_command("acme", false, false), b"exec acme");
        assert_eq!(apparmor_command("acme", false, true), b"exec acme");
        assert_eq!(apparmor_command("acme", true, false), b"exec acme");
        assert_eq!(apparmor_command("acme", true, true), b"stack acme");
    }

    // AC-T7.1.4 and CLAUDE.md §3: read the AppArmor parameter's contents, expect on only for `Y`, so a host
    // that reads anything else refuses the profile rather than drop it.
    #[test]
    fn apparmor_is_on_only_where_its_parameter_reads_y() {
        assert!(apparmor_enabled(b"Y\n"));
        assert!(!apparmor_enabled(b"N\n"));
        assert!(!apparmor_enabled(b""));
    }

    // AC-T7.1.4: read a `current` attribute, expect unconfined only for exactly `unconfined` (then nothing, a
    // newline or a space), so a profile named `unconfined_foo` or a short read counts as confined.
    #[test]
    fn only_the_unconfined_token_reads_unconfined() {
        assert!(!confined(b"unconfined"));
        assert!(!confined(b"unconfined\n"));
        assert!(!confined(b"unconfined "));
        assert!(confined(b"unconfined_"));
        assert!(confined(b"docker-def"));
        assert!(confined(b"unconf"));
    }
}
