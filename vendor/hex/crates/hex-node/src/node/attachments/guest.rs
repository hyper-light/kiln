//! AC-T7.4.1: dedicated Shards render receipts require actual boot and kernel correspondence.
//! This is attachment authority for one owner/full selected set, never measured GPU capacity.

use hex_api::{BoxSpec, Refusal};
use hex_core::bundle::ResolvedNode;
use hex_core::cdi::DeviceType;
use hex_runtime::guest::gpu::{self, Authority};

/// Called before guest_cdi selection. The real immutable kernel command line, rather than a box
/// payload/environment/catalog label, is the source of the paired owner and full physical set.
pub(super) fn authority(spec: &BoxSpec) -> Result<Authority, Refusal> {
    let authority =
        gpu::boot_authority()?.ok_or_else(|| invalid("no initialized guest GPU boot authority"))?;
    authority.check(spec)?;
    Ok(authority)
}

/// A Firecracker guest's attached PCI GPUs are granted as its boot words say (`gpu::Boot::check`).
/// A host's own kernel line carries no such words, so a host node's CDI GPUs are unaffected.
pub(super) fn pci(spec: &BoxSpec) -> Result<(), Refusal> {
    match gpu::boot()? {
        Some(boot @ gpu::Boot::Pci { .. }) => boot.check(spec),
        Some(gpu::Boot::Renderer(_)) | None => Ok(()),
    }
}

/// All injected objects must be initialized virtio render nodes. Ordinary host CDI continues to
/// use its native PCI/NVIDIA checks and cannot opt into this validation through a selector label.
pub(super) fn check(
    spec: &BoxSpec,
    authority: &Authority,
    nodes: &[ResolvedNode<'_>],
) -> Result<(), Refusal> {
    authority.check(spec)?;
    if nodes.is_empty() {
        return Err(invalid("guest GPU recipe grants no actual render node"));
    }
    for node in nodes {
        if node.kind != DeviceType::Char {
            return Err(invalid("guest GPU recipe includes a non-render device"));
        }
        gpu::check_render_node(node.major, node.minor)?;
    }
    Ok(())
}

fn invalid(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "dedicated guest GPU attachment",
        why: why.into(),
    }
}
