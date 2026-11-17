use std::io::{PipeReader, PipeWriter};
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runtime::{Allocation, CapacityResources};
use crate::{Bounds, BoxId, DeviceId, Refusal};

/// What a box holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Host processes in a resource bound: a cgroup on Linux, QoS and limits on macOS, a job object
    /// on Windows.
    Process,
    /// An OCI container: namespaces, cgroups, seccomp, rootless through user namespaces (Linux).
    Oci,
    /// A Linux KVM microVM run by an initialized Firecracker backend.
    Firecracker,
    /// A microVM run by an initialized Shards backend.
    Shards,
    /// A Shards image VM ([`ClassSpec::ShardsImage`]): an Agentfile image Shards' run host boots under shards-init
    /// (Shards' D231), a microVM of its own, counted apart from Hex's own Shards guests. Listed only where the node's
    /// image boot is configured and Shards' check of its host (`shards_api::host_check`, D262) says it runs microVMs
    /// (kiln plan 01a1dd2e…, P2.2; docs/design/shards-image-class.md §11). Appended (the wire is append-only).
    #[serde(rename = "shards-image")]
    ShardsImage,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Oci => "oci",
            Self::Firecracker => "firecracker",
            Self::Shards => "shards",
            Self::ShardsImage => "shards-image",
        }
    }
}

/// A box to make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxSpec {
    pub id: BoxId,
    pub bounds: Bounds,
    pub class: ClassSpec,
    /// Whose box it is, if the caller says ([`BoxOwner`]). Added after the spec's first fields, so a
    /// spec without it names no owner (append-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<BoxOwner>,
    /// Exact identities requested from the runtime pool, never an implicit class-wide grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceId>,
    /// The original workload stays in `bounds`/`class`. This immutable receipt pays initialized
    /// per-box runtime costs in the existing resource ledger until physical termination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<Allocation>,
    /// Actual initialized execution selected before admission, beside the original workload form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::runtime::RouteReceipt>,
    /// Its priority band as its submitter gave it, 0 the lowest: where its node must end one of its microVMs to hold
    /// its fleet's memory, the lowest band goes first (joint plan step 14, `docs/design/learned-charge.md` C5), as
    /// Kubernetes' kubelet evicts by priority. Added after the spec's other fields, so a spec without it is band 0
    /// (append-only), and a band-0 spec's record and native commitment are those of a spec before it.
    #[serde(default, skip_serializing_if = "is_lowest")]
    pub band: u8,
}

fn is_lowest(band: &u8) -> bool {
    *band == 0
}

impl BoxSpec {
    pub fn route_execution(&self) -> Option<crate::runtime::Execution> {
        self.route.map(|receipt| receipt.key.execution)
    }

    /// Resolve only the original form or its validated immutable execution receipt: the class of the backend that runs
    /// it. A Shards image VM's is the Shards backend, whose image class boots it (D231).
    pub fn backend_class(&self) -> Result<Class, Refusal> {
        self.check()?;
        Ok(match self.route_execution() {
            None | Some(crate::runtime::Execution::Native | crate::runtime::Execution::NodeVm) => {
                match self.class.class() {
                    Class::ShardsImage => Class::Shards,
                    class @ (Class::Process | Class::Oci | Class::Firecracker | Class::Shards) => {
                        class
                    }
                }
            }
            Some(
                crate::runtime::Execution::ShardsShared
                | crate::runtime::Execution::ShardsDedicated,
            ) => Class::Shards,
            Some(crate::runtime::Execution::Firecracker) => Class::Firecracker,
        })
    }

    /// Whether every value it names is well formed, before anything is made: its bounds, its owner and
    /// a structured container's spec, each refused by its field's name (CLAUDE.md §3), and a Shards image
    /// VM's size from its admitted allocation.
    pub fn check(&self) -> Result<(), Refusal> {
        self.check_payload()?;
        match &self.class {
            ClassSpec::ShardsImage(_) => self.check_image_size(),
            ClassSpec::Process
            | ClassSpec::Oci(_)
            | ClassSpec::Bundle(_)
            | ClassSpec::Firecracker(_)
            | ClassSpec::Shards(_) => Ok(()),
        }
    }

    /// Whether it is well formed as a scheduler's payload, before its admission (hex-node's `placement::validate`):
    /// every check of [`BoxSpec::check`] but a Shards image VM's size, which its admitted allocation gives, the
    /// scheduler deriving that from the original demand as it admits the box (docs/design/shards-image-class.md §11).
    pub fn check_payload(&self) -> Result<(), Refusal> {
        self.bounds.check()?;
        self.check_devices()?;
        if let Some(route) = self.route {
            route.check(
                self.class.class(),
                self.devices
                    .iter()
                    .any(|device| device.kind == crate::DeviceKind::Gpu),
            )?;
            if matches!(
                route.key.execution,
                crate::runtime::Execution::ShardsDedicated | crate::runtime::Execution::Firecracker
            ) && (self.allocation.is_none()
                || self.bounds.cpu_max_milli.is_none()
                || self.bounds.memory_max.is_none())
            {
                return Err(crate::runtime::invalid_route(
                    "dedicated execution requires finite original CPU/memory caps and its admitted allocation",
                ));
            }
        }

        if let Some(allocation) = self.allocation {
            allocation.covers(CapacityResources {
                cpu_milli: self.bounds.cpu_max_milli.map_or(0, u64::from),
                mem_bytes: self.bounds.memory_max.unwrap_or(0),
                disk_bytes: 0,
            })?;
        }
        if let Some(owner) = &self.owner {
            owner.check()?;
        }
        match &self.class {
            ClassSpec::Oci(oci) | ClassSpec::Firecracker(oci) | ClassSpec::Shards(oci) => {
                oci.check()?
            }
            ClassSpec::ShardsImage(image) => self.check_image(image)?,
            ClassSpec::Process | ClassSpec::Bundle(_) => {}
        }
        Ok(())
    }

    /// A Shards image VM: its image and command ([`ShardsImage::check`]), and no physical device, which
    /// D231's image boot takes none of.
    fn check_image(&self, image: &ShardsImage) -> Result<(), Refusal> {
        image.check()?;
        if !self.devices.is_empty() {
            return Err(invalid_image(
                "devices are named: D231's image boot attaches no physical device".to_owned(),
            ));
        }
        Ok(())
    }

    /// A Shards image VM's size: an admitted allocation of whole vCPUs and MiB ([`crate::Allocation::vm_size`]).
    fn check_image_size(&self) -> Result<(), Refusal> {
        self.allocation
            .ok_or_else(|| {
                invalid_image(
                    "allocation is missing: a Shards image VM is sized by its admitted allocation"
                        .to_owned(),
                )
            })?
            .vm_size()?;
        Ok(())
    }

    /// The physical claims alone, also checked by the wire without changing its other field codecs.
    pub fn check_devices(&self) -> Result<(), Refusal> {
        crate::devices::check_ids(self.devices.iter().copied())
    }

    /// An adapter without device attachment refuses before creating any physical or durable state.
    pub fn check_no_devices(&self, platform: &'static str) -> Result<(), Refusal> {
        self.check()?;
        if !self.devices.is_empty() {
            return Err(Refusal::Platform {
                what: "devices",
                platform,
                why: "this runtime does not attach physical device claims".to_owned(),
            });
        }
        Ok(())
    }

    /// Checks the spec of a run ([`crate::Runtime::run`]) as [`BoxSpec::check`] does, and refuses an `oci`
    /// box's as `Invalid` naming the run: its init is created and started, and a run's box is the
    /// `process` class's (decision 7 of docs/design/runtime-architecture.md).
    pub fn check_run(&self) -> Result<(), Refusal> {
        self.check()?;
        if self.class.class() != Class::Process {
            return Err(Refusal::Invalid {
                what: "run",
                why: format!(
                    "box {} is a {} box, whose init is created and started",
                    self.id,
                    self.class.class().name()
                ),
            });
        }
        Ok(())
    }
}

/// Whose box it is (goal 5 extended, step 6). A node may give a box whose spec names an owner a cgroup
/// an earlier box of that owner left, once reads that take no lock show it clean, unless the owner opts
/// out; a box never gets another owner's, and a box with no owner always gets a fresh one. A cgroup
/// handed on is the owner's environment rather than the box's: what the box reads of its cgroup through
/// its own cgroup namespace, its predecessors' cumulative counters among it, is its owner's to see, as
/// a reused Lambda environment is its function's (docs/research/runtime/goal5-cgroup-lock.md, f39).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoxOwner {
    /// 1 to [`BoxOwner::MAX_NAME`] ASCII letters, digits and `_`, `+`, `-` or `.`, as a box id's.
    pub name: String,
    /// Whether the owner's boxes may be given a cgroup its earlier boxes left, or a network namespace: on
    /// unless the owner opts out (Ada's calls, 2026-10-31 and 2026-11-03).
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub reuse: bool,
}

impl BoxOwner {
    /// Longest owner name: a box id's longest.
    pub const MAX_NAME: usize = BoxId::MAX_LEN;

    /// The owner `name`, its boxes' cgroups handed on between them.
    pub fn named(name: &str) -> Result<Self, Refusal> {
        let owner = Self {
            name: name.to_owned(),
            reuse: true,
        };
        owner.check()?;
        Ok(owner)
    }

    /// Whether its name is well formed, refused by the field's name when it isn't.
    pub fn check(&self) -> Result<(), Refusal> {
        let invalid = |why: String| Refusal::Invalid {
            what: "box owner",
            why,
        };
        if self.name.is_empty() || self.name.len() > Self::MAX_NAME {
            return Err(invalid(format!(
                "owner.name {:?} isn't 1 to {} characters",
                self.name,
                Self::MAX_NAME
            )));
        }
        if let Some(c) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.')))
        {
            return Err(invalid(format!("owner.name {:?} holds {c:?}", self.name)));
        }
        Ok(())
    }
}

fn on() -> bool {
    true
}

fn is_on(reuse: &bool) -> bool {
    *reuse
}

/// The class a box is of, with what that class needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassSpec {
    /// Host processes, started in the box by [`crate::Runtime::exec`].
    Process,
    /// An OCI container.
    Oci(Box<OciSpec>),
    /// An OCI container from a runtime-spec bundle: a directory holding `config.json` and the root
    /// filesystem it names, run as its config says, in the box's cgroup under its bounds. For callers
    /// that already speak the runtime-spec: kiln's executor, which unpacks images into bundles, and the
    /// bench, which runs one bundle under every runtime.
    Bundle(PathBuf),
    /// A guest workload in its own Firecracker microVM. Host kernel, initrd, binary and physical
    /// attachment configuration belong to the initialized backend, never this peer payload.
    Firecracker(Box<OciSpec>),
    /// A guest workload run by Shards, with host configuration owned by its initialized backend.
    Shards(Box<OciSpec>),
    /// An image Shards boots under shards-init, a microVM of its own (Shards' D231, joint plan step 12):
    /// an Agentfile's image, its members started in their confined domains as the Agentfile declares
    /// them. Sized by the box's admitted allocation ([`crate::Allocation::vm_size`]).
    ShardsImage(Box<ShardsImage>),
}

impl ClassSpec {
    pub fn class(&self) -> Class {
        match self {
            Self::Process => Class::Process,
            Self::Oci(_) | Self::Bundle(_) => Class::Oci,
            Self::Firecracker(_) => Class::Firecracker,
            Self::Shards(_) => Class::Shards,
            Self::ShardsImage(_) => Class::ShardsImage,
        }
    }
}

/// The image a Shards microVM boots under shards-init, and the run's own command (Shards' D231,
/// `shards_api::image::ImageSpec`). Its size is the box's allocation's, never a field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardsImage {
    /// The image as `shards run` names one in the node's Shards store, pinned by its digest
    /// (`NAME@sha256:…`; distribution's reference grammar, as Shards reads it): the box boots the image
    /// it was admitted with, whatever a tag names by then.
    pub image: String,
    /// The run's own command, as `shards run IMAGE COMMAND` takes one; `None` runs the image's members
    /// alone (D231's `ATTEND`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// What its members may reach past the VM and what its host publishes into them (joint plan step
    /// 15; docs/design/network-grants.md): none by default, so nothing passes the VM. After the command
    /// (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<crate::Grant>,
    /// The members its caller drives (joint plan step 17): each started with its stdin a pipe the caller
    /// writes and its stdout and stderr pipes the caller reads, as Shards' image boot hands an orchestrator a
    /// member's input (`MemberSpec::stdin`, `ImageGuest::take_input`) and output (`take_output`); the caller
    /// takes them, with the member's end, through [`crate::Runtime::member`]. The image's other members run
    /// as it declares them, their stdin the null device. After the grants (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ImageMember>,
    /// The host directories its guest sees (Shards' D231 shares, as `shards run -v HOST:AT[:ro]` gives one; kiln's
    /// step 18: a person's home volumes, and a project at its host path, D235): each a `Mount`, its `source` the host
    /// directory, absolute on the host its node runs on, its `destination` where the guest sees it, absolute, and
    /// `read_only` whether the guest may write it. A member gets one where its image's `VOLUME` at that path is for it
    /// (Shards' D109), or where its member scope names it (`Mount::members`, Shards' D277), and the run's own command
    /// gets every one. None by default: the guest sees only the shares named here, the image's own grants widening none.
    /// After the members (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<Mount>,
    /// Its members' credentials, granted by reference (Shards' D242; joint plan step 33; docs/design/credential-grants.md):
    /// each a credential its orchestrator's store names, never its value, which its member's requests to the hosts its
    /// grant names alone may have filled in at the VM's egress, leased as they first need it ([`crate::lease`]). None by
    /// default: nothing is filled in toward any host. The orchestrator keeps or drops them, and Hex adds none. After the
    /// shares (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<crate::CredentialGrant>,
    /// Its members' CLIs' sign-ins (Shards' D242, step 8; docs/design/credential-grants.md C8): each a sign-in its
    /// orchestrator's store names, its tokens taken outside the VM by its run's injector and kept by its orchestrator,
    /// its CLI given placeholders in their places. None by default. After the credentials (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sign_ins: Vec<crate::SignInGrant>,
    /// The volumes of the node's Shards home its members are given as its run starts (kiln plan 01a1dd2e…, P6.2;
    /// Shards' D271 and D281), each kept there past the run, so the next run given it finds what this one left. None
    /// by default. After the sign-ins (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<ImageVolume>,
}

/// A volume of the node's Shards home a Shards image's members are given as its run starts
/// ([`ShardsImage::volumes`]): plugged into its members in its running microVM as Shards plugs one (D271), beneath a
/// plug root its run is granted at its start, made there, empty, where it isn't, and held to its bound as its files are
/// written (D281). Its run holds it until it ends, so no clearing removes it meanwhile
/// ([`crate::Runtime::clear_volume`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageVolume {
    /// Its name in the store, whose grammar Shards holds it to as it plugs it.
    pub name: String,
    /// The most its files may take, by Shards' measure (D281: a unit of its host's file system for each name, and each
    /// file's length and attributes in whole units), a write past it refused ENOSPC in its members: drawn from its
    /// box's admitted disk, its box's volumes' bounds together within it. A volume runs hold is served with the bound
    /// they were given, so one asked with another is refused while any run holds it ([`crate::VolumeCause::Bound`]).
    pub bound: NonZeroU64,
    /// Where its members see it: absolute and plain, strictly beneath a top-level directory, which its run is granted
    /// as a plug root at its start (D271); no other volume's or share's place.
    pub at: String,
    /// The members it is plugged into, each once: at least one.
    pub members: Vec<crate::Member>,
    pub read_only: bool,
}

impl ImageVolume {
    /// Whether Shards can be asked to plug it, told by its place: a name the store and the node's wire can hold (not
    /// empty, no NUL; the rest of its grammar is Shards'), its place absolute and plain ([`plain`]) and beneath a
    /// top-level directory, and its members each named as a grant's member is and once, at least one.
    fn check(&self, at: usize) -> Result<(), Refusal> {
        if self.name.is_empty() || self.name.contains('\0') {
            return Err(invalid_image(format!(
                "volumes[{at}].name {:?} names no volume",
                self.name
            )));
        }
        if !plain(&self.at) || steps(&self.at).nth(1).is_none() {
            return Err(invalid_image(format!(
                "volumes[{at}].at {:?} isn't absolute, plain and beneath a top-level directory, its plug root",
                self.at
            )));
        }
        if self.members.is_empty() {
            return Err(invalid_image(format!(
                "volumes[{at}].members names none: a volume is plugged into the members named"
            )));
        }
        named_once(&format!("volumes[{at}].members"), &self.members)
    }
}

/// Each of `members` named as a grant's member is, and once, a share's or a volume's (Shards' D277, D271), refused by its
/// place in `field`.
fn named_once(field: &str, members: &[crate::Member]) -> Result<(), Refusal> {
    for (i, member) in members.iter().enumerate() {
        member
            .check_named(&format!("{field}[{i}]"))
            .map_err(invalid_image)?;
        if members.get(..i).unwrap_or_default().contains(member) {
            return Err(invalid_image(format!("{field}[{i}] names {member} again")));
        }
    }
    Ok(())
}

/// A member of a Shards image its caller drives ([`ShardsImage::members`]): which one, by the kind and name
/// its Agentfile gives it, the variables it gets over its image's, and the folder it works in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMember {
    pub member: crate::Member,
    /// Variables over its image's, each `(KEY, VALUE)`, as an exec's are checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Its working directory, absolute and plain, which must be there as it starts (Shards' D231); none for
    /// its image's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Its run-time arguments (Shards' D256, `MemberSpec::args`; kiln's step 17), in their order: its command line
    /// is its image's command for it (its startup descriptor's), then these, as a harness takes its session's own
    /// flags (Claude Code's `--resume`, `--model`) on the host. None: its image's command alone. No argument holds a
    /// NUL. A command line is no secret, read by whatever sees the member's processes: a credential goes in `env`.
    /// After the working directory (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Whether it has an MCP bridge of its own (Shards' D254, `MemberSpec::mcp`): `/run/shards/mcp`, a stdio server its
    /// harness's MCP config names as a command, each connection to which reaches its run's controller as a lane of the
    /// run's agent-control endpoint. So it needs that endpoint: a node that asks none of its run refuses a member asking
    /// one, by this field, as Shards' own check does. None by default. After the run-time arguments (append-only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mcp: bool,
}

impl ImageMember {
    /// `member`, given nothing over its image's, in its image's working directory, its image's command alone, with no
    /// MCP bridge.
    pub fn new(member: crate::Member) -> Self {
        Self {
            member,
            env: Vec::new(),
            workdir: None,
            args: Vec::new(),
            mcp: false,
        }
    }

    /// Whether Shards can be asked to start it: its name as a grant's member's is checked, its variables as
    /// an exec's (no empty key, no `=` in a key, no NUL), its working directory absolute and plain (no
    /// empty, `.` or `..` component, no NUL), as D231's `MemberSpec::workdir` takes one, and no run-time
    /// argument holding a NUL, which no command line exec'd holds (D256), told by its place, never its bytes.
    fn check(&self, at: usize) -> Result<(), Refusal> {
        self.member
            .check_named(&format!("members[{at}].member"))
            .map_err(invalid_image)?;
        if let Some((k, _)) = self
            .env
            .iter()
            .find(|(k, v)| k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0'))
        {
            return Err(invalid_image(format!(
                "members[{at}].env's variable {k:?} can't be set"
            )));
        }
        if let Some(dir) = &self.workdir
            && !plain(dir)
        {
            return Err(invalid_image(format!(
                "members[{at}].workdir {dir:?} isn't absolute and plain"
            )));
        }
        if let Some(i) = self.args.iter().position(|a| a.contains('\0')) {
            return Err(invalid_image(format!(
                "members[{at}].args[{i}] holds a NUL, which no command line holds"
            )));
        }
        Ok(())
    }
}

impl ShardsImage {
    /// The image `image` (pinned by digest), its members alone, granted and shared nothing.
    pub fn new(image: &str) -> Self {
        Self {
            image: image.to_owned(),
            command: None,
            grants: Vec::new(),
            members: Vec::new(),
            shares: Vec::new(),
            credentials: Vec::new(),
            sign_ins: Vec::new(),
            volumes: Vec::new(),
        }
    }

    /// Whether Shards can be asked to boot it: the image pinned by a digest go-digest validates (the
    /// algorithm `sha256`, `sha384` or `sha512` and exactly that many lowercase hex digits, as Shards'
    /// reference parser checks it), a command, where there is one, of a non-empty program and no NUL,
    /// grants [`crate::check_grants`] takes, and members its caller drives each named once and checked as
    /// [`ImageMember`] checks one. Refused by its field's name (CLAUDE.md §3); the rest of
    /// the name's grammar is Shards' to refuse as it resolves the image, and a grant Shards can't hold
    /// exactly is the runtime's to refuse as it programs the boot. Its volumes each checked as [`ImageVolume`]
    /// checks one, at no share's or other volume's place; their bounds against its box's admitted disk are the
    /// runtime's to hold as it creates the box. Its credentials, last, as [`crate::check_credentials`] takes them over
    /// its grants (refused naming `credentials`).
    pub fn check(&self) -> Result<(), Refusal> {
        check_pinned("shards image", &self.image)?;
        if let Some(command) = &self.command {
            match command.first() {
                None => {
                    return Err(invalid_image(
                        "command is empty: none runs the image's members alone".to_owned(),
                    ));
                }
                Some(program) if program.is_empty() => {
                    return Err(invalid_image("command[0] is empty".to_owned()));
                }
                Some(_) => {}
            }
            if let Some(at) = command.iter().position(|arg| arg.contains('\0')) {
                return Err(invalid_image(format!("command[{at}] holds a NUL")));
            }
        }
        crate::check_grants(&self.grants)?;
        for (at, member) in self.members.iter().enumerate() {
            member.check(at)?;
            if self
                .members
                .get(..at)
                .unwrap_or_default()
                .iter()
                .any(|m| m.member == member.member)
            {
                return Err(invalid_image(format!(
                    "members[{at}] names {} again",
                    member.member
                )));
            }
        }
        // Each share as a mount's paths are checked (its host directory absolute and UTF-8 with no NUL, where the guest
        // sees it absolute with no `..`), never at the guest's root, which it would hide, and each place seen once,
        // however its path is spelled (the review's B-10), so no share's read-only intent is lost to mount order.
        for (at, share) in self.shares.iter().enumerate() {
            let as_image = |e: Refusal| match e {
                Refusal::Invalid { why, .. } => invalid_image(why),
                other => other,
            };
            host_path(&format!("shares[{at}].source"), &share.source).map_err(as_image)?;
            let field = format!("shares[{at}].destination");
            container_path(&field, &share.destination).map_err(as_image)?;
            if path_parts(&share.destination).all(|part| part.is_empty() || part == ".") {
                return Err(invalid_image(format!(
                    "{field} is the guest's root, which a share would hide"
                )));
            }
            if self
                .shares
                .get(..at)
                .unwrap_or_default()
                .iter()
                .any(|s| same_place(&s.destination, &share.destination))
            {
                return Err(invalid_image(format!(
                    "{field} {:?} is another share's too",
                    share.destination
                )));
            }
            // Its member scope (Shards' D277): each member named as a grant's member is, once, as Shards refuses a
            // share given to one twice.
            named_once(&format!("shares[{at}].members"), &share.members)?;
        }
        // Its volumes (kiln plan 01a1dd2e…, P6.2), each checked and each at a place of its own, no share's or other
        // volume's however its path is spelled, so no plug hides another's files.
        for (at, volume) in self.volumes.iter().enumerate() {
            volume.check(at)?;
            let taken = self
                .shares
                .iter()
                .map(|s| s.destination.as_str())
                .chain(
                    self.volumes
                        .get(..at)
                        .unwrap_or_default()
                        .iter()
                        .map(|v| v.at.as_str()),
                )
                .any(|place| same_place(place, &volume.at));
            if taken {
                return Err(invalid_image(format!(
                    "volumes[{at}].at {:?} is a share's or another volume's place too",
                    volume.at
                )));
            }
        }
        // Its credentials, each one that could be filled in exactly as it says over the grants just taken (joint plan
        // step 33, docs/design/credential-grants.md C2).
        crate::check_credentials(&self.credentials, &self.grants)?;
        crate::check_sign_ins(&self.sign_ins, &self.credentials, &self.grants)
    }
}

/// Whether `image` is pinned by a digest go-digest validates, as Shards' reference parser checks one: a name, then `@`,
/// the algorithm `sha256`, `sha384` or `sha512` and exactly that many lowercase hex digits; refused `Invalid` naming
/// `what` where it isn't. The rest of the name's grammar is Shards' to refuse as it resolves the image. A box's image
/// is held to it ([`ShardsImage::check`]), and a node's own guest image (docs/design/node-under-shards-init.md §4; the
/// review's C-17), so an image boots as it was admitted, whatever a tag names by then.
pub fn check_pinned(what: &'static str, image: &str) -> Result<(), Refusal> {
    let refused = |why: String| Refusal::Invalid { what, why };
    let (name, digest) = image.rsplit_once('@').ok_or_else(|| {
        refused(format!(
            "image {image:?} isn't pinned by a digest (NAME@sha256:…)"
        ))
    })?;
    if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refused(format!(
            "image {image:?} names no image before its digest"
        )));
    }
    let (algorithm, hex) = digest.split_once(':').unwrap_or((digest, ""));
    // go-digest v1.0.0's algorithms and their encoded lengths (sha256, sha384, sha512), as Shards'
    // reference parser takes them (crates/image/src/reference.rs at 482c5d78).
    let length = match algorithm {
        "sha256" => 64,
        "sha384" => 96,
        "sha512" => 128,
        _ => {
            return Err(refused(format!(
                "image {image:?}'s digest algorithm isn't sha256, sha384 or sha512"
            )));
        }
    };
    if hex.len() != length || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(refused(format!(
            "image {image:?}'s digest isn't {length} lowercase hex digits"
        )));
    }
    Ok(())
}

fn invalid_image(why: String) -> Refusal {
    Refusal::Invalid {
        what: "shards image",
        why,
    }
}

/// A container: its root filesystem, the program it runs first, and what it sees of the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OciSpec {
    pub rootfs: Rootfs,
    /// The container's first program. The container runs while it does; [`crate::Runtime::exec`]
    /// runs others beside it, as `docker exec` does.
    pub init: Program,
    /// Host folders and files bound in, in order.
    pub mounts: Vec<Mount>,
    pub hostname: Option<String>,
    pub network: Network,
    /// Lines added to the container's `/etc/hosts`: a name and the address it resolves to.
    pub hosts: Vec<(String, String)>,
}

impl OciSpec {
    /// Longest hostname: Linux's (`HOST_NAME_MAX`, gethostname(2)).
    pub const MAX_HOSTNAME: usize = 64;
    /// Longest name in an `/etc/hosts` line: a DNS name's longest written form (RFC 1035 §2.3.4's
    /// 255 octets on the wire).
    pub const MAX_HOSTS_NAME: usize = 253;

    /// Whether every value it names is well formed, before anything is made: a value that isn't is
    /// refused by its field's name, never dropped or half-applied (CLAUDE.md §3). A runtime refuses
    /// more as it renders the spec, what its host can't make.
    pub fn check(&self) -> Result<(), Refusal> {
        match &self.rootfs {
            Rootfs::Dir(dir) => host_path("rootfs.dir", dir)?,
            Rootfs::Layers(layers) => {
                if layers.is_empty() {
                    return Err(invalid_spec(
                        "rootfs.layers is empty: an overlay needs a layer under its writable one"
                            .to_owned(),
                    ));
                }
                for (i, layer) in layers.iter().enumerate() {
                    host_path(&format!("rootfs.layers[{i}]"), layer)?;
                }
            }
        }
        self.init.check().map_err(|e| match e {
            Refusal::Invalid { why, .. } => invalid_spec(format!("init: {why}")),
            other => other,
        })?;
        if let Some(cwd) = &self.init.cwd {
            container_path("init.cwd", cwd)?;
        }
        for (i, m) in self.mounts.iter().enumerate() {
            host_path(&format!("mounts[{i}].source"), &m.source)?;
            let field = format!("mounts[{i}].destination");
            container_path(&field, &m.destination)?;
            if path_parts(&m.destination).all(|part| part.is_empty() || part == ".") {
                return Err(invalid_spec(format!(
                    "{field} is the root: a mount over it would hide the rootfs"
                )));
            }
            if !m.members.is_empty() {
                return Err(invalid_spec(format!(
                    "mounts[{i}].members names members a container doesn't have: a member scope is a Shards image \
                     VM's share's"
                )));
            }
        }
        if let Some(name) = &self.hostname {
            host_name("hostname", name, Self::MAX_HOSTNAME)?;
        }
        for (i, (name, address)) in self.hosts.iter().enumerate() {
            host_name(&format!("hosts[{i}].name"), name, Self::MAX_HOSTS_NAME)?;
            if address.parse::<IpAddr>().is_err() {
                return Err(invalid_spec(format!(
                    "hosts[{i}].address {address:?} isn't an IPv4 or IPv6 address"
                )));
            }
        }
        Ok(())
    }
}

fn invalid_spec(why: String) -> Refusal {
    Refusal::Invalid {
        what: "oci spec",
        why,
    }
}

/// Whether `path` is drive-absolute, Windows' absolute form (`C:\…` or `C:/…`); `C:data` is relative to
/// the drive's current folder and `\\server\share` a UNC path, neither absolute here.
pub fn drive_absolute(path: &str) -> bool {
    let mut c = path.chars();
    matches!(
        (c.next(), c.next(), c.next()),
        (Some(d), Some(':'), Some('\\' | '/')) if d.is_ascii_alphabetic()
    )
}

/// The components of an absolute path in either form: after its drive for a drive-absolute one, split on
/// both separators; split on `/` alone for a POSIX one, as Linux reads it.
fn path_parts(path: &str) -> impl Iterator<Item = &str> {
    let (rest, windows) = if drive_absolute(path) {
        (path.get(2..).unwrap_or(""), true)
    } else {
        (path, false)
    };
    rest.split(move |c| c == '/' || (windows && c == '\\'))
}

/// Whether two paths [`container_path`] took name one place: the same drive where either is drive-absolute (its letter
/// in either case, as Windows reads it), then the same steps, past the empty components a doubled or trailing
/// separator leaves and `.`, which names none (`..` is refused before).
fn same_place(a: &str, b: &str) -> bool {
    let drive = |path: &str| {
        path.chars()
            .next()
            .filter(|_| drive_absolute(path))
            .map(|letter| letter.to_ascii_lowercase())
    };
    drive(a) == drive(b) && steps(a).eq(steps(b))
}

/// The components of an absolute path that each name a step ([`path_parts`] past the empty ones and `.`).
fn steps(path: &str) -> impl Iterator<Item = &str> {
    path_parts(path).filter(|part| !part.is_empty() && *part != ".")
}

/// Whether `path` is absolute and plain, as Shards takes a member's working directory (D231) and a plug's place (D271)
/// in its Linux guest: from `/`, with no NUL, each component past it a step (none empty, `.` or `..`).
fn plain(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\0')
        && (path == "/"
            || path
                .get(1..)
                .unwrap_or_default()
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".."))
}

/// A host path a spec names: absolute on the host that runs the container (POSIX-absolute on Linux and
/// macOS, drive-absolute on Windows; the runtime refuses the form its host can't use), and UTF-8 without a
/// NUL, since `config.json` carries it as a JSON string and the kernel takes it as a C string.
fn host_path(field: &str, path: &Path) -> Result<(), Refusal> {
    let Some(text) = path.to_str() else {
        return Err(invalid_spec(format!(
            "{field} {path:?} isn't UTF-8, which config.json can't carry"
        )));
    };
    if text.contains('\0') {
        return Err(invalid_spec(format!("{field} {text:?} holds a NUL")));
    }
    if !text.starts_with('/') && !drive_absolute(text) {
        return Err(invalid_spec(format!(
            "{field} {text:?} isn't an absolute path"
        )));
    }
    Ok(())
}

/// A path in the container: absolute, without a NUL, and with no `..`, which could reach above the
/// rootfs before the runtime has pivoted into it. A POSIX path splits on `/` alone, as a Linux container
/// does; a drive-absolute one (a Windows container's) on both separators.
fn container_path(field: &str, path: &str) -> Result<(), Refusal> {
    if path.contains('\0') {
        return Err(invalid_spec(format!("{field} {path:?} holds a NUL")));
    }
    if !path.starts_with('/') && !drive_absolute(path) {
        return Err(invalid_spec(format!(
            "{field} {path:?} isn't an absolute path"
        )));
    }
    if path_parts(path).any(|part| part == "..") {
        return Err(invalid_spec(format!(
            "{field} {path:?} has a '..' component"
        )));
    }
    Ok(())
}

/// A hostname, or a name in an `/etc/hosts` line: 1 to `max` printable ASCII characters, none of them
/// a space or a `#`, so the line holds exactly the name it was given and starts no comment or other
/// line (hosts(5)).
fn host_name(field: &str, name: &str, max: usize) -> Result<(), Refusal> {
    if name.is_empty() || name.len() > max {
        return Err(invalid_spec(format!(
            "{field} {name:?} isn't 1 to {max} characters"
        )));
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii_graphic() || *c == '#') {
        return Err(invalid_spec(format!("{field} {name:?} holds {c:?}")));
    }
    Ok(())
}

/// A container's root filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rootfs {
    /// A directory, used as it is and written in place.
    Dir(PathBuf),
    /// Read-only layers, lowest first (an image's, unpacked), under a writable layer the box owns
    /// and deletes with itself.
    Layers(Vec<PathBuf>),
}

/// A host path bound into a container, or a host directory a Shards image VM's guest sees ([`ShardsImage::shares`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: PathBuf,
    /// Where it appears in the container: an absolute path.
    pub destination: String,
    pub read_only: bool,
    /// Its member scope, a Shards image VM's share's alone (Shards' D277, `ImageShare::members`): the members given it
    /// as they start, at `destination` in each one's own root before its command runs, so one may work in it from its
    /// first instruction (a project its harness's working directory is in), each named once by the kind and name its
    /// image's Agentfile gives it, and one its image declares that runs (Shards refuses another at boot). None: the
    /// run's own root alone, a member seeing it only where its image's `VOLUME` there is for it (D109). A container has
    /// no members: an `oci` mount naming one is refused. After `read_only` (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<crate::Member>,
}

/// What a container sees of the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// The host's network namespace.
    Host,
    /// A namespace of its own with loopback only.
    Private,
}

/// A program to run: `args[0]` is looked up in `env`'s `PATH` when it holds no `/`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub args: Vec<String>,
    /// Its whole environment: nothing is inherited.
    pub env: Vec<(String, String)>,
    /// Its working folder: a host path for `process`, a path in the container for `oci`.
    pub cwd: Option<String>,
    /// Who it runs as in a container; `process` runs everything as the node agent's user.
    pub user: Option<User>,
}

impl Program {
    /// `args` with `PATH` set to the usual system folders, nothing else in the environment.
    pub fn of(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: vec![(
                "PATH".to_owned(),
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
            )],
            cwd: None,
            user: None,
        }
    }

    /// Whether it can be started at all.
    pub fn check(&self) -> Result<(), Refusal> {
        let invalid = |why: String| {
            Err(Refusal::Invalid {
                what: "program",
                why,
            })
        };
        match self.args.first() {
            None => return invalid("no arguments: args[0] names the program".to_owned()),
            Some(a) if a.is_empty() => return invalid("args[0] is empty".to_owned()),
            Some(_) => {}
        }
        if let Some(a) = self.args.iter().find(|a| a.contains('\0')) {
            return invalid(format!("argument {a:?} holds a NUL"));
        }
        if let Some((k, _)) = self
            .env
            .iter()
            .find(|(k, v)| k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0'))
        {
            return invalid(format!("environment variable {k:?} can't be set"));
        }
        Ok(())
    }

    /// The bytes an exec takes for this program, counted as Linux's `execve` counts them
    /// (docs/research/runtime/exec-size-bounds.md), which is at least what XNU counts: each argument and
    /// each `KEY=VALUE` string with its NUL, the first argument once more for the filename, and a pointer
    /// for each string (one argument at least). Saturates at `usize::MAX`, which no bound admits.
    pub fn exec_size(&self) -> usize {
        let args = self
            .args
            .iter()
            .fold(0usize, |n, a| n.saturating_add(a.len()).saturating_add(1));
        let filename = self.args.first().map_or(1, |a| a.len().saturating_add(1));
        let env = self.env.iter().fold(0usize, |n, (k, v)| {
            // KEY=VALUE and its NUL.
            n.saturating_add(k.len())
                .saturating_add(v.len())
                .saturating_add(2)
        });
        let pointers = self
            .args
            .len()
            .max(1)
            .saturating_add(self.env.len())
            .saturating_mul(size_of::<usize>());
        args.saturating_add(filename)
            .saturating_add(env)
            .saturating_add(pointers)
    }

    /// Whether this OS's exec takes it: refused as `Invalid` naming the sizes and the bound's source when it
    /// doesn't, before any spawn (CLAUDE.md §2, §3).
    pub fn fits(&self, limit: &ExecLimit) -> Result<(), Refusal> {
        let size = limit.size(self);
        if size > limit.block {
            let (what, unit) = match limit.counts {
                BlockCount::Execve => ("its arguments and environment take", "bytes"),
                BlockCount::Environment => ("its environment takes", limit.unit.name()),
            };
            return Err(Refusal::Invalid {
                what: "program",
                why: format!(
                    "{what} {size} {unit}; {} allows {}",
                    limit.source, limit.block
                ),
            });
        }
        let Some(most) = limit.string else {
            return Ok(());
        };
        let unit = limit.unit;
        // Each string with its NUL, in the OS's own unit.
        let arg_size = |a: &str| unit.of(a).saturating_add(1);
        let var_size = |k: &str, v: &str| unit.of(k).saturating_add(unit.of(v)).saturating_add(2);
        if let Some(a) = self.args.iter().find(|a| arg_size(a) > most) {
            return Err(Refusal::Invalid {
                what: "program",
                why: format!(
                    "an argument of {} {}; {} allows {most} for one string",
                    arg_size(a),
                    unit.name(),
                    limit.source
                ),
            });
        }
        if let Some((k, v)) = self.env.iter().find(|(k, v)| var_size(k, v) > most) {
            return Err(Refusal::Invalid {
                what: "program",
                why: format!(
                    "environment variable {k:?} takes {} {}; {} allows {most} for one string",
                    var_size(k, v),
                    unit.name(),
                    limit.source
                ),
            });
        }
        Ok(())
    }
}

/// What an OS's exec bound counts of a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCount {
    /// Every argument and variable, the filename and their pointers, in bytes: [`Program::exec_size`]
    /// (Linux's execve, which bounds at least what XNU's does).
    Execve,
    /// The environment alone, each NAME=VALUE and its NUL in [`ExecLimit::unit`] (Windows' environment block;
    /// its command line is bounded apart).
    Environment,
}

/// The unit an OS counts one string of a program in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringUnit {
    /// UTF-8 bytes (Linux's `MAX_ARG_STRLEN`).
    Bytes,
    /// UTF-16 code units (Windows' 32,767 characters a variable).
    Utf16,
}

impl ExecLimit {
    /// `program`'s size as this bound counts it.
    pub fn size(&self, program: &Program) -> usize {
        match self.counts {
            BlockCount::Execve => program.exec_size(),
            BlockCount::Environment => program.env.iter().fold(0usize, |n, (k, v)| {
                // NAME=VALUE and its NUL.
                n.saturating_add(self.unit.of(k))
                    .saturating_add(self.unit.of(v))
                    .saturating_add(2)
            }),
        }
    }
}

impl StringUnit {
    /// `s`'s length in this unit.
    pub fn of(self, s: &str) -> usize {
        match self {
            Self::Bytes => s.len(),
            Self::Utf16 => s.encode_utf16().count(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Utf16 => "UTF-16 units",
        }
    }
}

/// The most an OS's exec takes of a program's arguments and environment
/// (docs/research/runtime/exec-size-bounds.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecLimit {
    /// The most a program may take, counted as [`ExecLimit::counts`] says.
    pub block: usize,
    /// What [`ExecLimit::block`] counts.
    pub counts: BlockCount,
    /// Any one string with its NUL, in [`ExecLimit::unit`], where the OS bounds that too.
    pub string: Option<usize>,
    /// What [`ExecLimit::string`] counts: Linux and macOS bound a string in bytes, Windows in UTF-16 units.
    pub unit: StringUnit,
    /// Where the bound was read, for the refusal.
    pub source: &'static str,
}

/// A user and its groups in a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

/// A program's standard streams: each one the caller's own (a pipe's end, given here), one the runtime makes
/// and gives the caller its end of ([`Stdio::ask`], [`crate::Proc::streams`]), or the null device.
#[derive(Debug, Default)]
pub struct Stdio {
    pub stdin: Option<PipeReader>,
    pub stdout: Option<PipeWriter>,
    pub stderr: Option<PipeWriter>,
    /// The streams the runtime makes: their far ends are the program's, the near ends the caller's, so the
    /// host copies no byte between them (native plan step 8). A stream both given above and asked for here
    /// is refused, naming it.
    pub ask: Ask,
}

/// Which of a program's standard streams the caller asks its runtime to make ([`Stdio::ask`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ask {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// The caller's ends of the streams it asked for ([`crate::Proc::streams`]): stdin's to write, stdout's and
/// stderr's to read, each to its end as a pipe's end is. Use [`crate::OwnedStream`] for portable I/O on an end.
#[derive(Debug, Default)]
pub struct Given {
    pub stdin: Option<End>,
    pub stdout: Option<End>,
    pub stderr: Option<End>,
}

/// A stream's end the caller holds: a file descriptor on Unix (a pipe's or a socket's).
#[cfg(unix)]
pub type End = std::os::fd::OwnedFd;
#[cfg(windows)]
pub use crate::end::End;

impl Stdio {
    /// Every stream the null device.
    pub fn null() -> Self {
        Self::default()
    }

    /// Makes each stream asked for as a pipe: the program's end put here, the caller's given back. A
    /// runtime whose programs take pipes as their stdio makes its asked streams so; one whose streams are
    /// elsewhere (a guest's) gives that stream's own end instead. Refused, naming the stream, when one is
    /// both given and asked for.
    pub fn make_asked(&mut self) -> Result<Given, crate::Refusal> {
        self.check()?;
        let pipe =
            || std::io::pipe().map_err(|e| crate::Refusal::os("making a stream asked for", e));
        let mut given = Given::default();
        if self.ask.stdin {
            let (r, w) = pipe()?;
            self.stdin = Some(r);
            given.stdin = Some(w.into());
        }
        if self.ask.stdout {
            let (r, w) = pipe()?;
            self.stdout = Some(w);
            given.stdout = Some(r.into());
        }
        if self.ask.stderr {
            let (r, w) = pipe()?;
            self.stderr = Some(w);
            given.stderr = Some(r.into());
        }
        self.ask = Ask::default();
        Ok(given)
    }

    /// Refused, naming the stream, when one is both given and asked for.
    pub fn check(&self) -> Result<(), crate::Refusal> {
        let both = [
            ("stdin", self.stdin.is_some() && self.ask.stdin),
            ("stdout", self.stdout.is_some() && self.ask.stdout),
            ("stderr", self.stderr.is_some() && self.ask.stderr),
        ];
        match both.into_iter().find(|(_, both)| *both) {
            Some((name, _)) => Err(crate::Refusal::Invalid {
                what: "stdio",
                why: format!("{name} is both given and asked for"),
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_that_cant_be_started_is_refused_before_it_is_tried() {
        assert!(Program::default().check().is_err());
        assert!(Program::of(&[""]).check().is_err());
        assert!(Program::of(&["sh", "a\0b"]).check().is_err());
        let mut p = Program::of(&["sh"]);
        p.env.push(("A=B".to_owned(), "c".to_owned()));
        assert!(p.check().is_err());
        assert_eq!(Program::of(&["sh", "-c", "true"]).check(), Ok(()));
    }

    /// docs/research/runtime/exec-size-bounds.md: do count `sh -c true` with one variable, expect Linux's
    /// count (each string and its NUL, the filename again, a pointer per string); do fit it against a block one
    /// byte short, expect a refusal naming the program and the bound's source; do a variable past the
    /// per-string bound, expect it named.
    #[test]
    fn a_program_is_counted_as_execve_counts_it_and_refused_past_a_bound() {
        let p = Program {
            args: vec!["sh".into(), "-c".into(), "true".into()],
            env: vec![("A".into(), "bcdef".into())],
            ..Program::default()
        };
        // "sh\0" "-c\0" "true\0" = 3 + 3 + 5; the filename "sh\0" = 3; "A=bcdef\0" = 8; 4 pointers.
        let size = 11 + 3 + 8 + 4 * size_of::<usize>();
        assert_eq!(p.exec_size(), size);
        let fits = ExecLimit {
            block: size,
            counts: BlockCount::Execve,
            string: None,
            unit: StringUnit::Bytes,
            source: "a test",
        };
        assert_eq!(p.fits(&fits), Ok(()));
        let short = ExecLimit {
            block: size - 1,
            ..fits
        };
        assert!(matches!(
            p.fits(&short),
            Err(Refusal::Invalid { what: "program", ref why }) if why.contains("a test")
        ));
        // Every argument fits in 6 bytes ("true\0" is the longest, 5); the variable's 8 don't.
        let one_string = ExecLimit {
            string: Some(6),
            ..fits
        };
        assert!(matches!(
            p.fits(&one_string),
            Err(Refusal::Invalid { what: "program", ref why }) if why.contains("\"A\"")
        ));
    }

    /// HEX-RT-14: a per-string bound counts in its OS's own unit. Do: a variable of three two-byte
    /// characters, "A=ééé": 8 bytes with its NUL, 6 UTF-16 units. Expect a bound of 6 to take it in UTF-16
    /// units (Windows' count) and refuse it in bytes (Linux's), naming the variable.
    #[test]
    fn a_per_string_bound_counts_in_the_os_unit() {
        let p = Program {
            args: vec!["sh".into()],
            env: vec![("A".into(), "ééé".into())],
            ..Program::default()
        };
        let utf16 = ExecLimit {
            block: usize::MAX,
            counts: BlockCount::Execve,
            string: Some(6),
            unit: StringUnit::Utf16,
            source: "a test",
        };
        assert_eq!(p.fits(&utf16), Ok(()));
        let bytes = ExecLimit {
            unit: StringUnit::Bytes,
            ..utf16
        };
        assert!(matches!(
            p.fits(&bytes),
            Err(Refusal::Invalid { what: "program", ref why }) if why.contains("\"A\"")
        ));
    }

    /// HEX-RT-14: Windows' block is the environment alone, in UTF-16 units (each NAME=VALUE and its NUL;
    /// the command line is bounded apart). Do: a long argument and the variable "A=ééé" (6 units with its
    /// NUL). Expect a 6-unit environment block to take it whatever the argument's length, and a 5-unit one
    /// to refuse it naming the environment.
    #[test]
    fn a_windows_block_counts_the_environment_alone_in_its_unit() {
        let p = Program {
            args: vec!["cmd".into(), "x".repeat(1000)],
            env: vec![("A".into(), "ééé".into())],
            ..Program::default()
        };
        let six = ExecLimit {
            block: 6,
            counts: BlockCount::Environment,
            string: None,
            unit: StringUnit::Utf16,
            source: "a test",
        };
        assert_eq!(six.size(&p), 6);
        assert_eq!(p.fits(&six), Ok(()));
        let five = ExecLimit { block: 5, ..six };
        assert!(matches!(
            p.fits(&five),
            Err(Refusal::Invalid { what: "program", ref why }) if why.contains("environment")
        ));
    }

    fn spec() -> OciSpec {
        OciSpec {
            rootfs: Rootfs::Layers(vec!["/images/base".into(), "/images/app".into()]),
            init: Program {
                cwd: Some("/work".to_owned()),
                ..Program::of(&["sleep", "infinity"])
            },
            mounts: vec![Mount {
                source: "/srv/data".into(),
                destination: "/data".to_owned(),
                read_only: true,
                members: Vec::new(),
            }],
            hostname: Some("box-1.example".to_owned()),
            network: Network::Private,
            hosts: vec![
                ("db_main".to_owned(), "10.0.0.2".to_owned()),
                ("cache".to_owned(), "fd00::2".to_owned()),
            ],
        }
    }

    /// AC-T7.1.1, CLAUDE.md §3: a spec with a value no runtime can render is refused before anything
    /// is made, typed, with the field it names, and a well-formed one passes.
    #[test]
    fn an_oci_spec_with_a_value_that_cant_be_rendered_is_refused_by_its_name() {
        assert_eq!(spec().check(), Ok(()));
        type Break = Box<dyn Fn(&mut OciSpec)>;
        let cases: Vec<(&str, Break)> = vec![
            (
                "rootfs.dir",
                Box::new(|s| s.rootfs = Rootfs::Dir("images/base".into())),
            ),
            (
                "rootfs.layers is empty",
                Box::new(|s| s.rootfs = Rootfs::Layers(vec![])),
            ),
            (
                "rootfs.layers[1]",
                Box::new(|s| s.rootfs = Rootfs::Layers(vec!["/a".into(), "b".into()])),
            ),
            ("init: ", Box::new(|s| s.init.args.clear())),
            ("init: ", Box::new(|s| s.init.args.push("a\0b".to_owned()))),
            (
                "init.cwd",
                Box::new(|s| s.init.cwd = Some("work".to_owned())),
            ),
            (
                "mounts[0].source",
                Box::new(|s| s.mounts[0].source = "srv/data".into()),
            ),
            (
                "mounts[0].destination",
                Box::new(|s| s.mounts[0].destination = "data".to_owned()),
            ),
            (
                "mounts[0].destination",
                Box::new(|s| s.mounts[0].destination = "/data/../../etc".to_owned()),
            ),
            (
                "mounts[0].destination",
                Box::new(|s| s.mounts[0].destination = "/data\0".to_owned()),
            ),
            (
                "mounts[0].destination is the root",
                Box::new(|s| s.mounts[0].destination = "//./".to_owned()),
            ),
            ("hostname", Box::new(|s| s.hostname = Some(String::new()))),
            (
                "hostname",
                Box::new(|s| s.hostname = Some("a".repeat(OciSpec::MAX_HOSTNAME + 1))),
            ),
            (
                "hostname",
                Box::new(|s| s.hostname = Some("two words".to_owned())),
            ),
            (
                "hosts[1].name",
                Box::new(|s| s.hosts[1].0 = "cache\n10.6.6.6 evil".to_owned()),
            ),
            (
                "hosts[1].name",
                Box::new(|s| s.hosts[1].0 = "cache#".to_owned()),
            ),
            (
                "hosts[0].address",
                Box::new(|s| s.hosts[0].1 = "10.0.0.256".to_owned()),
            ),
            (
                "hosts[0].address",
                Box::new(|s| s.hosts[0].1 = "db.example".to_owned()),
            ),
        ];
        for (field, break_it) in cases {
            let mut s = spec();
            break_it(&mut s);
            refused_naming(&s, field);
        }
    }

    /// AC-T7.1.1 (one code path, CLAUDE.md §8): a spec for a Windows node names Windows paths — a host
    /// path and a container path drive-absolute — and they pass as POSIX ones do; a drive-relative path
    /// (`C:data`), a UNC path, and a `..` behind a backslash are refused by the field's name.
    #[test]
    fn a_windows_path_passes_where_its_form_is_absolute() {
        let mut s = spec();
        s.rootfs = Rootfs::Layers(vec![r"D:\layers\base".into(), "C:/layers/app".into()]);
        s.mounts[0].source = r"D:\srv\data".into();
        s.mounts[0].destination = r"C:\data".to_owned();
        s.init.cwd = Some(r"c:\work".to_owned());
        assert_eq!(s.check(), Ok(()));
        type Break = Box<dyn Fn(&mut OciSpec)>;
        let cases: Vec<(&str, Break)> = vec![
            (
                "rootfs.layers[0]",
                Box::new(|s| s.rootfs = Rootfs::Layers(vec![r"D:layers".into()])),
            ),
            (
                "rootfs.layers[0]",
                Box::new(|s| s.rootfs = Rootfs::Layers(vec![r"\\server\share".into()])),
            ),
            (
                "mounts[0].destination",
                Box::new(|s| s.mounts[0].destination = r"C:\data\..\Windows".to_owned()),
            ),
            (
                "mounts[0].destination is the root",
                Box::new(|s| s.mounts[0].destination = r"C:\".to_owned()),
            ),
            (
                "init.cwd",
                Box::new(|s| s.init.cwd = Some("C:work".to_owned())),
            ),
        ];
        for (field, break_it) in cases {
            let mut w = s.clone();
            break_it(&mut w);
            refused_naming(&w, field);
        }
    }

    /// A host path config.json can't carry, bytes that aren't UTF-8, is refused by name rather than
    /// written lossily (a lossy path would name a different directory).
    #[cfg(unix)]
    #[test]
    fn a_host_path_that_isnt_utf8_is_refused_by_its_name() {
        use std::os::unix::ffi::OsStrExt;
        let mut s = spec();
        s.rootfs = Rootfs::Layers(vec![std::ffi::OsStr::from_bytes(b"/layers/\xff").into()]);
        refused_naming(&s, "rootfs.layers[0]");
    }

    /// AC-T7.4.1: explicit backend variants preserve the workload and exact host claims in JSON;
    /// the simulator creates their init and validates the same workload before accepting them.
    #[test]
    fn explicit_microvm_backends_roundtrip_and_validate_the_guest_workload() {
        use crate::{BoxState, Runtime};
        for (class, make) in [
            (
                Class::Firecracker,
                ClassSpec::Firecracker as fn(Box<OciSpec>) -> ClassSpec,
            ),
            (
                Class::Shards,
                ClassSpec::Shards as fn(Box<OciSpec>) -> ClassSpec,
            ),
        ] {
            let mut boxed = BoxSpec {
                id: BoxId::new(class.name()).unwrap(),
                bounds: Bounds::default(),
                class: make(Box::new(spec())),
                owner: None,
                allocation: None,
                route: None,
                devices: vec![DeviceId {
                    kind: crate::DeviceKind::Gpu,
                    key: u64::MAX,
                }],
                band: 0,
            };
            assert_eq!(boxed.class.class(), class);
            boxed.check().unwrap();
            assert!(boxed.check_run().is_err());
            let bytes = serde_json::to_vec(&boxed).unwrap();
            assert_eq!(serde_json::from_slice::<BoxSpec>(&bytes).unwrap(), boxed);
            boxed.devices.clear();
            let runtime = crate::sim::SimRuntime::new();
            let made = runtime.create(boxed.clone()).unwrap();
            assert_eq!((made.class, made.state), (class, BoxState::Created));
            runtime.start(&boxed.id).unwrap();
            runtime.delete(&boxed.id).unwrap();
            let mut bad = spec();
            bad.init.args.clear();
            boxed.class = make(Box::new(bad));
            assert!(boxed.check().is_err());
            assert!(runtime.create(boxed).is_err());
            assert!(runtime.list().unwrap().is_empty());
        }
    }

    fn image_box(image: ShardsImage) -> BoxSpec {
        BoxSpec {
            id: BoxId::new("image").unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::ShardsImage(Box::new(image)),
            owner: None,
            devices: Vec::new(),
            allocation: Some(crate::sim::sim_vm_allocation().unwrap()),
            route: None,
            band: 0,
        }
    }

    /// AC-T7.4.1 (joint plan step 17): do check an image VM naming the members its caller drives, a harness with a
    /// variable and a working directory and an agent with neither; expect it passes. Do name a member twice, give
    /// one a key with `=`, a value with a NUL, a relative, `..` or empty-component directory, a run-time argument
    /// holding a NUL (D256), and a name Shards' parser refuses; expect each refused as Invalid, naming the member's
    /// field, an argument by its place and never its bytes.
    #[test]
    fn the_members_an_image_vms_caller_drives_are_named_once_and_checked() {
        let harness = crate::Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let driven = ImageMember {
            member: harness.clone(),
            env: vec![("TZ".to_owned(), "UTC".to_owned())],
            workdir: Some("/home/me/project".to_owned()),
            args: vec![
                "--model".to_owned(),
                String::new(),
                "claude-opus-4".to_owned(),
            ],
            mcp: false,
        };
        let agent = ImageMember::new(crate::Member {
            kind: crate::MemberKind::Agent,
            name: "helper".to_owned(),
        });
        let with = |members: Vec<ImageMember>| ShardsImage {
            members,
            ..ShardsImage::new(crate::sim::SIM_IMAGE)
        };
        with(vec![driven.clone(), agent]).check().unwrap();
        image_box(with(vec![driven.clone()])).check().unwrap();
        let refused = |m: ImageMember| match with(vec![m]).check() {
            Err(Refusal::Invalid { what, why }) => (what, why),
            other => panic!("not refused as Invalid: {other:?}"),
        };
        match with(vec![driven.clone(), ImageMember::new(harness.clone())]).check() {
            Err(Refusal::Invalid { why, .. }) => assert!(why.contains("again"), "{why}"),
            other => panic!("a member named twice: {other:?}"),
        }
        let bad_key = ImageMember {
            env: vec![("A=B".to_owned(), "1".to_owned())],
            ..driven.clone()
        };
        let (what, why) = refused(bad_key);
        assert_eq!(
            (what, why.contains("members[0].env")),
            ("shards image", true),
            "{why}"
        );
        let bad_value = ImageMember {
            env: vec![("A".to_owned(), "1\0".to_owned())],
            ..driven.clone()
        };
        assert!(refused(bad_value).1.contains("members[0].env"));
        let bad_arg = ImageMember {
            args: vec!["--resume".to_owned(), "s-1\0--model".to_owned()],
            ..driven.clone()
        };
        let (what, why) = refused(bad_arg);
        assert_eq!(
            (
                what,
                why.contains("members[0].args[1]"),
                why.contains("s-1")
            ),
            ("shards image", true, false),
            "{why}"
        );
        for dir in [
            "relative/dir",
            "/home/../etc",
            "/home//me",
            "/home/./me",
            "",
        ] {
            let m = ImageMember {
                workdir: Some(dir.to_owned()),
                ..driven.clone()
            };
            assert!(refused(m).1.contains("members[0].workdir"), "{dir:?}");
        }
        let bad_name = ImageMember::new(crate::Member {
            kind: crate::MemberKind::Harness,
            name: "Claude".to_owned(),
        });
        assert!(refused(bad_name).1.contains("members[0].member.name"));
    }

    /// Joint plan step 12, Shards' D254 (AC-T7.4.1): a member's MCP bridge, asked or not. Do write a member asking none
    /// and one asking one as JSON, and read each back. Expect the first's JSON to leave `mcp` out, so that a member
    /// written before the field reads as asking none, and the second's to say it; each read back whole.
    #[test]
    fn w12_a_members_mcp_bridge_is_left_out_of_its_json_unless_asked() {
        let none = ImageMember::new(crate::Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        });
        let asked = ImageMember {
            mcp: true,
            ..none.clone()
        };
        let json = serde_json::to_value(&none).unwrap();
        assert!(json.get("mcp").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<ImageMember>(json).unwrap(), none);
        let json = serde_json::to_value(&asked).unwrap();
        assert_eq!(
            json.get("mcp"),
            Some(&serde_json::Value::Bool(true)),
            "{json}"
        );
        assert_eq!(serde_json::from_value::<ImageMember>(json).unwrap(), asked);
    }

    /// Joint plan step 12 for kiln's step 18 (AC-T7.4.1): the host directories an image VM's guest sees, each a share
    /// as Shards takes one (its host directory, where the guest sees it, read-only or not). Do check a VM sharing a
    /// home volume read-write and a project read-only, and one sharing none; then a share whose host directory is
    /// relative or holds a NUL, one the guest sees at a relative path, at its root or through `..`, and two the guest
    /// sees at one path. Expect the first two to pass and read back whole from JSON, none left out of it when there
    /// are none; each other refused as Invalid naming the share's field, by its place.
    #[test]
    fn w12_an_image_vms_shares_are_checked_by_field() {
        let share = |source: &str, destination: &str, read_only: bool| Mount {
            source: PathBuf::from(source),
            destination: destination.to_owned(),
            read_only,
            members: Vec::new(),
        };
        let home = share(
            "/var/lib/kiln/homes/ada/.claude",
            "/home/agent/.claude",
            false,
        );
        let project = share("/srv/projects/hex", "/work/hex", true);
        let with = |shares: Vec<Mount>| ShardsImage {
            shares,
            ..ShardsImage::new(crate::sim::SIM_IMAGE)
        };
        let shared = with(vec![home.clone(), project.clone()]);
        shared.check().unwrap();
        let json = serde_json::to_value(&shared).unwrap();
        assert_eq!(serde_json::from_value::<ShardsImage>(json).unwrap(), shared);
        let none = with(Vec::new());
        none.check().unwrap();
        assert!(serde_json::to_value(&none).unwrap().get("shares").is_none());
        let refused = |shares: Vec<Mount>| match with(shares).check() {
            Err(Refusal::Invalid { what, why }) => (what, why),
            other => panic!("not refused as Invalid: {other:?}"),
        };
        for (bad, field) in [
            (
                share("relative/home", "/home/agent", false),
                "shares[1].source",
            ),
            (share("/srv/a\0b", "/home/agent", false), "shares[1].source"),
            (
                share("/srv/home", "home/agent", false),
                "shares[1].destination",
            ),
            (share("/srv/home", "/", false), "shares[1].destination"),
            (
                share("/srv/home", "/home/../etc", false),
                "shares[1].destination",
            ),
            (
                share("/srv/other", "/work/hex", false),
                "shares[1].destination",
            ),
        ] {
            let (what, why) = refused(vec![project.clone(), bad.clone()]);
            assert_eq!(
                (what, why.contains(field)),
                ("shards image", true),
                "{bad:?}: {why}"
            );
        }
    }

    /// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1; Shards' D271, D281): the volumes of the node's Shards home an image VM's
    /// members are given as its run starts. Do check a VM given a volume read-write into its harness and one read-only
    /// into its harness and an agent, beside a project share, and one given none. Expect both to pass and read back
    /// whole from JSON, none left out of it when there are none. Do give a volume no name, a name with a NUL, a place
    /// that is relative, a top-level directory alone, through `..` or with an empty component, the share's place or
    /// another volume's, no member, a member twice and a member's name Shards' parser refuses. Expect each refused as
    /// Invalid naming the volume's field, by its place.
    #[test]
    fn p6_2_an_image_vms_volumes_are_checked_by_field() {
        let harness = crate::Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let agent = crate::Member {
            kind: crate::MemberKind::Agent,
            name: "helper".to_owned(),
        };
        let volume = |name: &str, at: &str, members: Vec<crate::Member>| ImageVolume {
            name: name.to_owned(),
            bound: NonZeroU64::new(1 << 30).unwrap(),
            at: at.to_owned(),
            members,
            read_only: false,
        };
        let cache = volume("kiln-agent-cache", "/volumes/cache", vec![harness.clone()]);
        let notes = ImageVolume {
            read_only: true,
            ..volume(
                "kiln-agent-notes",
                "/volumes/notes",
                vec![harness.clone(), agent.clone()],
            )
        };
        let with = |volumes: Vec<ImageVolume>| ShardsImage {
            shares: vec![Mount {
                source: PathBuf::from("/srv/projects/hex"),
                destination: "/work/hex".to_owned(),
                read_only: false,
                members: Vec::new(),
            }],
            volumes,
            ..ShardsImage::new(crate::sim::SIM_IMAGE)
        };
        let given = with(vec![cache.clone(), notes]);
        given.check().unwrap();
        let json = serde_json::to_value(&given).unwrap();
        assert_eq!(serde_json::from_value::<ShardsImage>(json).unwrap(), given);
        let none = with(Vec::new());
        none.check().unwrap();
        assert!(
            serde_json::to_value(&none)
                .unwrap()
                .get("volumes")
                .is_none()
        );
        let shouty = crate::Member {
            kind: crate::MemberKind::Harness,
            name: "Claude".to_owned(),
        };
        for (bad, field) in [
            (
                volume("", "/volumes/x", vec![harness.clone()]),
                "volumes[1].name",
            ),
            (
                volume("a\0b", "/volumes/x", vec![harness.clone()]),
                "volumes[1].name",
            ),
            (
                volume("x", "volumes/x", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (
                volume("x", "/volumes", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (
                volume("x", "/volumes/../etc", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (
                volume("x", "/volumes//x", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (
                volume("x", "/work/hex", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (
                volume("x", "/volumes/cache", vec![harness.clone()]),
                "volumes[1].at",
            ),
            (volume("x", "/volumes/x", Vec::new()), "volumes[1].members"),
            (
                volume("x", "/volumes/x", vec![harness.clone(), harness.clone()]),
                "volumes[1].members[1]",
            ),
            (
                volume("x", "/volumes/x", vec![shouty]),
                "volumes[1].members[0]",
            ),
        ] {
            match with(vec![cache.clone(), bad.clone()]).check() {
                Err(Refusal::Invalid { what, why }) => assert_eq!(
                    (what, why.contains(field)),
                    ("shards image", true),
                    "{bad:?}: {why}"
                ),
                other => panic!("{bad:?}: not refused as Invalid: {other:?}"),
            }
        }
    }

    /// P1.4 (kiln's share member scope; Shards' D277, AC-T7.4.1): a share names the members given it as they start. Do
    /// check a VM sharing a project with its harness; one whose share names its harness twice, or a member by no
    /// member's name; and a container mounting a directory for a member. Expect the first to pass and read back whole
    /// from JSON, a share with no scope written without one; the two shares refused as Invalid naming their member's
    /// field; and the container's mount refused as Invalid naming its members.
    #[test]
    fn p14_a_shares_member_scope_is_checked_and_a_containers_mount_names_none() {
        let codex = crate::Member {
            kind: crate::MemberKind::Harness,
            name: "codex".to_owned(),
        };
        let share = |members: Vec<crate::Member>| Mount {
            source: PathBuf::from("/srv/projects/hex"),
            destination: "/work/hex".to_owned(),
            read_only: false,
            members,
        };
        let with = |shares: Vec<Mount>| ShardsImage {
            shares,
            ..ShardsImage::new(crate::sim::SIM_IMAGE)
        };
        let scoped = with(vec![share(vec![codex.clone()])]);
        scoped.check().unwrap();
        let json = serde_json::to_value(&scoped).unwrap();
        assert_eq!(serde_json::from_value::<ShardsImage>(json).unwrap(), scoped);
        let unscoped = serde_json::to_value(share(Vec::new())).unwrap();
        assert!(unscoped.get("members").is_none(), "{unscoped}");
        let unnamed = crate::Member {
            kind: crate::MemberKind::Agent,
            name: "Not A Name".to_owned(),
        };
        for (members, field) in [
            (vec![codex.clone(), codex.clone()], "shares[0].members[1]"),
            (vec![unnamed], "shares[0].members[0]"),
        ] {
            match with(vec![share(members)]).check() {
                Err(Refusal::Invalid {
                    what: "shards image",
                    why,
                }) => assert!(why.contains(field), "{why}"),
                other => panic!("not refused as Invalid: {other:?}"),
            }
        }
        let mut container = spec();
        container.mounts = vec![share(vec![codex])];
        match container.check() {
            Err(Refusal::Invalid {
                what: "oci spec",
                why,
            }) => assert!(why.contains("mounts[0].members"), "{why}"),
            other => panic!("not refused as Invalid: {other:?}"),
        }
    }

    /// The review's B-10 (AC-T7.4.1, joint plan step 12): two shares the guest sees at one place are one share too
    /// many, however their paths are spelled, or the read-only one's intent is lost to the order they are mounted in.
    /// Do check a VM sharing a project read-only at `/work/hex` beside another directory, read-write, at the same place
    /// spelled apart: a trailing slash, a `.` component, doubled slashes, a trailing `.`; and drive-absolute, the same
    /// drive in the other case and with the other separator. Expect each refused as Invalid naming shares[1]'s
    /// destination. Then shares at other places: a sibling whose name only begins alike, a folder above it, and the
    /// same path on another drive. Expect those to pass.
    #[test]
    fn b10_two_shares_at_one_guest_place_spelled_apart_are_refused() {
        let share = |source: &str, destination: &str, read_only: bool| Mount {
            source: PathBuf::from(source),
            destination: destination.to_owned(),
            read_only,
            members: Vec::new(),
        };
        let with = |first: &str, second: &str| ShardsImage {
            shares: vec![
                share("/srv/projects/hex", first, true),
                share("/srv/other", second, false),
            ],
            ..ShardsImage::new(crate::sim::SIM_IMAGE)
        };
        for (first, second) in [
            ("/work/hex", "/work/hex/"),
            ("/work/hex", "/work/./hex"),
            ("/work/hex", "//work/hex"),
            ("/work/hex", "/work//hex"),
            ("/work/hex", "/work/hex/."),
            ("C:\\work\\hex", "c:/work/hex/"),
        ] {
            match with(first, second).check() {
                Err(Refusal::Invalid { what, why }) => assert_eq!(
                    (what, why.contains("shares[1].destination")),
                    ("shards image", true),
                    "{first:?} and {second:?}: {why}"
                ),
                other => panic!("{first:?} and {second:?}, one place: {other:?}"),
            }
        }
        for (first, second) in [
            ("/work/hex", "/work/hexa"),
            ("/work/hex", "/work"),
            ("C:\\work\\hex", "D:\\work\\hex"),
        ] {
            assert_eq!(
                with(first, second).check(),
                Ok(()),
                "{first:?} and {second:?} are two places"
            );
        }
    }

    /// AC-T7.4.1 (joint plan step 12): a Shards image VM of its own class (P2.2), its image pinned by a digest
    /// and its size its allocation's. Do: check a pinned image running its members alone, and one with a
    /// command, and read each back from JSON. Expect: both pass, read back whole, and a run of either is
    /// refused (a VM has an init). Do: break the image's pin, its digest's algorithm or digits, the name
    /// before it, the command, the allocation and the devices. Expect: each refused as Invalid naming the
    /// field, the box's whole check included.
    #[test]
    fn a_shards_image_vm_is_pinned_sized_and_refused_by_its_field() {
        let pinned = ShardsImage::new(crate::sim::SIM_IMAGE);
        let commanded = ShardsImage {
            command: Some(vec!["sleep".to_owned(), "600".to_owned()]),
            ..pinned.clone()
        };
        for image in [pinned.clone(), commanded] {
            let boxed = image_box(image);
            assert_eq!(boxed.class.class(), Class::ShardsImage);
            assert_eq!(boxed.check(), Ok(()));
            assert!(matches!(
                boxed.check_run(),
                Err(Refusal::Invalid { what: "run", .. })
            ));
            let json = serde_json::to_vec(&boxed).unwrap();
            assert_eq!(serde_json::from_slice::<BoxSpec>(&json).unwrap(), boxed);
        }
        let sha512 = format!("harness@sha512:{}", "0f".repeat(64));
        assert_eq!(ShardsImage::new(&sha512).check(), Ok(()));
        let digits = "ab".repeat(32);
        type Break = Box<dyn Fn(&mut BoxSpec)>;
        let image = |text: String| -> Break {
            Box::new(move |spec: &mut BoxSpec| {
                if let ClassSpec::ShardsImage(image) = &mut spec.class {
                    image.image = text.clone();
                }
            })
        };
        let cases: Vec<(&str, Break)> = vec![
            ("pinned", image("harness:1".to_owned())),
            ("algorithm", image(format!("harness@md5:{digits}"))),
            (
                "hex digits",
                image(format!("harness@sha256:{}", "AB".repeat(32))),
            ),
            ("hex digits", image(format!("harness@sha256:{digits}0"))),
            ("names no image", image(format!("@sha256:{digits}"))),
            (
                "names no image",
                image(format!("two words@sha256:{digits}")),
            ),
            (
                "command is empty",
                Box::new(|spec: &mut BoxSpec| {
                    if let ClassSpec::ShardsImage(image) = &mut spec.class {
                        image.command = Some(Vec::new());
                    }
                }),
            ),
            (
                "command[0] is empty",
                Box::new(|spec: &mut BoxSpec| {
                    if let ClassSpec::ShardsImage(image) = &mut spec.class {
                        image.command = Some(vec![String::new()]);
                    }
                }),
            ),
            (
                "command[1] holds a NUL",
                Box::new(|spec: &mut BoxSpec| {
                    if let ClassSpec::ShardsImage(image) = &mut spec.class {
                        image.command = Some(vec!["sh".to_owned(), "a\0b".to_owned()]);
                    }
                }),
            ),
            (
                "allocation",
                Box::new(|spec: &mut BoxSpec| spec.allocation = None),
            ),
            (
                "devices",
                Box::new(|spec: &mut BoxSpec| {
                    spec.devices = vec![DeviceId {
                        kind: crate::DeviceKind::Gpu,
                        key: 1,
                    }]
                }),
            ),
        ];
        for (says, break_it) in cases {
            let mut boxed = image_box(pinned.clone());
            break_it(&mut boxed);
            match boxed.check() {
                Err(Refusal::Invalid {
                    what: "shards image",
                    why,
                }) => assert!(why.contains(says), "{says}: refused as {why:?}"),
                other => panic!("{says}: {other:?}"),
            }
        }
    }

    /// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2; shards-image-class.md §11): a scheduler's payload of an image VM names no
    /// allocation, its scheduler deriving it as it admits the box. Do: check an image VM with no allocation as a
    /// payload and as a box to make; then break its pin, and name a device. Expect: the payload passes and the box is
    /// refused naming its allocation; each break refused by both checks alike, naming its field.
    #[test]
    fn p2_2_an_image_payload_is_checked_without_the_allocation_its_scheduler_derives() {
        let mut payload = image_box(ShardsImage::new(crate::sim::SIM_IMAGE));
        payload.allocation = None;
        assert_eq!(payload.check_payload(), Ok(()));
        match payload.check() {
            Err(Refusal::Invalid {
                what: "shards image",
                why,
            }) => assert!(why.contains("allocation"), "refused as {why:?}"),
            other => panic!("{other:?}"),
        }
        type Break = Box<dyn Fn(&mut BoxSpec)>;
        let cases: Vec<(&str, Break)> = vec![
            (
                "pinned",
                Box::new(|spec: &mut BoxSpec| {
                    if let ClassSpec::ShardsImage(image) = &mut spec.class {
                        image.image = "harness:1".to_owned();
                    }
                }),
            ),
            (
                "devices",
                Box::new(|spec: &mut BoxSpec| {
                    spec.devices = vec![DeviceId {
                        kind: crate::DeviceKind::Gpu,
                        key: 1,
                    }]
                }),
            ),
        ];
        for (says, break_it) in cases {
            let mut broken = payload.clone();
            break_it(&mut broken);
            for checked in [broken.check_payload(), broken.check()] {
                match checked {
                    Err(Refusal::Invalid {
                        what: "shards image",
                        why,
                    }) => assert!(why.contains(says), "{says}: refused as {why:?}"),
                    other => panic!("{says}: {other:?}"),
                }
            }
        }
    }

    fn refused_naming(s: &OciSpec, field: &str) {
        match s.check() {
            Err(Refusal::Invalid {
                what: "oci spec",
                why,
            }) => assert!(why.contains(field), "{field}: refused as {why:?}"),
            other => panic!("{field}: {other:?}"),
        }
    }
}
