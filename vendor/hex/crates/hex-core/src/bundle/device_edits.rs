//! CDI 1.1 Apply at 98a7d735, with host lookup supplied by the caller. Source/compatibility
//! notes: docs/research/measurements/hex-device-bridge-cdi/RENDER-READBACK.md.

use std::fmt;
use std::io::{self, Write};

use super::*;
use cdi::{DeviceType, HookName};

/// A host-stat receipt for one exact selected node, in selection order. The shell must obtain
/// these facts from that node's hostPath (or path when hostPath is absent/empty). This pure core
/// checks correspondence and values; it cannot authenticate an OS observation or prevent races.
pub struct ResolvedNode<'a> {
    pub node: &'a cdi::DeviceNode,
    pub kind: DeviceType,
    pub major: u32,
    pub minor: u32,
    /// Linux stat mode with file-type bits removed, including setuid/setgid/sticky permissions.
    pub file_mode: u32,
}

impl fmt::Debug for ResolvedNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedNode(<redacted>)")
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Device<'a> {
    path: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    major: i64,
    minor: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gid: Option<u32>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Hooks<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    prestart: Vec<Hook<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    create_runtime: Vec<Hook<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    create_container: Vec<Hook<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    start_container: Vec<Hook<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    poststart: Vec<Hook<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    poststop: Vec<Hook<'a>>,
}

#[derive(Serialize)]
struct Hook<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    args: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    env: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
enum Cause {
    Count,
    Selection,
    HostType,
    FileMode,
    TypeMismatch,
    Invalid,
    Limit,
    Allocation,
    Encoding,
}

/// The existing public Refusal has String presentation. Keep the bridge field/cause-only;
/// callers branch on Refusal variants, never on serde/OS/private payload text.
fn refuse(field: &'static str, item: Option<usize>, cause: Cause) -> Refusal {
    match cause {
        Cause::Limit => Refusal::Exhausted {
            what: field,
            bound: MAX_CONFIG_BYTES,
        },
        Cause::Allocation => Refusal::Exhausted {
            what: "cdi render allocation",
            bound: MAX_CONFIG_BYTES,
        },
        _ => Refusal::Invalid {
            what: field,
            why: format!("item {item:?}: {cause:?}"),
        },
    }
}

struct Budget(usize);

impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<(), Refusal> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|n| *n <= MAX_CONFIG_BYTES)
            .ok_or_else(|| refuse("cdi edits bytes", None, Cause::Limit))?;
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), Refusal> {
        self.charge(text.len())
    }

    fn list(&mut self, len: usize) -> Result<(), Refusal> {
        // Every represented list item consumes at least one encoded byte. Charge operations
        // even when a later replacement makes their final JSON smaller.
        self.charge(len)
    }
}

pub(super) fn check(edits: &cdi::Edits<'_>, resolved: &[ResolvedNode<'_>]) -> Result<(), Refusal> {
    // Native support must precede any returned output; writing an OCI field the backend ignores
    // would be a false success. Do not inspect or print either private payload.
    for (present, field) in [
        (!edits.net_devices.is_empty(), "cdi netDevices"),
        (edits.intel_rdt.is_some(), "cdi intelRdt"),
    ] {
        if present {
            return Err(Refusal::Platform {
                what: field,
                platform: "hex native OCI",
                why: "native enforcement is unavailable".to_owned(),
            });
        }
    }
    if resolved.len() != edits.device_nodes.len() {
        return Err(refuse("cdi resolution count", None, Cause::Count));
    }
    let mut budget = Budget(0);
    for len in [
        edits.env.len(),
        edits.device_nodes.len(),
        edits.mounts.len(),
        edits.hooks.len(),
        edits.additional_gids.len(),
    ] {
        budget.list(len)?;
    }
    for (i, env) in edits.env.iter().enumerate() {
        check_env(&mut budget, env, "cdi env", i)?;
    }
    for (i, (node, receipt)) in edits.device_nodes.iter().zip(resolved).enumerate() {
        if !std::ptr::eq(*node, receipt.node) {
            return Err(refuse("cdi resolution", Some(i), Cause::Selection));
        }
        if !matches!(
            receipt.kind,
            DeviceType::Block | DeviceType::Char | DeviceType::Fifo
        ) {
            return Err(refuse("cdi resolution", Some(i), Cause::HostType));
        }
        // stat.Mode &^ S_IFMT, Linux permissions and special mode bits (07777).
        if receipt.file_mode & !0o7777 != 0 {
            return Err(refuse("cdi resolution", Some(i), Cause::FileMode));
        }
        budget.text(&node.path)?;
        budget.text(node.host_path.as_deref().unwrap_or_default())?;
        budget.text(node.permissions.as_deref().unwrap_or_default())?;
        if node.path.is_empty()
            || node
                .permissions
                .as_deref()
                .is_some_and(|s| !s.bytes().all(|b| matches!(b, b'r' | b'w' | b'm')))
        {
            return Err(refuse("cdi deviceNodes", Some(i), Cause::Invalid));
        }
        // Validate the effective type using the same minimal-spec predicate as Go, before any
        // Config edit. Explicit minimally specified values deliberately override host identity.
        effective_node(node, receipt, 0, 0)?;
    }
    for (i, mount) in edits.mounts.iter().enumerate() {
        budget.text(&mount.host_path)?;
        budget.text(&mount.container_path)?;
        budget.text(mount.kind.as_deref().unwrap_or_default())?;
        budget.list(mount.options.len())?;
        for option in &mount.options {
            budget.text(option)?;
        }
        if mount.host_path.is_empty() || mount.container_path.is_empty() {
            return Err(refuse("cdi mounts", Some(i), Cause::Invalid));
        }
    }
    for (i, hook) in edits.hooks.iter().enumerate() {
        budget.text(&hook.path)?;
        budget.list(hook.args.len())?;
        for arg in &hook.args {
            budget.text(arg)?;
        }
        budget.list(hook.env.len())?;
        for env in &hook.env {
            check_env(&mut budget, env, "cdi hooks.env", i)?;
        }
        if !hook.path.starts_with('/') || hook.timeout.is_some_and(|n| n <= 0) {
            return Err(refuse("cdi hooks", Some(i), Cause::Invalid));
        }
    }
    Ok(())
}

fn check_env(budget: &mut Budget, env: &str, field: &'static str, i: usize) -> Result<(), Refusal> {
    budget.text(env)?;
    if !env.split_once('=').is_some_and(|(key, _)| !key.is_empty()) {
        return Err(refuse(field, Some(i), Cause::Invalid));
    }
    Ok(())
}

fn kind(kind: DeviceType) -> &'static str {
    match kind {
        DeviceType::Unspecified => "",
        DeviceType::Block => "b",
        DeviceType::Char => "c",
        DeviceType::UnbufferedChar => "u",
        DeviceType::Fifo => "p",
    }
}

fn effective_node<'a>(
    node: &'a cdi::DeviceNode,
    receipt: &ResolvedNode<'_>,
    uid: u32,
    gid: u32,
) -> Result<Device<'a>, Refusal> {
    let specified = node.kind.filter(|k| *k != DeviceType::Unspecified);
    let mut major = node.major.unwrap_or_default();
    let mut minor = node.minor.unwrap_or_default();
    let minimal = specified.is_some() && (major != 0 || specified == Some(DeviceType::Fifo));
    let actual = specified.unwrap_or(receipt.kind);
    if !minimal {
        if actual != receipt.kind {
            return Err(refuse("cdi deviceNodes.type", None, Cause::TypeMismatch));
        }
        if actual != DeviceType::Fifo && major == 0 {
            major = i64::from(receipt.major);
            minor = i64::from(receipt.minor);
        }
    }
    Ok(Device {
        path: &node.path,
        kind: kind(actual),
        major,
        minor,
        file_mode: Some(node.file_mode.unwrap_or(receipt.file_mode)),
        uid: node.uid.or(if uid != 0 { Some(uid) } else { None }),
        gid: node.gid.or(if gid != 0 { Some(gid) } else { None }),
    })
}

fn reserve<T>(values: &mut Vec<T>, extra: usize, field: &'static str) -> Result<(), Refusal> {
    values
        .len()
        .checked_add(extra)
        .filter(|n| *n <= MAX_CONFIG_BYTES)
        .ok_or_else(|| refuse(field, None, Cause::Limit))?;
    values
        .try_reserve(extra)
        .map_err(|_| refuse(field, None, Cause::Allocation))
}

pub(super) fn default_rules() -> Result<Vec<DeviceRule<'static>>, Refusal> {
    let mut rules = Vec::new();
    // One existing deny-all rule: make its new extensible representation fallible too.
    reserve(&mut rules, 1, "oci device rules")?;
    rules.push(DeviceRule {
        allow: false,
        kind: None,
        major: None,
        minor: None,
        access: "rwm",
    });
    Ok(rules)
}

pub(super) fn apply<'a>(
    config: &mut Config<'a>,
    edits: &'a cdi::Edits<'_>,
    resolved: &[ResolvedNode<'_>],
) -> Result<(), Refusal> {
    apply_env(&mut config.process.env, &edits.env)?;
    reserve(
        &mut config.linux.devices,
        edits.device_nodes.len(),
        "cdi deviceNodes",
    )?;
    reserve(
        &mut config.linux.resources.devices,
        edits.device_nodes.len(),
        "cdi device grants",
    )?;
    for (node, receipt) in edits.device_nodes.iter().zip(resolved) {
        let dev = effective_node(
            node,
            receipt,
            config.process.user.uid,
            config.process.user.gid,
        )?;
        if let Some(i) = config
            .linux
            .devices
            .iter()
            .position(|old| old.path == dev.path)
        {
            config.linux.devices.remove(i);
        }
        if matches!(dev.kind, "b" | "c") {
            let permissions = node
                .permissions
                .as_deref()
                .filter(|p| !p.is_empty())
                .unwrap_or("rwm");
            config.linux.resources.devices.push(DeviceRule {
                allow: true,
                kind: Some(dev.kind),
                major: Some(dev.major),
                minor: Some(dev.minor),
                access: permissions,
            });
        }
        config.linux.devices.push(dev);
    }
    apply_mounts(&mut config.mounts, &edits.mounts)?;
    if !edits.hooks.is_empty() {
        let mut hooks = Hooks::default();
        for hook in &edits.hooks {
            let list = match hook.hook_name {
                HookName::Prestart => &mut hooks.prestart,
                HookName::CreateRuntime => &mut hooks.create_runtime,
                HookName::CreateContainer => &mut hooks.create_container,
                HookName::StartContainer => &mut hooks.start_container,
                HookName::Poststart => &mut hooks.poststart,
                HookName::Poststop => &mut hooks.poststop,
            };
            reserve(list, 1, "cdi hooks")?;
            list.push(Hook {
                path: &hook.path,
                args: &hook.args,
                env: &hook.env,
                timeout: hook.timeout,
            });
        }
        config.hooks = Some(hooks);
    }
    if edits
        .additional_gids
        .iter()
        .any(|gid| *gid != 0 && !config.process.user.additional_gids.contains(gid))
    {
        let mut groups = Vec::new();
        reserve(
            &mut groups,
            config.process.user.additional_gids.len(),
            "cdi additionalGids",
        )?;
        groups.extend_from_slice(&config.process.user.additional_gids);
        reserve(
            &mut groups,
            edits.additional_gids.len(),
            "cdi additionalGids",
        )?;
        for gid in &edits.additional_gids {
            if *gid != 0 && !groups.contains(gid) {
                groups.push(*gid);
            }
        }
        config.process.user.additional_gids = Cow::Owned(groups);
    }
    Ok(())
}

fn apply_env<'a>(env: &mut Vec<Cow<'a, str>>, edits: &[&'a str]) -> Result<(), Refusal> {
    reserve(env, edits.len(), "cdi env")?;
    let original = env.len();
    for entry in edits {
        let key = entry
            .split_once('=')
            .map(|(key, _)| key)
            .ok_or_else(|| refuse("cdi env", None, Cause::Invalid))?;
        // Pinned runtime-tools seeds its map with full original entries, then keys additions
        // by variable name. Search only appended entries to reproduce that behavior, no cache.
        if let Some(old) = env
            .iter_mut()
            .skip(original)
            .find(|old| old.split_once('=').is_some_and(|(k, _)| k == key))
        {
            *old = Cow::Borrowed(entry);
        } else {
            env.push(Cow::Borrowed(entry));
        }
    }
    Ok(())
}

fn apply_mounts<'a>(mounts: &mut Vec<Mount<'a>>, edits: &[&'a cdi::Mount]) -> Result<(), Refusal> {
    if edits.is_empty() {
        return Ok(());
    }
    reserve(mounts, edits.len(), "cdi mounts")?;
    for mount in edits {
        if let Some(i) = mounts
            .iter()
            .position(|old| old.destination == mount.container_path)
        {
            mounts.remove(i);
        }
        mounts.push(Mount {
            destination: Cow::Borrowed(&mount.container_path),
            kind: mount.kind.as_deref().unwrap_or_default(),
            source: Cow::Borrowed(&mount.host_path),
            options: Cow::Borrowed(&mount.options),
            order: (0, 0),
        });
    }
    for (i, mount) in mounts.iter_mut().enumerate() {
        mount.order = (linux_depth(&mount.destination)?, i);
    }
    // An ordinal makes equal-depth ordering stable without std's allocating stable sort.
    mounts.sort_unstable_by_key(|mount| mount.order);
    Ok(())
}

/// Separator count after filepath.Clean on Linux, without normalizing the emitted destination.
fn linux_depth(path: &str) -> Result<usize, Refusal> {
    let mut parents = 0usize;
    let mut normal = 0usize;
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if normal > 0 => {
                normal = normal
                    .checked_sub(1)
                    .ok_or_else(|| refuse("cdi mounts", None, Cause::Invalid))?
            }
            ".." if !path.starts_with('/') => {
                parents = parents
                    .checked_add(1)
                    .ok_or_else(|| refuse("cdi mounts", None, Cause::Limit))?
            }
            ".." => {}
            _ => {
                normal = normal
                    .checked_add(1)
                    .ok_or_else(|| refuse("cdi mounts", None, Cause::Limit))?
            }
        }
    }
    let parts = parents
        .checked_add(normal)
        .ok_or_else(|| refuse("cdi mounts", None, Cause::Limit))?;
    Ok(if path.starts_with('/') {
        parts.max(1)
    } else {
        parts.saturating_sub(1)
    })
}

struct Output {
    bytes: Vec<u8>,
    failure: Option<Cause>,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_CONFIG_BYTES)
        {
            self.failure = Some(Cause::Limit);
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        if self.bytes.try_reserve(bytes.len()).is_err() {
            self.failure = Some(Cause::Allocation);
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode(config: &Config<'_>) -> Result<Vec<u8>, Refusal> {
    let mut output = Output {
        bytes: Vec::new(),
        failure: None,
    };
    if serde_json::to_writer_pretty(&mut output, config).is_err() {
        if matches!(output.failure, Some(Cause::Limit)) {
            return Err(invalid(format!(
                "config.json is over the {MAX_CONFIG_BYTES} the runtime reads"
            )));
        }
        return Err(refuse(
            "config.json",
            None,
            output.failure.unwrap_or(Cause::Encoding),
        ));
    }
    Ok(output.bytes)
}

#[cfg(test)]
mod tests;
