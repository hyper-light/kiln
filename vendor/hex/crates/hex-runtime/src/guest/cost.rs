//! Explicit administrator cost inputs. Hashes bind the fixed profile to reviewed
//! measurement evidence; they do not turn an unmeasured allowance into evidence.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use hex_api::{Refusal, RuntimeCost};
use sha2::{Digest, Sha256};

use super::GuestConfig;

mod cpu;
mod images;

pub use cpu::{CpuResponse, section as cpu_response_section, split as cpu_response_split};

/// The same initialization, with optional native declarations from its existing
/// SHA-verified evidence. Missing declarations grant no native worker authority.
#[derive(Debug)]
pub struct GuestInputs {
    pub config: GuestConfig,
    pub cost: RuntimeCost,
    pub native_images: Option<shards_api::NativeImages>,
    /// Measured managed CPU responses, by host class, from the same evidence (none: no row).
    pub cpu_responses: Vec<CpuResponse>,
}

pub const ENV_COST_FILE: &str = "HEX_GUEST_COST_FILE";
pub const ENV_COST_SHA256: &str = "HEX_GUEST_COST_SHA256";
pub const ENV_COST_EVIDENCE: &str = "HEX_GUEST_COST_EVIDENCE";

fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return exhausted();
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "Shards runtime cost input",
        why: text,
    }
}

fn exhausted() -> Refusal {
    Refusal::Exhausted {
        what: "Shards runtime cost configuration bytes",
        // Same existing bounded node-configuration protocol, not a tuning limit.
        bound: hex_core::bundle::MAX_CONFIG_BYTES,
    }
}

fn input_path(value: OsString) -> Result<PathBuf, Refusal> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid(
            "cost and evidence paths must be explicit absolute paths",
        ));
    }
    Ok(path)
}

fn hash(text: &str) -> Result<[u8; 32], Refusal> {
    let mut result = [0; 32];
    let digits = result.len().checked_mul(2).ok_or_else(exhausted)?;
    if text.len() != digits || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "expected exactly64 ASCII hexadecimal SHA-256 digits",
        ));
    }
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (byte, pair) in result.iter_mut().zip(pairs) {
        *byte = u8::from_str_radix(
            std::str::from_utf8(pair).map_err(|_| invalid("hash is not ASCII"))?,
            16,
        )
        .map_err(|_| invalid("hash byte is invalid"))?;
    }
    Ok(result)
}

fn read_config(path: &Path) -> Result<Vec<u8>, Refusal> {
    if !std::fs::metadata(path)
        .map_err(|why| Refusal::os("locating Shards runtime cost input", why))?
        .is_file()
    {
        return Err(invalid("cost inputs must be regular configuration files"));
    }
    let mut file = std::fs::File::open(path)
        .map_err(|why| Refusal::os("opening Shards runtime cost input", why))?;
    let metadata = file
        .metadata()
        .map_err(|why| Refusal::os("sizing Shards runtime cost input", why))?;
    if !metadata.is_file() {
        return Err(invalid(
            "cost inputs must remain regular configuration files",
        ));
    }
    let size = metadata.len();
    let size = usize::try_from(size)
        .ok()
        .filter(|size| *size <= hex_core::bundle::MAX_CONFIG_BYTES)
        .ok_or_else(exhausted)?;
    // One extra byte detects growth without read_to_end's implicit allocation.
    let room = size.checked_add(1).ok_or_else(exhausted)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(room).map_err(|_| exhausted())?;
    bytes.resize(room, 0);
    let mut count = 0usize;
    while count < room {
        let tail = bytes.get_mut(count..).ok_or_else(exhausted)?;
        let read = file
            .read(tail)
            .map_err(|why| Refusal::os("reading Shards runtime cost input", why))?;
        if read == 0 {
            break;
        }
        count = count.checked_add(read).ok_or_else(exhausted)?;
    }
    if count != size {
        return Err(invalid("cost input changed length while being read"));
    }
    bytes.truncate(count);
    Ok(bytes)
}

/// `file` is strict RuntimeCost JSON, pinned by the administrator's expected file
/// digest. Its nonzero `id` must hash the exact bounded evidence-manifest bytes.
/// ROOT reviews the actual measurements/config/source named by that manifest.
pub fn load_runtime_cost(
    file: &Path,
    expected_sha256: [u8; 32],
    evidence: &Path,
) -> Result<RuntimeCost, Refusal> {
    load_runtime_inputs(file, expected_sha256, evidence).map(|(cost, _)| cost)
}

pub fn load_runtime_inputs(
    file: &Path,
    expected_sha256: [u8; 32],
    evidence: &Path,
) -> Result<(RuntimeCost, Option<shards_api::NativeImages>), Refusal> {
    load_runtime_evidence(file, expected_sha256, evidence).map(|(cost, images, _)| (cost, images))
}

/// The same inputs with the evidence's measured CPU-response rows (AC-T7.4.1).
pub fn load_runtime_evidence(
    file: &Path,
    expected_sha256: [u8; 32],
    evidence: &Path,
) -> Result<
    (
        RuntimeCost,
        Option<shards_api::NativeImages>,
        Vec<CpuResponse>,
    ),
    Refusal,
> {
    if !file.is_absolute() || !evidence.is_absolute() {
        return Err(invalid(
            "cost and evidence paths must be explicit absolute paths",
        ));
    }
    let bytes = read_config(file)?;
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != expected_sha256 {
        return Err(invalid("cost file does not match its configured SHA-256"));
    }
    let cost: RuntimeCost = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("cost file is not an exact RuntimeCost object"))?;
    cost.check()?;
    let evidence = read_config(evidence)?;
    let actual: [u8; 32] = Sha256::digest(&evidence).into();
    if actual != cost.id {
        return Err(invalid(
            "cost identity does not match its measurement evidence",
        ));
    }
    let (responses, rest) = cpu::split(&evidence)?;
    let native = images::parse(rest, cost.id)?;
    Ok((cost, native, responses))
}

impl GuestConfig {
    /// Pure configuration/file validation; performs no boot or native discovery.
    /// The caller then supplies this exact cost to GuestRuntime::with_runtime_cost
    /// and the initialized native owner before scheduler admission/registration.
    pub fn from_env_with_cost() -> Result<(Self, RuntimeCost), Refusal> {
        Self::from_vars_with_cost(|name| std::env::var_os(name))
    }

    pub fn from_vars_with_cost(
        var: impl Fn(&str) -> Option<OsString>,
    ) -> Result<(Self, RuntimeCost), Refusal> {
        let input = Self::from_vars_with_native_inputs(var)?;
        Ok((input.config, input.cost))
    }

    pub fn from_env_with_native_inputs() -> Result<GuestInputs, Refusal> {
        Self::from_vars_with_native_inputs(|name| std::env::var_os(name))
    }

    pub fn from_vars_with_native_inputs(
        var: impl Fn(&str) -> Option<OsString>,
    ) -> Result<GuestInputs, Refusal> {
        let config = Self::from_vars(&var)?;
        let file =
            input_path(var(ENV_COST_FILE).ok_or_else(|| invalid("HEX_GUEST_COST_FILE is unset"))?)?;
        let evidence = input_path(
            var(ENV_COST_EVIDENCE).ok_or_else(|| invalid("HEX_GUEST_COST_EVIDENCE is unset"))?,
        )?;
        let expected =
            var(ENV_COST_SHA256).ok_or_else(|| invalid("HEX_GUEST_COST_SHA256 is unset"))?;
        let expected = hash(
            expected
                .to_str()
                .ok_or_else(|| invalid("cost hash is not ASCII"))?,
        )?;
        let (cost, native_images, cpu_responses) =
            load_runtime_evidence(&file, expected, &evidence)?;
        Ok(GuestInputs {
            config,
            cost,
            native_images,
            cpu_responses,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::CapacityResources;

    fn profile(id: [u8; 32]) -> RuntimeCost {
        // Explicit pure model inputs, not measured native overhead values.
        let floor = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 256 << 20,
            disk_bytes: 0,
        };
        RuntimeCost {
            id,
            floor,
            ceiling: floor,
            quantum: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
            overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
        }
    }

    /// AC-T7.4.1: an explicit measured profile is admitted only with both exact
    /// file integrity and the fixed evidence identity, before any native effect.
    #[test]
    fn runtime_cost_file_and_evidence_must_both_match_exact_digests() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = dir.path().join("evidence.json");
        let file = dir.path().join("cost.json");
        std::fs::write(&evidence, b"model evidence, no native measurement claim").unwrap();
        let id = Sha256::digest(std::fs::read(&evidence).unwrap()).into();
        let cost = profile(id);
        let bytes = serde_json::to_vec(&cost).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        let expected = Sha256::digest(&bytes).into();
        assert_eq!(load_runtime_cost(&file, expected, &evidence).unwrap(), cost);
        assert!(load_runtime_cost(&file, [0; 32], &evidence).is_err());
        std::fs::write(&evidence, b"changed evidence").unwrap();
        assert!(load_runtime_cost(&file, expected, &evidence).is_err());
        assert!(load_runtime_cost(Path::new("relative"), expected, &evidence).is_err());
    }

    /// AC-T7.4.1: missing fields, unknown inputs, malformed digests and excessive
    /// files cannot supply a default/zero allowance or unbounded input buffer.
    #[test]
    fn runtime_cost_inputs_are_strict_and_bounded() {
        for bad in [
            "",
            &format!("+{}", "0".repeat(63)),
            &"0".repeat(63),
            &"g".repeat(64),
        ] {
            assert!(hash(bad).is_err());
        }
        assert_eq!(hash(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cost.json");
        let evidence = dir.path().join("evidence.json");
        std::fs::write(&evidence, b"model").unwrap();
        for bytes in [b"{}".as_slice(), b"{\"unknown\":0}".as_slice()] {
            std::fs::write(&file, bytes).unwrap();
            assert!(load_runtime_cost(&file, Sha256::digest(bytes).into(), &evidence).is_err());
        }
        let mut object = serde_json::to_value(profile(Sha256::digest(b"model").into())).unwrap();
        object
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), 0.into());
        let bytes = serde_json::to_vec(&object).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        assert!(load_runtime_cost(&file, Sha256::digest(&bytes).into(), &evidence).is_err());
        object.as_object_mut().unwrap().remove("unknown");
        object.as_object_mut().unwrap().remove("overhead");
        let bytes = serde_json::to_vec(&object).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        assert!(load_runtime_cost(&file, Sha256::digest(&bytes).into(), &evidence).is_err());
        let file = std::fs::File::create(&file).unwrap();
        file.set_len(u64::try_from(hex_core::bundle::MAX_CONFIG_BYTES).unwrap() + 1)
            .unwrap();
        assert!(read_config(&dir.path().join("cost.json")).is_err());
    }
}
