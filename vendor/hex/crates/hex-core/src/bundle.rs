//! The structured [`OciSpec`] rendered to a runtime-spec bundle, which the `oci` class runs as it runs
//! any bundle (decision 3 of docs/design/runtime-architecture.md; the defaults and their sources are in
//! docs/design/runtime.md, "Decision: a box from an OciSpec runs as `docker run` runs its image").
//! [`render`] turns a spec, and where its bundle goes, into the files and directories the node writes,
//! and refuses by name what it can't render (CLAUDE.md §3).
//!
//! The container is what `docker run` makes of an image when nothing else is named, as far as the
//! runtime-spec carries it: Docker's capabilities, mounts, masked and read-only paths and namespaces,
//! and its generated `/etc/hosts`. A node that isn't root gives the container a user namespace mapping
//! itself to root, with runc's rootless changes. A [`Rootfs::Layers`] root is an overlay the container
//! mounts in its own mount namespace over a writable layer in the bundle, so nothing is mounted on the
//! host and the layer goes when the bundle does.

use std::borrow::Cow;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use hex_api::{Network, OciSpec, Refusal, Rootfs};
use serde::Serialize;

use crate::cdi;

mod device_edits;
pub use device_edits::ResolvedNode;

/// The runtime-spec version the rendered `config.json` is written to.
pub const OCI_VERSION: &str = "1.3.0";
/// The bundle's `config.json`.
pub const CONFIG: &str = "config.json";
/// The container's `/etc/hosts`, bound in from the bundle.
pub const HOSTS: &str = "hosts";
/// Where a [`Rootfs::Layers`] root is mounted, its writable layer, and overlayfs's work directory.
pub const ROOTFS: &str = "rootfs";
pub const UPPER: &str = "upper";
pub const WORK: &str = "work";

/// The most `config.json` the `oci` class reads (`hex_runtime`'s `spec::MAX_CONFIG_BYTES`): a spec
/// that renders larger is refused here rather than written for the runtime to refuse.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;
/// Most layers an overlay stacks: the kernel's `OVL_MAX_STACK` (fs/overlayfs/params.h).
pub const MAX_LAYERS: usize = 500;
/// Longest overlay option string: mount(2) copies one page of options (`copy_mount_options`,
/// fs/namespace.c), 4,096 bytes on the smallest page Hex runs on, and the string ends in a NUL.
pub const MAX_OVERLAY_OPTIONS: usize = 4095;

/// The capabilities a container's processes hold: Docker's defaults (moby
/// daemon/pkg/oci/caps/defaults.go), which containerd's default spec also gives
/// (pkg/oci/spec.go, `defaultUnixCaps`).
pub const CAPABILITIES: [&str; 14] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_MKNOD",
    "CAP_NET_RAW",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETFCAP",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_SYS_CHROOT",
    "CAP_KILL",
    "CAP_AUDIT_WRITE",
];

/// Paths hidden from the container, Docker's fixed list (daemon/pkg/oci/defaults.go,
/// `defaultLinuxMaskedPaths`); the host adds its CPUs' `thermal_throttle` ([`Place::masked`]).
pub const MASKED_PATHS: [&str; 12] = [
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/devices/virtual/powercap",
    "/sys/firmware",
];

/// Paths the container sees read-only, Docker's (daemon/pkg/oci/defaults.go).
pub const READONLY_PATHS: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// The lines every container's `/etc/hosts` starts with, Docker's (daemon/libnetwork/etchosts,
/// `defaultContentIPv4` and `defaultContentIPv6`): an address and the names it resolves.
pub const HOSTS_DEFAULTS: [(&str, &str); 6] = [
    ("127.0.0.1", "localhost"),
    ("::1", "localhost ip6-localhost ip6-loopback"),
    ("fe00::", "ip6-localnet"),
    ("ff00::", "ip6-mcastprefix"),
    ("ff02::1", "ip6-allnodes"),
    ("ff02::2", "ip6-allrouters"),
];

/// Who the node agent runs as, which decides the user namespace its containers get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Root: the container shares the host's user namespace, as `docker run`'s does.
    Root,
    /// Any other user: the container gets a user namespace of its own that maps this uid and gid,
    /// alone, to root, as runc's rootless containers do (libcontainer/specconv, `ToRootless`).
    Rootless { uid: u32, gid: u32 },
}

/// Where a bundle is rendered, and for whom.
#[derive(Debug, Clone, Copy)]
pub struct Place<'a> {
    /// The box's bundle directory, which holds what [`render`] returns.
    pub dir: &'a Path,
    pub owner: Owner,
    /// Paths to mask that only the host knows: each of its CPUs' `thermal_throttle`, which Docker
    /// and containerd mask too (GHSA-6fw5-f8r9-fgfm).
    pub masked: &'a [String],
}

/// A rendered bundle: what to write in its directory, and the directories to make there first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// `config.json`'s bytes.
    pub config: Vec<u8>,
    /// The container's `/etc/hosts`, written to [`HOSTS`] and bound in.
    pub hosts: Vec<u8>,
    /// For a [`Rootfs::Layers`] root, where it's mounted, its writable layer and overlayfs's work
    /// directory, in the bundle; nothing for a [`Rootfs::Dir`], which is used in place.
    pub dirs: Vec<PathBuf>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Config<'a> {
    oci_version: &'static str,
    root: Root,
    #[serde(skip_serializing_if = "Option::is_none")]
    hostname: Option<&'a str>,
    mounts: Vec<Mount<'a>>,
    process: Process<'a>,
    linux: Linux<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hooks: Option<device_edits::Hooks<'a>>,
}

#[derive(Serialize)]
struct Root {
    path: String,
    readonly: bool,
}

#[derive(Serialize)]
struct Mount<'a> {
    destination: Cow<'a, str>,
    #[serde(rename = "type", skip_serializing_if = "str::is_empty")]
    kind: &'a str,
    source: Cow<'a, str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    options: Cow<'a, [String]>,
    /// Temporary stable-sort keys; never serialized or retained beyond rendering.
    #[serde(skip)]
    order: (usize, usize),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Process<'a> {
    terminal: bool,
    user: User<'a>,
    args: &'a [String],
    env: Vec<Cow<'a, str>>,
    cwd: &'a str,
    capabilities: Capabilities,
    no_new_privileges: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct User<'a> {
    uid: u32,
    gid: u32,
    #[serde(skip_serializing_if = "<[u32]>::is_empty")]
    additional_gids: Cow<'a, [u32]>,
}

#[derive(Serialize)]
struct Capabilities {
    bounding: [&'static str; 14],
    effective: [&'static str; 14],
    permitted: [&'static str; 14],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Linux<'a> {
    namespaces: Vec<Namespace>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uid_mappings: Vec<IdMapping>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    gid_mappings: Vec<IdMapping>,
    resources: Resources<'a>,
    masked_paths: Vec<String>,
    readonly_paths: [&'static str; 5],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    devices: Vec<device_edits::Device<'a>>,
}

#[derive(Serialize)]
struct Namespace {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct IdMapping {
    #[serde(rename = "containerID")]
    container_id: u32,
    #[serde(rename = "hostID")]
    host_id: u32,
    size: u32,
}

#[derive(Serialize)]
struct Resources<'a> {
    devices: Vec<DeviceRule<'a>>,
}

#[derive(Serialize)]
struct DeviceRule<'a> {
    allow: bool,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    major: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minor: Option<i64>,
    access: &'a str,
}

/// `spec` as a bundle in `at.dir`: `config.json`, the container's `/etc/hosts`, and the directories a
/// layered root needs. Refused by name, with nothing to write, when a value can't be rendered: one
/// [`OciSpec::check`] refuses, a user a rootless node can't map, a layered root past overlayfs's
/// bounds, or a config past the runtime's.
pub fn render(spec: &OciSpec, at: &Place<'_>) -> Result<Bundle, Refusal> {
    render_with_devices(spec, at, &cdi::Edits::default(), &[])
}

/// Render selected CDI edits using caller-owned host metadata. This performs no device lookup,
/// mount or hook execution. Unsupported native edits and invalid resolutions return no bundle.
pub fn render_with_devices(
    spec: &OciSpec,
    at: &Place<'_>,
    edits: &cdi::Edits<'_>,
    resolved: &[ResolvedNode<'_>],
) -> Result<Bundle, Refusal> {
    device_edits::check(edits, resolved)?;
    spec.check()?;
    let dir = utf8(at.dir, "the bundle directory")?;
    let rootless = matches!(at.owner, Owner::Rootless { .. });
    let user = spec.init.user.clone().unwrap_or_default();
    if rootless && (user.uid != 0 || user.gid != 0 || !user.groups.is_empty()) {
        return Err(invalid(format!(
            "init.user: uid {}, gid {} and groups {:?} can't be mapped: a node that isn't root maps \
             only itself, as root, into its containers",
            user.uid, user.gid, user.groups
        )));
    }

    let mut mounts = Vec::new();
    let (root, dirs) = match &spec.rootfs {
        Rootfs::Dir(rootfs) => (utf8(rootfs, "rootfs.dir")?.to_owned(), Vec::new()),
        Rootfs::Layers(layers) => {
            let (overlay, dirs) = overlay(layers, at.dir, rootless)?;
            mounts.push(overlay);
            (format!("{dir}/{ROOTFS}"), dirs)
        }
    };
    mounts.extend(default_mounts(at.owner, spec.network));
    mounts.push(Mount {
        destination: "/etc/hosts".into(),
        kind: "bind",
        source: format!("{dir}/{HOSTS}").into(),
        options: vec!["bind".to_owned()].into(),
        order: (0, 0),
    });
    for m in &spec.mounts {
        mounts.push(Mount {
            destination: m.destination.clone().into(),
            kind: "bind",
            source: utf8(&m.source, "a mount source")?.to_owned().into(),
            // A read-only mount binds the source's own mount alone: a recursive bind remounted
            // read-only would leave the mounts beneath it writable.
            options: if m.read_only {
                vec!["bind".to_owned(), "ro".to_owned()]
            } else {
                vec!["rbind".to_owned()]
            }
            .into(),
            order: (0, 0),
        });
    }

    let mut namespaces = vec![
        Namespace { kind: "pid" },
        Namespace { kind: "ipc" },
        Namespace { kind: "uts" },
        Namespace { kind: "mount" },
        Namespace { kind: "cgroup" },
    ];
    if spec.network == Network::Private {
        namespaces.push(Namespace { kind: "network" });
    }
    let (uid_mappings, gid_mappings) = match at.owner {
        Owner::Root => (Vec::new(), Vec::new()),
        Owner::Rootless { uid, gid } => {
            namespaces.push(Namespace { kind: "user" });
            (vec![to_root(uid)], vec![to_root(gid)])
        }
    };
    let mut masked_paths: Vec<String> = MASKED_PATHS.iter().map(|p| (*p).to_owned()).collect();
    masked_paths.extend(at.masked.iter().cloned());

    let mut config = Config {
        oci_version: OCI_VERSION,
        root: Root {
            path: root,
            readonly: false,
        },
        hostname: spec.hostname.as_deref(),
        mounts,
        process: Process {
            terminal: false,
            user: User {
                uid: user.uid,
                gid: user.gid,
                additional_gids: Cow::Borrowed(&user.groups),
            },
            args: &spec.init.args,
            env: spec
                .init
                .env
                .iter()
                .map(|(k, v)| Cow::Owned(format!("{k}={v}")))
                .collect(),
            cwd: spec.init.cwd.as_deref().unwrap_or("/"),
            capabilities: Capabilities {
                bounding: CAPABILITIES,
                effective: CAPABILITIES,
                permitted: CAPABILITIES,
            },
            // Docker's default: a setuid program in the image works as under `docker run`.
            no_new_privileges: false,
        },
        linux: Linux {
            namespaces,
            uid_mappings,
            gid_mappings,
            // Deny by default; the runtime allows the devices every container has after it, as runc
            // and crun do.
            resources: Resources {
                devices: device_edits::default_rules()?,
            },
            masked_paths,
            readonly_paths: READONLY_PATHS,
            devices: Vec::new(),
        },
        hooks: None,
    };
    device_edits::apply(&mut config, edits, resolved)?;
    let config = device_edits::encode(&config)?;
    Ok(Bundle {
        config,
        hosts: hosts(&spec.hosts)?,
        dirs,
    })
}

/// The overlay a layered root is: mounted at `/` first of the container's mounts, so every other
/// lands on it, with the bundle's writable layer over `layers`. overlayfs stacks its lower
/// directories leftmost on top (Documentation/filesystems/overlayfs.rst, "Multiple lower layers"),
/// and the spec lists them lowest first. A rootless container mounts it in its own user namespace,
/// where overlayfs keeps its attributes as `user.overlay.` (`userxattr`).
fn overlay(
    layers: &[PathBuf],
    bundle: &Path,
    rootless: bool,
) -> Result<(Mount<'static>, Vec<PathBuf>), Refusal> {
    if layers.len() > MAX_LAYERS {
        return Err(invalid(format!(
            "rootfs.layers holds {} layers, over overlayfs's {MAX_LAYERS}",
            layers.len()
        )));
    }
    let mut lower = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate().rev() {
        lower.push(option_path(layer, &format!("rootfs.layers[{i}]"))?);
    }
    let rootfs = linux_join(bundle, ROOTFS);
    let upper = linux_join(bundle, UPPER);
    let work = linux_join(bundle, WORK);
    let mut options = vec![
        format!("lowerdir={}", lower.join(":")),
        format!(
            "upperdir={}",
            option_path(&upper, "the box's writable layer")?
        ),
        format!(
            "workdir={}",
            option_path(&work, "the box's overlay work directory")?
        ),
    ];
    if rootless {
        options.push("userxattr".to_owned());
    }
    let length = options.join(",").len();
    if length > MAX_OVERLAY_OPTIONS {
        return Err(invalid(format!(
            "rootfs.layers: the overlay's options are {length} bytes, over the \
             {MAX_OVERLAY_OPTIONS} a mount(2) call carries"
        )));
    }
    let mount = Mount {
        destination: "/".into(),
        kind: "overlay",
        source: "overlay".into(),
        options: options.into(),
        order: (0, 0),
    };
    Ok((mount, vec![rootfs, upper, work]))
}

/// `dir/name` as the Linux that mounts it reads a path: joined by `/` on every host, since
/// `Path::join` uses the host's (`\` on Windows), which overlayfs's options read as a separator.
/// `dir` keeps its bytes as given.
fn linux_join(dir: &Path, name: &str) -> PathBuf {
    let mut path = dir.as_os_str().to_owned();
    if dir.as_os_str().as_encoded_bytes().last() != Some(&b'/') {
        path.push("/");
    }
    path.push(name);
    PathBuf::from(path)
}

/// The mounts every container gets, Docker's (daemon/pkg/oci/defaults.go) with containerd's sized
/// `/dev/shm` (pkg/oci/mounts.go), the 64 MiB Docker's `--shm-size` defaults to, and the cgroup
/// filesystem named as the version Hex runs (`cgroup2`). A container in a user namespace of its own
/// has no gid 5 to give its terminals, and can't mount a sysfs while it shares the host's network
/// namespace, so it binds the host's instead, both as runc's rootless containers do
/// (libcontainer/specconv, `ToRootless`). That bind must be recursive (a user namespace refuses a
/// plain bind of a mount with locked mounts beneath it), so it carries the host's cgroup filesystem
/// beneath the read-only `/sys`. A box's processes run as the node's uid, which owns every box's
/// cgroup files, so the runtime remounts that carried cgroup filesystem read-only too (a recursive
/// bind's read-only covers only its top; `hex_runtime`'s init `remount_cgroupfs_readonly`): else a box
/// holding `CAP_SYS_ADMIN` in its user namespace could reach every box's cgroup files by unmounting
/// its own cgroup mounts off it (T-T7.1.4). The container's own cgroup filesystem goes over it, so it
/// sees its own box's cgroups and no other's, with a read-only tmpfs between: mount(2) refuses a
/// filesystem over the same filesystem's root (fs/namespace.c, "Refuse the same filesystem on the
/// same mount point").
fn default_mounts(owner: Owner, network: Network) -> Vec<Mount<'static>> {
    let rootless = matches!(owner, Owner::Rootless { .. });
    let fs = |destination: &str, kind: &'static str, source: &str, options: &[&str]| Mount {
        destination: destination.to_owned().into(),
        kind,
        source: source.to_owned().into(),
        options: options
            .iter()
            .map(|o| (*o).to_owned())
            .collect::<Vec<_>>()
            .into(),
        order: (0, 0),
    };
    let mut pts = vec![
        "nosuid",
        "noexec",
        "newinstance",
        "ptmxmode=0666",
        "mode=0620",
    ];
    if !rootless {
        pts.push("gid=5");
    }
    let sys = if rootless && network == Network::Host {
        vec![
            fs(
                "/sys",
                "bind",
                "/sys",
                &["rbind", "nosuid", "noexec", "nodev", "ro"],
            ),
            fs(
                "/sys/fs/cgroup",
                "tmpfs",
                "tmpfs",
                &["nosuid", "noexec", "nodev", "ro", "mode=755"],
            ),
        ]
    } else {
        vec![fs(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        )]
    };
    let mut mounts = vec![
        fs("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"]),
        fs(
            "/dev",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        ),
        fs("/dev/pts", "devpts", "devpts", &pts),
        fs(
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        ),
        fs(
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"],
        ),
    ];
    mounts.extend(sys);
    mounts.push(fs(
        "/sys/fs/cgroup",
        "cgroup2",
        "cgroup",
        &["nosuid", "noexec", "nodev", "ro"],
    ));
    mounts
}

/// The container's `/etc/hosts`: Docker's lines, then the spec's, each an address and its name
/// (`<address>\t<name>`, as moby's etchosts writes them), the address in its canonical form.
fn hosts(lines: &[(String, String)]) -> Result<Vec<u8>, Refusal> {
    let mut out = String::new();
    for (address, names) in HOSTS_DEFAULTS {
        out.push_str(&format!("{address}\t{names}\n"));
    }
    for (i, (name, address)) in lines.iter().enumerate() {
        let address: IpAddr = address.parse().map_err(|_| {
            invalid(format!(
                "hosts[{i}].address {address:?} isn't an IPv4 or IPv6 address"
            ))
        })?;
        out.push_str(&format!("{address}\t{name}\n"));
    }
    Ok(out.into_bytes())
}

/// A path written into the overlay's options: UTF-8, and holding none of the characters overlayfs
/// reads as its own (`,` between options, `:` between layers, `\` escaping either).
fn option_path(path: &Path, what: &str) -> Result<String, Refusal> {
    let text = utf8(path, what)?;
    if let Some(c) = text.chars().find(|c| matches!(c, ',' | ':' | '\\')) {
        return Err(invalid(format!(
            "{what} {text:?} holds {c:?}, which overlayfs reads as a separator"
        )));
    }
    Ok(text.to_owned())
}

/// A user namespace mapping of one host id to root.
fn to_root(host_id: u32) -> IdMapping {
    IdMapping {
        container_id: 0,
        host_id,
        size: 1,
    }
}

fn utf8<'p>(path: &'p Path, what: &str) -> Result<&'p str, Refusal> {
    path.to_str()
        .ok_or_else(|| invalid(format!("{what} {path:?} isn't UTF-8")))
}

fn invalid(why: String) -> Refusal {
    Refusal::Invalid {
        what: "oci spec",
        why,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{Mount as Bind, Program, User as Who};
    use serde_json::Value;

    const BUNDLE: &str = "/var/lib/hex/bundles/box-1";
    const ROOTLESS: Owner = Owner::Rootless {
        uid: 1000,
        gid: 1000,
    };

    /// The plan step's example: layers [base, app], init `sleep infinity`, one bind mount and a
    /// private network.
    fn layered() -> OciSpec {
        OciSpec {
            rootfs: Rootfs::Layers(vec!["/images/base".into(), "/images/app".into()]),
            init: Program::of(&["sleep", "infinity"]),
            mounts: vec![Bind {
                source: "/srv/data".into(),
                destination: "/data".to_owned(),
                read_only: true,
            }],
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        }
    }

    /// Every other field set: a directory root, the host's network, a hostname, a user with groups, a
    /// working folder, a read-only and a read-write mount, and lines for `/etc/hosts`.
    fn in_place() -> OciSpec {
        let mut init = Program::of(&["/bin/sh", "-c", "exec sleep infinity"]);
        init.env.push(("LANG".to_owned(), "C.UTF-8".to_owned()));
        init.cwd = Some("/work".to_owned());
        init.user = Some(Who {
            uid: 1000,
            gid: 1000,
            groups: vec![10, 20],
        });
        OciSpec {
            rootfs: Rootfs::Dir("/srv/rootfs".into()),
            init,
            mounts: vec![
                Bind {
                    source: "/srv/cache".into(),
                    destination: "/cache".to_owned(),
                    read_only: true,
                },
                Bind {
                    source: "/srv/work".into(),
                    destination: "/work".to_owned(),
                    read_only: false,
                },
            ],
            hostname: Some("box-2".to_owned()),
            network: Network::Host,
            hosts: vec![
                ("db_main".to_owned(), "10.0.0.2".to_owned()),
                ("cache".to_owned(), "fd00:0:0::2".to_owned()),
            ],
        }
    }

    fn at(owner: Owner, masked: &[String]) -> Place<'_> {
        Place {
            dir: Path::new(BUNDLE),
            owner,
            masked,
        }
    }

    /// The first field where `got` differs from `want`, as a JSON pointer, walking both trees with a
    /// stack rather than recursion (CLAUDE.md §11).
    fn first_difference(want: &Value, got: &Value) -> Option<String> {
        let mut stack = vec![(String::new(), want, got)];
        while let Some((at, want, got)) = stack.pop() {
            match (want, got) {
                (Value::Object(w), Value::Object(g)) => {
                    for key in w.keys().chain(g.keys().filter(|k| !w.contains_key(*k))) {
                        match (w.get(key), g.get(key)) {
                            (Some(wv), Some(gv)) => stack.push((format!("{at}/{key}"), wv, gv)),
                            (Some(_), None) => return Some(format!("{at}/{key} is missing")),
                            (None, _) => return Some(format!("{at}/{key} isn't expected")),
                        }
                    }
                }
                (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
                    for (i, (wv, gv)) in w.iter().zip(g).enumerate() {
                        stack.push((format!("{at}/{i}"), wv, gv));
                    }
                }
                _ if want != got => return Some(format!("{at}: want {want}, got {got}")),
                _ => {}
            }
        }
        None
    }

    fn golden(name: &str, rendered: &Bundle) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/bundle")
            .join(name);
        let want: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let got: Value = serde_json::from_slice(&rendered.config).unwrap();
        if let Some(diff) = first_difference(&want, &got) {
            panic!(
                "{name}: {diff}\nrendered:\n{}",
                String::from_utf8_lossy(&rendered.config)
            );
        }
    }

    /// AC-T7.1.1: the plan step's example, rootless: the overlay is the first mount, its lower
    /// directories top first, with `userxattr`; the user namespace maps the node to root; the
    /// network namespace is the box's own; the bind is read-only and not recursive. The bundle's
    /// directories are Linux paths, `/`-joined on every host (CLAUDE.md §12: the core renders a Linux
    /// container's bundle the same on Windows, where `Path::join` would put `\` in them).
    #[test]
    fn layers_with_a_private_network_render_rootless_to_their_golden_config() {
        let b = render(&layered(), &at(ROOTLESS, &[])).unwrap();
        golden("layers-private-rootless.json", &b);
        assert_eq!(
            b.dirs,
            ["rootfs", "upper", "work"].map(|d| PathBuf::from(format!("{BUNDLE}/{d}")))
        );
    }

    /// AC-T7.1.1: the same spec on a root node: no user namespace, the terminals' gid 5, and an
    /// overlay with the kernel's own attributes (no `userxattr`).
    #[test]
    fn layers_with_a_private_network_render_as_root_to_their_golden_config() {
        let b = render(&layered(), &at(Owner::Root, &[])).unwrap();
        golden("layers-private-root.json", &b);
    }

    /// AC-T7.1.1: a directory root used in place (no overlay, nothing to make), the host's network
    /// (no network namespace), a hostname, a user with groups, a working folder, both kinds of bind,
    /// and the host's own masked path after Docker's.
    #[test]
    fn a_directory_on_the_hosts_network_renders_as_root_to_its_golden_config() {
        let masked = ["/sys/devices/system/cpu/cpu0/thermal_throttle".to_owned()];
        let b = render(&in_place(), &at(Owner::Root, &masked)).unwrap();
        golden("dir-host-root.json", &b);
        assert!(b.dirs.is_empty(), "a directory root is used in place");
    }

    /// AC-T7.1.1: rootless on the host's network, a container can't mount a sysfs of its own, so it
    /// binds the host's read-only, as runc's rootless containers do, with its own cgroup filesystem
    /// over the bind's, a read-only tmpfs between them.
    #[test]
    fn a_directory_on_the_hosts_network_renders_rootless_to_its_golden_config() {
        let mut spec = in_place();
        spec.init.user = None;
        let b = render(&spec, &at(ROOTLESS, &[])).unwrap();
        golden("dir-host-rootless.json", &b);
    }

    /// The container's `/etc/hosts` is Docker's lines and then the spec's, address first, each
    /// address in its canonical form.
    #[test]
    fn the_hosts_file_is_dockers_lines_then_the_specs() {
        let b = render(&in_place(), &at(Owner::Root, &[])).unwrap();
        assert_eq!(
            String::from_utf8(b.hosts).unwrap(),
            "127.0.0.1\tlocalhost\n\
             ::1\tlocalhost ip6-localhost ip6-loopback\n\
             fe00::\tip6-localnet\n\
             ff00::\tip6-mcastprefix\n\
             ff02::1\tip6-allnodes\n\
             ff02::2\tip6-allrouters\n\
             10.0.0.2\tdb_main\n\
             fd00::2\tcache\n"
        );
    }

    fn refused(spec: &OciSpec, place: &Place<'_>, field: &str) {
        match render(spec, place) {
            Err(Refusal::Invalid {
                what: "oci spec",
                why,
            }) => assert!(why.contains(field), "{field}: refused as {why:?}"),
            other => panic!("{field}: {other:?}"),
        }
    }

    /// CLAUDE.md §3: what a node can't render is refused by name with nothing to write: a value
    /// `OciSpec::check` refuses, a user a rootless node can't map, layers past overlayfs's bounds or
    /// with its separators in their paths, and a config past the runtime's bound.
    #[test]
    fn hostile_values_are_refused_by_name() {
        let mut s = layered();
        s.mounts[0].destination = "data".to_owned();
        refused(&s, &at(ROOTLESS, &[]), "mounts[0].destination");

        let s = in_place();
        refused(&s, &at(ROOTLESS, &[]), "init.user");
        let mut s = in_place();
        s.init.user = Some(Who {
            uid: 0,
            gid: 0,
            groups: vec![10],
        });
        refused(&s, &at(ROOTLESS, &[]), "init.user");

        let mut s = layered();
        s.rootfs = Rootfs::Layers((0..=MAX_LAYERS).map(|i| format!("/l/{i}").into()).collect());
        refused(&s, &at(Owner::Root, &[]), "over overlayfs's 500");

        let mut s = layered();
        s.rootfs = Rootfs::Layers(
            (0..250)
                .map(|i| format!("/images/layer-{i:04}").into())
                .collect(),
        );
        refused(
            &s,
            &at(Owner::Root, &[]),
            "over the 4095 a mount(2) call carries",
        );

        for bad in ["/images/a,upperdir=/etc", "/images/a:b", "/images/a\\"] {
            let mut s = layered();
            s.rootfs = Rootfs::Layers(vec!["/images/base".into(), bad.into()]);
            refused(&s, &at(Owner::Root, &[]), "rootfs.layers[1]");
        }

        let place = Place {
            dir: Path::new("/var/lib/hex,x/bundles/box-1"),
            owner: Owner::Root,
            masked: &[],
        };
        refused(&layered(), &place, "the box's writable layer");

        let mut s = in_place();
        s.init
            .env
            .push(("BIG".to_owned(), "x".repeat(MAX_CONFIG_BYTES)));
        refused(
            &s,
            &at(Owner::Root, &[]),
            "over the 1048576 the runtime reads",
        );
    }

    /// A layered root at overlayfs's own bounds renders: 500 layers whose options fit a page.
    #[test]
    fn layers_at_overlayfs_bounds_render() {
        let mut s = layered();
        s.rootfs = Rootfs::Layers((0..MAX_LAYERS).map(|i| format!("/{i}").into()).collect());
        let b = render(&s, &at(Owner::Root, &[])).unwrap();
        let config: Value = serde_json::from_slice(&b.config).unwrap();
        let lower = config["mounts"][0]["options"][0].as_str().unwrap();
        assert!(lower.starts_with("lowerdir=/499:/498:"), "{lower}");
        assert!(lower.ends_with(":/1:/0"), "{lower}");
    }

    /// The bundle directory isn't UTF-8: config.json can't name it, so it's refused, not written
    /// lossily.
    #[cfg(unix)]
    #[test]
    fn a_bundle_directory_that_isnt_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let place = Place {
            dir: Path::new(std::ffi::OsStr::from_bytes(b"/var/lib/hex/\xff")),
            owner: Owner::Root,
            masked: &[],
        };
        refused(&layered(), &place, "the bundle directory");
    }
}
