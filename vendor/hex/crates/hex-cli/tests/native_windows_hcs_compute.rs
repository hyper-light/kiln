//! AC-T7.3.1/AC-T7.4.1/T-T7.1.2: ROOT-only HCS GPU execution and recovery witness. It extends
//! windows_assigned_device.rs's exact-interface contract: a source-pinned D3D12 compute program runs in
//! the box granted the selected GPU and its checked output is read back, an ungranted CPU sibling is
//! refused that work and keeps progressing, a mismatched catalog refuses recovery without changing
//! durable bytes, recovery keeps the same physical identity and init, and delete returns the device.
//! Inputs are the environment variables `Inputs::read` names; the caller supplies a finite external
//! whole-process guard. The native cases are ignored: implementer lanes compile them. No host
//! feature, driver, service or permission change, and no substitute hardware.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use hex_api::{BackingKey, BoxId, DeviceHolder};

/// AC-T7.4.1: the selected GPU's capacity holder is exactly `expected`: free is no holder at all,
/// and a box's grant names that box. A canonical `Backing` claim names no box, so a projection
/// through `DeviceHolder::box_id` would report a Backing-held GPU as free.
fn ordinary_holder_matches(actual: Option<&DeviceHolder>, expected: Option<&BoxId>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(DeviceHolder::Box(held)), Some(id)) => held == id,
        _ => false,
    }
}

/// AC-T7.4.1: do: match free, box-held and Backing-held devices against free and a box's grant;
/// expect a Backing claim never to count as free. Pure: it calls no native backend.
#[test]
fn native_windows_backing_holder_never_counts_as_unclaimed() {
    let id = BoxId::new("native-holder-control").unwrap();
    let owned = DeviceHolder::Box(id.try_clone().unwrap());
    let backing = DeviceHolder::Backing(BackingKey {
        pool: [0x41; 32],
        generation: [0x93; 16],
    });
    assert!(ordinary_holder_matches(None, None));
    assert!(ordinary_holder_matches(Some(&owned), Some(&id)));
    assert!(!ordinary_holder_matches(Some(&backing), Some(&id)));
    assert!(
        !ordinary_holder_matches(Some(&backing), None),
        "a canonical Backing holder must never satisfy an unclaimed native device"
    );
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
    use super::ordinary_holder_matches;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use hex_api::{
        Bounds, BoxId, BoxSpec, BoxState, Class, ClassSpec, DeviceId, DeviceKind, Exit, Program,
        Refusal, Runtime, Stdio,
    };
    use hex_node::recorded::Recorded;
    use hex_node::store::Store;
    use hex_runtime::devices::{Binding, Catalog, Route};
    use hex_runtime::windows::{WinRuntime, keeper, oci, sys};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const RESULTS: &str = r"C:\device-results";

    fn required(name: &str) -> String {
        std::env::var(name)
            .unwrap_or_else(|error| panic!("UNVERIFIED fixture input {name}: {error}"))
    }

    fn decimal(name: &str) -> u64 {
        required(name)
            .parse()
            .unwrap_or_else(|_| panic!("UNVERIFIED invalid decimal {name}"))
    }

    #[derive(Clone)]
    struct Interface {
        id: DeviceId,
        path: String,
        location: String,
    }

    fn interface(catalog: &Catalog, prefix: &str) -> Interface {
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key: decimal(&format!("{prefix}_KEY")),
        };
        let path = required(&format!("{prefix}_INTERFACE"));
        assert!(
            path.starts_with(r"\\?\") && !path.contains('\0'),
            "UNVERIFIED exact device interface path"
        );
        let class = hex_core::hcs::class_guid(
            "ROOT GPU interface class",
            &required(&format!("{prefix}_CLASS")),
        )
        .expect("UNVERIFIED interface class GUID");
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.info.id == id)
            .unwrap_or_else(|| panic!("UNVERIFIED {id:?} absent from ROOT catalog"));
        let Some(Binding::Hcs { location }) = entry.binding(Route::Hcs) else {
            panic!("UNVERIFIED GPU has no HCS binding")
        };
        eprintln!(
            "ROOT_GPU id={id:?} model={:?} bytes={} class={class} location={location:?} interface={path:?}",
            entry.info.model, entry.info.size
        );
        Interface {
            id,
            path,
            location: location.clone(),
        }
    }

    /// Exactly the production oci::devices query semantics: access0, share READ|WRITE, OPEN_EXISTING,
    /// attributes0, non-inheritable/no template. Host and child both call this one function.
    fn query(path: &str) -> Value {
        assert!(!path.contains('\0'), "UNVERIFIED interface contains NUL");
        let mut wide = Vec::new();
        wide.try_reserve_exact(path.encode_utf16().count().checked_add(1).unwrap())
            .unwrap();
        wide.extend(path.encode_utf16());
        wide.push(0);
        // SAFETY: wide is a live NUL-terminated UTF-16 path; no inherited security/template handle.
        // Query access0/OPEN_EXISTING creates no device/file and requests no read/write access.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            // SAFETY: called immediately after this thread's failed CreateFileW, no intervening OS call.
            let error = unsafe { GetLastError() };
            return json!({"path":path,"opened":false,"error":error,"closed":null});
        }
        // SAFETY: this is the successful query handle just created, owned by this call alone. Close
        // before any allocating/reporting operation; a close failure is recorded, never called success.
        let closed = unsafe { CloseHandle(handle) } != 0;
        let error = if closed {
            None
        } else {
            // SAFETY: immediately follows this thread's failed CloseHandle. Cleanup uncertainty is explicit.
            Some(unsafe { GetLastError() })
        };
        json!({"path":path,"opened":true,"error":error,"closed":closed})
    }

    fn probe_value(selected: &str, foreign: Option<&str>) -> Value {
        json!({"selected":query(selected), "foreign":foreign.map(query)})
    }

    fn check_query(row: &Value, path: &str, opened: bool) {
        assert_eq!(row["path"], path, "probe reported a different device path");
        assert_eq!(row["opened"], opened, "exact device open disagrees: {row}");
        if opened {
            assert_eq!(row["closed"], true, "query handle cleanup failed: {row}");
            assert!(
                row["error"].is_null(),
                "successful query carried an error: {row}"
            );
        } else {
            assert!(
                row["closed"].is_null() && row["error"].as_u64().is_some(),
                "missing failed-open fact: {row}"
            );
        }
    }

    struct BorrowedVolume {
        volume: String,
        folder: PathBuf,
        sentinel: PathBuf,
        sentinel_sha256: String,
    }

    impl BorrowedVolume {
        fn check(&self) {
            let actual = oci::sys::layer_mount_path(self.folder.to_str().unwrap()).unwrap();
            assert_eq!(
                actual.trim_end_matches('\\'),
                self.volume.trim_end_matches('\\'),
                "caller-owned volume mapping changed"
            );
            assert_eq!(
                digest(&self.sentinel),
                self.sentinel_sha256,
                "borrowed sentinel bytes changed"
            );
        }
    }

    struct Inputs {
        scratch: PathBuf,
        layers: Vec<String>,
        bounds: Bounds,
        wait: Duration,
        catalog: Vec<u8>,
        selected: Interface,
        foreign: Option<Interface>,
        compute: PathBuf,
        compute_sha256: String,
        luid: String,
        borrowed: Option<BorrowedVolume>,
    }

    fn bounded_read(path: &Path, bound: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut file = fs::File::open(path).expect("open bounded fixture input");
        (&mut file)
            .take(u64::try_from(bound).unwrap().checked_add(1).unwrap())
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(
            bytes.len() <= bound,
            "fixture input exceeded its derived/schema byte bound"
        );
        bytes
    }

    impl Inputs {
        fn read(negative: bool) -> Self {
            let scratch = PathBuf::from(required("HEX_WINDOWS_DEVICE_SCRATCH"));
            assert!(
                scratch.is_absolute() && scratch.is_dir(),
                "UNVERIFIED ROOT-owned scratch parent"
            );
            let layers: Vec<_> = required("HEX_WINDOWS_LAYERS")
                .split(';')
                .map(str::to_owned)
                .collect();
            assert!(
                !layers.is_empty()
                    && layers
                        .iter()
                        .all(|path| Path::new(path).is_absolute() && Path::new(path).is_dir()),
                "UNVERIFIED ROOT image layers; no empty/guessed layer"
            );
            let bounds: Bounds = serde_json::from_str(&required("HEX_WINDOWS_DEVICE_BOUNDS"))
                .expect("ROOT Bounds JSON");
            bounds.check().expect("ROOT bounds must be valid");
            let millis = decimal("HEX_WINDOWS_DEVICE_WAIT_MS");
            assert!(
                millis > 0 && millis < u64::from(u32::MAX),
                "UNVERIFIED finite native wait budget"
            );
            let catalog = bounded_read(
                Path::new(&required("HEX_WINDOWS_DEVICE_CATALOG")),
                hex_core::bundle::MAX_CONFIG_BYTES,
            );
            let parsed = Catalog::parse(&catalog).expect("ROOT native catalog");
            let selected = interface(&parsed, "HEX_WINDOWS_DEVICE");
            let foreign = negative.then(|| interface(&parsed, "HEX_WINDOWS_FOREIGN"));
            if let Some(other) = &foreign {
                assert_ne!(
                    selected.id, other.id,
                    "UNVERIFIED foreign GPU must be a different physical identity"
                );
                assert_ne!(
                    selected.path.to_lowercase(),
                    other.path.to_lowercase(),
                    "UNVERIFIED duplicate interface path"
                );
                assert_ne!(
                    selected.location, other.location,
                    "UNVERIFIED duplicate HCS physical location"
                );
            }
            let borrowed = match required("HEX_NATIVE_WINDOWS_STORAGE").as_str() {
                "owned" => None,
                "borrowed" => {
                    let volume = required("HEX_NATIVE_WINDOWS_BORROWED_VOLUME");
                    assert!(
                        volume.starts_with(r"\\?\Volume{")
                            && volume.trim_end_matches('\\').ends_with('}')
                    );
                    let folder = PathBuf::from(required("HEX_NATIVE_WINDOWS_BORROWED_FOLDER"))
                        .canonicalize()
                        .unwrap();
                    assert!(
                        folder.starts_with(scratch.parent().unwrap().canonicalize().unwrap()),
                        "borrowed input escaped caller-owned root"
                    );
                    let sentinel = PathBuf::from(format!(
                        "{}hex-native-borrowed-sentinel",
                        hex_core::hcs::volume_root(&volume)
                    ));
                    let borrowed = BorrowedVolume {
                        volume,
                        folder,
                        sentinel,
                        sentinel_sha256: required("HEX_NATIVE_WINDOWS_BORROWED_SENTINEL_SHA256"),
                    };
                    borrowed.check();
                    Some(borrowed)
                }
                _ => panic!("UNVERIFIED explicit owned/borrowed storage mode required"),
            };
            Self {
                scratch,
                layers,
                bounds,
                wait: Duration::from_millis(millis),
                catalog,
                selected,
                foreign,
                compute: PathBuf::from(required("HEX_NATIVE_WINDOWS_COMPUTE_EXE")),
                compute_sha256: required("HEX_NATIVE_WINDOWS_COMPUTE_SHA256"),
                luid: required("HEX_WINDOWS_GPU_COMPUTE_LUID"),
                borrowed,
            }
        }
    }

    struct OwnedBox {
        id: BoxId,
        scratch: PathBuf,
    }

    struct Lab {
        inputs: Inputs,
        root: PathBuf,
        state: PathBuf,
        keeping: PathBuf,
        catalog: PathBuf,
        bin: PathBuf,
        probe: PathBuf,
        results: PathBuf,
        nonce: String,
        runtime: Option<Recorded<WinRuntime>>,
        boxes: Vec<OwnedBox>,
    }

    impl Lab {
        fn new(inputs: Inputs) -> Self {
            // Persist immediately: no TempDir destructor may erase claims after a failed native cleanup.
            let root = tempfile::Builder::new()
                .prefix("hex-device-")
                .tempdir_in(&inputs.scratch)
                .unwrap()
                .keep();
            eprintln!(
                "OWNED_LAB_CREATED {} (retained if setup fails)",
                root.display()
            );
            let nonce = root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect();
            let state = root.join("state");
            let keeping = state.join("keeper");
            let catalog = state.join("devices");
            let probe = root.join("probe");
            let results = root.join("results");
            fs::create_dir_all(&state).unwrap();
            fs::create_dir(&probe).unwrap();
            fs::create_dir(&results).unwrap();
            fs::write(&catalog, &inputs.catalog).unwrap();
            assert_eq!(
                digest(&inputs.compute),
                inputs.compute_sha256,
                "pinned host compute binary"
            );
            fs::copy(
                &inputs.compute,
                probe.join("completion_windows_compute.exe"),
            )
            .unwrap();
            assert_eq!(
                digest(&probe.join("completion_windows_compute.exe")),
                inputs.compute_sha256
            );
            let bin = Path::new(env!("CARGO_BIN_EXE_hex"))
                .parent()
                .unwrap()
                .to_owned();
            eprintln!(
                "OWNED_LAB {} (retain on failure; borrowed layers never removed)",
                root.display()
            );
            Self {
                inputs,
                root,
                state,
                keeping,
                catalog,
                bin,
                probe,
                results,
                nonce,
                runtime: None,
                boxes: Vec::new(),
            }
        }

        fn open(&mut self) {
            assert!(self.runtime.is_none());
            let command = vec![env!("CARGO_BIN_EXE_hex").to_owned(), "keep".to_owned()];
            let native = WinRuntime::outliving_with_devices(
                &keeper::node_name(&self.keeping),
                &self.keeping,
                &command,
                &self.catalog,
            )
            .expect("native outliving runtime with exact catalog")
            .with_launcher(self.bin.clone());
            assert!(
                native.reclaim_failures().is_empty(),
                "native scratch reclaim failures: {:?}",
                native.reclaim_failures()
            );
            // Match the native CLI's one real provider owned by this Recorded runtime.
            // hcsgpu1 retained the original missing-provider refusal before any HCS create.
            let prober =
                hex_node::facts::shared_prober(&self.state, hex_node::facts::Declared::default())
                    .expect("initialize the same native physical provider as the Windows CLI");
            self.runtime = Some(Recorded::open_with_prober(native, &self.state, prober).unwrap());
        }

        fn retain_state(&self, label: &str) {
            // Keep exact original bytes before failure cleanup can remove native markers/store rows.
            // This captures the observed phase; a snapshot alone is not before-effects ordering proof.
            let original = durable_bytes(&self.state);
            let charged = original.values().try_fold(0_usize, |total, bytes| {
                total
                    .checked_add(bytes.len())
                    .and_then(|sum| sum.checked_add(1))
            });
            assert!(
                charged.is_some_and(|bytes| bytes <= hex_core::bundle::MAX_CONFIG_BYTES),
                "diagnostic state exceeds the existing config-sized caller read bound; retain lab"
            );
            let path = self.results.join(format!("state-{label}.json"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(&serde_json::to_vec(&original).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            eprintln!(
                "RETAINED_STATE phase={label} files={} sha256={}",
                original.len(),
                digest(&path)
            );
        }

        fn rt(&self) -> &Recorded<WinRuntime> {
            self.runtime.as_ref().expect("an open runtime")
        }

        fn store(&self) -> Store {
            Store::open(&self.state, &self.state.join("@records")).unwrap()
        }

        fn spec(&mut self, label: &str, selected: bool) -> BoxSpec {
            let id = BoxId::new(&format!("wd-{}-{label}", self.nonce)).unwrap();
            let bundle = self.root.join(format!("bundle-{label}"));
            let scratch = self.root.join(format!("scratch-{label}"));
            fs::create_dir(&bundle).unwrap();
            let mut layers = self.inputs.layers.clone();
            layers.push(scratch.to_str().unwrap().to_owned());
            // Same Windows bundle/layer/hex selftest-box wait contract as windows_oci_box::Lab.
            let mut config = json!({"ociVersion":"1.2.0", "process":{"args":[r"C:\hex\hex.exe","selftest-box","wait"],"cwd":"C:\\"},
                "mounts":[{"source":self.probe,"destination":r"C:\device-probe","options":["ro"]},
                    {"source":self.results,"destination":RESULTS}], "windows":{"layerFolders":layers}});
            if selected && let Some(borrowed) = &self.inputs.borrowed {
                borrowed.check();
                config["root"] = json!({"path": hex_core::hcs::volume_root(&borrowed.volume)});
            }
            fs::write(
                bundle.join("config.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
            self.boxes.push(OwnedBox {
                id: id.clone(),
                scratch,
            });
            BoxSpec {
                route: None,
                allocation: None,
                id,
                bounds: self.inputs.bounds.clone(),
                class: ClassSpec::Bundle(bundle),
                owner: None,
                devices: if selected {
                    vec![self.inputs.selected.id]
                } else {
                    Vec::new()
                },
                band: 0,
            }
        }

        fn holder(&self, expected: Option<&BoxId>) {
            let capacity = self
                .rt()
                .capacity(Class::Oci)
                .unwrap()
                .expect("native attachment capacity");
            capacity.check().unwrap();
            let entries: Vec<_> = capacity
                .devices
                .iter()
                .filter(|info| info.id == self.inputs.selected.id)
                .collect();
            assert_eq!(entries.len(), 1, "selected GPU lost/duplicated in capacity");
            assert!(
                ordinary_holder_matches(entries[0].holder.as_ref(), expected),
                "native holder differs: {capacity:?}"
            );
            if let Some(other) = &self.inputs.foreign {
                let entry = capacity
                    .devices
                    .iter()
                    .find(|info| info.id == other.id)
                    .expect("foreign GPU still advertised");
                assert!(entry.holder.is_none(), "ungranted foreign GPU was reserved");
            }
            eprintln!(
                "CAPACITY selected={:?} holder={expected:?}",
                self.inputs.selected.id
            );
        }

        fn exec(&self, id: &BoxId, program: Program) {
            let mut process = self
                .rt()
                .exec(id, program, Stdio::null())
                .expect("exec owned probe/member");
            let pid = process.pid();
            let exit = process
                .wait_for(self.inputs.wait)
                .expect("wait for owned member")
                .expect("owned member exceeded ROOT deadline");
            assert_eq!(exit, Exit::Code(0), "owned member {pid} failed");
        }

        fn probe(&self, id: &BoxId, phase: &str, granted: bool) {
            let mut program = Program::of(&[
                r"C:\device-probe\completion_windows_compute.exe",
                "--ignored",
                "--exact",
                "root_windows_exact_adapter_executes_checked_kernel",
                "--nocapture",
                "--test-threads=1",
            ]);
            program.env = vec![
                (
                    "HEX_WINDOWS_GPU_COMPUTE_INTERFACE".into(),
                    self.inputs.selected.path.clone(),
                ),
                (
                    "HEX_WINDOWS_GPU_COMPUTE_LUID".into(),
                    self.inputs.luid.clone(),
                ),
                (
                    "HEX_WINDOWS_GPU_COMPUTE_WAIT_MS".into(),
                    self.inputs.wait.as_millis().to_string(),
                ),
            ];
            let (reader, writer) = std::io::pipe().unwrap();
            let path = self.results.join(format!("compute-{phase}.log"));
            let held = path.clone();
            let (sent, received) = std::sync::mpsc::sync_channel(1);
            let output = std::thread::Builder::new()
                .name("native-compute-output".into())
                .spawn(move || {
                    let mut bytes = Vec::new();
                    reader
                        .take(u64::try_from(hex_core::bundle::MAX_CONFIG_BYTES).unwrap() + 1)
                        .read_to_end(&mut bytes)
                        .unwrap();
                    assert!(
                        bytes.len() <= hex_core::bundle::MAX_CONFIG_BYTES,
                        "native output bound"
                    );
                    fs::write(held, &bytes).unwrap();
                    sent.send(bytes).unwrap();
                })
                .unwrap();
            let mut process = self
                .rt()
                .exec(
                    id,
                    program,
                    Stdio {
                        stdout: Some(writer.try_clone().unwrap()),
                        stderr: Some(writer),
                        ..Stdio::null()
                    },
                )
                .expect("actual source-pinned compute process in selected HCS context");
            let pid = process.pid();
            let exit = process
                .wait_for(self.inputs.wait)
                .expect("actual compute wait")
                .expect("compute exceeded finite caller budget; retain owned scope");
            drop(process);
            let bytes = received
                .recv_timeout(self.inputs.wait)
                .expect("native output did not close under caller budget");
            output.join().unwrap(); // Reader already reported EOF; outer guard retains OS join uncertainty.
            let text = String::from_utf8_lossy(&bytes);
            eprintln!(
                "HCS_COMPUTE phase={phase} granted={granted} pid={pid} exit={exit:?} sha256={:x} output={text}",
                Sha256::digest(&bytes)
            );
            if granted {
                assert_eq!(exit, Exit::Code(0), "actual selected GPU compute failed");
                assert!(
                    text.contains(&format!(
                        "windows_compute_verified luid={} elements=1024 mismatches=0",
                        self.inputs.luid
                    )),
                    "missing checked actual GPU output"
                );
                assert!(text.contains("windows_compute_fence wait=0x0 value=1 device=Ok(())"));
                assert!(
                    text.contains("windows_compute_handles kmt=1/1 events=1/1 close_failed=false")
                );
            } else {
                assert_ne!(
                    exit,
                    Exit::Code(0),
                    "ungranted CPU sibling executed selected GPU work"
                );
                assert!(
                    !text.contains("windows_compute_verified")
                        && !text.contains("windows_compute_selected"),
                    "ungranted effects escaped identity boundary"
                );
                assert!(
                    text.contains("KMT open")
                        || text.contains("KMT identity")
                        || text.contains("DXGI exact LUID"),
                    "negative failed for unrelated reason"
                );
            }
        }

        fn progress(&self, id: &BoxId, phase: &str) {
            let path = self.results.join(format!("cpu-{phase}.txt"));
            let container = format!(r"{RESULTS}\cpu-{phase}.txt");
            let program = Program::of(&[r"C:\hex\hex.exe", "selftest-box", "write", &container]);
            self.exec(id, program);
            assert!(path.is_file(), "CPU sibling made no actual progress marker");
            eprintln!("CPU_PROGRESS {phase} file={}", path.display());
        }

        fn marker(&self, id: &BoxId) -> PathBuf {
            self.keeping
                .join("boxes")
                .join(format!("{id}.scratch.json"))
        }

        fn cleanup(&mut self) -> Vec<String> {
            let mut errors = Vec::new();
            if self.runtime.is_none() && self.boxes.is_empty() && !self.keeping.exists() {
                return errors; // Host control failed before any native runtime/keeper was opened.
            }
            if self.runtime.is_none() {
                let reopened =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.open()));
                if reopened.is_err() {
                    errors.push("reopening own runtime for cleanup failed".into());
                    return errors;
                }
                if let Err(error) = self.rt().recover() {
                    errors.push(format!("cleanup recovery: {error}"));
                }
            }
            for owned in self.boxes.iter().rev() {
                let deleted = self.rt().delete(&owned.id);
                if let Err(error) = deleted
                    && !matches!(error, Refusal::NoSuchBox(_))
                {
                    errors.push(format!("delete {}: {error}", owned.id));
                }
                match oci::sys::open_system(owned.id.as_str()) {
                    Ok(None) => {}
                    Ok(Some(_)) => errors.push(format!("HCS still holds {}", owned.id)),
                    Err(error) => {
                        errors.push(format!("HCS absence unknown {}: {error:?}", owned.id))
                    }
                }
                match self.store().get(&owned.id) {
                    Ok(None) => {}
                    other => errors.push(format!("record remains/unknown {}: {other:?}", owned.id)),
                }
                if self.marker(&owned.id).exists() || owned.scratch.exists() {
                    errors.push(format!("native marker/scratch remains for {}", owned.id));
                }
            }
            match self.store().all() {
                Ok(records) if records.is_empty() => {}
                other => errors.push(format!("private box records remain/unknown: {other:?}")),
            }
            if errors.is_empty() {
                // Only our private keeper owner identity, never a process chosen by command/name.
                let path = self.keeping.join("keeper.owner");
                match fs::read_to_string(&path) {
                    Ok(text) => {
                        let mut words = text.split_whitespace();
                        let identity = words
                            .next()
                            .and_then(|s| s.parse::<u32>().ok())
                            .zip(words.next().and_then(|s| s.parse::<u64>().ok()));
                        if let Some((pid, created)) = identity {
                            match keeper::NodeKeeper::open(pid, created, &self.keeping) {
                                Ok(keeper) => keeper.stop(),
                                Err(error) => errors.push(format!("own keeper open: {error}")),
                            }
                            match sys::open_process(pid) {
                                Ok(Some(process)) => {
                                    match sys::created(&process) {
                                        Ok(actual) if actual == created => {
                                            match sys::wait(
                                                &process,
                                                u32::try_from(self.inputs.wait.as_millis())
                                                    .unwrap(),
                                            ) {
                                                Ok(true) => {}
                                                other => errors.push(format!(
                                                    "own keeper not reaped/unknown: {other:?}"
                                                )),
                                            }
                                        }
                                        Ok(_) => {} // PID reuse proves our exact keeper ended; never stop its successor.
                                        Err(error) => errors
                                            .push(format!("own keeper identity unknown: {error}")),
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => errors.push(format!("own keeper absence: {error}")),
                            }
                        } else {
                            errors.push("own keeper identity malformed".into());
                        }
                    }
                    Err(error) => errors.push(format!("own keeper identity read: {error}")),
                }
            }
            self.runtime.take();
            if let Some(borrowed) = &self.inputs.borrowed {
                let preserved =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| borrowed.check()));
                if preserved.is_err() {
                    errors.push("caller-owned borrowed volume/sentinel not preserved".into());
                }
            }
            errors
        }
    }

    fn digest(path: &Path) -> String {
        let mut file = fs::File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut bytes = [0_u8; 8192];
        loop {
            let size = file.read(&mut bytes).unwrap();
            if size == 0 {
                break;
            }
            hash.update(&bytes[..size]);
        }
        format!("{:x}", hash.finalize())
    }

    fn durable_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut result = std::collections::BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    result.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        result
    }

    #[derive(Clone, Copy)]
    enum Campaign {
        Combined,
        SelectedOnly,
    }

    fn exercise(lab: &mut Lab, campaign: Campaign) {
        let host = probe_value(
            &lab.inputs.selected.path,
            lab.inputs.foreign.as_ref().map(|v| v.path.as_str()),
        );
        assert_eq!(
            host["selected"]["opened"], true,
            "UNVERIFIED selected GPU exact-interface host-positive control unavailable: {host}"
        );
        check_query(&host["selected"], &lab.inputs.selected.path, true);
        if let Some(other) = &lab.inputs.foreign {
            assert_eq!(
                host["foreign"]["opened"], true,
                "UNVERIFIED actual second GPU exact-interface host-positive control unavailable: {host}"
            );
            check_query(&host["foreign"], &other.path, true);
        }
        eprintln!("EXACT_HOST {host}");
        lab.open();
        lab.holder(None);
        let cpu_id = match campaign {
            Campaign::Combined => {
                let cpu = lab.spec("cpu", false);
                let id = cpu.id.clone();
                lab.rt()
                    .create(cpu)
                    .expect("ungranted CPU/OCI control create");
                lab.retain_state("cpu-before-start");
                lab.rt().start(&id).unwrap();
                lab.probe(&id, "ungranted", false);
                Some(id)
            }
            Campaign::SelectedOnly => {
                eprintln!(
                    "CPU_SIBLING UNVERIFIED: independent selected case; combined case remains required"
                );
                None
            }
        };
        lab.holder(None);
        let selected = lab.spec("selected", true);
        let selected_id = selected.id.clone();
        lab.rt()
            .create(selected)
            .expect("exact physical GPU create");
        lab.holder(Some(&selected_id));
        lab.retain_state("selected-before-start");
        lab.rt().start(&selected_id).unwrap();
        lab.probe(&selected_id, "granted", true);
        let marker_path = lab.marker(&selected_id);
        let marker_before = fs::read(&marker_path).unwrap();
        let native_before = oci::read_marker(&marker_path).unwrap();
        assert_eq!(
            native_before.devices,
            vec![hex_core::hcs::DeviceBinding {
                id: lab.inputs.selected.id,
                location: lab.inputs.selected.location.clone()
            }]
        );
        assert_eq!(native_before.system.as_ref(), Some(&selected_id));
        assert_eq!(
            native_before.storage,
            if lab.inputs.borrowed.is_some() {
                hex_core::hcs::RecordStorage::BorrowedVolume
            } else {
                hex_core::hcs::RecordStorage::OwnedFolder
            }
        );
        if let Some(borrowed) = &lab.inputs.borrowed {
            assert!(native_before.made.is_empty());
            assert_eq!(
                native_before.folder.trim_end_matches('\\'),
                borrowed.volume.trim_end_matches('\\')
            );
            borrowed.check();
        }
        assert!(native_before.outlives && native_before.init.is_some());
        fs::write(
            lab.results.join("selected-marker-before.json"),
            &marker_before,
        )
        .unwrap();
        let before = lab.store().get(&selected_id).unwrap().unwrap().unwrap();
        fs::write(
            lab.results.join("selected-record-before.json"),
            serde_json::to_vec_pretty(&before).unwrap(),
        )
        .unwrap();
        assert_eq!(before.spec.devices, [lab.inputs.selected.id]);
        let overlap = lab.spec("overlap", true);
        let overlap_id = overlap.id.clone();
        let refusal = lab
            .rt()
            .create(overlap)
            .expect_err("same physical GPU cannot be granted twice");
        assert!(
            matches!(refusal, Refusal::Invalid { .. } | Refusal::Exists(_)),
            "unexpected overlap refusal: {refusal}"
        );
        lab.holder(Some(&selected_id));
        assert_eq!(
            fs::read(&marker_path).unwrap(),
            marker_before,
            "overlap changed native durable bytes"
        );
        assert_eq!(
            lab.store().get(&selected_id).unwrap().unwrap().unwrap(),
            before
        );
        assert!(lab.store().get(&overlap_id).unwrap().is_none());
        assert!(!lab.marker(&overlap_id).exists());
        assert!(
            oci::sys::open_system(overlap_id.as_str())
                .unwrap()
                .is_none(),
            "refused overlap created a physical container"
        );
        if let Some(cpu_id) = &cpu_id {
            lab.progress(cpu_id, "after-overlap");
        }
        lab.runtime.take(); // Same private keeper retains actual box jobs/systems across owner restart.
        let configured = fs::read(&lab.catalog).unwrap();
        let text = String::from_utf8(configured.clone()).unwrap();
        let old = &lab.inputs.selected.location;
        let changed = text.replace(old, &format!("{old}-native-mismatch"));
        assert_ne!(changed.as_bytes(), configured);
        fs::write(&lab.catalog, changed.as_bytes()).unwrap();
        let durable_before = durable_bytes(&lab.state);
        lab.open();
        let refusal = lab
            .rt()
            .recover()
            .expect_err("mismatched current mapping must refuse recovery");
        eprintln!("MAPPING_MISMATCH refusal={refusal:?}");
        assert_eq!(
            durable_bytes(&lab.state),
            durable_before,
            "mismatch rewrote native/durable bytes"
        );
        lab.runtime.take();
        fs::write(&lab.catalog, &configured).unwrap();
        assert_eq!(fs::read(&lab.catalog).unwrap(), lab.inputs.catalog);
        lab.open();
        let recovered = lab
            .rt()
            .recover()
            .expect("recover exact recorded native bindings");
        assert!(
            recovered.removed.is_empty(),
            "recovery removed a surviving box: {recovered:?}"
        );
        assert!(recovered.adopted.contains(&selected_id));
        if let Some(cpu_id) = &cpu_id {
            assert!(recovered.adopted.contains(cpu_id));
        }
        let info = lab.rt().inspect(&selected_id).unwrap();
        assert_eq!((info.state, info.adopted), (BoxState::Running, true));
        lab.holder(Some(&selected_id));
        let native_after = oci::read_marker(&marker_path).unwrap();
        assert_eq!(native_after.devices, native_before.devices);
        assert_eq!(
            native_after.init, native_before.init,
            "recovery restarted/substituted init"
        );
        assert_eq!(
            lab.store().get(&selected_id).unwrap().unwrap().unwrap(),
            before
        );
        lab.probe(&selected_id, "recovered", true);
        if let Some(cpu_id) = &cpu_id {
            lab.progress(cpu_id, "after-restart");
        }
        lab.rt()
            .delete(&selected_id)
            .expect("release exact device/physical container");
        lab.holder(None);
        assert!(
            oci::sys::open_system(selected_id.as_str())
                .unwrap()
                .is_none()
        );
        assert!(lab.store().get(&selected_id).unwrap().is_none() && !marker_path.exists());
        if let Some(cpu_id) = &cpu_id {
            lab.progress(cpu_id, "after-release");
            lab.rt()
                .delete(cpu_id)
                .expect("delete independent CPU sibling");
        }
    }

    fn parent(negative: bool, campaign: Campaign) {
        let inputs = Inputs::read(negative); // Foreign inputs are mandatory only in the foreign GPU case.
        let mut lab = Lab::new(inputs);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            exercise(&mut lab, campaign)
        }));
        if result.is_err() {
            let retained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lab.retain_state("failure-before-cleanup");
            }));
            if retained.is_err() {
                eprintln!("STATE_RETENTION_FAILURE before cleanup; original failure/lab preserved");
            }
        }
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lab.cleanup()));
        let errors = cleanup
            .unwrap_or_else(|_| vec!["cleanup itself panicked; private lab retained".into()]);
        for error in &errors {
            eprintln!("CLEANUP_FAILURE {error}; retained {}", lab.root.display());
        }
        if let Err(original) = result {
            eprintln!("ORIGINAL_FAILURE preserved; lab {}", lab.root.display());
            std::panic::resume_unwind(original);
        }
        assert!(
            errors.is_empty(),
            "native cleanup incomplete; lab retained: {}",
            lab.root.display()
        );
        assert!(lab.root.starts_with(&lab.inputs.scratch));
        let evidence = lab.inputs.scratch.join(format!("{}-evidence", lab.nonce));
        fs::rename(&lab.results, &evidence).unwrap();
        fs::remove_dir_all(&lab.root)
            .expect("remove only proven-empty owned lab, never borrowed layers/volume");
        eprintln!("NATIVE_EVIDENCE {}", evidence.display());
        eprintln!("OWNED_CLEANUP complete {}", lab.root.display());
    }

    /// Actual single-GPU positive/baseline/recovery witness; no second GPU or USB requirement.
    #[test]
    #[ignore = "assigned adversarial actual HCS GPU compute; requires pinned native inputs and external process guard"]
    fn native_windows_hcs_selected_compute_survives_restart() {
        assert_eq!(required("HEX_NATIVE_WINDOWS_STORAGE"), "owned");
        parent(false, Campaign::Combined);
    }

    /// Independent selected-device case when a separately retained CPU start prerequisite fails.
    /// A pass here cannot establish ungranted CPU exclusion or sibling progress.
    #[test]
    #[ignore = "assigned bounded selected-only HCS case; CPU baseline remains separately unverified"]
    fn native_windows_hcs_selected_compute_without_cpu_precondition() {
        assert_eq!(required("HEX_NATIVE_WINDOWS_STORAGE"), "owned");
        parent(false, Campaign::SelectedOnly);
    }

    /// AC-T7.3.1/T-T7.1.2: the same native marker/recovery contract preserves caller-owned volume data.
    #[test]
    #[ignore = "assigned actual HCS borrowed GPU compute; requires caller-prepared owned volume and external guard"]
    fn native_windows_hcs_borrowed_compute_survives_restart() {
        assert_eq!(required("HEX_NATIVE_WINDOWS_STORAGE"), "borrowed");
        parent(false, Campaign::Combined);
    }

    /// T-T7.1.2: ordinary HCS/keeper/OwnedFolder recovery independent of an unavailable GPU base image.
    #[test]
    #[ignore = "assigned bounded native HCS CPU control; requires actual layers and external guard"]
    fn native_windows_hcs_owned_cpu_recovery_and_cleanup() {
        let mut lab = Lab::new(Inputs::read(false));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lab.open();
            let spec = lab.spec("cpu-recovery", false);
            let id = spec.id.clone();
            lab.rt()
                .create(spec)
                .expect("ordinary native CPU HCS create");
            lab.retain_state("cpu-before-start");
            lab.rt().start(&id).expect("ordinary native CPU HCS start");
            lab.progress(&id, "cpu-original");
            let before = oci::read_marker(&lab.marker(&id)).unwrap();
            assert_eq!(before.storage, hex_core::hcs::RecordStorage::OwnedFolder);
            assert!(before.devices.is_empty() && before.init.is_some());
            let record = lab.store().get(&id).unwrap().unwrap().unwrap();
            fs::write(
                lab.results.join("cpu-marker-before.json"),
                fs::read(lab.marker(&id)).unwrap(),
            )
            .unwrap();
            lab.runtime.take();
            lab.open();
            let summary = lab.rt().recover().expect("actual CPU HCS recovery");
            assert_eq!(summary.adopted, vec![id.clone()]);
            assert!(summary.removed.is_empty());
            let after = oci::read_marker(&lab.marker(&id)).unwrap();
            assert_eq!(after, before, "CPU recovery changed native identity/marker");
            assert_eq!(lab.store().get(&id).unwrap().unwrap().unwrap(), record);
            lab.progress(&id, "cpu-recovered");
            lab.rt()
                .delete(&id)
                .expect("ordinary CPU HCS physical delete");
            assert!(oci::sys::open_system(id.as_str()).unwrap().is_none());
            assert!(!lab.marker(&id).exists());
            assert!(lab.store().get(&id).unwrap().is_none());
            eprintln!(
                "CPU_HCS_RECOVERY verified native_init={:?} physical_absence=true",
                before.init
            );
        }));
        if result.is_err() {
            let retained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lab.retain_state("failure-before-cleanup");
            }));
            if retained.is_err() {
                eprintln!("STATE_RETENTION_FAILURE before cleanup; original failure/lab preserved");
            }
        }
        let errors = lab.cleanup();
        for error in &errors {
            eprintln!("CLEANUP_FAILURE {error}; retained {}", lab.root.display());
        }
        if let Err(original) = result {
            std::panic::resume_unwind(original);
        }
        assert!(errors.is_empty(), "native cleanup uncertainty: {errors:?}");
        let evidence = lab.inputs.scratch.join(format!("{}-evidence", lab.nonce));
        fs::rename(&lab.results, &evidence).unwrap();
        fs::remove_dir_all(&lab.root).unwrap();
        eprintln!("OWNED_CLEANUP complete; evidence {}", evidence.display());
    }
}
