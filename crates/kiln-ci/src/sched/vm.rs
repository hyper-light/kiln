//! U01/U03: one Firecracker guest shape carved from an existing outer executor grant.
//! This pure policy does not claim kernel enforcement or shared-cache peak admission (U07).

use serde::{Deserialize, Serialize};
use super::state::Resources;

const MIB: u64 = 1 << 20;
const BLOCK: u64 = 4096;

/// Actual CPU/memory configuration used by a guest and its sequential image builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestShape {
    pub vcpus: u32,
    pub memory_mib: u64,
}

impl GuestShape {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=32).contains(&self.vcpus) || self.memory_mib == 0 || self.memory_mib.checked_mul(MIB).is_none() {
            return Err("Firecracker requires a 1..32 vCPUs and a positive representable MiB guest shape".into());
        }
        Ok(())
    }
}

/// Persisted relationship between the immutable host grant, explicit overhead and usable guest.
/// Private derived fields are still validated after decoding before any startup or replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirecrackerShape {
    outer: Resources,
    overhead: Resources,
    guest: GuestShape,
    scratch_bytes: u64,
}

impl FirecrackerShape {
    /// Never round a guest above its host grant. CPU/MiB/block tails remain outer headroom.
    pub fn derive(outer: Resources, overhead: Resources) -> Result<Self, String> {
        if !outer.cpu.is_finite() || outer.cpu <= 0.0 || !overhead.cpu.is_finite() || overhead.cpu < 0.0
            || overhead.mem == 0 || overhead.disk == 0 || outer.gpu != 0 || overhead.gpu != 0 {
            return Err("Firecracker needs a finite positive outer CPU grant, explicit positive memory/disk overhead, nonnegative CPU overhead, and no GPU request".into());
        }
        let cpus = (outer.cpu - overhead.cpu).floor();
        let memory_mib = outer.mem.checked_sub(overhead.mem).ok_or("Firecracker memory overhead exceeds the existing grant")? / MIB;
        let disk = outer.disk.checked_sub(overhead.disk).ok_or("Firecracker disk overhead exceeds the existing grant")?;
        let scratch_bytes = disk / BLOCK * BLOCK;
        if !(1.0..=32.0).contains(&cpus) || memory_mib == 0 || scratch_bytes == 0 {
            return Err("Firecracker overhead leaves no usable whole-vCPU/MiB/block guest; increase this pool's declared resources or correct its explicit overhead".into());
        }
        let guest = GuestShape { vcpus: cpus as u32, memory_mib };
        guest.validate()?;
        Ok(Self { outer, overhead: Resources { cpu: if overhead.cpu == 0.0 { 0.0 } else { overhead.cpu }, ..overhead }, guest, scratch_bytes })
    }

    pub fn validate(&self) -> Result<(), String> {
        let expected = Self::derive(self.outer, self.overhead)?;
        if &expected != self { return Err("Firecracker shape differs from its immutable outer grant and overhead".into()); }
        Ok(())
    }

    pub fn validate_grant(&self, outer: &Resources) -> Result<(), String> {
        self.validate()?;
        if &self.outer != outer { return Err("Firecracker startup changed its existing outer grant".into()); }
        Ok(())
    }

    pub fn guest(&self) -> GuestShape { self.guest }
    pub fn scratch_bytes(&self) -> u64 { self.scratch_bytes }
    pub fn outer(&self) -> Resources { self.outer }
    pub fn overhead(&self) -> Resources { self.overhead }
    pub fn guest_resources(&self) -> Resources {
        Resources { cpu: f64::from(self.guest.vcpus), mem: self.guest.memory_mib.saturating_mul(MIB), disk: self.scratch_bytes, gpu: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u01_firecracker_shape_keeps_overhead_inside_its_original_grant() -> Result<(), String> {
        let outer = Resources { cpu: 4.5, mem: (3 << 30) + 513, disk: (9 << 30) + 37, gpu: 0 };
        let overhead = Resources { cpu: 0.25, mem: 256 << 20, disk: 64 << 20, gpu: 0 };
        let shape = FirecrackerShape::derive(outer, overhead)?;
        assert_eq!(shape.guest(), GuestShape { vcpus: 4, memory_mib: 2816 });
        assert_eq!(shape.scratch_bytes(), (9 << 30) - (64 << 20));
        let guest = shape.guest_resources();
        assert!(guest.cpu + overhead.cpu <= outer.cpu && guest.mem + overhead.mem <= outer.mem && guest.disk + overhead.disk <= outer.disk);
        shape.validate_grant(&outer)?;
        assert!(shape.validate_grant(&Resources { cpu: 8.0, ..outer }).is_err(), "config changes cannot grow the old grant");
        for (grant, overhead) in [(Resources::default(), overhead), (outer, Resources { mem: 0, ..overhead }), (outer, Resources { disk: 0, ..overhead }), (outer, Resources { cpu: f64::NAN, ..overhead }), (outer, Resources { cpu: outer.cpu, ..overhead }), (outer, Resources { mem: outer.mem, ..overhead })] {
            assert!(FirecrackerShape::derive(grant, overhead).is_err());
        }
        let mut tampered = shape.clone();
        tampered.guest.vcpus += 1;
        assert!(tampered.validate().is_err(), "a decoded guest cannot exceed its stored grant relationship");
        Ok(())
    }
}
