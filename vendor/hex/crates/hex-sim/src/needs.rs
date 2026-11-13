//! What a box needs of a node, in kiln's grammar (kiln `crates/kiln-ci/src/sched/needs.rs` at
//! `a63dd98ef`, adapted to boxes): what the node reserves for it until it is deleted (CPU, memory, disk,
//! GPUs), the platform its image is built for, the class that runs it, and the devices it is given.
//!
//! Its spelling is words separated by commas or spaces,
//! `cpus=8,mem=32g,disk=50g,gpus=1,linux/amd64,class:oci,gpu.memory>=16g`:
//!
//! - `cpus=8` (or `cpu=`; `0.5` and `500m` too), `mem=32g` (or `memory=`), `disk=50g`, `gpus=1` (or
//!   `gpu=`): reserved on the node. Sizes are binary whatever their case (`32g`, `32G`, `32GB`,
//!   `32Gi`); a plain number is bytes.
//! - `linux/amd64`, `linux`, `arm64`, …: the image's OS and architecture, by any of their names.
//! - `class:oci`, `class:firecracker`, `class:shards`: what runs it.
//! - `gpu.model==nvidia-geforce-rtx-5060-ti`: each GPU of that model (its name lowercase, its words
//!   joined by `-`); `gpu.memory>=16g`: each GPU with that much memory free.
//! - `vfio=1`, `block=1`, `net=1`: that many PCI functions bound to `vfio-pci`, block devices and
//!   network devices, each given whole; `block.size>=100g`: each block device that large.
//! - `kvm`: `/dev/kvm` in the box.
//!
//! Anything else is an error that names the word: `memm=4g: no such need`. Kiln's words for CI
//! (`+label`, `tool:`, `executor:`, `flag:`, `binfmt:`, comparisons on its routing facts, `exclusive`
//! and `together:`) name what a CI runner has or how a leg is held; a node's boxes have none of them.

use crate::SimError;
use crate::model::{Arch, Ask, Class, Cluster, Os, Resources};

/// What a box needs of a node, as spelled: its GPU model by name until it is resolved against a
/// cluster's ([`Needs::ask`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needs {
    /// What the node reserves.
    pub need: Resources,
    /// What runs it, where the spelling says.
    pub class: Option<Class>,
    /// Its platform and devices, but its GPU model.
    pub ask: Ask,
    /// Its GPU model's name ([`crate::model::model_name`]).
    pub gpu_model: Option<String>,
}

/// Reads the spelling (the module's docs). An error names the first word that isn't a need.
pub fn parse(spec: &str) -> Result<Needs, String> {
    let mut needs = Needs::default();
    for word in spec
        .split([',', ' '])
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        needs.add(word).map_err(|why| format!("{word}: {why}"))?;
    }
    Ok(needs)
}

const NOT_CPUS: &str = "not a number of CPUs (8, 0.5, 500m)";
const FINER: &str = "finer than 1m (0.001 CPU)";

impl Needs {
    /// Its ask, its GPU model's name interned in `cluster`.
    pub fn ask(&self, cluster: &mut Cluster) -> Result<Ask, SimError> {
        let gpu_model = match &self.gpu_model {
            Some(name) => Some(cluster.model(name)?),
            None => None,
        };
        Ok(Ask {
            gpu_model,
            ..self.ask
        })
    }

    /// Adds one word of the spelling. An error says why it isn't one, without the word.
    pub fn add(&mut self, w: &str) -> Result<(), String> {
        if w.eq_ignore_ascii_case("kvm") {
            self.ask.kvm = true;
            return Ok(());
        }
        // `gpu.memory>=16g` is a constraint and `gpus=1` a request: comparisons first.
        if let Some((fact, op, value)) = comparison(w) {
            return self.compare(fact, op, value);
        }
        if let Some((key, value)) = w.split_once('=') {
            return self.reserve(key, value);
        }
        if let Some((kind, item)) = w.split_once(':') {
            if !kind.eq_ignore_ascii_case("class") {
                return Err("no such need (class:)".into());
            }
            self.class = Some(match item.to_ascii_lowercase().as_str() {
                "oci" => Class::Oci,
                "firecracker" => Class::Firecracker,
                "shards" => Class::Shards,
                _ => return Err("no such class (oci, firecracker, shards)".into()),
            });
            return Ok(());
        }
        if let Some((os, arch)) = w.split_once('/') {
            self.ask.os = Some(Os::named(os).ok_or("no such OS (linux, macos, windows, freebsd)")?);
            self.ask.arch = Some(
                Arch::named(arch)
                    .ok_or("no such architecture (amd64, arm64, x86, arm, riscv64)")?,
            );
            return Ok(());
        }
        if let Some(os) = Os::named(w) {
            self.ask.os = Some(os);
            return Ok(());
        }
        if let Some(arch) = Arch::named(w) {
            self.ask.arch = Some(arch);
            return Ok(());
        }
        Err("no such need".into())
    }

    /// `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`, `vfio=1`, `block=1`, `net=1`.
    fn reserve(&mut self, key: &str, value: &str) -> Result<(), String> {
        let count = |v: &str, what: &'static str| v.trim().parse::<u16>().map_err(|_| what);
        match key.to_ascii_lowercase().as_str() {
            "cpus" | "cpu" => self.need.cpu_milli = cpus(value)?,
            "mem" | "memory" => self.need.mem_bytes = size(value)?,
            "disk" => self.need.disk_bytes = size(value)?,
            "gpus" | "gpu" => {
                self.need.gpus = value.trim().parse().map_err(|_| "not a number of GPUs")?;
            }
            "vfio" => self.ask.vfio = count(value, "not a number of VFIO functions")?,
            "block" => self.ask.block = count(value, "not a number of block devices")?,
            "net" => self.ask.net = count(value, "not a number of network devices")?,
            _ => return Err("no such need".into()),
        }
        Ok(())
    }

    /// `gpu.model==<name>`, `gpu.memory>=<size>`, `block.size>=<size>`.
    fn compare(&mut self, fact: &str, op: &str, value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            return Err("a value is missing".into());
        }
        match (fact.to_ascii_lowercase().as_str(), op) {
            ("gpu.model", "==") => self.gpu_model = Some(crate::model::model_name(value)),
            ("gpu.memory", ">=") => self.ask.gpu_memory = size(value)?,
            ("block.size", ">=") => self.ask.block_size = size(value)?,
            ("gpu.model" | "gpu.memory" | "block.size", _) => {
                return Err("compared only as gpu.model==, gpu.memory>= or block.size>=".into());
            }
            _ => return Err("no such fact (gpu.model, gpu.memory, block.size)".into()),
        }
        Ok(())
    }
}

/// `fact>=n` and the like: the fact, its operator and the value.
fn comparison(w: &str) -> Option<(&str, &'static str, &str)> {
    ["==", "!=", ">=", "<=", ">", "<"]
        .into_iter()
        .find_map(|op| w.split_once(op).map(|(fact, value)| (fact, op, value)))
}

/// CPUs read exactly, digit by digit, into thousandths, as Kubernetes counts them (kiln
/// `sched/pools.rs` `cpus`): `8`, `0.5`, `500m`; a spelling finer than a thousandth is refused.
pub fn cpus(s: &str) -> Result<u64, &'static str> {
    let s = s.trim();
    let (number, per_unit) = match s.strip_suffix('m') {
        Some(milli) => (milli, 1u64),
        None => (s, 1000u64),
    };
    let number = number.strip_prefix('+').unwrap_or(number);
    let (whole, part) = number.split_once('.').unwrap_or((number, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && part.is_empty()) || !digits(whole) || !digits(part) {
        return Err(NOT_CPUS);
    }
    // The digits a thousandth can say (three of a CPU, none of a milli-CPU); the rest must be 0.
    let kept = if per_unit == 1000 {
        part.len().min(3)
    } else {
        0
    };
    let (said, rest) = (
        part.get(..kept).unwrap_or(""),
        part.get(kept..).unwrap_or(""),
    );
    if rest.bytes().any(|b| b != b'0') {
        return Err(FINER);
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| NOT_CPUS)?
    };
    let said: u64 = if said.is_empty() {
        0
    } else {
        format!("{said:0<3}").parse().map_err(|_| NOT_CPUS)?
    };
    whole
        .checked_mul(per_unit)
        .and_then(|w| w.checked_add(said))
        .ok_or(NOT_CPUS)
}

/// Bytes as the spelling writes them: `32g`, `32G`, `32GB`, `32Gi` and `512m` are binary, and a
/// plain number is bytes. Read exactly, as [`cpus`] is (kiln's reads through a float): `1.5g` is
/// 1,610,612,736 bytes, and a spelling that isn't a whole number of bytes is refused.
pub fn size(s: &str) -> Result<u64, &'static str> {
    const NOT: &str = "not a size (32g, 512m)";
    let s = s.trim().to_ascii_lowercase();
    let cut = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = (s.get(..cut).ok_or(NOT)?, s.get(cut..).ok_or(NOT)?);
    let shift: u32 = match unit {
        "" | "b" => 0,
        "k" | "kb" | "ki" | "kib" => 10,
        "m" | "mb" | "mi" | "mib" => 20,
        "g" | "gb" | "gi" | "gib" => 30,
        "t" | "tb" | "ti" | "tib" => 40,
        _ => return Err(NOT),
    };
    let (whole, part) = num.split_once('.').unwrap_or((num, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && part.is_empty()) || !digits(whole) || !digits(part) {
        return Err(NOT);
    }
    let unit = 1u128.checked_shl(shift).ok_or(NOT)?;
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| NOT)?
    };
    // part / 10^len, times the unit, must be whole bytes.
    let part = part.trim_end_matches('0');
    let scale = 10u128
        .checked_pow(u32::try_from(part.len()).map_err(|_| NOT)?)
        .ok_or(NOT)?;
    let frac: u128 = if part.is_empty() {
        0
    } else {
        part.parse().map_err(|_| NOT)?
    };
    let frac_bytes = frac.checked_mul(unit).ok_or(NOT)?;
    if frac_bytes.checked_rem(scale) != Some(0) {
        return Err("finer than a byte");
    }
    let bytes = whole
        .checked_mul(unit)
        .and_then(|w| w.checked_add(frac_bytes.checked_div(scale)?))
        .ok_or(NOT)?;
    u64::try_from(bytes).map_err(|_| NOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn res(cpu_milli: u64, mem_bytes: u64, disk_bytes: u64, gpus: u32) -> Resources {
        Resources {
            cpu_milli,
            mem_bytes,
            gpus,
            disk_bytes,
        }
    }

    /// CP-T7.4: do read kiln's spellings and Hex's words; expect each to say what it reserves, its
    /// platform, its class and its devices.
    #[test]
    fn the_spelling_reads_reservations_platform_class_and_devices() {
        let n =
            parse("cpus=8,mem=32g,disk=50g,gpus=1,linux/amd64,class:oci,gpu.memory>=16g").unwrap();
        assert_eq!(n.need, res(8000, 32 * GIB, 50 * GIB, 1));
        assert_eq!(
            (n.ask.os, n.ask.arch),
            (Some(Os::Linux), Some(Arch::X86_64))
        );
        assert_eq!((n.class, n.ask.gpu_memory), (Some(Class::Oci), 16 * GIB));
        let n = parse("cpu=500m memory=512Mi darwin/apple-silicon class:shards").unwrap();
        assert_eq!(n.need, res(500, 512 << 20, 0, 0));
        assert_eq!(
            (n.ask.os, n.ask.arch),
            (Some(Os::Macos), Some(Arch::Aarch64))
        );
        let n = parse("vfio=2,block=1,block.size>=100g,net=1,kvm,windows,arm64").unwrap();
        assert_eq!(
            (n.ask.vfio, n.ask.block, n.ask.net, n.ask.kvm),
            (2, 1, 1, true)
        );
        assert_eq!(n.ask.block_size, 100 * GIB);
        assert_eq!(
            (n.ask.os, n.ask.arch),
            (Some(Os::Windows), Some(Arch::Aarch64))
        );
        let n = parse("gpus=2,gpu.model==NVIDIA_GeForce_RTX_5060_Ti").unwrap();
        assert_eq!(n.gpu_model.as_deref(), Some("nvidia-geforce-rtx-5060-ti"));
        assert_eq!(parse("").unwrap(), Needs::default());
    }

    /// CLAUDE.md §6, hostile input to every parser: do give a bad word; expect an error naming it and
    /// why, never a guess.
    #[test]
    fn a_bad_spelling_names_the_bad_part() {
        let cases = [
            ("memm=4g", "memm=4g: no such need"),
            ("cpus=8,mem=lots", "mem=lots: not a size (32g, 512m)"),
            (
                "cpus=many",
                "cpus=many: not a number of CPUs (8, 0.5, 500m)",
            ),
            ("gpus=-1", "gpus=-1: not a number of GPUs"),
            (
                "linx/amd64",
                "linx/amd64: no such OS (linux, macos, windows, freebsd)",
            ),
            (
                "linux/sparc",
                "linux/sparc: no such architecture (amd64, arm64, x86, arm, riscv64)",
            ),
            ("fedora", "fedora: no such need"),
            ("tool:cargo", "tool:cargo: no such need (class:)"),
            (
                "class:kata",
                "class:kata: no such class (oci, firecracker, shards)",
            ),
            (
                "cpu.cores>=8",
                "cpu.cores>=8: no such fact (gpu.model, gpu.memory, block.size)",
            ),
            (
                "gpu.memory<=8g",
                "gpu.memory<=8g: compared only as gpu.model==, gpu.memory>= or block.size>=",
            ),
            ("gpu.memory>=", "gpu.memory>=: a value is missing"),
            ("vfio=lots", "vfio=lots: not a number of VFIO functions"),
            ("exclusive", "exclusive: no such need"),
            ("mem=0.1", "mem=0.1: finer than a byte"),
        ];
        for (spelling, want) in cases {
            assert_eq!(parse(spelling), Err(want.to_owned()), "{spelling}");
        }
    }

    /// kiln's E1 (W05-R03): CPUs are read exactly into thousandths, and finer is refused.
    #[test]
    fn cpus_are_read_exactly_in_thousandths() {
        for (spelling, milli) in [
            ("8", 8000),
            ("0.5", 500),
            ("500m", 500),
            ("0.25", 250),
            (".5", 500),
            ("+2", 2000),
            ("2.000", 2000),
            ("0.0010", 1),
            ("1m", 1),
            ("0", 0),
        ] {
            assert_eq!(cpus(spelling), Ok(milli), "{spelling}");
        }
        for (spelling, why) in [
            ("0.0005", FINER),
            ("1.5m", FINER),
            ("-1", NOT_CPUS),
            ("1e3", NOT_CPUS),
            (".", NOT_CPUS),
            ("99999999999999999999", NOT_CPUS),
        ] {
            assert_eq!(cpus(spelling), Err(why), "{spelling}");
        }
    }

    #[test]
    fn sizes_are_binary_in_any_case_and_exact() {
        for (s, want) in [
            ("32g", Ok(32 * GIB)),
            ("32G", Ok(32 * GIB)),
            ("32GB", Ok(32 * GIB)),
            ("32Gi", Ok(32 * GIB)),
            ("512m", Ok(512 << 20)),
            ("1.5t", Ok(3 << 39)),
            ("1.5g", Ok(1_610_612_736)),
            ("1024", Ok(1024)),
            ("4h", Err("not a size (32g, 512m)")),
            ("lots", Err("not a size (32g, 512m)")),
            ("-1g", Err("not a size (32g, 512m)")),
            ("99999999999t", Err("not a size (32g, 512m)")),
        ] {
            assert_eq!(size(s), want, "{s}");
        }
    }

    /// A GPU model spelled in a need resolves to the cluster's id for it, whatever its case and
    /// separators.
    #[test]
    fn a_gpu_model_resolves_against_its_cluster() {
        let mut c = Cluster::single_owner(Vec::new(), Vec::new());
        let id = c.model("NVIDIA GeForce RTX 5060 Ti").unwrap();
        let ask = parse("gpus=1,gpu.model==nvidia-geforce-rtx-5060-ti")
            .unwrap()
            .ask(&mut c)
            .unwrap();
        assert_eq!(ask.gpu_model, Some(id));
        assert_eq!(c.models.len(), 1);
    }
}
