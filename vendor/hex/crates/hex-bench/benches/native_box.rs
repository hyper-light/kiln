//! Each OS's `process` class against its named competitors on the same machine (native plan step 7,
//! CLAUDE.md §7; docs/research/runtime/native-process-competitors.md). One driver per run:
//! - `hex`: the OS's class through the hex-api `Runtime` seam, in process (Linux's node agent, macOS's
//!   and Windows' runtimes);
//! - Linux `scope`: a systemd transient scope (`systemd-run --user --scope -p MemoryMax=`);
//! - macOS `taskpolicy`: `taskpolicy -m <MiB>`, the same fatal jetsam cap Hex arms, on one process;
//! - macOS `launchd`: a transient job (`launchctl bootstrap gui/<uid>` of a plist, `bootout`), its
//!   `HardResourceLimits` `ResidentSetSize` the bound launchd offers a user's job;
//! - Windows `procgov`: Process Governor (`procgov --maxjobmem`), a job object as Hex's.
//!
//! Phases, every program a `hex selftest-box` act, every box bounded to 256 MiB:
//! - `lifecycle`: create, start (to the program's "up" line: `serve`) and delete (to the program's end),
//!   each timed alone, `n` times after `warmup`; a competitor with no separate create times its create
//!   and start together as start, and records create as absent;
//! - `bound`: `runs` programs climbing to 1 GiB (`climb`), each reporting the KiB it has touched: the
//!   last it reported is how far it got past the bound, and the time from start to its end how fast the
//!   bound ended it.
//!
//!   native_box <out.json> <driver> <n> <warmup> <runs> [climb step bytes, 1 MiB unless given]
//!
//! (`cargo bench -p hex-bench --bench native_box -- ...`). `HEX_BIN` names the `hex` binary, else the one
//! beside this bench. The records are in bench/native/.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("SKIP: the native box bench runs on Linux, macOS and Windows");
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> std::process::ExitCode {
    bench::main()
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod bench {
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitCode, Stdio};
    use std::time::{Duration, Instant};

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Proc, Program, Runtime};
    use serde_json::{Value, json};

    pub const MIB: u64 = 1 << 20;
    /// Test inputs, not tunings: every driver gets the same box and the same climb, and only the order
    /// footprint < bound < climb matters. The bound is the native bar's 256 MiB box
    /// (bench/native/macos/RESULT.md); the program's own footprint is at most 6 MiB (its climbs stopped at
    /// 250-254 MiB in that box on all three OSes, bench/native/RESULT.md), and the climb passes the bound
    /// four times over.
    pub const BOUND: u64 = 256 * MIB;
    pub const CLIMB: u64 = 1024 * MIB;

    /// The hex binary whose `selftest-box` acts are the programs: `HEX_BIN`, else the `hex` beside this
    /// bench (cargo puts both under the same profile directory).
    pub fn hex_binary() -> PathBuf {
        if let Some(p) = std::env::var_os("HEX_BIN") {
            return PathBuf::from(p);
        }
        let me = std::env::current_exe().expect("current exe");
        let name = if cfg!(windows) { "hex.exe" } else { "hex" };
        let hex = me
            .parent()
            .and_then(|deps| deps.parent())
            .map(|profile| profile.join(name))
            .expect("a profile directory above the bench");
        assert!(
            hex.exists(),
            "no hex binary at {} (set HEX_BIN)",
            hex.display()
        );
        hex
    }

    pub fn act(hex: &Path, act: &[&str]) -> Vec<String> {
        let mut args = vec![
            hex.to_string_lossy().into_owned(),
            "selftest-box".to_owned(),
        ];
        args.extend(act.iter().map(|a| (*a).to_owned()));
        args
    }

    /// One started program and how its driver ends it.
    pub trait Started {
        /// Reads the program's stdout up to its first line.
        fn line(&mut self) -> String;
        /// The rest of its stdout, to its end.
        fn rest(&mut self) -> String;
        /// Waits for its end; its exit as text.
        fn ended(&mut self) -> String;
        /// Ends it and what it started, where its driver's delete is a kill (macOS's taskpolicy,
        /// Windows' procgov; Linux's scope has `systemctl stop`).
        #[cfg(not(target_os = "linux"))]
        fn kill(&mut self) {}
    }

    /// A way to run a bounded program: Hex's class or a competitor.
    pub trait Driver {
        fn name(&self) -> &'static str;
        /// Makes a box, untimed by the caller when it returns `None` (no separate create).
        fn create(&mut self, i: usize, bound: u64) -> Option<()>;
        fn start(&mut self, i: usize, bound: u64, program: &[String]) -> Box<dyn Started>;
        fn delete(&mut self, i: usize, started: &mut dyn Started);
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// p50, p99, p99.9 (nearest rank) and max, in ms.
    fn stats(mut xs: Vec<f64>) -> Value {
        if xs.is_empty() {
            return Value::Null;
        }
        xs.sort_by(f64::total_cmp);
        let rank = |p: f64| xs[((p * xs.len() as f64).ceil() as usize).clamp(1, xs.len()) - 1];
        json!({"n": xs.len(), "p50": rank(0.50), "p99": rank(0.99), "p999": rank(0.999), "max": xs[xs.len() - 1]})
    }

    fn lifecycle(d: &mut dyn Driver, hex: &Path, n: usize, warmup: usize) -> Value {
        let serve = act(hex, &["serve"]);
        let (mut create, mut start, mut delete) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..warmup + n {
            let t = Instant::now();
            let made = d.create(i, BOUND);
            let c = t.elapsed();
            let t = Instant::now();
            let mut s = d.start(i, BOUND, &serve);
            assert_eq!(s.line(), "up\n", "{}: start {i}", d.name());
            let st = t.elapsed();
            let t = Instant::now();
            d.delete(i, s.as_mut());
            let _ = s.ended();
            let de = t.elapsed();
            if i >= warmup {
                if made.is_some() {
                    create.push(ms(c));
                }
                start.push(ms(st));
                delete.push(ms(de));
            }
        }
        json!({"create": stats(create), "start": stats(start), "delete": stats(delete)})
    }

    fn bound(d: &mut dyn Driver, hex: &Path, runs: usize, first: usize, step: usize) -> Value {
        let climb = act(hex, &["climb", &CLIMB.to_string(), &step.to_string()]);
        let mut each = Vec::new();
        let (mut reached, mut took) = (Vec::new(), Vec::new());
        for r in 0..runs {
            let i = first + r;
            d.create(i, BOUND);
            let t = Instant::now();
            let mut s = d.start(i, BOUND, &climb);
            let out = format!("{}{}", s.line(), s.rest());
            let exit = s.ended();
            let end = t.elapsed();
            d.delete(i, s.as_mut());
            let kib: u64 = out
                .lines()
                .rev()
                .find_map(|l| l.trim().parse().ok())
                .unwrap_or(0);
            let mib = kib as f64 / 1024.0;
            each.push(json!({"reached_mib": mib, "ms": ms(end), "exit": exit}));
            reached.push(mib);
            took.push(ms(end));
        }
        let bound_mib = (BOUND / MIB) as f64;
        let over = reached
            .iter()
            .map(|m| m - bound_mib)
            .fold(f64::MIN, f64::max);
        json!({
            "bound_mib": bound_mib,
            "climb_mib": CLIMB / MIB,
            "step_bytes": step,
            "most_past_bound_mib": over,
            "bounded_runs": reached.iter().filter(|m| **m < (CLIMB / MIB) as f64).count(),
            "end_ms": stats(took),
            "runs": each,
        })
    }

    /// Hex's class through the seam.
    pub struct Hex<R: Runtime> {
        pub rt: R,
        pub prefix: String,
    }

    struct HexProc {
        proc: Box<dyn Proc>,
        out: BufReader<std::io::PipeReader>,
    }

    impl Started for HexProc {
        fn line(&mut self) -> String {
            let mut l = String::new();
            self.out.read_line(&mut l).expect("read a line");
            l
        }
        fn rest(&mut self) -> String {
            let mut s = String::new();
            self.out.read_to_string(&mut s).expect("read the rest");
            s
        }
        fn ended(&mut self) -> String {
            // The suite's learned wait: twice the slowest end of the record and this run.
            let asked = Instant::now();
            match self
                .proc
                .wait_for(hex_api::conformance::patience())
                .expect("wait")
            {
                Some(e) => {
                    hex_api::conformance::ENDS.saw(asked.elapsed());
                    format!("{e:?}")
                }
                None => "running".to_owned(),
            }
        }
    }

    impl<R: Runtime> Hex<R> {
        fn id(&self, i: usize) -> BoxId {
            BoxId::new(&format!("{}-{i}", self.prefix)).unwrap()
        }
    }

    impl<R: Runtime> Driver for Hex<R> {
        fn name(&self) -> &'static str {
            "hex"
        }
        fn create(&mut self, i: usize, bound: u64) -> Option<()> {
            let spec = BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: self.id(i),
                bounds: Bounds {
                    memory_max: Some(bound),
                    ..Bounds::default()
                },
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            };
            self.rt.create(spec).expect("create");
            Some(())
        }
        fn start(&mut self, i: usize, _bound: u64, program: &[String]) -> Box<dyn Started> {
            let (r, w) = std::io::pipe().unwrap();
            let args: Vec<&str> = program.iter().map(String::as_str).collect();
            let stdio = hex_api::Stdio {
                stdout: Some(w),
                ..hex_api::Stdio::null()
            };
            let proc = self
                .rt
                .exec(&self.id(i), Program::of(&args), stdio)
                .expect("exec");
            Box::new(HexProc {
                proc,
                out: BufReader::new(r),
            })
        }
        fn delete(&mut self, i: usize, _started: &mut dyn Started) {
            self.rt.delete(&self.id(i)).expect("delete");
        }
    }

    /// A competitor's program, a child of this bench.
    pub struct ChildProc {
        pub child: Child,
        pub out: BufReader<std::process::ChildStdout>,
    }

    impl ChildProc {
        pub fn spawn(cmd: &mut Command) -> Self {
            let mut child = cmd
                .stdout(Stdio::piped())
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn");
            let out = BufReader::new(child.stdout.take().unwrap());
            Self { child, out }
        }
    }

    impl Started for ChildProc {
        fn line(&mut self) -> String {
            let mut l = String::new();
            self.out.read_line(&mut l).expect("read a line");
            l
        }
        fn rest(&mut self) -> String {
            let mut s = String::new();
            let _ = self.out.read_to_string(&mut s);
            s
        }
        fn ended(&mut self) -> String {
            format!("{:?}", self.child.wait().expect("wait"))
        }
        #[cfg(not(target_os = "linux"))]
        fn kill(&mut self) {
            if cfg!(windows) {
                // The tree: the competitor's own process and the program it started.
                let pid = self.child.id().to_string();
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid])
                    .output();
            } else {
                let _ = self.child.kill();
            }
        }
    }

    /// The machine and load beside the figures (CLAUDE.md §7).
    fn context() -> Value {
        let run = |c: &str, a: &[&str]| {
            Command::new(c)
                .args(a)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default()
        };
        if cfg!(windows) {
            json!({"os": "windows", "arch": std::env::consts::ARCH, "host": run("hostname", &[])})
        } else {
            json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "host": run("hostname", &[]),
                "uname": run("uname", &["-a"]),
                "uptime": run("uptime", &[]),
            })
        }
    }

    pub fn main() -> ExitCode {
        // `cargo bench` passes `--bench` to a harness-less bench.
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .map(|a| a.into_string().expect("Unicode arguments"))
            .filter(|a| a != "--bench")
            .collect();
        let (out, driver, n, warmup, runs, step) = match args.as_slice() {
            [out, driver, n, warmup, runs] => (out, driver, n, warmup, runs, MIB as usize),
            [out, driver, n, warmup, runs, step] => {
                (out, driver, n, warmup, runs, step.parse().unwrap())
            }
            _ => panic!(
                "usage: native_box <out.json> <driver> <n> <warmup> <runs> [climb step bytes]"
            ),
        };
        let (n, warmup, runs): (usize, usize, usize) = (
            n.parse().unwrap(),
            warmup.parse().unwrap(),
            runs.parse().unwrap(),
        );
        let hex = hex_binary();
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let before = context();
        let Some(result) = super::os::run(driver, &args, &hex, |d| {
            let l = lifecycle(d, &hex, n, warmup);
            let b = bound(d, &hex, runs, warmup + n, step);
            json!({"lifecycle": l, "bound": b})
        }) else {
            return ExitCode::SUCCESS;
        };
        let record = json!({
            "bench": "native_box",
            "driver": driver,
            "n": n, "warmup": warmup, "runs": runs,
            "started_unix": started,
            "before": before,
            "after": context(),
            "result": result,
        });
        std::fs::write(out, serde_json::to_string_pretty(&record).unwrap()).expect("write");
        println!("{}", serde_json::to_string(&record["result"]).unwrap());
        ExitCode::SUCCESS
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use hex_node::Node;
    use serde_json::Value;

    use super::bench::{BOUND, ChildProc, Driver, Hex, MIB, Started};

    fn own_cgroup() -> PathBuf {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
        let rel = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .expect("a 0:: cgroup line")
            .trim_start_matches('/');
        PathBuf::from("/sys/fs/cgroup").join(rel)
    }

    /// As seam_box: moves this process into a leaf of its own cgroup and checks a box root there gets
    /// every controller.
    fn prepare() -> Result<PathBuf, String> {
        let own = own_cgroup();
        let leaf = own.join("hex-agent");
        std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf: {e}"))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| format!("moving self into a leaf: {e}"))?;
        // The probe root opening proves every controller a box uses is delegated here: `BoxRoot::open`
        // refuses by name otherwise (HEX-INST-1).
        hex_runtime::linux::cgroup::BoxRoot::open(&own.join("hex-probe"), Vec::new())
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        Ok(own)
    }

    /// A systemd transient scope per program, its memory bound `MemoryMax=` with no swap.
    struct Scope;

    impl Driver for Scope {
        fn name(&self) -> &'static str {
            "scope"
        }
        fn create(&mut self, _i: usize, _bound: u64) -> Option<()> {
            None
        }
        fn start(&mut self, i: usize, bound: u64, program: &[String]) -> Box<dyn Started> {
            let unit = format!("hex-native-bench-{}-{i}", std::process::id());
            Box::new(ChildProc::spawn(
                Command::new("systemd-run")
                    .args(["--user", "--scope", "--quiet", "--unit", &unit])
                    .arg(format!("--property=MemoryMax={}M", bound / MIB))
                    .arg("--property=MemorySwapMax=0")
                    .arg("--")
                    .args(program),
            ))
        }
        fn delete(&mut self, i: usize, _started: &mut dyn Started) {
            let unit = format!("hex-native-bench-{}-{i}.scope", std::process::id());
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &unit])
                .status();
        }
    }

    pub fn run(
        driver: &str,
        args: &[String],
        _hex: &Path,
        go: impl FnOnce(&mut dyn Driver) -> Value,
    ) -> Option<Value> {
        match driver {
            "hex" => {
                let own = match prepare() {
                    Ok(own) => own,
                    Err(e) if std::env::var_os("HEX_DELEGATED").is_none() => {
                        let me = std::env::current_exe().expect("current exe");
                        let status = Command::new("systemd-run")
                            .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                            .arg(&me)
                            .args(args)
                            .env("HEX_DELEGATED", "1")
                            .status()
                            .unwrap_or_else(|s| panic!("no delegated cgroup ({e}): {s}"));
                        assert!(status.success(), "the delegated re-run failed");
                        return None;
                    }
                    Err(e) => panic!("no usable cgroup delegation: {e}"),
                };
                let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME"))
                    .join(".cache/hex-t7/nativebench");
                std::fs::create_dir_all(&cache).expect("cache dir");
                let state = tempfile::Builder::new()
                    .prefix("native-")
                    .tempdir_in(&cache)
                    .expect("a state dir");
                let node = Node::open(&own.join("hex-boxes-native"), state.path(), state.path())
                    .expect("the node agent");
                Some(go(&mut Hex {
                    rt: node,
                    prefix: "native".to_owned(),
                }))
            }
            "scope" => Some(go(&mut Scope)),
            other => panic!("driver {other:?}: Linux runs hex and scope ({BOUND} byte bound)"),
        }
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Instant;

    use hex_runtime::macos::MacRuntime;
    use serde_json::{Value, json};

    use super::bench::{ChildProc, Driver, Hex, MIB, Started};

    /// `taskpolicy -m <MiB>`: the fatal jetsam cap on the one process it starts; delete is a KILL.
    struct Taskpolicy;

    impl Driver for Taskpolicy {
        fn name(&self) -> &'static str {
            "taskpolicy"
        }
        fn create(&mut self, _i: usize, _bound: u64) -> Option<()> {
            None
        }
        fn start(&mut self, _i: usize, bound: u64, program: &[String]) -> Box<dyn Started> {
            Box::new(ChildProc::spawn(
                Command::new("taskpolicy")
                    .args(["-m", &(bound / MIB).to_string()])
                    .args(program),
            ))
        }
        fn delete(&mut self, _i: usize, started: &mut dyn Started) {
            started.kill();
        }
    }

    /// A launchd transient job in the user's gui domain: create is `bootstrap` of its plist (which
    /// starts it, RunAtLoad), so it is timed as start; delete is `bootout`. Its stdout is a FIFO.
    struct Launchd {
        dir: tempfile::TempDir,
        uid: String,
    }

    struct LaunchdProc {
        out: std::io::BufReader<std::fs::File>,
    }

    impl Started for LaunchdProc {
        fn line(&mut self) -> String {
            use std::io::BufRead;
            let mut l = String::new();
            let _ = self.out.read_line(&mut l);
            l
        }
        fn rest(&mut self) -> String {
            use std::io::Read;
            let mut s = String::new();
            let _ = self.out.read_to_string(&mut s);
            s
        }
        fn ended(&mut self) -> String {
            // launchd reaps its job; the FIFO's end (read by `rest`) is the program's.
            "launchd's".to_owned()
        }
    }

    impl Launchd {
        fn label(i: usize) -> String {
            format!("dev.hex.native-bench.{}.{i}", std::process::id())
        }
    }

    impl Driver for Launchd {
        fn name(&self) -> &'static str {
            "launchd"
        }
        fn create(&mut self, _i: usize, _bound: u64) -> Option<()> {
            None
        }
        fn start(&mut self, i: usize, bound: u64, program: &[String]) -> Box<dyn Started> {
            let label = Self::label(i);
            let fifo = self.dir.path().join(format!("{i}.out"));
            let plist = self.dir.path().join(format!("{i}.plist"));
            let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
            assert!(status.success(), "mkfifo");
            let args: String = program
                .iter()
                .map(|a| format!("<string>{a}</string>"))
                .collect();
            std::fs::write(
                &plist,
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array>{args}</array><key>RunAtLoad</key><true/><key>StandardOutPath</key><string>{}</string><key>HardResourceLimits</key><dict><key>ResidentSetSize</key><integer>{bound}</integer></dict></dict></plist>\n",
                    fifo.display()
                ),
            )
            .expect("write the plist");
            let status = Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", self.uid)])
                .arg(&plist)
                .status()
                .expect("launchctl bootstrap");
            assert!(status.success(), "launchctl bootstrap");
            let out = std::fs::File::open(&fifo).expect("open the job's stdout");
            Box::new(LaunchdProc {
                out: std::io::BufReader::new(out),
            })
        }
        fn delete(&mut self, i: usize, _started: &mut dyn Started) {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}/{}", self.uid, Self::label(i))])
                .status();
        }
    }

    /// How fast a member can grow on this Mac, measured at bench start (the runtime's pacing rates,
    /// CLAUDE.md §5): bytes a second, from taking one box's bound in this process and filling every byte
    /// with an incompressible stream, as the bench's climbing program does; processes a second, from
    /// spawning and reaping `n` of the smallest program, `true`, one after another (a member's own spawn
    /// rate, not its program's start-up).
    fn growth_rates(n: usize) -> (NonZeroU64, NonZeroU64) {
        let t = Instant::now();
        let mut touched = vec![0u8; super::bench::BOUND as usize];
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for word in touched.as_chunks_mut::<{ size_of::<u64>() }>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *word = x.to_le_bytes();
        }
        std::hint::black_box(&touched);
        let bytes = (super::bench::BOUND as f64 / t.elapsed().as_secs_f64()) as u64;
        drop(touched);
        let t = Instant::now();
        for _ in 0..n {
            Command::new("true")
                .stdout(std::process::Stdio::null())
                .status()
                .expect("spawn a program");
        }
        let procs = (n as f64 / t.elapsed().as_secs_f64()) as u64;
        (
            NonZeroU64::new(bytes).expect("a measured byte rate"),
            NonZeroU64::new(procs).expect("a measured process rate"),
        )
    }

    pub fn run(
        driver: &str,
        args: &[String],
        _hex: &Path,
        go: impl FnOnce(&mut dyn Driver) -> Value,
    ) -> Option<Value> {
        match driver {
            "hex" => {
                let n: usize = args[2].parse().expect("n");
                let (bytes, procs) = growth_rates(n);
                let mut v = go(&mut Hex {
                    rt: MacRuntime::new(
                        bytes,
                        procs,
                        hex_core::watch::lateness_seed(
                            &hex_runtime::macos::machine_model().expect("this Mac's model"),
                        ),
                    )
                    .expect("a macOS runtime"),
                    prefix: "native".to_owned(),
                });
                if let Value::Object(m) = &mut v {
                    m.insert(
                        "growth_rates".to_owned(),
                        json!({"bytes_per_second": bytes.get(), "processes_per_second": procs.get()}),
                    );
                }
                Some(v)
            }
            "taskpolicy" => Some(go(&mut Taskpolicy)),
            "launchd" => {
                let uid = Command::new("id").arg("-u").output().expect("id -u");
                let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME"))
                    .join(".cache/hex-t7/nativebench");
                std::fs::create_dir_all(&cache).expect("cache dir");
                Some(go(&mut Launchd {
                    dir: tempfile::Builder::new()
                        .prefix("launchd-")
                        .tempdir_in(&cache)
                        .expect("a plist dir"),
                    uid: String::from_utf8_lossy(&uid.stdout).trim().to_owned(),
                }))
            }
            other => panic!("driver {other:?}: macOS runs hex, taskpolicy and launchd"),
        }
    }
}

#[cfg(target_os = "windows")]
mod os {
    use std::path::Path;
    use std::process::Command;

    use hex_runtime::windows::WinRuntime;
    use serde_json::Value;

    use super::bench::{ChildProc, Driver, Hex, MIB, Started};

    /// Process Governor: `procgov --maxjobmem <M> -- <program>`, a job object it makes; delete ends
    /// the tree with `taskkill /T /F` of procgov's process ([`Started::kill`]).
    struct Procgov;

    impl Driver for Procgov {
        fn name(&self) -> &'static str {
            "procgov"
        }
        fn create(&mut self, _i: usize, _bound: u64) -> Option<()> {
            None
        }
        fn start(&mut self, _i: usize, bound: u64, program: &[String]) -> Box<dyn Started> {
            Box::new(ChildProc::spawn(
                // -q: procgov's banner on stdout would be read as the program's first line (jupiter,
                // bench/install/records/windows-jupiter-20261105.txt entry 7).
                Command::new("procgov")
                    .args(["-q", "--maxjobmem", &format!("{}M", bound / MIB), "--"])
                    .args(program),
            ))
        }
        fn delete(&mut self, _i: usize, started: &mut dyn Started) {
            started.kill();
        }
    }

    pub fn run(
        driver: &str,
        _args: &[String],
        _hex: &Path,
        go: impl FnOnce(&mut dyn Driver) -> Value,
    ) -> Option<Value> {
        match driver {
            "hex" => Some(go(&mut Hex {
                rt: WinRuntime::new(),
                prefix: "native".to_owned(),
            })),
            "procgov" => Some(go(&mut Procgov)),
            other => panic!("driver {other:?}: Windows runs hex and procgov"),
        }
    }
}
