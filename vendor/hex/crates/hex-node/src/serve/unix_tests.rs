//! The socket's tests that hold on each OS it serves (Linux's sequenced packets, macOS's framed stream): a live
//! server in this process, reached through `hex-client`.

use super::*;

use std::io::{Read, Write as _};
use std::os::unix::net::UnixStream;

use hex_api::sim::SimRuntime;
use hex_api::{Ask, Bounds, BoxInfo, BoxSpec, ClassSpec, Signal as HexSignal, Usage};
use hex_client::Client;

/// The probe's count of sends (HEX-GUEST-41, at 31d5b387): one end of a Unix socket pair sent 20,000 times while
/// another thread made and closed Unix socket pairs (140,461 of them), on macOS 26.4 (XNU 25.4.0). With the sender
/// closing its copy at once, 719 arrived dead; with it kept until received, 0.
const PROBED: usize = 20_000;

/// A program that doesn't end while the test runs: a `sleep` the test started, a child of this process as the node's
/// programs are of the node's (the server watches no other process: GPU run r1119), which its kill leaves be.
struct Unending(u32);

impl Proc for Unending {
    fn pid(&self) -> u32 {
        self.0
    }
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        Ok(None)
    }
    fn wait_for(&mut self, _timeout: Duration) -> Result<Option<Exit>, Refusal> {
        Ok(None)
    }
    fn kill(&mut self) -> Result<(), Refusal> {
        Ok(())
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// Boxes in the simulator; each program [`Unending`], the one `sleep` whose pid is `program`, and the stdout an exec
/// asks for one end of a Unix socket pair whose other end has written one byte and closed: a guest program's streams
/// are connections like it.
struct SocketHost {
    sim: SimRuntime,
    program: u32,
}

impl Runtime for SocketHost {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, _program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.inspect(id)?;
        let mut given = hex_api::Given::default();
        if stdio.ask.stdout {
            let (mut near, far) =
                UnixStream::pair().map_err(|e| Refusal::os("pairing a stream", e))?;
            near.write_all(b"x")
                .map_err(|e| Refusal::os("writing a stream", e))?;
            given.stdout = Some(far.into());
        }
        Ok(Box::new(hex_api::Asked::new(Unending(self.program), given)))
    }
    /// The run's box in the simulator, its program the exec's, as the other hosts' run is.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let id = spec.id.clone();
        self.sim.create(spec)?;
        self.exec(&id, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: HexSignal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

impl Host for SocketHost {
    fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(netns::Counts::default())
    }
}

fn limits() -> Limits {
    Limits {
        connections: 4,
        message_bytes: 4096,
        in_flight: 4,
        programs: 4,
    }
}

fn b() -> BoxId {
    BoxId::new("b").unwrap()
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): a given answer's sockets arrive alive while other Unix sockets
/// close. XNU's collector of descriptors in flight (`unp_gc`, run as a Unix socket closes while any are in flight)
/// flushes a socket whose only reference is a message in flight, which arrives reading nothing: the probe's 719 of
/// [`PROBED`]. The node keeps its copies of a given answer's ends until its client says it took them, so as many
/// sent under the same churn each arrive with their byte. Linux's collector takes only what no socket can reach, so
/// there this holds either way.
#[test]
fn a_given_answers_sockets_arrive_alive_while_other_unix_sockets_close() {
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/serve");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("sock-")
        .tempdir_in(&cache)
        .unwrap();
    let mut program = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .unwrap();
    let host = Arc::new(SocketHost {
        sim: SimRuntime::default(),
        program: program.id(),
    });
    host.sim
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: b(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        })
        .unwrap();
    let server = Server::bind(
        &dir.path().join("node.sock"),
        Arc::clone(&host) as Arc<dyn Host>,
        limits(),
        8,
    )
    .unwrap();
    let client = Client::connect(server.path()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut closed = 0usize;
            while !stop.load(Ordering::Relaxed) {
                drop(UnixStream::pair().unwrap());
                closed += 1;
            }
            closed
        })
    };
    let mut dead = 0usize;
    for _ in 0..PROBED {
        let asked = Stdio {
            ask: Ask {
                stdout: true,
                ..Ask::default()
            },
            ..Stdio::null()
        };
        let mut proc = client
            .exec(&b(), Program::of(&["unending"]), asked)
            .unwrap();
        let stdout = proc.streams().unwrap().stdout.unwrap();
        let mut got = Vec::new();
        let read = std::fs::File::from(stdout).read_to_end(&mut got);
        if !(matches!(read, Ok(1)) && got == b"x") {
            dead += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let closed = churn.join().unwrap();
    // The server first, so the program's end, which this test makes, reaches no watcher of it.
    drop((client, server));
    let _ = program.kill();
    let _ = program.wait();
    assert_eq!(
        dead, 0,
        "{dead} of {PROBED} given sockets arrived dead while {closed} other Unix socket pairs closed"
    );
}
