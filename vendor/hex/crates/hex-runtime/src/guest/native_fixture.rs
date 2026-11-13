//! ONE fixed typed builder for the measurement parent and SAME Hex private
//! entry. ROOT registers this module/feature. It starts no runtime/provider,
//! process or VM and introduces no production manifest or native transport.
#![cfg(any(test, feature = "native-trusted-entry-measurement"))]

use std::path::Path;

use hex_api::{Allocation, Refusal};
use shards_api::{NativeBoot, PreparedBoot, RuntimePaths};

use super::{GuestConfig, load_runtime_inputs, route};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Input<'a> {
    pub config: &'a GuestConfig,
    pub cost_file: &'a Path,
    pub cost_sha256: [u8; 32],
    pub evidence: &'a Path,
    pub paths: &'a RuntimePaths,
    pub socket: &'a Path,
    pub console_kept: usize,
}

pub(crate) fn prepare(input: Input<'_>) -> Result<(PreparedBoot, Allocation), Refusal> {
    let (cost, images) = load_runtime_inputs(input.cost_file, input.cost_sha256, input.evidence)?;
    let allocation = input.config.checked_runtime_allocation(cost)?;
    let images = images.ok_or_else(|| {
        route::invalid("trusted-entry fixture requires native image declarations")
    })?;
    if images.evidence() != allocation.cost.id {
        return Err(route::invalid(
            "trusted-entry images differ from the original cost evidence",
        ));
    }
    let spec = input.config.try_spec()?;
    let plan = shards_api::prepare_native_boot(
        &spec,
        &input.config.binary,
        input.socket,
        NativeBoot {
            paths: input.paths,
            images,
            console_kept: input.console_kept,
        },
    )
    .map_err(|why| super::refusal("preparing the trusted-entry fixture", &why))?;
    // Require the real borrowed accessor, never argv inspection or a pointer
    // retained from before reexec. Each image builds this view independently.
    plan.native_startup()
        .map_err(|why| super::refusal("checking the trusted-entry fixture", &why))?;
    Ok((plan, allocation))
}

#[cfg(test)]
mod tests;
