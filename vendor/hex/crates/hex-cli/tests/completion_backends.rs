//! AC-T7.3.1/4.1: ROOT-only actual typed backend/socket/checked-GPU/recovery fixture.
//! No test self-enables hardware. Missing hardware/configuration is an explicit failure.
//! The external ROOT runner bounds the whole process, including native cleanup uncertainty.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as HostStdio};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hex_api::{
    BackingPhase, BoxId, BoxSpec, Class, DeviceHolder, DeviceId, Exit, Program, Runtime, Stdio,
};
use hex_client::Client;
use hex_core::record::{BoxRecord, Phase};
use hex_core::schedule::{self as dto, Payload};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name]
        .as_str()
        .unwrap_or_else(|| panic!("UNVERIFIED missing text field {name}"))
}

fn count(value: &Value, name: &str) -> u64 {
    value[name]
        .as_u64()
        .unwrap_or_else(|| panic!("UNVERIFIED missing unsigned field {name}"))
}

fn bounded(path: &Path, budget: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(budget.checked_add(1).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        u64::try_from(bytes.len()).unwrap() <= budget,
        "artifact exceeded caller budget"
    );
    bytes
}

fn digest(path: &Path) -> String {
    let mut file = File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let size = file.read(&mut buffer).unwrap();
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    format!("{:x}", digest.finalize())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn journal(value: Value) {
    let path = std::env::var_os("HEX_COMPLETION_PID_JOURNAL").expect("ROOT PID journal required");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{value}").unwrap();
    file.sync_all().unwrap();
}

fn last_json(bytes: &[u8]) -> Value {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("native witness produced no JSON result")
}

#[derive(Debug)]
enum OutputEnd {
    Eof,
    Limit,
    Deadline,
    Read(String),
    Retain(String),
}

impl OutputEnd {
    fn receipt(&self) -> Value {
        match self {
            Self::Eof => json!({"state":"eof"}),
            Self::Limit => json!({"state":"limit"}),
            Self::Deadline => json!({"state":"deadline"}),
            Self::Read(error) => json!({"state":"read-error","error":error}),
            Self::Retain(error) => json!({"state":"retain-error","error":error}),
        }
    }
}

struct OutputRead {
    bytes: Vec<u8>,
    end: OutputEnd,
    at: Instant,
}

fn read_output(
    mut reader: std::io::PipeReader,
    mut retained: File,
    limit: usize,
    until: Option<Instant>,
) -> OutputRead {
    let mut bytes = Vec::new();
    let mut buffer = Vec::new();
    let prepared = bytes.try_reserve_exact(limit).and_then(|()| {
        buffer.try_reserve_exact(
            limit
                .checked_add(1)
                .expect("output bound checked before exec"),
        )
    });
    if let Err(error) = prepared {
        return OutputRead {
            bytes,
            end: OutputEnd::Read(format!("output allocation: {error}")),
            at: Instant::now(),
        };
    }
    buffer.resize(limit + 1, 0);
    let mut end = loop {
        if until.is_some_and(|until| Instant::now() >= until) {
            break OutputEnd::Deadline;
        }
        // Read one byte beyond the retained prefix to distinguish a full budget
        // from EOF. A bounded Take reader's zero would not establish pipe EOF.
        let room = limit - bytes.len();
        match reader.read(&mut buffer[..room + 1]) {
            Ok(0) => break OutputEnd::Eof,
            Ok(size) => {
                let keep = size.min(room);
                bytes.extend_from_slice(&buffer[..keep]);
                if let Err(error) = retained.write_all(&buffer[..keep]) {
                    break OutputEnd::Retain(error.to_string());
                }
                if size > room {
                    break OutputEnd::Limit;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => break OutputEnd::Read(error.to_string()),
        }
    };
    if let Err(error) = retained.sync_all() {
        end = OutputEnd::Retain(format!("{end:?}; sync: {error}"));
    }
    // The terminal notification is sent only after this function has closed
    // both owned handles; a notified worker has no remaining blocking IO.
    OutputRead {
        bytes,
        end,
        at: Instant::now(),
    }
}

type CreatingRecords = Vec<(BoxId, Vec<u8>)>;

fn capture_creating(
    root: &Path,
    class: Class,
    entries: u64,
    bytes: u64,
    captured: &mut CreatingRecords,
) -> Result<(), String> {
    let mut inspected = 0_u64;
    let mut retained = captured.iter().try_fold(0_u64, |sum, (_, raw)| {
        sum.checked_add(u64::try_from(raw.len()).map_err(|error| error.to_string())?)
            .ok_or_else(|| "Creating byte overflow".to_owned())
    })?;
    // Canonical Store layout: root/shard/id.json. No recursion or Store writes.
    for shard in fs::read_dir(root).map_err(|error| error.to_string())? {
        inspected = inspected.checked_add(1).ok_or("record count overflow")?;
        if inspected > entries {
            return Err("Creating directory budget exceeded".into());
        }
        let shard = shard.map_err(|error| error.to_string())?;
        if !shard
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        for file in fs::read_dir(shard.path()).map_err(|error| error.to_string())? {
            inspected = inspected.checked_add(1).ok_or("record count overflow")?;
            if inspected > entries {
                return Err("Creating file budget exceeded".into());
            }
            let file = file.map_err(|error| error.to_string())?;
            if !file
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                continue;
            }
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.to_string()),
            };
            let mut raw = Vec::new();
            file.take(bytes.checked_add(1).ok_or("Creating byte bound overflow")?)
                .read_to_end(&mut raw)
                .map_err(|error| error.to_string())?;
            if u64::try_from(raw.len()).map_err(|error| error.to_string())? > bytes {
                return Err("Creating record exceeds byte budget".into());
            }
            let record = BoxRecord::decode(&raw)?;
            if record.phase != Phase::Creating
                || record.init.is_some()
                || record.spec.class.class() != class
                || record.native_scope.is_none()
                || hex_node::store::runtime_record(root, &record.spec.id) != path
                || captured.iter().any(|(id, _)| id == &record.spec.id)
            {
                continue;
            }
            retained = retained
                .checked_add(u64::try_from(raw.len()).map_err(|error| error.to_string())?)
                .ok_or("retained Creating bytes overflow")?;
            if retained > bytes {
                return Err("retained Creating records exceed byte budget".into());
            }
            captured.push((record.spec.id, raw));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct CreatingChanges {
    fd: std::os::fd::OwnedFd,
    watched: std::collections::BTreeSet<PathBuf>,
    buffer: Vec<std::mem::MaybeUninit<u8>>,
}

#[cfg(target_os = "linux")]
impl CreatingChanges {
    fn new(bytes: u64) -> Result<Self, String> {
        use rustix::fs::inotify;
        Ok(Self {
            fd: inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
                .map_err(|error| error.to_string())?,
            watched: Default::default(),
            buffer: vec![
                std::mem::MaybeUninit::uninit();
                usize::try_from(bytes).map_err(|error| error.to_string())?
            ],
        })
    }
    fn refresh(&mut self, root: &Path, limit: u64) -> Result<(), String> {
        use rustix::fs::inotify;
        let mut paths = vec![root.to_path_buf()];
        let mut count = 0_u64;
        for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
            count = count
                .checked_add(1)
                .ok_or("Creating watch count overflow")?;
            if count > limit {
                return Err("Creating watch entry budget exceeded".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                paths.push(entry.path());
            }
        }
        for path in paths {
            if !self.watched.contains(&path) {
                if u64::try_from(self.watched.len()).map_err(|error| error.to_string())? >= limit {
                    return Err("Creating watch budget exceeded".into());
                }
                inotify::add_watch(
                    &self.fd,
                    &path,
                    inotify::WatchFlags::CREATE
                        | inotify::WatchFlags::MOVED_TO
                        | inotify::WatchFlags::DELETE_SELF
                        | inotify::WatchFlags::MOVE_SELF,
                )
                .map_err(|error| error.to_string())?;
                self.watched.insert(path);
            }
        }
        Ok(())
    }
    fn changed(
        &mut self,
        wake: &std::io::PipeReader,
        until: Instant,
        limit: u64,
    ) -> Result<bool, String> {
        use rustix::event::{PollFd, PollFlags, Timespec};
        use rustix::fs::inotify;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let timeout = Timespec {
                tv_sec: i64::try_from(left.as_secs()).map_err(|error| error.to_string())?,
                tv_nsec: i64::from(left.subsec_nanos()),
            };
            let mut fds = [
                PollFd::new(&self.fd, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN | PollFlags::HUP),
            ];
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.to_string()),
                Ok(0) => return Ok(false),
                Ok(_) => {}
            }
            if fds.get(1).is_some_and(|fd| !fd.revents().is_empty()) {
                return Ok(false);
            }
            let mut reader = inotify::Reader::new(&self.fd, &mut self.buffer);
            let mut count = 0_u64;
            loop {
                match reader.next() {
                    Ok(event) => {
                        count = count
                            .checked_add(1)
                            .ok_or("Creating event count overflow")?;
                        if count > limit
                            || event.events().intersects(
                                inotify::ReadFlags::QUEUE_OVERFLOW
                                    | inotify::ReadFlags::DELETE_SELF
                                    | inotify::ReadFlags::MOVE_SELF
                                    | inotify::ReadFlags::UNMOUNT
                                    | inotify::ReadFlags::IGNORED,
                            )
                        {
                            return Err(
                                "Creating notification overflow/directory uncertainty".into()
                            );
                        }
                    }
                    Err(rustix::io::Errno::AGAIN) => return Ok(count > 0),
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
}

struct CreatingBounds {
    wait: Duration,
    entries: u64,
    events: u64,
    bytes: u64,
    notification_bytes: u64,
}

struct CreatingObserver {
    stop: Arc<std::sync::atomic::AtomicBool>,
    ready: mpsc::Receiver<()>,
    wake: Option<std::io::PipeWriter>,
    child: Option<Child>,
    worker: Option<JoinHandle<Result<CreatingRecords, String>>>,
}

impl CreatingObserver {
    fn start(
        root: PathBuf,
        class: Class,
        bounds: CreatingBounds,
        external: Option<Value>,
        scope: &Path,
    ) -> Result<Self, String> {
        let CreatingBounds {
            wait,
            entries,
            events,
            bytes,
            notification_bytes,
        } = bounds;
        let until = Instant::now()
            .checked_add(wait)
            .ok_or("Creating observer deadline overflow")?;
        if wait.is_zero() || entries == 0 || events == 0 || bytes == 0 || notification_bytes == 0 {
            return Err("explicit positive Creating observer bounds required".into());
        }
        if !root.is_dir() || root.is_symlink() {
            return Err("private Store root unavailable".into());
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready, armed) = mpsc::sync_channel(1);
        #[cfg(target_os = "linux")]
        {
            let _ = (external, scope);
            let signal = Arc::clone(&stop);
            let (wake_reader, wake) = std::io::pipe().map_err(|error| error.to_string())?;
            // Same enabled safe inotify API as tests/oci_reuse.rs, ONE instance
            // for root+all shards. No periodic scan or extra native provider.
            let worker = std::thread::Builder::new()
                .name("completion-creating-observer".into())
                .spawn(move || {
                    let mut changes = CreatingChanges::new(notification_bytes)?;
                    changes.refresh(&root, entries)?;
                    let mut captured = Vec::new();
                    ready.send(()).map_err(|error| error.to_string())?;
                    loop {
                        capture_creating(&root, class, entries, bytes, &mut captured)?;
                        if signal.load(std::sync::atomic::Ordering::Acquire)
                            || !changes.changed(&wake_reader, until, events)?
                        {
                            return Ok(captured);
                        }
                        changes.refresh(&root, entries)?;
                    }
                })
                .map_err(|error| error.to_string())?;
            Ok(Self {
                stop,
                ready: armed,
                wake: Some(wake),
                child: None,
                worker: Some(worker),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (until, events, notification_bytes);
            let input =
                external.ok_or("required pinned filesystem notification program missing")?;
            let path = PathBuf::from(text(&input, "path"));
            if digest(&path) != text(&input, "sha256") {
                return Err("Creating notification executable pin changed".into());
            }
            let args: Vec<_> = input["args"]
                .as_array()
                .ok_or("notification argv missing")?
                .iter()
                .map(|value| value.as_str().ok_or("notification arg is not text"))
                .collect::<Result<_, _>>()?;
            let mut child = Command::new(&path)
                .args(&args)
                .stdin(HostStdio::null())
                .stdout(HostStdio::piped())
                .stderr(
                    File::create(scope.join("creating-notifications.stderr"))
                        .map_err(|error| error.to_string())?,
                )
                .spawn()
                .map_err(|error| error.to_string())?;
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    let cleanup = child.wait();
                    return Err(format!("notification stdout missing;cleanup:{cleanup:?}"));
                }
            };
            let worker = std::thread::Builder::new()
                .name("completion-creating-observer".into())
                .spawn(move || {
                    let mut source = BufReader::new(
                        stdout.take(
                            bytes
                                .checked_add(1)
                                .ok_or("notification byte budget overflow")?,
                        ),
                    );
                    let mut captured = Vec::new();
                    let mut total = 0_u64;
                    loop {
                        let mut line = String::new();
                        let count = source
                            .read_line(&mut line)
                            .map_err(|error| error.to_string())?;
                        if count == 0 {
                            return Ok(captured);
                        }
                        total = total
                            .checked_add(u64::try_from(count).map_err(|error| error.to_string())?)
                            .ok_or("notification bytes overflow")?;
                        if total > bytes {
                            return Err("notification output byte budget exceeded".into());
                        }
                        match line.trim_end() {
                            "hex-completion-creating-ready" => {
                                ready.send(()).map_err(|error| error.to_string())?;
                            }
                            "hex-completion-creating-changed" => {}
                            _ => return Err("unexpected Creating notification protocol".into()),
                        }
                        capture_creating(&root, class, entries, bytes, &mut captured)?;
                    }
                });
            match worker {
                Ok(worker) => Ok(Self {
                    stop,
                    ready: armed,
                    wake: None,
                    child: Some(child),
                    worker: Some(worker),
                }),
                Err(error) => {
                    let _ = child.kill();
                    let cleanup = child.wait();
                    Err(format!("notification worker:{error};cleanup:{cleanup:?}"))
                }
            }
        }
    }

    fn announce(&self) {
        if let Some(child) = &self.child {
            journal(json!({"event":"spawn","role":"creating-notifications","pid":child.id()}));
        }
    }

    fn finish(&mut self) -> Result<CreatingRecords, String> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let mut uncertainty = None;
        let mut reaped = None;
        if let Some(wake) = &mut self.wake
            && let Err(error) = wake.write_all(b"x")
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            uncertainty = Some(error.to_string());
        }
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = child.kill() {
                        uncertainty = Some(error.to_string());
                    }
                }
                Err(error) => {
                    uncertainty = Some(error.to_string());
                    let _ = child.kill();
                }
            }
            match child.wait() {
                Ok(status) => {
                    reaped = Some(
                        json!({"event":"reaped","role":"creating-notifications","pid":child.id(),"status":status.code()}),
                    )
                }
                Err(error) => uncertainty = Some(error.to_string()),
            }
        }
        let joined = match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| "Creating observer worker panicked".to_owned())
                .and_then(|result| result),
            None => Ok(Vec::new()),
        };
        if let Some(reaped) = reaped {
            journal(reaped);
        }
        if let Some(error) = uncertainty {
            return Err(error);
        }
        joined
    }
}

impl Drop for CreatingObserver {
    fn drop(&mut self) {
        if (self.worker.is_some() || self.child.is_some())
            && let Err(error) = self.finish()
        {
            eprintln!("UNVERIFIED Creating observer drop:{error}");
        }
    }
}

struct Service {
    name: String,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
}

impl Service {
    fn start(row: &Value, hex: &Path, scope: &Path, wait: Duration, budget: u64) -> Self {
        let name = text(row, "name").to_owned();
        let ready = text(row, "ready").to_owned();
        let arguments: Vec<_> = row["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let mut command = Command::new(hex);
        command
            .args(&arguments)
            .stdin(HostStdio::null())
            .stdout(HostStdio::piped())
            .stderr(File::create(scope.join(format!("{name}.stderr"))).unwrap());
        for (name, value) in row["env"].as_object().unwrap() {
            command.env(name, value.as_str().unwrap());
        }
        let mut child = command.spawn().expect("actual backend/scheduler child");
        journal(
            json!({"event":"spawn","role":name,"pid":child.id(),"executable":hex,"args":arguments}),
        );
        let stdout = child.stdout.take().unwrap();
        let path = scope.join(format!("{name}.stdout"));
        let (sent, received) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut log = File::create(path).unwrap();
            let mut source = BufReader::new(stdout.take(budget.checked_add(1).unwrap()));
            let mut total = 0_u64;
            let mut announced = false;
            loop {
                let mut line = String::new();
                let size = source.read_line(&mut line).unwrap();
                if size == 0 {
                    break;
                }
                total += u64::try_from(size).unwrap();
                assert!(total <= budget, "service output exceeded admitted bound");
                log.write_all(line.as_bytes()).unwrap();
                if !announced && line.trim_end() == ready {
                    let _ = sent.send(());
                    announced = true;
                }
            }
        });
        let mut service = Self {
            name,
            child: Some(child),
            reader: Some(reader),
        };
        if received.recv_timeout(wait).is_err() {
            service.stop();
            panic!("UNVERIFIED actual service did not announce readiness");
        }
        service
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let _ = child.kill(); // Exact Child handle only, never process-name discovery.
            let status = child
                .wait()
                .expect("owned service reaped; external ROOT deadline bounds OS wait");
            journal(json!({"event":"reaped","role":self.name,"pid":pid,"status":status.code()}));
        }
        if let Some(reader) = self.reader.take() {
            reader.join().expect("owned output reader joined");
        }
    }

    fn finish(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let status = child
                .wait()
                .expect("finite workload; external ROOT guard bounds OS wait");
            journal(
                json!({"event":"reaped","role":self.name,"pid":pid,"status":status.code(),"natural":true}),
            );
            assert!(status.success(), "owned native workload failed");
        }
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Lab {
    input: Value,
    backend: String,
    topology: String,
    scope: PathBuf,
    services: Vec<Service>,
    events: Vec<Value>,
    keys: Vec<dto::Key>,
    boxes: Vec<BoxId>,
    wait: Duration,
    budget: u64,
    creating: Option<CreatingObserver>,
    scope_observations: Vec<Value>,
}

impl Lab {
    fn new(backend: &str, topology: &str, proof: &str) -> Self {
        assert_eq!(
            std::env::var("HEX_COMPLETION_ROOT_AUTHORIZED").as_deref(),
            Ok("1"),
            "UNVERIFIED fixture is ROOT-only"
        );
        let config = PathBuf::from(std::env::var_os("HEX_COMPLETION_CONFIG").expect("ROOT config"));
        // The existing wire ceiling also bounds this test's configuration.
        let bytes = bounded(&config, u64::try_from(hex_core::wire::MAX_MESSAGE).unwrap());
        let raw: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(text(&raw, "backend"), backend);
        assert_eq!(text(&raw, "topology"), topology);
        assert_eq!(text(&raw, "proof"), proof);
        let wait_ms = count(&raw, "wait_ms");
        let budget = count(&raw, "byte_limit");
        assert!(wait_ms > 0 && budget > 0);
        let scratch = Path::new(text(&raw, "scratch"));
        let scope = tempfile::Builder::new()
            .prefix("hex-completion-")
            .tempdir_in(scratch)
            .unwrap()
            .keep();
        journal(json!({"event":"scope","path":scope,"backend":backend,"topology":topology}));
        let escaped = serde_json::to_string(scope.to_str().unwrap()).unwrap();
        let escaped = escaped
            .strip_prefix('"')
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        let expanded = String::from_utf8(bytes)
            .unwrap()
            .replace("@SCOPE@", escaped);
        let input: Value = serde_json::from_str(&expanded).unwrap();
        for file in input["files"].as_array().unwrap() {
            let path = Path::new(text(file, "path"));
            assert!(
                path.starts_with(&scope),
                "fixture may write only its own new scope"
            );
            assert!(
                !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
                "fixture path cannot escape through parent components"
            );
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if let Some(source) = file["copy_from"].as_str() {
                assert_eq!(
                    digest(Path::new(source)),
                    text(file, "sha256"),
                    "source-pinned fixture helper"
                );
                assert!(
                    fs::metadata(source).unwrap().len() <= budget,
                    "copied helper exceeds admitted byte bound"
                );
                fs::copy(source, path).unwrap();
                assert_eq!(digest(path), text(file, "sha256"));
            } else {
                let bytes = if let Some(contents) = file["text"].as_str() {
                    contents.as_bytes().to_vec()
                } else {
                    serde_json::to_vec_pretty(&file["json"]).unwrap()
                };
                assert!(u64::try_from(bytes.len()).unwrap() <= budget);
                fs::write(path, bytes).unwrap();
            }
        }
        Self {
            input,
            backend: backend.into(),
            topology: topology.into(),
            scope,
            services: Vec::new(),
            events: Vec::new(),
            keys: Vec::new(),
            boxes: Vec::new(),
            wait: Duration::from_millis(wait_ms),
            budget,
            creating: None,
            scope_observations: Vec::new(),
        }
    }

    fn class(&self) -> Class {
        serde_json::from_value(json!(self.backend))
            .expect("UNVERIFIED typed production Runtime backend not registered")
    }

    fn start_all(&mut self) {
        let hex = Path::new(text(&self.input["hex"], "path"));
        assert_eq!(
            digest(hex),
            text(&self.input["hex"], "sha256"),
            "source-pinned Hex executable"
        );
        for row in self.input["services"].as_array().unwrap() {
            self.services.push(Service::start(
                row,
                hex,
                &self.scope,
                self.wait,
                self.budget,
            ));
        }
        assert!(!self.services.is_empty());
    }

    fn restart(&mut self, role: &str) {
        let index = self
            .services
            .iter()
            .position(|service| service.name == role)
            .expect("owned service role");
        self.services[index].stop();
        let row = self.input["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| text(row, "name") == role)
            .unwrap();
        self.services[index] = Service::start(
            row,
            Path::new(text(&self.input["hex"], "path")),
            &self.scope,
            self.wait,
            self.budget,
        );
    }

    fn backend(&self) -> Client {
        Client::connect(Path::new(text(&self.input, "backend_socket"))).unwrap()
    }
    fn control(&self) -> Client {
        Client::connect(Path::new(text(&self.input, "scheduler_socket"))).unwrap()
    }

    fn resource_snapshot(&self, phase: &str) -> Value {
        let backend = self.backend();
        let capacity = backend
            .capacity(self.class())
            .unwrap()
            .expect("initialized actual resource pool");
        capacity.check().unwrap();
        let physical = backend
            .physical_report()
            .expect("same native authority over existing Runtime RPC");
        physical.check().unwrap();
        let boxes: Vec<_> = backend
            .list()
            .unwrap()
            .into_iter()
            .map(|row| {
                json!({
                    "id":row.id,"class":row.class,"bounds":row.bounds,"allocation":row.allocation,
                })
            })
            .collect();
        let devices: Vec<_> = capacity
            .devices
            .iter()
            .map(|row| json!({"id":row.id,"holder":row.holder}))
            .collect();
        let native = if self
            .input
            .get("native_resources")
            .is_some_and(Value::is_object)
        {
            self.host("native_resources", &[("@PHASE@", phase.into())])
        } else {
            json!({"observed":false,"reason":"native driver/whole-scope terminal producer contract not yet supplied"})
        };
        let observed = native["observed"] == json!(true);
        if observed {
            assert_eq!(native["schema"], "hex-completion-native-resources-2");
            assert_eq!(native["source"], self.input["source"]);
            assert_eq!(native["backend"], self.backend);
            assert_eq!(native["phase"], phase);
            assert_eq!(native["authority"], physical.authority);
        }
        json!({"schema":"hex-completion-resources-2","scope":"root-native-resources",
            "source":self.input["source"],"backend":self.backend,"phase":phase,
            "observed":observed,"api_observed":true,"authority":physical.authority,"pool":capacity.id,
            "total":capacity.total,"free":capacity.free,"boxes":boxes,"devices":devices,
            "physical_report":physical,"owned_boxes":self.boxes,"scope_history":self.scope_observations,
            "driver_bindings":native["driver_bindings"],"terminal_scopes":native["terminal_scopes"],
            "owned_native_processes":native["owned_native_processes"],"owned_artifacts":native["owned_artifacts"],
            "native_observation":native})
    }

    fn write_resources(&self, phase: &str) {
        let variable = match phase {
            "before" => "HEX_COMPLETION_RESOURCES_BEFORE",
            "after" => "HEX_COMPLETION_RESOURCES_AFTER",
            _ => panic!("unknown resource observation phase"),
        };
        let path = PathBuf::from(
            std::env::var_os(variable).expect("retained resource observation output"),
        );
        fs::write(
            path,
            serde_json::to_vec_pretty(&self.resource_snapshot(phase)).unwrap(),
        )
        .unwrap();
    }

    fn emit(&mut self, stage: &str, mut value: Value) {
        value["stage"] = json!(stage);
        value["backend"] = json!(self.backend);
        value["topology"] = json!(self.topology);
        value["outcome"] = json!("verified");
        self.events.push(value);
        let path = std::env::var_os("HEX_COMPLETION_EVENTS").expect("ROOT event path");
        fs::write(path, serde_json::to_vec_pretty(&self.events).unwrap()).unwrap();
    }

    fn record(&self, id: &BoxId) -> Value {
        let path = hex_node::store::runtime_record(Path::new(text(&self.input, "record_root")), id);
        assert!(path.starts_with(&self.scope));
        let record: Value = serde_json::from_slice(&bounded(&path, self.budget)).unwrap();
        assert_eq!(record["spec"]["id"], json!(id));
        assert_eq!(
            record["phase"], "ready",
            "physical creation must be durable before workload dispatch"
        );
        record
    }

    fn retain_scope_record(&self, id: &BoxId, stage: &str, raw: &[u8]) -> Value {
        let output = PathBuf::from(
            std::env::var_os("HEX_COMPLETION_SCOPE_EVIDENCE").expect("scope evidence directory"),
        );
        let history = PathBuf::from(
            std::env::var_os("HEX_COMPLETION_SCOPE_HISTORY").expect("scope history output"),
        );
        assert_eq!(
            output.parent(),
            history.parent(),
            "one retained runner artifact directory"
        );
        fs::create_dir_all(&output).unwrap();
        let path = output.join(format!("{id}-{stage}.record.json"));
        fs::write(&path, raw).unwrap();
        json!({"path":path.strip_prefix(history.parent().unwrap()).unwrap().to_string_lossy().replace('\\',"/"),
            "sha256":digest(&path),"bytes":raw.len()})
    }

    fn scope_observation(&self, id: &BoxId, stage: &str) -> Value {
        let path = hex_node::store::runtime_record(Path::new(text(&self.input, "record_root")), id);
        assert!(path.starts_with(&self.scope));
        let raw = bounded(&path, self.budget);
        let record = BoxRecord::decode(&raw).expect("native original scope record");
        assert_eq!(&record.spec.id, id);
        assert_eq!(record.phase, Phase::Ready);
        json!({"stage":stage,"observed":true,"source_phase":"Ready",
            "native_scope":hex_bytes(record.native_scope.expect("actual immutable native scope").bytes()),
            "native_commitment":hex_bytes(&record.native_commitment().unwrap()),
            "record":self.retain_scope_record(id,stage,&raw)})
    }

    fn append_scope_observation(&mut self, id: &BoxId, stage: &str) {
        let observation = self.scope_observation(id, stage);
        let history = self
            .scope_observations
            .iter_mut()
            .find(|row| row["id"] == json!(id))
            .unwrap();
        history["observations"]
            .as_array_mut()
            .unwrap()
            .push(observation);
        self.write_scope_observations();
    }

    fn write_scope_observations(&self) {
        let path = PathBuf::from(
            std::env::var_os("HEX_COMPLETION_SCOPE_HISTORY").expect("scope history output"),
        );
        fs::write(
            path,
            serde_json::to_vec_pretty(&self.scope_observations).unwrap(),
        )
        .unwrap();
    }

    fn selected(&self, id: &BoxId) -> Vec<DeviceId> {
        let selected: Vec<DeviceId> =
            serde_json::from_value(self.record(id)["spec"]["devices"].clone()).unwrap();
        assert_eq!(
            selected.len(),
            1,
            "this fixture requests exactly one physical GPU"
        );
        selected
    }

    fn holder(&self, id: &BoxId, selected: &[DeviceId]) {
        let backend = self.backend();
        assert!(
            backend.classes().contains(&self.class()),
            "actual typed Runtime capability"
        );
        let capacity = backend
            .capacity(self.class())
            .unwrap()
            .expect("actual backend physical capacity");
        capacity.check().unwrap();
        let spec: BoxSpec = serde_json::from_value(self.record(id)["spec"].clone()).unwrap();
        let info = backend.inspect(id).unwrap();
        assert_eq!(spec.id, *id, "original recorded workload identity");
        assert_eq!(
            spec.devices.as_slice(),
            selected,
            "original selected claims"
        );
        assert_eq!(
            info.route, spec.route,
            "retained route, not a new selection"
        );
        assert_eq!(info.class, spec.class.class(), "original workload class");
        let expected = match spec.route.and_then(|route| route.backing) {
            Some(key) => {
                let backing = capacity
                    .backings
                    .iter()
                    .find(|backing| backing.key == key)
                    .expect("UNVERIFIED committed canonical backing is absent");
                assert_eq!(backing.phase, BackingPhase::Live, "live canonical backing");
                assert_eq!(
                    backing.devices.len(),
                    selected.len(),
                    "complete original physical grant"
                );
                assert!(
                    selected
                        .iter()
                        .all(|id| backing.devices.binary_search(id).is_ok())
                );
                DeviceHolder::Backing(key)
            }
            None => DeviceHolder::Box(id.try_clone().unwrap()),
        };
        for device in selected {
            let rows: Vec<_> = capacity
                .devices
                .iter()
                .filter(|row| row.id == *device)
                .collect();
            assert_eq!(rows.len(), 1, "exact unique physical offer");
            assert_eq!(
                rows[0].holder.as_ref(),
                Some(&expected),
                "single production holder authority"
            );
        }
        assert_eq!(
            capacity
                .devices
                .iter()
                .filter(|row| row.holder.as_ref() == Some(&expected))
                .count(),
            selected.len()
        );
        assert_eq!(
            info.class,
            self.class(),
            "actual class, not scheduler label"
        );
    }

    fn host(&self, name: &str, substitutions: &[(&str, String)]) -> Value {
        let row = &self.input[name];
        let program = Path::new(text(row, "path"));
        assert_eq!(
            digest(program),
            text(row, "sha256"),
            "source-pinned host witness"
        );
        let arguments: Vec<_> = row["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| {
                let mut value = arg.as_str().unwrap().to_owned();
                for (key, replacement) in substitutions {
                    value = value.replace(key, replacement);
                }
                value
            })
            .collect();
        let stdout = self
            .scope
            .join(format!("host-{name}-{}.stdout", self.events.len()));
        let stderr = self
            .scope
            .join(format!("host-{name}-{}.stderr", self.events.len()));
        let mut child = Command::new(program)
            .args(&arguments)
            .stdin(HostStdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        journal(
            json!({"event":"spawn","role":name,"pid":child.id(),"executable":program,"args":arguments}),
        );
        let pid = child.id();
        let status = child
            .wait()
            .expect("external ROOT run deadline bounds native command");
        journal(json!({"event":"reaped","role":name,"pid":pid,"status":status.code()}));
        assert!(
            status.success(),
            "native host witness failed; inspect retained {stderr:?}"
        );
        last_json(&bounded(&stdout, self.budget))
    }

    fn exec<T>(&self, id: &BoxId, field: &str, uuid: &str, oracle: impl FnOnce(Value) -> T) -> T {
        let began = Instant::now();
        assert!(
            !self.wait.is_zero() && self.budget > 0,
            "positive caller output bounds"
        );
        let until = began
            .checked_add(self.wait)
            .expect("caller output deadline overflow before exec");
        let limit = usize::try_from(self.budget)
            .ok()
            .filter(|limit| limit.checked_add(1).is_some())
            .expect("caller output byte limit overflow before exec");
        let mut raw = serde_json::to_string(&self.input[field])
            .unwrap()
            .replace("@UUID@", uuid);
        raw = raw.replace("@BOX@", id.as_str());
        let program: Program = serde_json::from_str(&raw).unwrap();
        program.check().unwrap();
        let backend = self.backend();
        let prepared = (|| -> std::io::Result<_> {
            let scope = tempfile::Builder::new()
                .prefix(&format!("exec-{field}-"))
                .tempdir_in(&self.scope)?
                .keep();
            journal(
                json!({"event":"runtime-output-scope","box":id,"field":field,"path":scope,
                "byte_limit_per_stream":self.budget,"deadline_origin":"before-pipe-setup"}),
            );
            let (stdout, out) = std::io::pipe()?;
            let (stderr, err) = std::io::pipe()?;
            let output = File::create(scope.join("stdout"))?;
            let error = File::create(scope.join("stderr"))?;
            Ok((
                scope,
                Stdio {
                    stdout: Some(out),
                    stderr: Some(err),
                    ..Stdio::null()
                },
                [("stdout", stdout, output), ("stderr", stderr, error)],
            ))
        })();
        let (scope, stdio, streams) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                journal(
                    json!({"event":"runtime-output-setup-error","box":id,"field":field,
                    "error":error.to_string(),"exec_started":false,"readers_started":0}),
                );
                panic!("UNVERIFIED output pipe setup: {error}");
            }
        };
        // Exactly one terminal notice per stdout/stderr observer. There are no
        // progress sends that could block draining while the caller waits for exit.
        let (sent, received) = mpsc::sync_channel(2);
        let mut workers: [Option<JoinHandle<()>>; 2] = [None, None];
        let mut captured: [Option<OutputRead>; 2] = [None, None];
        let mut errors = Vec::new();
        for (index, (name, reader, retained)) in streams.into_iter().enumerate() {
            let sent = sent.clone();
            match std::thread::Builder::new()
                .name(format!("completion-{name}"))
                .spawn(move || {
                    let output = read_output(reader, retained, limit, Some(until));
                    let _ = sent.send((index, output));
                }) {
                Ok(worker) => workers[index] = Some(worker),
                Err(error) => {
                    errors.push(format!("starting {name} observer: {error}"));
                    break;
                }
            }
        }
        drop(sent);
        let mut proc = None;
        let mut end = None;
        let mut proc_cleanup_uncertain = false;
        if errors.is_empty() && Instant::now() < until {
            match backend.exec(id, program, stdio) {
                Ok(started) => {
                    journal(
                        json!({"event":"runtime-exec","box":id,"reported_pid":started.pid(),
                        "field":field,"output_scope":scope,
                        "note":"PID is remote/guest scoped; never signal it from host"}),
                    );
                    proc = Some(started);
                }
                Err(error) => {
                    proc_cleanup_uncertain = true;
                    errors.push(format!("exec: {error}"));
                }
            }
        } else {
            drop(stdio);
            errors.push("caller deadline or observer setup prevented exec".to_owned());
        }
        if let Some(proc) = proc.as_mut() {
            let left = until.saturating_duration_since(Instant::now());
            let waited = if left.is_zero() {
                proc.try_wait()
            } else {
                proc.wait_for(left)
            };
            match waited {
                Ok(observed) => end = observed,
                Err(error) => errors.push(format!("program end: {error}")),
            }
            proc_cleanup_uncertain = end.is_none();
            if end != Some(Exit::Code(0)) {
                errors.push(format!("successful program end not observed: {end:?}"));
            }
            if end.is_none() {
                // Cleanup never starts another wait budget or changes an expired
                // operation into success. Only this owned Proc may be signalled.
                let kill = proc.kill();
                let cleanup_end = proc.try_wait();
                proc_cleanup_uncertain = !matches!(cleanup_end, Ok(Some(_)));
                journal(
                    json!({"event":"runtime-output-proc-cleanup","box":id,"field":field,
                    "kill":format!("{kill:?}"),"end":format!("{cleanup_end:?}"),
                    "after_deadline":Instant::now() >= until,
                    "whole_native_scope_release":"not proved by Proc exit"}),
                );
            }
        }
        let expected = workers.iter().filter(|worker| worker.is_some()).count();
        for _ in 0..expected {
            match received.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok((index, output)) => captured[index] = Some(output),
                Err(error) => {
                    errors.push(format!("both output EOF notices: {error}"));
                    break;
                }
            }
        }
        for (index, output) in received.try_iter() {
            captured[index] = Some(output);
        }
        let mut pending = Vec::new();
        for (index, name) in ["stdout", "stderr"].into_iter().enumerate() {
            if let Some(worker) = workers[index].take() {
                if captured[index].is_some() || worker.is_finished() {
                    if worker.join().is_err() {
                        errors.push(format!("{name} observer panicked"));
                    }
                } else {
                    // A blocked portable pipe read cannot be safely cancelled
                    // here. Retain uncertainty for ROOT's existing process guard.
                    pending.push(name);
                }
            }
            let observation = match &captured[index] {
                Some(output) => {
                    if !matches!(&output.end, OutputEnd::Eof) || output.at > until {
                        errors.push(format!(
                            "{name}: {:?} within caller deadline={}",
                            output.end,
                            output.at <= until
                        ));
                    }
                    json!({"observed":true,"terminal":output.end.receipt(),
                        "bytes":output.bytes.len(),"captured_prefix_sha256":format!("{:x}",Sha256::digest(&output.bytes)),
                        "within_caller_deadline":output.at <= until})
                }
                None => {
                    errors.push(format!("{name} EOF/observer completion unverified"));
                    json!({"observed":false,"terminal":{"state":"uncertain"},
                        "retained_prefix_may_still_be_written":pending.contains(&name)})
                }
            };
            journal(
                json!({"event":"runtime-output-stream","box":id,"field":field,
                "stream":name,"path":scope.join(name),"observation":observation}),
            );
        }
        let checked = if errors.is_empty() && Instant::now() < until {
            Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || oracle(last_json(&captured[0].as_ref().unwrap().bytes)),
            )))
        } else {
            None
        };
        if Instant::now() >= until {
            errors.push(
                "caller absolute deadline expired before checked output completed".to_owned(),
            );
        }
        // Keep the program/connection owner through both EOFs and the oracle;
        // dropping it earlier must not truncate an offered stream's final copy.
        drop(proc);
        drop(backend);
        let oracle_error = checked
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .map(|error| {
                error
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        error
                            .downcast_ref::<&str>()
                            .map(|message| (*message).to_owned())
                    })
                    .unwrap_or_else(|| "non-string oracle panic".to_owned())
            });
        journal(
            json!({"event":"runtime-output-result","box":id,"field":field,"output_scope":scope,
            "caller_budget_ns":self.wait.as_nanos().to_string(),"deadline_reset":false,
            "program_end":format!("{end:?}"),"oracle_error":oracle_error,"errors":errors,
            "unjoined_readers":pending,"reader_cleanup_uncertain":!pending.is_empty(),
            "proc_cleanup_uncertain":proc_cleanup_uncertain,
            "whole_native_scope_release":"separate canonical cleanup proof required"}),
        );
        if Instant::now() >= until && errors.is_empty() {
            journal(
                json!({"event":"runtime-output-deadline","box":id,"field":field,
                "phase":"final-receipt","output_scope":scope}),
            );
            errors
                .push("caller absolute deadline expired while retaining final receipt".to_owned());
        }
        assert!(
            errors.is_empty(),
            "UNVERIFIED checked output: {errors:?}; {scope:?}"
        );
        match checked {
            Some(Ok(value)) => value,
            Some(Err(error)) => std::panic::resume_unwind(error),
            None => panic!("UNVERIFIED output oracle did not run; {scope:?}"),
        }
    }

    fn native_join(&self, id: &BoxId, selected: &[DeviceId]) -> (String, Value, Value) {
        let translation = if self.backend == "firecracker" {
            self.host(
                "physical_translation",
                &[
                    ("@BOX@", id.as_str().into()),
                    ("@KEY@", selected[0].key.to_string()),
                ],
            )
        } else {
            Value::Null
        };
        let join = |inventory: Value| {
            let mut matches = Vec::new();
            for row in inventory["devices"]
                .as_array()
                .expect("independent actual host inventory")
            {
                if let Some(luid) = row["luid"].as_str() {
                    let mask = count(row, "node_mask");
                    assert!(
                        mask > 0 && mask.is_power_of_two(),
                        "linked LUID mask does not select one physical node"
                    );
                    assert_eq!(
                        inventory["devices"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|device| device["luid"].as_str() == Some(luid))
                            .count(),
                        1,
                        "LUID shared by multiple actual physical devices"
                    );
                }
                if let Some(pci) = row["pci"].as_array() {
                    assert_eq!(pci.len(), 4);
                    let domain = pci[0].as_u64().unwrap();
                    let bus = pci[1].as_u64().unwrap();
                    let device = pci[2].as_u64().unwrap();
                    let function = pci[3].as_u64().unwrap();
                    assert!(
                        domain <= u64::from(u32::MAX)
                            && bus <= 255
                            && device <= 31
                            && function <= 7
                    );
                    let key = (domain << 16) | (bus << 8) | (device << 3) | function;
                    let exact = if translation.is_null() {
                        selected[0].key == key
                    } else {
                        translation["native_key"] == json!(selected[0].key)
                            && translation["guest_pci"] == row["pci"]
                    };
                    if exact {
                        matches.push(text(row, "uuid").to_owned());
                    }
                } else if row["metal_registry_id"].as_u64() == Some(selected[0].key) {
                    matches.push(text(row, "uuid").to_owned());
                }
            }
            assert_eq!(
                matches.len(),
                1,
                "UNVERIFIED exact native-key-to-UUID join is missing/ambiguous"
            );
            (
                matches.pop().unwrap(),
                inventory["devices"].clone(),
                translation,
            )
        };
        if self.backend == "firecracker" {
            self.exec(id, "inventory", "", join)
        } else {
            join(self.host("host_inventory", &[]))
        }
    }

    fn compute(&self, id: &BoxId, uuid: &str) -> Value {
        self.exec(id, "compute", uuid, |value| {
            assert_eq!(text(&value, "uuid"), uuid);
            let words = count(&value, "words");
            assert!(words > 0);
            assert_eq!(value["mismatches"], 0);
            assert_eq!(count(&value, "prefill_mismatches"), words);
            assert_eq!(value["hardware"], true);
            assert_eq!(value["dispatch_completed"], true);
            value
        })
    }

    fn exercise(&mut self) {
        self.start_all();
        self.write_resources("before");
        assert!(
            Path::new(text(&self.input, "record_root")).starts_with(&self.scope),
            "private observed Store only"
        );
        self.creating = Some(
            CreatingObserver::start(
                PathBuf::from(text(&self.input, "record_root")),
                self.class(),
                CreatingBounds {
                    wait: self.wait,
                    entries: count(&self.input, "creating_entry_limit"),
                    events: count(&self.input, "creating_event_limit"),
                    bytes: self.budget,
                    notification_bytes: count(&self.input, "creating_notification_bytes"),
                },
                self.input.get("creating_notifications").cloned(),
                &self.scope,
            )
            .expect("bounded Creating observer source"),
        );
        self.creating.as_ref().unwrap().announce();
        self.creating
            .as_ref()
            .unwrap()
            .ready
            .recv_timeout(self.wait)
            .expect("Creating observer armed BEFORE Submit");
        let payload: Payload = serde_json::from_value(self.input["payload"].clone()).unwrap();
        assert!(
            payload.spec.devices.is_empty(),
            "only production scheduler selects identities"
        );
        assert_eq!(
            payload.spec.class.class(),
            self.class(),
            "typed backend payload"
        );
        let reply = self
            .control()
            .schedule(dto::Command::Submit {
                demand: text(&self.input, "demand").into(),
                payload,
            })
            .unwrap();
        if let Some(key) = reply.key {
            self.keys.push(key);
        }
        if let Some(id) = &reply.physical {
            self.boxes.push(id.clone());
        }
        assert!(
            reply.refusals.is_empty() && reply.created && reply.tracked && !reply.ended,
            "{reply:?}"
        );
        assert_eq!(
            reply.at,
            Some(u32::try_from(count(&self.input, "runner")).unwrap())
        );
        let key = reply.key.unwrap();
        let id = reply.physical.unwrap();
        let selected = self.selected(&id);
        let actual_creating = self.creating.as_mut().unwrap().finish();
        self.creating = None;
        let creating = match actual_creating {
            Ok(rows) => match rows.into_iter().find(|(observed, _)| observed == &id) {
                Some((_, bytes)) => {
                    let record =
                        BoxRecord::decode(&bytes).expect("captured actual Creating record");
                    json!({"stage":"creating","observed":true,"source_phase":"Creating",
                        "init_published":false,"native_scope":hex_bytes(record.native_scope.unwrap().bytes()),
                        "native_commitment":hex_bytes(&record.native_commitment().unwrap()),
                        "record":self.retain_scope_record(&id,"creating",&bytes)})
                }
                None => json!({"stage":"creating","observed":false,"source_phase":null,
                    "reason":"actual Creating-without-init window missed; immutable Ready origin is separate"}),
            },
            Err(error) => json!({"stage":"creating","observed":false,"source_phase":null,
                "reason":format!("Creating observer uncertainty: {error}")}),
        };
        let ready = self.scope_observation(&id, "binding");
        self.scope_observations
            .push(json!({"id":id,"observations":[ready,creating]}));
        self.write_scope_observations();
        self.holder(&id, &selected);
        fs::write(
            self.scope.join("placement.json"),
            serde_json::to_vec(&json!({
                "source":self.input["source"],"key":key,"physical":id,"runner":reply.at.unwrap()
            }))
            .unwrap(),
        )
        .unwrap();
        if !self.input["origin_barrier"].is_null() {
            let origin = self.host("origin_barrier", &[]);
            assert_eq!(
                origin["origin_verified"], true,
                "actual peer origin must observe this physical box"
            );
        }
        let native = self.host("native_observer", &[("@BOX@", id.as_str().into())]);
        assert_eq!(native["backend"], self.backend);
        assert_eq!(native["native_process_verified"], true);
        journal(
            json!({"event":"native-observed","box":id,"pid":native["pid"],
            "start_token":native["start_token"],"backend":self.backend,
            "authority":"observe only; ordinary Runtime deletion owns native termination"}),
        );
        fs::write(
            self.scope.join("native-proof.json"),
            serde_json::to_vec(&native).unwrap(),
        )
        .unwrap();
        self.emit("capability", json!({"runtime_class":self.class().name(),
            "native_process_verified":true,"native_executable_sha256":native["native_executable_sha256"]}));
        self.emit(
            "binding",
            json!({"selected":selected,"holder_count":1,"durable_before_execution":true,"physical":id,"key":key}),
        );
        let (uuid, native_inventory, translation) = self.native_join(&id, &selected);
        let mut compute = self.compute(&id, &uuid);
        compute["selected"] = json!(selected);
        compute["native_join_verified"] = json!(true);
        compute["native_inventory"] = native_inventory;
        compute["physical_translation"] = translation;
        self.emit("compute", compute);
        if text(&self.input, "proof") == "selected" {
            self.cleanup().expect("ordinary physical cleanup");
            self.emit(
                "cleanup",
                json!({"boxes":0,"holders":0,"owned_native_processes":0,"owned_artifacts":0}),
            );
            self.emit("complete", json!({"selected_stages":5}));
            return;
        }
        let inventory = self.host("host_inventory", &[]);
        let foreign: Vec<_> = inventory["devices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| text(row, "uuid") != uuid && row["device_type"] != 4)
            .collect();
        assert!(
            !foreign.is_empty(),
            "UNVERIFIED second physical GPU absent; selected-only proof remains separate"
        );
        for row in &foreign {
            let positive = self.host(
                "foreign_host_compute",
                &[("@UUID@", text(row, "uuid").into())],
            );
            assert_eq!(positive["uuid"], row["uuid"]);
            assert_eq!(
                positive["dispatch_completed"], true,
                "foreign positive must dispatch on host"
            );
            assert_eq!(positive["mismatches"], 0);
            self.exec(&id, "reject", text(row, "uuid"), |denied| {
                assert_eq!(denied["refused"], true);
                assert_eq!(denied["dispatches"], 0);
            });
        }
        self.emit(
            "foreign",
            json!({"host_accessible":foreign.len(),"guest_accessible":0}),
        );
        let wrong = text(&self.input, "wrong_uuid").to_owned();
        assert!(
            inventory["devices"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| text(row, "uuid") != wrong)
        );
        let denied = self.exec(&id, "reject", &wrong, |denied| {
            assert_eq!(denied["refused"], true);
            assert_eq!(denied["dispatches"], 0);
            denied
        });
        self.emit("wrong-id", denied);
        self.restart("scheduler");
        let status = self
            .control()
            .schedule(dto::Command::Status { key })
            .unwrap();
        assert!(status.created && status.tracked && status.refusals.is_empty());
        assert_eq!(status.physical.as_ref(), Some(&id));
        assert_eq!(self.selected(&id), selected);
        self.holder(&id, &selected);
        self.compute(&id, &uuid);
        let retained = self.host("native_observer", &[("@BOX@", id.as_str().into())]);
        assert_eq!(retained["pid"], native["pid"]);
        assert_eq!(
            retained["start_token"], native["start_token"],
            "scheduler restart recreated native work"
        );
        self.append_scope_observation(&id, "scheduler-recovery");
        self.emit(
            "scheduler-recovery",
            json!({"selected":selected,"holder_count":1,
            "same_physical_box":true,"compute_verified":true}),
        );
        let before_load = self.host("load_before", &[]);
        let actor_row = &self.input["load_actor"];
        let actor_path = Path::new(text(actor_row, "path"));
        assert_eq!(
            digest(actor_path),
            text(actor_row, "sha256"),
            "source-pinned native load actor"
        );
        let mut actor = Service::start(actor_row, actor_path, &self.scope, self.wait, self.budget);
        let catalog = &self.input["catalog"];
        let catalog_path = Path::new(text(catalog, "path"));
        assert!(catalog_path.starts_with(&self.scope));
        let original = bounded(catalog_path, self.budget);
        assert_eq!(original, text(catalog, "before").as_bytes());
        assert_ne!(text(catalog, "before"), text(catalog, "after"));
        let before_text = text(catalog, "before");
        let after_text = text(catalog, "after");
        assert_eq!(before_text.lines().next(), after_text.lines().next());
        let mut old_rows: Vec<_> = before_text.lines().skip(1).collect();
        let mut new_rows: Vec<_> = after_text.lines().skip(1).collect();
        old_rows.sort_unstable();
        new_rows.sort_unstable();
        assert_eq!(
            old_rows, new_rows,
            "reorder preserves every exact catalog entry"
        );
        fs::write(catalog_path, text(catalog, "after")).unwrap();
        let changed = self.host("load_change", &[]);
        assert_ne!(
            before_load["order"], changed["order"],
            "actual measured load order did not change"
        );
        assert!(
            before_load["order"]
                .as_array()
                .is_some_and(|order| !order.is_empty())
        );
        assert!(
            changed["order"]
                .as_array()
                .is_some_and(|order| !order.is_empty())
        );
        assert!(
            actor.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "load actor already completed"
        );
        self.restart("backend");
        self.restart("scheduler");
        let status = self
            .control()
            .schedule(dto::Command::Status { key })
            .unwrap();
        assert!(status.created && status.tracked && status.refusals.is_empty());
        assert_eq!(status.physical.as_ref(), Some(&id));
        assert_eq!(self.selected(&id), selected);
        self.holder(&id, &selected);
        self.compute(&id, &uuid);
        let retained = self.host("native_observer", &[("@BOX@", id.as_str().into())]);
        assert_eq!(retained["pid"], native["pid"]);
        assert_eq!(
            retained["start_token"], native["start_token"],
            "runtime recovery recreated native work"
        );
        assert!(
            actor.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "load was not retained across recovery"
        );
        actor.finish();
        self.append_scope_observation(&id, "runtime-recovery");
        self.emit("runtime-recovery", json!({"selected":selected,"holder_count":1,
            "same_physical_box":true,"compute_verified":true,"catalog_reordered":true,"load_order_changed":true}));
        self.cleanup().expect("physical and ordinary owned cleanup");
        self.emit(
            "cleanup",
            json!({"boxes":0,"holders":0,"owned_native_processes":0,"owned_artifacts":0}),
        );
        self.emit("complete", json!({"selected_stages":9}));
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let observer = self.creating.as_mut().map(CreatingObserver::finish);
        self.creating = None;
        if let Some(Err(error)) = observer {
            // Stop/join first; ordinary native deletion still runs before reporting it.
            eprintln!("UNVERIFIED Creating observer cleanup: {error}");
        }
        for key in &self.keys {
            self.control()
                .schedule(dto::Command::Delete { key: *key })
                .map_err(|error| error.to_string())?;
        }
        for id in &self.boxes {
            if self
                .backend()
                .list()
                .map_err(|error| error.to_string())?
                .iter()
                .any(|row| row.id == *id)
            {
                return Err(format!("owned physical box remains: {id}"));
            }
            let capacity = self
                .backend()
                .capacity(self.class())
                .map_err(|error| error.to_string())?
                .ok_or("missing native cleanup capacity")?;
            // This checks only the removed Box claim. Backing remains a distinct holder in
            // the resource snapshot; Box absence cannot prove that native pool was released.
            if capacity
                .devices
                .iter()
                .any(|device| device.holder.as_ref().and_then(DeviceHolder::box_id) == Some(id))
            {
                return Err(format!("owned physical holder remains: {id}"));
            }
        }
        let result = self.host("cleanup_observer", &[]);
        if result["owned_native_processes"] != 0 || result["owned_artifacts"] != 0 {
            return Err(format!("actual native cleanup unverified: {result}"));
        }
        self.write_resources("after");
        self.keys.clear();
        self.boxes.clear();
        for service in self.services.iter_mut().rev() {
            service.stop();
        }
        Ok(())
    }
}

fn case(backend: &str, topology: &str, proof: &str) {
    let mut lab = Lab::new(backend, topology, proof);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lab.exercise()));
    if let Err(original) = result {
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lab.cleanup()));
        eprintln!(
            "completion_original_failure scope={:?} cleanup={cleanup:?}; preserve evidence/PID journal",
            lab.scope
        );
        std::panic::resume_unwind(original);
    }
    // Evidence stays outside scope; retain raw files in scope for ROOT review, never erase uncertain state.
    eprintln!("completion_verified_raw_scope {:?}", lab.scope);
}

macro_rules! native_case {
    ($name:ident, $backend:literal, $topology:literal) => {
        #[test]
        #[ignore = "ROOT-only actual GPU/backend/VM/crash; explicit finite envelope/config required"]
        fn $name() { case($backend, $topology, "full"); }
    };
}

native_case!(completion_oci_laptop, "oci", "laptop");
native_case!(completion_oci_peer, "oci", "peer");
native_case!(completion_firecracker_laptop, "firecracker", "laptop");
native_case!(completion_firecracker_peer, "firecracker", "peer");
native_case!(completion_shards_laptop, "shards", "laptop");
native_case!(completion_shards_peer, "shards", "peer");

macro_rules! selected_case {
    ($name:ident, $backend:literal, $topology:literal) => {
        #[test]
        #[ignore = "ROOT-only single-GPU positive; excludes full foreign/recovery proof"]
        fn $name() {
            case($backend, $topology, "selected");
        }
    };
}
selected_case!(completion_oci_laptop_selected, "oci", "laptop");
selected_case!(completion_oci_peer_selected, "oci", "peer");
selected_case!(
    completion_firecracker_laptop_selected,
    "firecracker",
    "laptop"
);
selected_case!(completion_firecracker_peer_selected, "firecracker", "peer");
selected_case!(completion_shards_laptop_selected, "shards", "laptop");
selected_case!(completion_shards_peer_selected, "shards", "peer");

/// AC-T7.4.1: fixture parsing is checked without touching any socket, driver or VM.
#[test]
fn completion_receipt_parser_uses_the_actual_last_json_result() {
    assert_eq!(
        last_json(b"dispatch_started\n{\"mismatches\":0}\n")["mismatches"],
        0
    );
    assert_eq!(
        serde_json::from_value::<Class>(json!("oci"))
            .unwrap()
            .name(),
        "oci"
    );
}

/// A pipe's bytes and end that a test writes itself: no recorded operation makes them, so their read
/// waits measuring, for the fact alone, its answers told to the census (`hex_testkit::waits`).
static PIPE_READ: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.completion_backends.pipe_read");

/// Reads `reader` within `PIPE_READ`, telling it the answer where the read ended at its fact.
fn read_pipe(
    reader: std::io::PipeReader,
    retained: File,
    limit: usize,
) -> (OutputRead, Option<Instant>) {
    let began = Instant::now();
    let until = PIPE_READ
        .deadline()
        .and_then(|bound| began.checked_add(bound));
    let output = read_output(reader, retained, limit, until);
    if !matches!(output.end, OutputEnd::Deadline) {
        hex_testkit::waits::answered(&PIPE_READ, output.at.saturating_duration_since(began));
    }
    (output, until)
}

/// AC-T7.4.1: reaching the byte ceiling must still read a real zero from the pipe.
#[test]
fn completion_output_exact_limit_needs_real_pipe_eof() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"x").unwrap();
    drop(writer);
    let retained = tempfile::tempfile().unwrap();
    let (output, until) = read_pipe(reader, retained, 1);
    assert!(matches!(output.end, OutputEnd::Eof));
    assert_eq!(output.bytes, b"x");
    assert!(until.is_none_or(|until| output.at <= until));
}

/// AC-T7.4.1: over-limit data is refused, with only the admitted prefix retained.
#[test]
fn completion_output_over_limit_retains_bounded_prefix() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let worker = std::thread::spawn(move || writer.write_all(b"xy"));
    let mut retained = tempfile::tempfile().unwrap();
    let (output, _) = read_pipe(reader, retained.try_clone().unwrap(), 1);
    let written = worker.join();
    std::io::Seek::rewind(&mut retained).unwrap();
    let mut prefix = Vec::new();
    let read = retained.read_to_end(&mut prefix);
    assert!(written.unwrap().is_ok());
    assert!(read.is_ok());
    assert!(matches!(output.end, OutputEnd::Limit));
    assert_eq!(output.bytes, b"x");
    assert_eq!(prefix, b"x");
}

/// AC-T7.4.1: an expired caller cannot block waiting on a writer that remains open.
#[test]
fn completion_output_expired_deadline_does_not_wait_for_writer() {
    let (reader, writer) = std::io::pipe().unwrap();
    let retained = tempfile::tempfile().unwrap();
    let output = read_output(reader, retained, 1, Some(Instant::now()));
    drop(writer);
    assert!(matches!(output.end, OutputEnd::Deadline));
    assert!(output.bytes.is_empty());
}

fn creating_capture_record(id: &str) -> BoxRecord {
    let mut record = BoxRecord::new(
        BoxSpec {
            id: BoxId::new(id).unwrap(),
            bounds: hex_api::Bounds::default(),
            class: hex_api::ClassSpec::Process,
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
            band: 0,
        },
        Phase::Creating,
        Vec::new(),
    );
    record.native_scope = Some(hex_core::record::NativeScope::from_bytes([3; 16]));
    record
}

/// AC-T7.4.1: Ready, missing scope and already-published init cannot become Creating evidence.
#[test]
fn completion_creating_capture_retains_only_the_original_unpublished_scoped_row() {
    let dir = tempfile::tempdir().unwrap();
    let original = creating_capture_record("creating");
    let mut ready = creating_capture_record("ready");
    ready.phase = Phase::Ready;
    let mut missing_scope = creating_capture_record("unscoped");
    missing_scope.native_scope = None;
    let mut published = creating_capture_record("published");
    published.init = Some(hex_core::record::ProcId {
        pid: 1,
        start_ticks: 1,
    });
    let records = [original.clone(), ready, missing_scope, published];
    let mut entries = 0u64;
    for record in &records {
        let path = hex_node::store::runtime_record(dir.path(), &record.spec.id);
        if !path.parent().unwrap().exists() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            entries += 1;
        }
        fs::write(path, record.encode().unwrap()).unwrap();
        entries += 1;
    }
    let mut captured = Vec::new();
    capture_creating(
        dir.path(),
        Class::Process,
        entries,
        hex_core::record::MAX_RECORD_BYTES as u64,
        &mut captured,
    )
    .unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, original.spec.id);
    assert_eq!(captured[0].1, original.encode().unwrap());
    let mut now_ready = original.clone();
    now_ready.phase = Phase::Ready;
    fs::write(
        hex_node::store::runtime_record(dir.path(), &original.spec.id),
        now_ready.encode().unwrap(),
    )
    .unwrap();
    capture_creating(
        dir.path(),
        Class::Process,
        entries,
        hex_core::record::MAX_RECORD_BYTES as u64,
        &mut captured,
    )
    .unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(BoxRecord::decode(&captured[0].1).unwrap(), original);
}

/// AC-T7.4.1: refused output bounds retain both the source row and empty evidence.
#[test]
fn completion_creating_capture_refuses_an_oversized_record_without_publishing_it() {
    let dir = tempfile::tempdir().unwrap();
    let record = creating_capture_record("creating");
    let path = hex_node::store::runtime_record(dir.path(), &record.spec.id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let raw = record.encode().unwrap();
    fs::write(&path, &raw).unwrap();
    let mut captured = Vec::new();
    assert!(
        capture_creating(
            dir.path(),
            Class::Process,
            2,
            u64::try_from(raw.len() - 1).unwrap(),
            &mut captured
        )
        .is_err()
    );
    assert!(captured.is_empty());
    assert_eq!(fs::read(path).unwrap(), raw);
}
