//! AC-T7.3.1: bounded, externally event-driven native load samples. ROOT only.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use hex_gpu::Gpus;
use std::io::{Read, Write};

/// Read exactly the caller's sample count. Each sample needs one input byte;
/// no timer, guessed warmup interval, periodic probe or skipped hardware pass.
#[test]
#[ignore = "ROOT-only native inventory/load; externally bounded and signalled via stdin"]
fn completion_native_load_samples() {
    assert_eq!(
        std::env::var("HEX_COMPLETION_ROOT_AUTHORIZED").as_deref(),
        Ok("1")
    );
    let samples: u32 = std::env::var("HEX_COMPLETION_LOAD_SAMPLES")
        .expect("explicit finite count")
        .parse()
        .unwrap();
    assert!(samples > 0);
    let mut gpus = Gpus::open();
    let inventory = gpus
        .inventory()
        .expect("UNVERIFIED actual GPU inventory")
        .to_vec();
    assert!(!inventory.is_empty());
    let mut keys: Vec<_> = inventory.iter().map(|gpu| gpu.key()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), inventory.len(), "ambiguous native key");
    for round in 0..samples {
        let mut signal = [0_u8; 1];
        std::io::stdin()
            .read_exact(&mut signal)
            .expect("ROOT sample event; outer guard bounds wait");
        let loads = gpus.sample().expect("UNVERIFIED native load sample");
        assert_eq!(loads.len(), inventory.len());
        let mut ranked = Vec::new();
        let mut rows = Vec::new();
        for (gpu, load) in inventory.iter().zip(loads) {
            let busy = load
                .busy_percent
                .expect("UNVERIFIED unread GPU busy counter");
            let memory = load
                .memory_used_bytes
                .expect("UNVERIFIED unread GPU memory counter");
            ranked.push((busy, memory, gpu.key()));
            rows.push(format!(
                "{{\"key\":{},\"busy_percent\":{busy},\"memory_used_bytes\":{memory}}}",
                gpu.key()
            ));
        }
        ranked.sort_unstable();
        let order: Vec<_> = ranked.iter().map(|row| row.2.to_string()).collect();
        println!(
            "{{\"round\":{round},\"order\":[{}],\"loads\":[{}]}}",
            order.join(","),
            rows.join(",")
        );
        std::io::stdout().flush().unwrap();
    }
}
