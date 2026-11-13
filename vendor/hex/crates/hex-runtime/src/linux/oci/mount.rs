//! Turning a spec's mounts into mount operations, which the container init carries out in its
//! mount namespace, each at its destination resolved inside the container's root ([`super::inroot`]).
//! The option parsing is pure and tested; the imperative apply sits on rustix's mount calls.

use rustix::mount::{MountFlags, MountPropagationFlags};

use crate::linux::oci::OciError;
use crate::linux::oci::spec::Mount;

/// A mount's options, parsed into what the kernel's `mount(2)` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub flags: MountFlags,
    /// Set when the options name a propagation (private/slave/shared/unbindable), applied by a
    /// second `mount_change` after the mount.
    pub propagation: Option<MountPropagationFlags>,
    /// The leftover `key=value` options, comma-joined, passed as the mount's data.
    pub data: String,
    pub is_bind: bool,
    pub recursive_bind: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            flags: MountFlags::empty(),
            propagation: None,
            data: String::new(),
            is_bind: false,
            recursive_bind: false,
        }
    }
}

/// Parses a mount's options. Flag words become [`MountFlags`]; propagation words become a
/// propagation flag; everything else (`mode=755`, `size=…`, `gid=5`, SELinux `context=…`) is data.
pub fn parse_options(options: &[String]) -> Options {
    let mut out = Options::default();
    let mut data: Vec<&str> = Vec::new();
    for opt in options {
        match opt.as_str() {
            "ro" => out.flags |= MountFlags::RDONLY,
            "rw" => out.flags.remove(MountFlags::RDONLY),
            "nosuid" => out.flags |= MountFlags::NOSUID,
            "suid" => out.flags.remove(MountFlags::NOSUID),
            "nodev" => out.flags |= MountFlags::NODEV,
            "dev" => out.flags.remove(MountFlags::NODEV),
            "noexec" => out.flags |= MountFlags::NOEXEC,
            "exec" => out.flags.remove(MountFlags::NOEXEC),
            "sync" => out.flags |= MountFlags::SYNCHRONOUS,
            "dirsync" => out.flags |= MountFlags::DIRSYNC,
            "relatime" => out.flags |= MountFlags::RELATIME,
            "norelatime" => out.flags.remove(MountFlags::RELATIME),
            "noatime" => out.flags |= MountFlags::NOATIME,
            "strictatime" => out.flags |= MountFlags::STRICTATIME,
            "nodiratime" => out.flags |= MountFlags::NODIRATIME,
            "nosymfollow" => out.flags |= MountFlags::NOSYMFOLLOW,
            "bind" => out.is_bind = true,
            "rbind" => {
                out.is_bind = true;
                out.recursive_bind = true;
            }
            "private" => out.propagation = Some(MountPropagationFlags::PRIVATE),
            "rprivate" => {
                out.propagation = Some(MountPropagationFlags::PRIVATE | MountPropagationFlags::REC)
            }
            "slave" => out.propagation = Some(MountPropagationFlags::DOWNSTREAM),
            "rslave" => {
                out.propagation =
                    Some(MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC)
            }
            "shared" => out.propagation = Some(MountPropagationFlags::SHARED),
            "rshared" => {
                out.propagation = Some(MountPropagationFlags::SHARED | MountPropagationFlags::REC)
            }
            "unbindable" => out.propagation = Some(MountPropagationFlags::UNBINDABLE),
            "runbindable" => {
                out.propagation =
                    Some(MountPropagationFlags::UNBINDABLE | MountPropagationFlags::REC)
            }
            "defaults" | "rw-" => {}
            other => data.push(other),
        }
    }
    out.data = data.join(",");
    out
}

/// Refuses a destination with a `..` component: a mount names where it goes plainly.
fn check_destination(destination: &str) -> Result<(), OciError> {
    if destination.split('/').any(|part| part == "..") {
        return Err(OciError::Config(format!(
            "mount destination {destination:?} has a '..' component"
        )));
    }
    Ok(())
}

/// The kind of mount to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A filesystem of this type (`proc`, `sysfs`, `tmpfs`, `mqueue`, `cgroup2`, …).
    Fs(&'a str),
    /// A bind of a host path.
    Bind(&'a str),
}

/// One mount to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    /// Where it goes in the container: resolved inside the root as the mount is made.
    pub destination: String,
    pub kind_source: String,
    pub is_bind: bool,
    pub recursive_bind: bool,
    pub fstype: String,
    pub flags: MountFlags,
    pub propagation: Option<MountPropagationFlags>,
    pub data: String,
}

/// The ordered mount operations for a spec's mounts.
pub fn plan(mounts: &[Mount]) -> Result<Vec<Op>, OciError> {
    let mut ops = Vec::with_capacity(mounts.len());
    for m in mounts {
        check_destination(&m.destination)?;
        let opts = parse_options(&m.options);
        let fstype = m.kind.clone().unwrap_or_default();
        let is_bind = opts.is_bind || fstype == "bind";
        ops.push(Op {
            destination: m.destination.clone(),
            kind_source: m.source.clone().unwrap_or_default(),
            is_bind,
            recursive_bind: opts.recursive_bind,
            fstype: if is_bind { String::new() } else { fstype },
            flags: opts.flags,
            propagation: opts.propagation,
            data: opts.data,
        });
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(words: &[&str]) -> Options {
        parse_options(&words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn flag_words_become_mount_flags() {
        let o = opts(&["nosuid", "nodev", "noexec", "ro", "relatime"]);
        assert!(o.flags.contains(
            MountFlags::NOSUID
                | MountFlags::NODEV
                | MountFlags::NOEXEC
                | MountFlags::RDONLY
                | MountFlags::RELATIME
        ));
        assert!(o.data.is_empty());
        assert!(!o.is_bind);
    }

    #[test]
    fn rw_and_exec_clear_their_flags() {
        let o = opts(&["ro", "rw", "noexec", "exec"]);
        assert!(!o.flags.contains(MountFlags::RDONLY));
        assert!(!o.flags.contains(MountFlags::NOEXEC));
    }

    #[test]
    fn unknown_options_become_data_and_bind_and_propagation_are_recognised() {
        let o = opts(&["rbind", "rprivate", "mode=1777", "size=65536k", "gid=5"]);
        assert!(o.is_bind && o.recursive_bind);
        assert_eq!(
            o.propagation,
            Some(MountPropagationFlags::PRIVATE | MountPropagationFlags::REC)
        );
        assert_eq!(o.data, "mode=1777,size=65536k,gid=5");
    }

    #[test]
    fn a_destination_with_a_parent_component_is_refused() {
        let at = |destination: &str| Mount {
            destination: destination.into(),
            kind: Some("tmpfs".into()),
            source: Some("tmpfs".into()),
            options: vec![],
        };
        assert!(plan(&[at("/proc")]).is_ok());
        assert!(plan(&[at("/../etc")]).is_err());
        assert!(plan(&[at("/a/../../etc")]).is_err());
    }

    #[test]
    fn a_plan_marks_binds_and_filesystems() {
        let mounts = vec![
            Mount {
                destination: "/proc".into(),
                kind: Some("proc".into()),
                source: Some("proc".into()),
                options: vec![],
            },
            Mount {
                destination: "/data".into(),
                kind: Some("bind".into()),
                source: Some("/host/data".into()),
                options: vec!["rbind".into(), "ro".into()],
            },
        ];
        let ops = plan(&mounts).unwrap();
        assert_eq!(ops[0].fstype, "proc");
        assert!(!ops[0].is_bind);
        assert!(ops[1].is_bind && ops[1].recursive_bind);
        assert!(ops[1].flags.contains(MountFlags::RDONLY));
        assert_eq!(ops[1].kind_source, "/host/data");
    }
}
