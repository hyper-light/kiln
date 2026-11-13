//! Explicit reviewed inputs for the existing guest workload. Verifying this
//! declared closure does not discover omitted loader/dlopen dependencies: ROOT
//! must supply and validate that complete artifact/execution contract.

use std::io::Read as _;
use std::path::{Component, PathBuf};

use hex_api::{Allocation, Bounds, CapacityResources, Program, Refusal};
use sha2::{Digest, Sha256};

use super::{driver, exhausted, invalid};
use crate::guest::{GuestConfig, shares};

mod load;
pub use load::{ENV_GPU_OBSERVER_FILE, ENV_GPU_OBSERVER_SHA256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuArtifactRole {
    Program,
    Interpreter,
    /// The Vulkan loader library, separately from an ELF interpreter.
    Loader,
    Icd,
    Driver,
    Dependency,
}

/// A source and its actual read-only guest path. Length/digest describe real
/// bytes, not an allowance or inferred library/driver identity.
pub struct GpuArtifact {
    pub role: GpuArtifactRole,
    pub host: PathBuf,
    pub guest: String,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// All fields are explicit reviewed inputs; there is no program/PATH, loader,
/// artifact, queue, memory or execution-context default. The Program's stderr
/// bound is no input: the node sizes it ([`super::stderr_room`]).
pub struct GpuObserverInput {
    /// Hash of ROOT's reviewed complete source/build/ABI/loader/context evidence.
    pub evidence: [u8; 32],
    pub artifacts: Vec<GpuArtifact>,
    /// Exact executable prefix, including an explicit loader invocation if the
    /// reviewed artifact needs one. Its last word names the Program artifact.
    pub launch: Vec<String>,
    pub max_queue_families: u32,
    pub max_allocation_bytes: usize,
    pub bounds: Bounds,
}

pub struct GpuObserver {
    input: GpuObserverInput,
    /// The most of the Program's stderr kept and shown: [`super::stderr_room`]. A launch keeps
    /// less where its receipt holds less beside all else.
    stderr: usize,
    identity: [u8; 32],
}

fn text(value: &str) -> Result<String, Refusal> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| exhausted(value.len()))?;
    copy.push_str(value);
    Ok(copy)
}

fn guest_path(path: &str) -> Result<(), Refusal> {
    if !path.starts_with('/')
        || path.len() <= 1
        || path.contains(['\0', '\n', '\r', ':', '\\'])
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(invalid(
            "observer artifact needs one exact absolute Linux guest path",
        ));
    }
    Ok(())
}

fn mapped(artifact: &GpuArtifact, config: &GuestConfig) -> Result<(), Refusal> {
    if !artifact.host.is_absolute() || artifact.bytes == 0 || artifact.sha256 == [0; 32] {
        return Err(invalid(
            "observer artifact needs an absolute source, actual size and digest",
        ));
    }
    guest_path(&artifact.guest)?;
    if config
        .shares
        .iter()
        .any(|share| !share.read_only && artifact.host.starts_with(&share.dir))
    {
        return Err(invalid(
            "observer artifact is writable through another configured share",
        ));
    }
    let matches = config.shares.iter().any(|share| {
        if !share.read_only {
            return false;
        }
        let Ok(relative) = artifact.host.strip_prefix(&share.dir) else {
            return false;
        };
        let Some(relative_guest) = artifact
            .guest
            .strip_prefix(share.at.trim_end_matches('/'))
            .and_then(|path| path.strip_prefix('/'))
        else {
            return false;
        };
        let mut guest = relative_guest.split('/');
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return false;
            };
            if guest.next() != name.to_str() {
                return false;
            }
        }
        guest.next().is_none()
    });
    if !matches {
        return Err(invalid(
            "observer artifact is not the same configured read-only shared file",
        ));
    }
    Ok(())
}

impl GpuObserver {
    pub fn new(
        input: GpuObserverInput,
        config: &GuestConfig,
        allocation: Allocation,
    ) -> Result<Self, Refusal> {
        if input.evidence == [0; 32]
            || input.max_queue_families == 0
            || input.max_allocation_bytes == 0
            || input.artifacts.is_empty()
            || input.launch.is_empty()
        {
            return Err(invalid(
                "observer requires reviewed positive explicit inputs",
            ));
        }
        if config.shares.len() > shares::MAX_SHARES
            || input
                .artifacts
                .len()
                .checked_mul(size_of::<[u8; 32]>())
                .is_none_or(|bytes| bytes > hex_core::bundle::MAX_CONFIG_BYTES)
        {
            return Err(exhausted(hex_core::bundle::MAX_CONFIG_BYTES));
        }
        // Transient sorted references detect both aliases without a retained
        // inventory/cache or a quadratic walk of a user-sized closure.
        let mut sorted = Vec::new();
        sorted
            .try_reserve_exact(input.artifacts.len())
            .map_err(|_| exhausted(hex_core::bundle::MAX_CONFIG_BYTES))?;
        sorted.extend(input.artifacts.iter());
        sorted.sort_unstable_by(|left, right| left.guest.cmp(&right.guest));
        if sorted
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.guest == right.guest))
        {
            return Err(invalid("observer artifact closure repeats a guest path"));
        }
        sorted.sort_unstable_by(|left, right| left.host.cmp(&right.host));
        if sorted
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.host == right.host))
        {
            return Err(invalid("observer artifact closure repeats a host path"));
        }
        drop(sorted);
        input.bounds.check()?;
        let cpu = input
            .bounds
            .cpu_max_milli
            .ok_or_else(|| invalid("observer CPU limit is not initialized"))?;
        let memory = input
            .bounds
            .memory_max
            .ok_or_else(|| invalid("observer memory limit is not initialized"))?;
        if input.bounds.pids_max.is_none() {
            return Err(invalid(
                "observer native thread/process limit is not initialized",
            ));
        }
        let stderr = super::stderr_room()?;
        let mut encoded = 0usize;
        let mut hash = Sha256::new();
        let mut part = |bytes: &[u8]| -> Result<(), Refusal> {
            encoded = encoded
                .checked_add(bytes.len())
                .and_then(|n| n.checked_add(size_of::<u64>()))
                .filter(|n| *n <= hex_core::bundle::MAX_CONFIG_BYTES)
                .ok_or_else(|| exhausted(hex_core::bundle::MAX_CONFIG_BYTES))?;
            hash.update(
                u64::try_from(bytes.len())
                    .map_err(|_| exhausted(encoded))?
                    .to_be_bytes(),
            );
            hash.update(bytes);
            Ok(())
        };
        part(b"hex-shards-guest-observer/v2-work-ready")?;
        part(&input.evidence)?;
        part(&input.max_queue_families.to_be_bytes())?;
        for count in [input.max_allocation_bytes, stderr] {
            part(
                &u64::try_from(count)
                    .map_err(|_| exhausted(count))?
                    .to_be_bytes(),
            )?;
        }
        part(&cpu.to_be_bytes())?;
        part(&memory.to_be_bytes())?;
        // Fixed field order and presence tags retain ALL optional bounds without
        // a debug/serde representation or another allocation/charge formula.
        for value in [
            input.bounds.cpu_weight.map(u64::from),
            input.bounds.cpu_max_milli.map(u64::from),
            input.bounds.memory_max,
            input.bounds.memory_high,
            input.bounds.io_weight.map(u64::from),
            input.bounds.io_latency_us.map(u64::from),
            input.bounds.pids_max.map(u64::from),
        ] {
            match value {
                Some(value) => {
                    part(&[1])?;
                    part(&value.to_be_bytes())?;
                }
                None => part(&[0])?,
            }
        }
        part(&[u8::from(input.bounds.io_max.is_some())])?;
        if let Some(rates) = input.bounds.io_max {
            for value in [
                rates.read_bps,
                rates.write_bps,
                rates.read_iops,
                rates.write_iops,
            ] {
                match value {
                    Some(value) => {
                        part(&[1])?;
                        part(&value.to_be_bytes())?;
                    }
                    None => part(&[0])?,
                }
            }
        }
        let mut program = None;
        let mut interpreter = None;
        let mut loader = None;
        let mut icd = None;
        let mut drivers = 0usize;
        for (index, artifact) in input.artifacts.iter().enumerate() {
            mapped(artifact, config)?;
            let slot = match artifact.role {
                GpuArtifactRole::Program => Some(&mut program),
                GpuArtifactRole::Interpreter => Some(&mut interpreter),
                GpuArtifactRole::Loader => Some(&mut loader),
                GpuArtifactRole::Icd => Some(&mut icd),
                GpuArtifactRole::Driver => {
                    drivers = drivers.checked_add(1).ok_or_else(|| exhausted(index))?;
                    None
                }
                GpuArtifactRole::Dependency => None,
            };
            if let Some(slot) = slot
                && slot.replace(artifact.guest.as_str()).is_some()
            {
                return Err(invalid("observer artifact closure repeats a required role"));
            }
            part(match artifact.role {
                GpuArtifactRole::Program => b"program",
                GpuArtifactRole::Interpreter => b"interpreter",
                GpuArtifactRole::Loader => b"loader",
                GpuArtifactRole::Icd => b"icd",
                GpuArtifactRole::Driver => b"driver",
                GpuArtifactRole::Dependency => b"dependency",
            })?;
            part(artifact.host.as_os_str().as_encoded_bytes())?;
            part(artifact.guest.as_bytes())?;
            part(&artifact.bytes.to_be_bytes())?;
            part(&artifact.sha256)?;
        }
        let program = program.ok_or_else(|| invalid("observer Program artifact is missing"))?;
        if loader.is_none()
            || icd.is_none()
            || drivers == 0
            || input.launch.last().map(String::as_str) != Some(program)
            || input
                .launch
                .first()
                .is_none_or(|word| Some(word.as_str()) != interpreter && word.as_str() != program)
        {
            return Err(invalid(
                "observer loader/ICD/driver closure or exact executable prefix is missing",
            ));
        }
        for word in &input.launch {
            if word.is_empty() || word.contains('\0') {
                return Err(invalid("observer launch word is invalid"));
            }
            part(word.as_bytes())?;
        }
        let identity = hash.finalize().into();
        let observer = Self {
            input,
            stderr,
            identity,
        };
        observer.check_allocation(allocation)?;
        observer.verify(config)?;
        Ok(observer)
    }

    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }

    pub fn check_allocation(&self, allocation: Allocation) -> Result<(), Refusal> {
        let cpu = self
            .input
            .bounds
            .cpu_max_milli
            .ok_or_else(|| invalid("observer CPU limit is not initialized"))?;
        let memory = self
            .input
            .bounds
            .memory_max
            .ok_or_else(|| invalid("observer memory limit is not initialized"))?;
        let room = allocation
            .guest()?
            .checked_sub(allocation.cost.guest_overhead)
            .ok_or_else(|| invalid("configured backing has no observer payload room"))?;
        if !room.holds(CapacityResources {
            cpu_milli: u64::from(cpu),
            mem_bytes: memory,
            disk_bytes: 0,
        }) || u64::try_from(self.input.max_allocation_bytes)
            .ok()
            .is_none_or(|bytes| bytes > memory)
        {
            return Err(invalid(
                "observer profile exceeds the already paid guest payload room",
            ));
        }
        Ok(())
    }

    pub fn queues(&self) -> u32 {
        self.input.max_queue_families
    }

    pub fn stderr_bytes(&self) -> usize {
        self.stderr
    }

    pub fn bounds(&self) -> &Bounds {
        &self.input.bounds
    }

    pub fn verify(&self, config: &GuestConfig) -> Result<(), Refusal> {
        for artifact in &self.input.artifacts {
            mapped(artifact, config)?;
            verify(artifact)?;
        }
        Ok(())
    }

    pub fn program(&self, expected: usize) -> Result<Program, Refusal> {
        let output = self.output_bytes(expected)?;
        let icd = self
            .input
            .artifacts
            .iter()
            .find(|artifact| artifact.role == GpuArtifactRole::Icd)
            .ok_or_else(|| invalid("observer lost its initialized ICD artifact"))?;
        let mut program = Program::default();
        let count = self
            .input
            .launch
            .len()
            .checked_add(10)
            .ok_or_else(|| exhausted(output))?;
        program
            .args
            .try_reserve_exact(count)
            .map_err(|_| exhausted(count))?;
        for word in &self.input.launch {
            program.args.push(text(word)?);
        }
        for (flag, value) in [
            (
                "--expected-devices",
                u64::try_from(expected).map_err(|_| exhausted(expected))?,
            ),
            (
                "--max-queue-families",
                u64::from(self.input.max_queue_families),
            ),
            (
                "--max-allocation-bytes",
                u64::try_from(self.input.max_allocation_bytes).map_err(|_| exhausted(output))?,
            ),
            (
                "--max-output-bytes",
                u64::try_from(output).map_err(|_| exhausted(output))?,
            ),
        ] {
            program.args.push(text(flag)?);
            let mut number = String::new();
            number
                .try_reserve_exact(
                    usize::try_from(
                        u64::MAX
                            .ilog10()
                            .checked_add(1)
                            .ok_or_else(|| exhausted(output))?,
                    )
                    .map_err(|_| exhausted(output))?,
                )
                .map_err(|_| exhausted(output))?;
            use std::fmt::Write as _;
            write!(&mut number, "{value}").map_err(|_| exhausted(output))?;
            program.args.push(number);
        }
        program.args.push(text("--icd")?);
        program.args.push(text(&icd.guest)?);
        program.env.try_reserve_exact(2).map_err(|_| exhausted(2))?;
        program
            .env
            .push((text("VK_DRIVER_FILES")?, text(&icd.guest)?));
        program
            .env
            .push((text("VK_LOADER_LAYERS_DISABLE")?, text("~all~")?));
        program.check()?;
        Ok(program)
    }

    pub fn output_bytes(&self, expected: usize) -> Result<usize, Refusal> {
        let output = driver::output_bound(expected)?;
        let combined = output
            .checked_add(self.stderr)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| exhausted(output))?;
        if self
            .input
            .bounds
            .memory_max
            .is_none_or(|memory| combined > memory)
        {
            return Err(invalid(
                "observer bounded output exceeds its initialized workload memory",
            ));
        }
        Ok(output)
    }
}

fn verify(artifact: &GpuArtifact) -> Result<(), Refusal> {
    if !std::fs::symlink_metadata(&artifact.host)
        .map_err(|why| Refusal::os("locating observer artifact", why))?
        .is_file()
    {
        return Err(invalid(
            "observer artifact must be a regular file, not a link or special object",
        ));
    }
    let mut file = std::fs::File::open(&artifact.host)
        .map_err(|why| Refusal::os("opening observer artifact", why))?;
    let before = file
        .metadata()
        .map_err(|why| Refusal::os("sizing observer artifact", why))?;
    if !before.is_file() || before.len() != artifact.bytes {
        return Err(invalid(
            "observer artifact differs from its initialized size",
        ));
    }
    let identity = hex_client::guest::stage::identity(&before);
    let mut digest = Sha256::new();
    // SHA-256's 512-bit compression block (FIPS180-4), fixed stack storage, not
    // an independently tuned or silently growing read buffer.
    let mut block = [0u8; 64];
    let mut left = artifact.bytes;
    while left > 0 {
        let room = usize::try_from(
            left.min(u64::try_from(block.len()).map_err(|_| exhausted(block.len()))?),
        )
        .map_err(|_| exhausted(block.len()))?;
        let got = file
            .read(block.get_mut(..room).ok_or_else(|| exhausted(room))?)
            .map_err(|why| Refusal::os("hashing observer artifact", why))?;
        if got == 0 {
            return Err(invalid(
                "observer artifact was truncated during verification",
            ));
        }
        digest.update(block.get(..got).ok_or_else(|| exhausted(room))?);
        left = left
            .checked_sub(u64::try_from(got).map_err(|_| exhausted(room))?)
            .ok_or_else(|| invalid("observer artifact read exceeds its declared size"))?;
    }
    let mut extra = [0u8; 1];
    let grew = file
        .read(&mut extra)
        .map_err(|why| Refusal::os("ending observer artifact verification", why))?;
    let after = file
        .metadata()
        .map_err(|why| Refusal::os("checking verified observer artifact", why))?;
    let actual: [u8; 32] = digest.finalize().into();
    if grew != 0
        || hex_client::guest::stage::identity(&after) != identity
        || actual != artifact.sha256
    {
        return Err(invalid(
            "observer artifact bytes or identity changed during verification",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::guest) mod tests {
    use super::*;

    pub(in crate::guest) fn fixture(
        dir: &std::path::Path,
    ) -> (GuestConfig, GpuObserverInput, Allocation) {
        let mut config = crate::guest::tests::config();
        // Absolute on every host (a drive's on Windows), as a launch's plan requires of its binary.
        config.binary = dir.join("shards");
        config.shares = vec![shares::Share {
            dir: dir.to_path_buf(),
            read_only: true,
            at: "/run/hex/shares/0".into(),
        }];
        let mut artifacts = Vec::new();
        for (role, name) in [
            (GpuArtifactRole::Program, "inventory"),
            (GpuArtifactRole::Interpreter, "interpreter"),
            (GpuArtifactRole::Loader, "libvulkan.so"),
            (GpuArtifactRole::Icd, "venus.json"),
            (GpuArtifactRole::Driver, "venus.so"),
        ] {
            // Hash/mapping model bytes only: not executable/ELF, a driver
            // readiness result or measured native resource requirements.
            let bytes = format!("model artifact: {name}");
            let host = dir.join(name);
            std::fs::write(&host, bytes.as_bytes()).unwrap();
            artifacts.push(GpuArtifact {
                role,
                host,
                guest: format!("/run/hex/shares/0/{name}"),
                bytes: u64::try_from(bytes.len()).unwrap(),
                sha256: Sha256::digest(bytes.as_bytes()).into(),
            });
        }
        let floor = crate::guest::selected::resources(&config).unwrap();
        let cost = hex_api::RuntimeCost {
            id: [1; 32],
            floor,
            ceiling: floor,
            quantum: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
            guest_overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
        };
        let allocation = cost.allocate(CapacityResources::default()).unwrap();
        let input = GpuObserverInput {
            evidence: [2; 32],
            artifacts,
            launch: vec!["/run/hex/shares/0/inventory".into()],
            max_queue_families: 8,
            max_allocation_bytes: 4096,
            bounds: Bounds {
                cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                memory_max: Some(hex_api::MIN_MEMORY),
                pids_max: Some(8),
                ..Bounds::default()
            },
        };
        (config, input, allocation)
    }

    /// AC-T7.4.1: actual configured bytes/read-only guest mappings bind the
    /// readiness Program; PATH and an unverified model label supply no input.
    #[test]
    fn observer_artifact_bytes_paths_and_clean_program_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = fixture(dir.path());
        let observer = GpuObserver::new(input, &config, allocation).unwrap();
        assert_ne!(observer.identity(), [0; 32]);
        let program = observer.program(1).unwrap();
        assert_eq!(program.args.first().unwrap(), "/run/hex/shares/0/inventory");
        assert_eq!(program.args.len(), 11);
        assert_eq!(
            program.env,
            vec![
                (
                    "VK_DRIVER_FILES".into(),
                    "/run/hex/shares/0/venus.json".into()
                ),
                ("VK_LOADER_LAYERS_DISABLE".into(), "~all~".into()),
            ]
        );
        std::fs::write(dir.path().join("venus.so"), b"changed driver").unwrap();
        assert!(observer.verify(&config).is_err());
    }

    /// AC-T7.4.1: the readiness child spends only its original finite remainder.
    #[test]
    fn work_ready_program_uses_only_explicit_artifacts_and_finite_remaining_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = fixture(dir.path());
        let observer = GpuObserver::new(input, &config, allocation).unwrap();
        let remaining = std::time::Duration::from_nanos(123_456);
        let program = driver::program(&observer, 1, remaining).unwrap();
        let suffix = program.args.get(program.args.len() - 2..).unwrap();
        assert_eq!(suffix, ["--wait-ns", "123456"]);
        assert_eq!(program.env, observer.program(1).unwrap().env);
        for invalid in [
            std::time::Duration::ZERO,
            std::time::Duration::MAX,
            std::time::Duration::from_nanos(u64::MAX),
        ] {
            assert!(driver::program(&observer, 1, invalid).is_err());
        }
    }

    /// AC-T7.4.1: unknown/mutable/aliased artifacts and an observer exceeding
    /// its already paid payload room cannot initialize a backing capability.
    #[test]
    fn observer_configuration_refuses_alias_unknown_and_uncharged_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, input, allocation) = fixture(dir.path());
        config.shares.first_mut().unwrap().read_only = false;
        assert!(GpuObserver::new(input, &config, allocation).is_err());
        let (config, mut input, allocation) = fixture(dir.path());
        input.evidence = [0; 32];
        assert!(GpuObserver::new(input, &config, allocation).is_err());
        let (config, mut input, allocation) = fixture(dir.path());
        input.bounds.memory_max = Some(allocation.guest().unwrap().mem_bytes);
        assert!(GpuObserver::new(input, &config, allocation).is_err());
        let (config, mut input, allocation) = fixture(dir.path());
        let duplicate = input.artifacts.first().unwrap().guest.clone();
        input.artifacts.last_mut().unwrap().guest = duplicate;
        assert!(GpuObserver::new(input, &config, allocation).is_err());
    }

    /// AC-T7.4.1: the same initialized artifact cannot be mutable through a
    /// second guest path. A read-only child share does not seal a writable parent.
    /// Original validation remains unchanged until this control is reproduced.
    #[test]
    fn observer_artifact_refuses_a_writable_parent_share_alias() {
        let dir = tempfile::tempdir().unwrap();
        let closure = dir.path().join("closure");
        std::fs::create_dir(&closure).unwrap();
        let (mut config, input, allocation) = fixture(&closure);
        config.shares.push(shares::Share {
            dir: dir.path().to_path_buf(),
            read_only: false,
            at: "/run/hex/shares/1".into(),
        });
        let initialized = GpuObserver::new(input, &config, allocation);
        assert!(
            initialized.is_err(),
            "read-only artifact bytes remain writable through the second parent share"
        );
    }
}
