//! A light check on real GPUs (`AC-T7.3.1`): where the machine has NVIDIA or AMD GPUs, hex-gpu lists
//! each as the vendor's own sources name it (nvidia-smi where it is installed; amdgpu's sysfs read
//! apart on Linux; Windows' own video controllers on Windows) and samples each without allocating.
//! Where none is present it says why and stops: on macOS that must be `Unsupported` for both vendors.
//!
//! This binary holds one test, so nothing else allocates while it counts a sample's allocations.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::alloc::System;

use cap::Cap;
use kiln_gpu::{Gpus, Vendor};

#[global_allocator]
static ALLOC: Cap<System> = Cap::new(System, usize::MAX);

/// AC-T7.3.1: do open the machine's GPUs, list them and sample them twice; expect each listed with a
/// model, its memory, its PCI address and its driver's version, NVIDIA's first and each vendor's in
/// PCI order, matching the vendor's own sources; each sample a load per GPU, in range, for no
/// allocation at all. Where the machine has none: expect the reason, and on macOS the platform's.
#[test]
fn the_machines_gpus_are_listed_and_sampled_without_allocating() {
    let mut gpus = Gpus::open();
    let inventory = match gpus.inventory() {
        Ok(found) => found.to_vec(),
        Err(none) => {
            #[cfg(target_os = "macos")]
            assert_eq!(
                (&none.nvidia, &none.amd),
                (&kiln_gpu::Absent::Unsupported, &kiln_gpu::Absent::Unsupported)
            );
            eprintln!("skipped: {none}; devices skipped: {:?}", gpus.skipped());
            return;
        }
    };
    eprintln!("skipped devices: {:?}", gpus.skipped());
    for gpu in &inventory {
        eprintln!("found {gpu:?}");
        assert!(!gpu.model.is_empty() && !gpu.driver_version.is_empty());
        assert!(gpu.memory_total_bytes > 0);
    }
    let order: Vec<_> = inventory.iter().map(|g| (g.vendor, g.pci)).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "NVIDIA's first, each vendor's in PCI order");
    matches_the_vendors_sources(&inventory);

    for round in 0..2 {
        let before = ALLOC.total_allocated();
        let loads = gpus.sample().unwrap();
        let allocated = ALLOC.total_allocated() - before;
        assert_eq!(loads.len(), inventory.len());
        for (gpu, load) in inventory.iter().zip(loads) {
            eprintln!("sample {round}: {} {}: {load:?}", gpu.pci, gpu.model);
            if let Ok(busy) = load.busy_percent {
                assert!(busy <= 100);
            }
            let used = load.memory_used_bytes.unwrap();
            assert!(used <= gpu.memory_total_bytes);
        }
        assert_eq!(allocated, 0, "sample {round} allocated {allocated} bytes");
    }
}

/// Each GPU as its vendor's own sources name it: NVIDIA's as nvidia-smi does, where it is installed;
/// AMD's VRAM as amdgpu's sysfs says, read apart from hex-gpu.
fn matches_the_vendors_sources(inventory: &[kiln_gpu::Gpu]) {
    let nvidia: Vec<_> = inventory
        .iter()
        .filter(|g| g.vendor == Vendor::Nvidia)
        .collect();
    if !nvidia.is_empty() {
        let query = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=pci.bus_id,name,memory.total,driver_version",
                "--format=csv,noheader,nounits",
            ])
            .output();
        match query {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8(out.stdout).unwrap();
                let rows: Vec<Vec<String>> = text
                    .lines()
                    .map(|l| l.split(", ").map(str::to_owned).collect())
                    .collect();
                assert_eq!(rows.len(), nvidia.len(), "{text}");
                for gpu in &nvidia {
                    let row = rows
                        .iter()
                        .find(|r| kiln_gpu::PciAddress::parse(&r[0]) == Some(gpu.pci))
                        .unwrap_or_else(|| panic!("nvidia-smi has no {}: {text}", gpu.pci));
                    assert_eq!(row[1], gpu.model);
                    // nvidia-smi prints MiB, its total floored.
                    assert_eq!(row[2].parse::<u64>().unwrap(), gpu.memory_total_bytes >> 20);
                    assert_eq!(row[3], gpu.driver_version);
                }
            }
            other => eprintln!("nvidia-smi not compared: {other:?}"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let names = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }",
            ])
            .output()
            .unwrap();
        let names = String::from_utf8(names.stdout).unwrap();
        for gpu in inventory.iter().filter(|g| g.vendor == Vendor::Amd) {
            assert!(
                names.lines().any(|name| name.trim() == gpu.model),
                "Windows names no {}: {names}",
                gpu.model
            );
        }
    }
    #[cfg(target_os = "linux")]
    for gpu in inventory.iter().filter(|g| g.vendor == Vendor::Amd) {
        let path = format!("/sys/bus/pci/devices/{}/mem_info_vram_total", gpu.pci);
        let total: u64 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(total, gpu.memory_total_bytes, "{path}");
    }
}
