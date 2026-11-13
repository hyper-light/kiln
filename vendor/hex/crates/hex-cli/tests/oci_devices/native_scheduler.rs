//! AC-T7.4.1: real scheduler/socket/native-CDI attachment, never a simulated runtime.
//! Explicitly ignored: ROOT admits the root/GPU lab and supplies finite workload bounds.

use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use hex_api::{Class, DeviceId, Exit, Mount, Network, OciSpec, Rootfs};
use hex_client::Client;
use hex_core::record::{BoxRecord, MAX_RECORD_BYTES, Phase};
use hex_core::{schedule as dto, wire};
use hex_node::scheduler::{Backend, Config, Service};
use hex_node::serve::{self, Server};

use super::native_devices::{NativeGpu, Vendor, discover, write_fixture};

const SELECTED: &str = "/dev/hex-selected-gpu";
const VISIBLE: [&str; 2] = ["/dev/hex-visible-gpu-0", "/dev/hex-visible-gpu-1"];

#[test]
#[ignore = "ROOT-only: root, delegated cgroup v2, NVIDIA + companion GPU, explicit HEX_NATIVE_DEVICE_BOUNDS"]
fn nvidia_scheduler_socket_cdi_binding_survives_restart() {
    native_case(Vendor::Nvidia);
}

#[test]
#[ignore = "ROOT-only: root, delegated cgroup v2, AMD + companion GPU, explicit HEX_NATIVE_DEVICE_BOUNDS"]
fn amd_scheduler_socket_cdi_binding_survives_restart() {
    native_case(Vendor::Amd);
}

/// The two sockets and all owners of them are stopped in dependency order, before reopening.
struct Running {
    control: Client,
    frontend: Server,
    service: Arc<Service>,
    backend: Client,
    server: Server,
    node: Arc<Node>,
}

impl Running {
    fn stop(self) -> Result<(), hex_api::Refusal> {
        let Self {
            control,
            frontend,
            service,
            backend,
            server,
            node,
        } = self;
        drop(control);
        drop(frontend);
        let result = service.shutdown();
        drop(service);
        drop(backend);
        drop(server);
        drop(node);
        result
    }
}

struct Fixture<'a> {
    lab: &'a Lab,
    gpus: Vec<NativeGpu>,
    bounds: Bounds,
    state: PathBuf,
    box_root: PathBuf,
    config: Config,
    running: Option<Running>,
    keys: Vec<dto::Key>,
    physical: Vec<BoxId>,
}

enum CleanupFailure {
    Control(hex_api::Refusal),
    DeleteRefused,
    Backend(hex_api::Refusal),
    RemainingBoxes,
    RemainingClaims,
    RemainingRecord,
    RemainingContainer,
    RemainingCgroup,
}

impl std::fmt::Debug for CleanupFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Control(error) => out.debug_tuple("Control").field(error).finish(),
            Self::Backend(error) => out.debug_tuple("Backend").field(error).finish(),
            Self::DeleteRefused => out.write_str("DeleteRefused"),
            Self::RemainingBoxes => out.write_str("RemainingBoxes"),
            Self::RemainingClaims => out.write_str("RemainingClaims"),
            Self::RemainingRecord => out.write_str("RemainingRecord"),
            Self::RemainingContainer => out.write_str("RemainingContainer"),
            Self::RemainingCgroup => out.write_str("RemainingCgroup"),
        }
    }
}

impl Fixture<'_> {
    fn start(&mut self) -> Vec<BoxId> {
        assert!(self.running.is_none(), "one native node/service owner");
        let node = Arc::new(self.lab.node(&self.box_root, &self.state));
        let recovered = node.recover().expect("ordinary native recovery");
        assert!(
            recovered.removed.is_empty(),
            "no silently removed native boxes: {recovered:?}"
        );
        let platform = node.sizes().platform();
        let backend_path = &self.config.backends[0].socket;
        let server = Server::bind(
            backend_path,
            node.clone(),
            serve::limits(&platform),
            platform.threads,
        )
        .expect("native backend socket");
        let backend = Client::connect(backend_path).expect("real backend client");
        let service = Service::open(&self.config, Arc::new(|_, _| {}))
            .expect("production scheduler over real backend socket");
        let frontend = Server::bind(
            &self.config.socket,
            service.clone(),
            serve::limits(&platform),
            platform.threads,
        )
        .expect("scheduler control socket");
        let control = Client::connect(&self.config.socket).expect("real scheduler client");
        self.running = Some(Running {
            control,
            frontend,
            service,
            backend,
            server,
            node,
        });
        recovered.adopted
    }

    fn up(&self) -> &Running {
        self.running.as_ref().expect("fixture is running")
    }

    fn stop(&mut self) {
        self.running
            .take()
            .expect("fixture is running")
            .stop()
            .expect("ordinary service stop");
    }

    fn spec(&self, name: &str, gpu: bool) -> BoxSpec {
        let class = if gpu {
            let mut mounts = vec![
                Mount {
                    source: PathBuf::from("/usr"),
                    destination: "/usr".into(),
                    read_only: true,
                },
                Mount {
                    source: PathBuf::from(&self.lab.exe),
                    destination: "/hex".into(),
                    read_only: true,
                },
            ];
            // Visibility alone grants no cgroup authority. Both real nodes are present in every
            // GPU box, independently of which exact identity the production picker selects.
            mounts.extend(
                self.gpus
                    .iter()
                    .zip(VISIBLE)
                    .map(|(gpu, destination)| Mount {
                        source: gpu.node.clone(),
                        destination: destination.into(),
                        read_only: true,
                    }),
            );
            ClassSpec::Oci(Box::new(OciSpec {
                rootfs: Rootfs::Dir(self.lab.rootfs.clone()),
                init: Program::of(&["/hex", "selftest-box", "wait"]),
                mounts,
                hostname: None,
                network: Network::Host,
                hosts: Vec::new(),
            }))
        } else {
            ClassSpec::Process
        };
        BoxSpec {
            route: None,
            allocation: None,
            id: BoxId::new(name).unwrap(),
            devices: Vec::new(),
            bounds: self.bounds.clone(),
            class,
            owner: None,
            band: 0,
        }
    }

    fn submit(&mut self, name: &str, model: Option<&str>) -> (dto::Key, BoxId) {
        let gpu = model.is_some();
        let mut demand = format!(
            "class:oci,cpus={}m,mem={},gpus={}",
            self.bounds.cpu_max_milli.unwrap(),
            self.bounds.memory_max.unwrap(),
            u8::from(gpu)
        );
        if let Some(model) = model {
            demand.push_str(",gpu.model==");
            demand.push_str(&model_word(model));
        }
        let reply = self
            .up()
            .control
            .schedule(dto::Command::Submit {
                demand: demand.clone(),
                payload: dto::Payload {
                    spec: self.spec(name, gpu),
                    start: gpu,
                },
            })
            .expect("real scheduler submit");
        // Keep every returned identity before asserting, so failures still take ordinary Delete.
        if let Some(key) = reply.key {
            self.keys.push(key);
        }
        if let Some(id) = &reply.physical {
            self.physical.push(id.clone());
        }
        self.receipt("submit", serde_json::json!({"demand":demand,"reply":reply}));
        assert_refusals_empty(&reply);
        assert!(
            reply.created && reply.tracked && !reply.ended,
            "actual synchronous creation: {reply:?}"
        );
        assert_eq!(reply.at, Some(0));
        let key = reply.key.expect("durable key");
        let id = reply.physical.expect("production physical identity");
        let record = self.record(&id);
        assert_eq!(record.spec.id, id);
        assert_ne!(
            id.as_str(),
            name,
            "caller name is not the placement identity"
        );
        assert_eq!(record.phase, Phase::Ready);
        assert_eq!(record.spec.devices.len(), usize::from(gpu));
        (key, id)
    }

    fn record_path(&self, id: &BoxId) -> PathBuf {
        let run = hex_node::records_dir(self.up().node.oci_root(), &self.state).unwrap();
        hex_node::store::runtime_record(&run, id)
    }

    fn record(&self, id: &BoxId) -> BoxRecord {
        let file = File::open(self.record_path(id)).expect("actual native BoxRecord");
        let mut bytes = Vec::new();
        file.take(u64::try_from(MAX_RECORD_BYTES).unwrap() + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        BoxRecord::decode(&bytes).expect("bounded native BoxRecord decode")
    }

    fn exec(&self, id: &BoxId, program: Program) -> String {
        let (out, writer) = std::io::pipe().unwrap();
        let mut proc = self
            .up()
            .backend
            .exec(
                id,
                program,
                Stdio {
                    stdout: Some(writer),
                    ..Stdio::null()
                },
            )
            .expect("actual native exec through backend socket");
        let end = hex_api::conformance::end_of(&mut proc).expect("native completion");
        assert_eq!(end, Exit::Code(0), "workload completed");
        // The fixed workload writes three short open receipts (or one CPU word); it cannot fill
        // its pipe. Wait first, then read under the existing wire byte bound, including one excess.
        let mut bytes = Vec::new();
        out.take(u64::try_from(wire::MAX_MESSAGE).unwrap() + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= wire::MAX_MESSAGE);
        String::from_utf8(bytes).unwrap()
    }

    fn opens(&self, id: &BoxId, selected: DeviceId) -> Vec<Open> {
        let index = self
            .gpus
            .iter()
            .position(|gpu| gpu.id == selected)
            .expect("selected offered identity");
        let other = if index == 0 { 1 } else { 0 };
        let text = self.exec(
            id,
            Program::of(&[
                "/hex",
                "selftest-box",
                "open",
                &format!("r:{SELECTED}"),
                &format!("r:{}", VISIBLE[index]),
                &format!("r:{}", VISIBLE[other]),
            ]),
        );
        self.receipt(
            "opens",
            serde_json::json!({"box":id,"selected":selected,"output":text}),
        );
        parse_opens(&text)
    }

    fn holders(&self) -> Vec<(DeviceId, Option<hex_api::DeviceHolder>)> {
        let capacity = self
            .up()
            .backend
            .capacity(Class::Oci)
            .unwrap()
            .expect("actual native capacity");
        assert_eq!(capacity.devices.len(), self.gpus.len());
        let mut holders: Vec<_> = capacity
            .devices
            .into_iter()
            .map(|gpu| (gpu.id, gpu.holder))
            .collect();
        holders.sort_by_key(|entry| entry.0);
        holders
    }

    fn receipt(&self, event: &str, value: serde_json::Value) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.lab.dir.path().join("native-scheduler.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::json!({"event":event,"value":value})).unwrap();
    }

    fn exercise(&mut self) {
        assert!(self.start().is_empty());
        assert!(self.holders().iter().all(|(_, holder)| holder.is_none()));
        let primary_model = self.gpus[0].model.clone();
        let (first_key, first) = self.submit("gpu-before", Some(&primary_model));
        let before = self.record(&first);
        let selected = before.spec.devices[0];
        let actual = self.gpus.iter().find(|gpu| gpu.id == selected).unwrap();
        assert_eq!(
            actual.vendor, self.gpus[0].vendor,
            "requested vendor selected by exact model"
        );
        assert_eq!(actual.model, primary_model);
        assert!(
            before.init.is_some(),
            "positive init PID/start-time witness"
        );
        let before_opens = self.opens(&first, selected);
        let (cpu_key, cpu) = self.submit("cpu-sibling", None);
        assert_eq!(
            self.exec(
                &cpu,
                Program::of(&[&self.lab.exe, "selftest-box", "say", "cpu-before"])
            ),
            "cpu-before\n"
        );
        self.receipt(
            "before-restart",
            serde_json::json!({"key":first_key,"record":before,"holders":self.holders()}),
        );
        self.stop();
        // Change only the order of the same private administrator rows, while no catalog owns it.
        let catalog = self.state.join("devices");
        let text = fs::read_to_string(&catalog).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("hex-devices-1"));
        let rows: Vec<_> = lines.collect();
        assert_eq!(rows.len(), self.gpus.len());
        let reordered = format!(
            "hex-devices-1\n{}\n",
            rows.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
        hex_runtime::devices::Catalog::parse(reordered.as_bytes()).unwrap();
        fs::write(&catalog, reordered).unwrap();
        let adopted = self.start();
        assert_eq!(adopted.len(), 2, "both existing physical boxes adopted");
        assert!(adopted.contains(&first) && adopted.contains(&cpu));
        let status = self
            .up()
            .control
            .schedule(dto::Command::Status { key: first_key })
            .unwrap();
        assert_refusals_empty(&status);
        assert_eq!(status.key, Some(first_key));
        assert_eq!(status.physical.as_ref(), Some(&first));
        assert!(status.created && !status.ended);
        let after = self.record(&first);
        assert_eq!(
            after.spec, before.spec,
            "exact durable binding, not a new pick"
        );
        assert_eq!(
            after.init, before.init,
            "same live init PID AND start ticks: no second create"
        );
        assert!(self.up().backend.inspect(&first).unwrap().adopted);
        let after_opens = self.opens(&first, selected);
        assert_eq!(
            self.exec(
                &cpu,
                Program::of(&[&self.lab.exe, "selftest-box", "say", "cpu-after"])
            ),
            "cpu-after\n"
        );
        let cpu_status = self
            .up()
            .control
            .schedule(dto::Command::Status { key: cpu_key })
            .unwrap();
        assert_refusals_empty(&cpu_status);
        assert_eq!(cpu_status.physical.as_ref(), Some(&cpu));
        let companion_model = self
            .gpus
            .iter()
            .find(|gpu| gpu.id != selected)
            .unwrap()
            .model
            .clone();
        let (fresh_key, fresh) = self.submit("gpu-after", Some(&companion_model));
        let fresh_record = self.record(&fresh);
        let other = fresh_record.spec.devices[0];
        assert_ne!(other, selected, "new work uses the remaining physical GPU");
        let fresh_opens = self.opens(&fresh, other);
        let holders = self.holders();
        assert!(holders.contains(&(selected, Some(hex_api::DeviceHolder::Box(first.clone())))));
        assert!(holders.contains(&(other, Some(hex_api::DeviceHolder::Box(fresh.clone())))));
        self.receipt(
            "after-restart",
            serde_json::json!({"first_key":first_key,"fresh_key":fresh_key,
            "record":after,"fresh_record":fresh_record,"holders":holders}),
        );
        // Keep forbidden-open observations until after all progress/cleanup actions have a chance.
        for observed in [before_opens, after_opens, fresh_opens] {
            assert_eq!(
                observed,
                [Opened, Opened, Refused],
                "selected CDI + visible node open; other visible GPU EPERM"
            );
        }
    }

    fn cleanup(&mut self) -> Vec<CleanupFailure> {
        let mut errors = Vec::new();
        if let Some(up) = &self.running {
            for key in &self.keys {
                match up.control.schedule(dto::Command::Delete { key: *key }) {
                    Ok(reply) if reply.refusals.is_empty() && reply.ended => {
                        self.receipt("delete", serde_json::json!({"key":key,"reply":reply}));
                    }
                    Ok(_) => errors.push(CleanupFailure::DeleteRefused),
                    Err(error) => errors.push(CleanupFailure::Control(error)),
                }
            }
        }
        // Failed submissions may have made a native record before returning a key. The private
        // Node is the sole authority for those leftovers; ordinary list/delete handles them too.
        let fallback;
        let node = match &self.running {
            Some(up) => &*up.node,
            None => {
                fallback = self.lab.node(&self.box_root, &self.state);
                if let Err(error) = fallback.recover() {
                    errors.push(CleanupFailure::Backend(error));
                }
                &fallback
            }
        };
        match node.list() {
            Ok(boxes) => {
                for info in boxes {
                    // Fallback deletion is cleanup, never evidence that Service Delete worked.
                    errors.push(CleanupFailure::RemainingBoxes);
                    if !self.physical.contains(&info.id) {
                        self.physical.push(info.id.clone());
                    }
                    if let Err(error) = node.delete(&info.id) {
                        errors.push(CleanupFailure::Backend(error));
                    }
                }
            }
            Err(error) => errors.push(CleanupFailure::Backend(error)),
        }
        match node.list() {
            Ok(boxes) if boxes.is_empty() => {}
            Ok(_) => errors.push(CleanupFailure::RemainingBoxes),
            Err(error) => errors.push(CleanupFailure::Backend(error)),
        }
        match node.capacity(Class::Oci) {
            Ok(Some(capacity)) if capacity.devices.iter().all(|gpu| gpu.holder.is_none()) => {}
            Ok(_) => errors.push(CleanupFailure::RemainingClaims),
            Err(error) => errors.push(CleanupFailure::Backend(error)),
        }
        let run = hex_node::records_dir(node.oci_root(), &self.state).unwrap();
        for id in &self.physical {
            if hex_node::store::runtime_record(&run, id).exists()
                || self.state.join(format!("{id}.json")).exists()
            {
                errors.push(CleanupFailure::RemainingRecord);
            }
            if node.container_dir(id).exists() {
                errors.push(CleanupFailure::RemainingContainer);
            }
            if node.cgroup_dir(id).is_some() {
                errors.push(CleanupFailure::RemainingCgroup);
            }
        }
        self.receipt(
            "cleanup",
            serde_json::json!({"known_keys":self.keys,"physical":self.physical,
            "failure_count":errors.len()}),
        );
        errors
    }
}

fn assert_refusals_empty(reply: &dto::Reply) {
    // Decode the real typed wire refusals. Never classify native outcomes by display text.
    let decoded: Vec<_> = reply
        .refusals
        .iter()
        .map(|bytes| {
            let received = wire::Received {
                bytes,
                size: bytes.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            let (_, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
            answer
        })
        .collect();
    assert!(
        decoded.is_empty(),
        "scheduler operation refused: {decoded:?}"
    );
}

fn model_word(model: &str) -> String {
    // needs::parse splits at commas/spaces and its existing model_name accepts underscores
    // between words. Encode only token separators; production still resolves/matches the model.
    assert!(!model.is_empty() && !model.contains([',', '\0']));
    model
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect()
}

fn native_case(vendor: Vendor) {
    assert_eq!(
        host_id("-u"),
        0,
        "explicitly selected native fixture requires ROOT's root lab"
    );
    let bounds: Bounds = serde_json::from_slice(
        &std::env::var("HEX_NATIVE_DEVICE_BOUNDS")
            .expect("ROOT supplies finite admitted Bounds JSON, no guessed workload defaults")
            .into_bytes(),
    )
    .unwrap();
    bounds.check().unwrap();
    assert!(bounds.cpu_max_milli.is_some() && bounds.memory_max.is_some());
    let lab = Lab::new(&cache_dir(), env!("CARGO_BIN_EXE_hex"));
    let state = lab.dir.path().join("node");
    fs::create_dir(&state).unwrap();
    let mut discovered =
        discover(&state).expect("actual GPU discovery; unavailable is unverified, never a pass");
    assert!(
        discovered.len() >= VISIBLE.len(),
        "requested vendor plus any other verified physical GPU required"
    );
    let primary = discovered
        .iter()
        .position(|gpu| gpu.vendor == vendor)
        .expect("requested vendor present in actual discovery");
    let primary = discovered.remove(primary);
    let companion = discovered
        .into_iter()
        .next()
        .expect("another verified physical GPU");
    let gpus = vec![primary, companion];
    assert_ne!(gpus[0].id, gpus[1].id);
    assert_ne!(
        (gpus[0].major, gpus[0].minor),
        (gpus[1].major, gpus[1].minor)
    );
    let files = write_fixture(&state, &gpus).expect("independently verified private CDI/catalog");
    assert_eq!(files.catalog, state.join("devices"));
    assert!(files.cdi.is_file());
    let own =
        prepare_cgroup(lab.dir.path()).expect("ROOT delegates every required cgroup controller");
    let box_root = own.join(lab.dir.path().file_name().unwrap());
    assert!(!box_root.exists(), "never adopt a preexisting cgroup scope");
    let config = Config {
        cluster: format!(
            "native-device-{}",
            lab.dir.path().file_name().unwrap().to_str().unwrap()
        ),
        node: 0,
        state: lab.dir.path().join("scheduler"),
        socket: lab.dir.path().join("s.sock"),
        nodes: Vec::new(),
        models: Vec::new(),
        backends: vec![Backend {
            class: "oci".into(),
            socket: lab.dir.path().join("n.sock"),
        }],
        peers: Vec::new(),
        capacity: None,
    };
    let mut fixture = Fixture {
        lab: &lab,
        gpus,
        bounds,
        state,
        box_root: box_root.clone(),
        config,
        running: None,
        keys: Vec::new(),
        physical: Vec::new(),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| fixture.exercise()));
    let cleanup = catch_unwind(AssertUnwindSafe(|| fixture.cleanup()));
    let stop = catch_unwind(AssertUnwindSafe(|| {
        fixture.running.take().map(Running::stop).transpose()
    }));
    drop(fixture);
    let clean = matches!(&cleanup, Ok(errors) if errors.is_empty()) && matches!(&stop, Ok(Ok(_)));
    if !clean || outcome.is_err() {
        let kept = lab.dir.keep();
        panic!(
            "native fixture failed; body_ok={}, cleanup={cleanup:?}, stop={stop:?}; private evidence/state retained at {}",
            outcome.is_ok(),
            kept.display()
        );
    }
    let kept = lab.dir.keep();
    sweep(&box_root);
    eprintln!(
        "native fixture receipts/state retained at {}",
        kept.display()
    );
    assert!(
        !box_root.exists(),
        "owned cgroup root removed after ordinary deletion"
    );
}
