//! W13-R01: coordinator-only native Apple counter check, separate from pure/provider tests.
#![cfg(target_os = "macos")]

use kiln_gpu::{Gpus, Identity, Vendor};

#[test]
#[ignore = "requires a physical Apple Silicon GPU with native IOKit/IOReport counters"]
fn w13_r01_native_apple_gpu_has_observed_counters_and_full_identity() -> Result<(), String> {
    let mut provider = Gpus::open();
    let inventory = provider.inventory().map_err(ToString::to_string)?.to_vec();
    if inventory.is_empty() { return Err("native Apple inventory is empty".into()); }
    for card in &inventory {
        if card.vendor != Vendor::Apple || !matches!(card.identity, Identity::Metal(_)) {
            return Err(format!("expected a native Metal identity, got {:?}", card.identity));
        }
        if card.memory_total_bytes == 0 { return Err("Metal working-set budget is zero".into()); }
        println!("native Apple GPU: {} id={} budget={}", card.model, card.identity, card.memory_total_bytes);
    }
    // Two observations distinguish the first delta window from a retained known reading.
    // There is no imposed pause, interval, polling loop or guessed replacement for NoSample.
    let mut observed = vec![false; inventory.len()];
    for observation in 0..2 {
        let loads = provider.sample().map_err(ToString::to_string)?;
        if loads.len() != inventory.len() { return Err("native inventory/sample lengths differ".into()); }
        for ((card, load), known) in inventory.iter().zip(loads).zip(&mut observed) {
            println!("observation {observation}: id={} busy={:?} mapped={:?}", card.identity, load.busy_percent, load.memory_used_bytes);
            // Mapped allocation may exceed Metal's performance budget. Preserve the actual value.
            *known |= load.busy_percent.is_ok_and(|busy| busy <= 100) && load.memory_used_bytes.is_ok();
        }
    }
    if observed.iter().any(|known| !known) { return Err("a native Apple GPU supplied no paired compute/memory observation".into()); }
    // Drop exercises owned native release, but this one check is not a leak/cleanup proof.
    drop(provider);
    Ok(())
}
