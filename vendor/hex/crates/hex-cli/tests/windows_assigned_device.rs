//! ROOT-only exact Windows GPU assignment witnesses (AC-T7.3.1/AC-T7.4.1/T-T7.1.2).
//! Source/compile-only on implementer lanes. Required inputs/guard: docs/hex-windows-device-witness.
//! Reuses windows_oci_box's layer/bundle/launcher and Recorded recovery contracts, not its
//! class-wide win-devices counts. Native opens below are exact-path query-only calls.

#![cfg(target_os = "windows")]
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

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
use hex_runtime::devices::{Binding, Catalog};
use hex_runtime::windows::{WinRuntime, keeper, oci, sys};
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const CHILD: &str = "assigned_device_exact_path_probe";
const PROBE_EXE: &str = r"C:\device-probe\assigned_device_probe.exe";
const RESULTS: &str = r"C:\device-results";

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|error| panic!("UNVERIFIED fixture input {name}: {error}"))
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
    let Some(Binding::Hcs { location }) = entry.binding(hex_runtime::devices::Route::Hcs) else {
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

/// Only the parent invokes this private copied test executable inside its HCS box. Missing inputs,
/// missing foreign mode input, close failure or write failure is a failure, never a silent return.
#[test]
#[ignore = "ROOT-only CHILD exact interface probe; parent supplies private mapped result path"]
fn assigned_device_exact_path_probe() {
    assert_eq!(
        required("HEX_WIN_PROBE_CHILD"),
        "1",
        "not a parent-owned probe invocation"
    );
    let selected = required("HEX_WIN_PROBE_SELECTED");
    let foreign = match required("HEX_WIN_PROBE_MODE").as_str() {
        "single" => None,
        "foreign" => Some(required("HEX_WIN_PROBE_FOREIGN")),
        _ => panic!("UNVERIFIED probe mode"),
    };
    let result = required("HEX_WIN_PROBE_RESULT");
    let result_path = Path::new(&result);
    assert_eq!(
        result_path.parent(),
        Some(Path::new(RESULTS)),
        "result escaped mapped private folder"
    );
    let value = probe_value(&selected, foreign.as_deref());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(result_path)
        .expect("new private result");
    file.write_all(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
    for row in [
        Some(&value["selected"]),
        foreign.as_ref().map(|_| &value["foreign"]),
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            row["closed"] != false,
            "native query handle close failed: {row}"
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
        let bounds: Bounds =
            serde_json::from_str(&required("HEX_WINDOWS_DEVICE_BOUNDS")).expect("ROOT Bounds JSON");
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
        Self {
            scratch,
            layers,
            bounds,
            wait: Duration::from_millis(millis),
            catalog,
            selected,
            foreign,
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
        fs::copy(
            std::env::current_exe().unwrap(),
            probe.join("assigned_device_probe.exe"),
        )
        .unwrap();
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
        // As the Windows CLI opens its node (src/node.rs): device capacity reads the one shared
        // native provider, and without it every capacity call is refused before any HCS create.
        let prober =
            hex_node::facts::shared_prober(&self.state, hex_node::facts::Declared::default())
                .expect("the native provider the Windows CLI initializes");
        self.runtime = Some(Recorded::open_with_prober(native, &self.state, prober).unwrap());
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
        let config = json!({"ociVersion":"1.2.0", "process":{"args":[r"C:\hex\hex.exe","selftest-box","wait"],"cwd":"C:\\"},
            "mounts":[{"source":self.probe,"destination":r"C:\device-probe","options":["ro"]},
                {"source":self.results,"destination":RESULTS}], "windows":{"layerFolders":layers}});
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
        let expected = expected.map(|id| hex_api::DeviceHolder::Box(id.clone()));
        assert_eq!(
            entries[0].holder.as_ref(),
            expected.as_ref(),
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
        let name = format!("{phase}.json");
        let output = self.results.join(&name);
        assert!(!output.exists(), "old result could fake a probe");
        let mut program = Program::of(&[
            PROBE_EXE,
            "--ignored",
            "--exact",
            CHILD,
            "--nocapture",
            "--test-threads=1",
        ]);
        program.env = vec![
            ("HEX_WIN_PROBE_CHILD".into(), "1".into()),
            (
                "HEX_WIN_PROBE_MODE".into(),
                if self.inputs.foreign.is_some() {
                    "foreign"
                } else {
                    "single"
                }
                .into(),
            ),
            (
                "HEX_WIN_PROBE_SELECTED".into(),
                self.inputs.selected.path.clone(),
            ),
            ("HEX_WIN_PROBE_RESULT".into(), format!(r"{RESULTS}\{name}")),
        ];
        if let Some(other) = &self.inputs.foreign {
            program
                .env
                .push(("HEX_WIN_PROBE_FOREIGN".into(), other.path.clone()));
        }
        self.exec(id, program);
        // Same JSON shape with longest booleans/error numbers gives a path-derived byte bound;
        // no stdout/harness noise or pipe-draining worker is needed.
        let worst =
            |path: &str| json!({"path":path,"opened":false,"error":u32::MAX,"closed":false});
        let bound = serde_json::to_vec(&json!({"selected":worst(&self.inputs.selected.path),
            "foreign":self.inputs.foreign.as_ref().map(|other|worst(&other.path))}))
        .unwrap()
        .len();
        let value: Value = serde_json::from_slice(&bounded_read(&output, bound))
            .expect("actual probe output JSON");
        check_query(&value["selected"], &self.inputs.selected.path, granted);
        if let Some(other) = &self.inputs.foreign {
            check_query(&value["foreign"], &other.path, false);
            let control = query(&other.path);
            assert_eq!(
                control["opened"], true,
                "UNVERIFIED foreign GPU disappeared after denied child result {phase}: {control}"
            );
            check_query(&control, &other.path, true);
            eprintln!("EXACT_FOREIGN_HOST_AFTER phase={phase} result={control}");
        } else {
            assert!(value["foreign"].is_null());
        }
        eprintln!("EXACT_CHILD phase={phase} result={value}");
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
            let reopened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.open()));
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
                Err(error) => errors.push(format!("HCS absence unknown {}: {error:?}", owned.id)),
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
                                            u32::try_from(self.inputs.wait.as_millis()).unwrap(),
                                        ) {
                                            Ok(true) => {}
                                            other => errors.push(format!(
                                                "own keeper not reaped/unknown: {other:?}"
                                            )),
                                        }
                                    }
                                    Ok(_) => {} // PID reuse proves our exact keeper ended; never stop its successor.
                                    Err(error) => {
                                        errors.push(format!("own keeper identity unknown: {error}"))
                                    }
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
        errors
    }
}

fn exercise(lab: &mut Lab) {
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
    let cpu = lab.spec("cpu", false);
    let cpu_id = cpu.id.clone();
    lab.rt()
        .create(cpu)
        .expect("ungranted CPU/OCI control create");
    lab.rt().start(&cpu_id).unwrap();
    lab.probe(&cpu_id, "ungranted", false);
    lab.holder(None);
    let selected = lab.spec("selected", true);
    let selected_id = selected.id.clone();
    lab.rt()
        .create(selected)
        .expect("exact physical GPU create");
    lab.holder(Some(&selected_id));
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
    assert!(native_before.outlives && native_before.init.is_some());
    let before = lab.store().get(&selected_id).unwrap().unwrap().unwrap();
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
    lab.progress(&cpu_id, "after-overlap");
    lab.runtime.take(); // Same private keeper retains actual box jobs/systems across owner restart.
    lab.open();
    let recovered = lab
        .rt()
        .recover()
        .expect("recover exact recorded native bindings");
    assert!(
        recovered.removed.is_empty(),
        "recovery removed a surviving box: {recovered:?}"
    );
    assert!(recovered.adopted.contains(&selected_id) && recovered.adopted.contains(&cpu_id));
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
    lab.progress(&cpu_id, "after-restart");
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
    lab.progress(&cpu_id, "after-release");
    lab.rt()
        .delete(&cpu_id)
        .expect("delete independent CPU sibling");
}

fn parent(negative: bool) {
    let inputs = Inputs::read(negative); // Foreign inputs are mandatory only in the foreign GPU case.
    let mut lab = Lab::new(inputs);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(&mut lab)));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lab.cleanup()));
    let errors =
        cleanup.unwrap_or_else(|_| vec!["cleanup itself panicked; private lab retained".into()]);
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
    fs::remove_dir_all(&lab.root)
        .expect("remove only proven-empty owned lab, never borrowed layers/volume");
    eprintln!("OWNED_CLEANUP complete {}", lab.root.display());
}

/// Actual single-GPU positive/baseline/recovery witness; no second GPU or USB requirement.
#[test]
#[ignore = "ROOT-only actual HCS/GPU; required ROOT inputs/external guard; implementers compile only"]
fn root_windows_selected_gpu_assignment_survives_restart() {
    parent(false);
}

/// Actual second-GPU isolation negative. Missing actual second GPU => UNVERIFIED failure, not skip.
#[test]
#[ignore = "ROOT-only actual two-GPU exact-interface negative; second GPU/host-open controls mandatory"]
fn root_windows_foreign_gpu_remains_unreachable() {
    parent(true);
}
