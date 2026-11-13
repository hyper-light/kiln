//! AC-T7.4.1: the native Linux consumer of the scheduler's exact, durable device claims.

use super::*;
use hex_api::{DeviceId, DeviceKind};
use hex_core::cdi::{DeviceType, Edits};
use hex_runtime::devices::{Catalog, Route};

mod guest;
mod identity;

impl Node {
    pub(super) fn hold_attachments(&self, devices: &[DeviceId]) -> Result<Vec<Turn<'_>>, Refusal> {
        let ordered = self.attachments.conflict_ids(devices)?;
        let mut held = Vec::new();
        held.try_reserve_exact(ordered.len())
            .map_err(|_| full(ordered.len()))?;
        for device in ordered {
            let id = BoxId::new(&format!("device-{}-{}", device.kind.name(), device.key))?;
            held.push(self.attachment_turns.take(&id)?);
        }
        Ok(held)
    }

    pub(super) fn attachment_capacity(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        if self.attachments.entries().is_empty() {
            return Ok(Vec::new());
        }
        let records = self.store.all()?;
        let pools = self.store.all_pools()?;
        let mut reports = Vec::new();
        reports
            .try_reserve_exact(self.attachments.entries().len())
            .map_err(|_| full(self.attachments.entries().len()))?;
        for entry in self.attachments.entries_for(Route::Cdi) {
            if entry.info.id.kind == DeviceKind::Net {
                return Err(Refusal::Platform {
                    what: "device attachment",
                    platform: "Linux OCI",
                    why: "configured native binding is unsupported".into(),
                });
            }
            if entry.info.id.kind == DeviceKind::Block {
                let major = entry.info.id.key >> u32::BITS;
                let minor = entry.info.id.key & u64::from(u32::MAX);
                let size = fs::read_to_string(format!("/sys/dev/block/{major}:{minor}/size"))
                    .map_err(|error| Refusal::os("reading native block device capacity", error))?;
                if hex_sim::facts::block_size(&size) != Some(entry.info.size) {
                    return Err(Refusal::Invalid {
                        what: "native block device",
                        why: "configured size disagrees with the kernel device".into(),
                    });
                }
            }
            let holder = self.attachments.holder(entry.info.id, &records, &pools)?;
            reports.push(entry.report(holder)?);
        }
        Ok(reports)
    }

    pub(super) fn render_attachments(
        &self,
        full: &BoxSpec,
        spec: &OciSpec,
        place: &Place<'_>,
    ) -> Result<Bundle, Refusal> {
        let guest = self
            .attachments
            .entries_for(Route::GuestGpu)
            .any(|entry| full.devices.contains(&entry.info.id));
        let authority = if guest {
            Some(guest::authority(full)?)
        } else {
            None
        };
        let plan = match &authority {
            Some(authority) => self.attachments.guest_cdi(full, authority)?,
            None => {
                if full
                    .devices
                    .iter()
                    .any(|device| device.kind == DeviceKind::Gpu)
                {
                    guest::pci(full)?;
                }
                self.attachments.cdi(&full.devices)?
            }
        };
        let edits = plan.edits()?;
        let resolved = match &authority {
            Some(authority) => {
                let nodes = resolve_nodes(&edits)?;
                guest::check(full, authority, &nodes)?;
                nodes
            }
            None => resolve(&edits, &full.devices)?,
        };
        bundle::render_with_devices(spec, place, &edits, &resolved)
    }
}

fn full(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "native device attachment items",
        bound,
    }
}

fn resolve<'a>(
    edits: &Edits<'a>,
    devices: &[DeviceId],
) -> Result<Vec<bundle::ResolvedNode<'a>>, Refusal> {
    let nodes = resolve_nodes(edits)?;
    let block_key = |node: &bundle::ResolvedNode<'_>| {
        if node.kind == DeviceType::Block {
            Some((u64::from(node.major) << u32::BITS) | u64::from(node.minor))
        } else {
            None
        }
    };
    if devices
        .iter()
        .filter(|device| device.kind == DeviceKind::Block)
        .any(|device| !nodes.iter().any(|node| block_key(node) == Some(device.key)))
        || nodes.iter().filter_map(block_key).any(|key| {
            !devices
                .iter()
                .any(|device| device.kind == DeviceKind::Block && device.key == key)
        })
    {
        return Err(Refusal::Invalid {
            what: "CDI block device assignment",
            why: "selected device nodes disagree with reserved block identities".into(),
        });
    }
    identity::check(&nodes, devices)?;
    Ok(nodes)
}

fn resolve_nodes<'a>(edits: &Edits<'a>) -> Result<Vec<bundle::ResolvedNode<'a>>, Refusal> {
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(edits.device_nodes.len())
        .map_err(|_| full(edits.device_nodes.len()))?;
    for node in &edits.device_nodes {
        let path = node
            .host_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(&node.path);
        let stat = rustix::fs::stat(path)
            .map_err(|error| Refusal::os("reading a selected CDI device", error))?;
        let kind = match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::CharacterDevice => DeviceType::Char,
            rustix::fs::FileType::BlockDevice => DeviceType::Block,
            rustix::fs::FileType::Fifo => DeviceType::Fifo,
            _ => {
                return Err(Refusal::Invalid {
                    what: "CDI device node",
                    why: "selected host object is not a character, block or FIFO device".into(),
                });
            }
        };
        nodes.push(bundle::ResolvedNode {
            node,
            kind,
            major: rustix::fs::major(stat.st_rdev),
            minor: rustix::fs::minor(stat.st_rdev),
            // POSIX permission/setid/sticky bits; file type is represented separately above.
            file_mode: stat.st_mode & 0o7777,
        });
    }
    Ok(nodes)
}

pub(super) fn catalog(path: &Path) -> Result<(Catalog, Turns), Refusal> {
    let catalog = Catalog::open(path)?;
    let turns = Turns::new(
        "assigning or removing a physical device",
        "physical device changes under way",
        catalog.entries().len(),
        Patience::measuring("node.turns"),
    );
    Ok((catalog, turns))
}
