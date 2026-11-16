//! A stand-in for the machine's microVMs in tests (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step
//! 17): images whose members are programs of this machine, each launch kept as it was asked, its
//! member run as the seam's host implementation runs a program, named as a microVM of its own and its
//! stdio said to ride the link its runs are given. What it stands in for is Shards' image boot through
//! Hex; what it holds a session to is the seam's contract alone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use futures::future::BoxFuture;
use kiln_seams::sandbox::{Boundary, Change, Egress, Holds, Launch, Limits, Link, NO_POOLS, Output, Pool, PoolHeld, Refusal, Run, Sandbox, Signal};
use parking_lot::Mutex;

use super::host::HostSandbox;

/// Images, by name, of one member each, which runs `program`.
pub struct FakeVms {
    images: Arc<Vec<(String, String, PathBuf)>>,
    launched: Mutex<Vec<Launch>>,
    /// The link each run's stdio is said to ride, once it's started.
    link: Option<Link>,
    /// Each change a run took while it ran (`Run::change`), by its microVM's name, in order.
    changed: Arc<Mutex<Vec<(String, Change)>>>,
    /// Each member swapped into a microVM (`Run::swap`), by its microVM's name, in order.
    swapped: Arc<Mutex<Vec<(String, Launch)>>>,
    /// It swaps a member for another in place (`Holds::swaps`): off unless asked ([`FakeVms::swapping`]).
    swaps: bool,
    /// It lays components over its images' own (`Holds::components`): off unless asked ([`FakeVms::laying`]).
    components: bool,
    /// It keeps warm pools for its launches (`Holds::pools`): off unless asked ([`FakeVms::pooling`]).
    pooling: bool,
    /// The pools it keeps now.
    pools: Mutex<BTreeSet<Pool>>,
    /// Each pool it was asked to keep (`keep`) or let go of (`let go`), in order.
    pool_asks: Mutex<Vec<(&'static str, Pool)>>,
}

/// What a stand-in member's program is told of the components laid for it (`Launch::components`): each `<at> <source>`,
/// `;` apart.
pub const COMPONENTS: &str = "KILN_FAKE_COMPONENTS";

/// `launch` as its member's program runs it on this machine: `program`, with nothing of a microVM's, and told the
/// components laid for it.
fn on_host(launch: Launch, program: String) -> Launch {
    let mut env = launch.env.clone();
    if !launch.components.is_empty() {
        env.push((COMPONENTS.to_owned(), launch.components.iter().map(|c| format!("{} {}", c.at, c.component.source)).collect::<Vec<_>>().join(";")));
    }
    Launch { program, shares: Vec::new(), limits: Limits::NONE, egress: Egress::Any, boundary: Boundary::Host, components: Vec::new(), env, ..launch }
}

/// The program `image`'s `member` runs, of `images`.
fn program_of(images: &[(String, String, PathBuf)], image: &str, member: &str) -> Result<String, Refusal> {
    images.iter().find(|(i, m, _)| i == image && m == member).map(|(_, _, p)| p.to_string_lossy().into_owned()).ok_or_else(|| Refusal::NoSuchProgram(format!("{image}'s {member}")))
}

impl FakeVms {
    /// One image, `image`, whose member `member` runs `program`.
    pub fn with(image: &str, member: &str, program: &Path) -> Self {
        Self {
            images: Arc::new(vec![(image.to_owned(), member.to_owned(), program.to_path_buf())]),
            launched: Mutex::new(Vec::new()),
            link: None,
            changed: Arc::default(),
            swapped: Arc::default(),
            swaps: false,
            components: false,
            pooling: false,
            pools: Mutex::new(BTreeSet::new()),
            pool_asks: Mutex::new(Vec::new()),
        }
    }

    /// It keeps warm pools for its launches, as Hex's will (kiln plan 01a1dd2e…, P3.1): each holding one host warm, as
    /// Shards' demand rule keeps one at least while its pool is in use.
    pub fn pooling(self) -> Self {
        Self { pooling: true, ..self }
    }

    /// Each pool it was asked to keep (`keep`) or let go of (`let go`), in order.
    pub fn pool_asks(&self) -> Vec<(&'static str, Pool)> {
        self.pool_asks.lock().clone()
    }

    /// It swaps a member for another in its microVM, as Hex's will (P6c.2).
    pub fn swapping(self) -> Self {
        Self { swaps: true, ..self }
    }

    /// It lays components over its images' own, at a launch and while a run runs, as Hex's will (P6c.2): a member's
    /// program is told those laid for it in [`COMPONENTS`].
    pub fn laying(self) -> Self {
        Self { components: true, ..self }
    }

    /// Another image, `image`, whose member `member` runs `program`.
    pub fn and(self, image: &str, member: &str, program: &Path) -> Self {
        let mut images: Vec<(String, String, PathBuf)> = self.images.iter().cloned().collect();
        images.push((image.to_owned(), member.to_owned(), program.to_path_buf()));
        Self { images: Arc::new(images), ..self }
    }

    /// Each member swapped into one of its microVMs, by its microVM's name, in order.
    pub fn swapped(&self) -> Vec<(String, Launch)> {
        self.swapped.lock().clone()
    }

    /// Its runs' stdio said to ride `link`.
    pub fn linked(self, link: Link) -> Self {
        Self { link: Some(link), ..self }
    }

    /// Every launch asked of it, in order.
    pub fn launched(&self) -> Vec<Launch> {
        self.launched.lock().clone()
    }

    /// Each change its runs took while they ran, by its microVM's name, in order: what the stand-in runtime records of a
    /// session's live overrides.
    pub fn changed(&self) -> Vec<(String, Change)> {
        self.changed.lock().clone()
    }
}

/// A stand-in for kiln's MCP lane through Hex (Shards D254's controller, `Execs::mcp`): the connections its runs'
/// harnesses make, handed out in turn, each to whichever run asks or to its own run alone, then `Ended`; and each
/// run asked for one, in order.
#[derive(Default)]
pub struct FakeLanes {
    conns: Mutex<std::collections::VecDeque<(Option<String>, super::McpConnection)>>,
    asked: Mutex<Vec<String>>,
}

impl FakeLanes {
    /// Handing out `conns` in turn, to whichever run asks.
    pub fn of(conns: Vec<super::McpConnection>) -> Self {
        Self { conns: Mutex::new(conns.into_iter().map(|c| (None, c)).collect()), asked: Mutex::new(Vec::new()) }
    }

    /// Handing out each of `conns` to its own run alone, as Hex's lane takes a run's connections by its box and member
    /// (`Runtime::mcp`).
    pub fn per_run(conns: Vec<(&str, super::McpConnection)>) -> Self {
        Self { conns: Mutex::new(conns.into_iter().map(|(run, c)| (Some(run.to_owned()), c)).collect()), asked: Mutex::new(Vec::new()) }
    }

    /// Each run a connection was asked for, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

impl super::Lanes for FakeLanes {
    fn carried(&self) -> Result<(), Refusal> {
        Ok(())
    }

    fn next(&self, run: &str) -> Result<super::McpConnection, Refusal> {
        self.asked.lock().push(run.to_owned());
        let mut conns = self.conns.lock();
        let at = conns.iter().position(|(owner, _)| owner.as_deref().is_none_or(|o| o == run)).ok_or(Refusal::Ended)?;
        conns.remove(at).map(|(_, c)| c).ok_or(Refusal::Ended)
    }
}

/// What a stand-in connection is sent, kept to be read.
#[derive(Clone, Default)]
pub struct Said(pub std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Said {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Said {
    /// Each JSON line said so far.
    pub fn lines(&self) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(&self.0.lock()).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}

/// A stand-in connection that sends `lines`, each a JSON-RPC message, then ends; what it's sent kept in the [`Said`].
pub fn connection(lines: &[serde_json::Value]) -> (super::McpConnection, Said) {
    let input: String = lines.iter().map(|v| format!("{v}\n")).collect();
    let said = Said::default();
    (super::McpConnection { from: Box::new(std::io::Cursor::new(input.into_bytes())), to: Box::new(said.clone()) }, said)
}

impl Sandbox for FakeVms {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            let n = {
                let mut launched = self.launched.lock();
                launched.push(launch.clone());
                launched.len()
            };
            let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a run outside a microVM".into())) };
            if !launch.components.is_empty() && !self.components {
                return Err(Refusal::Unsupported("components laid over its image's own".into()));
            }
            let program = program_of(&self.images, image, &launch.program)?;
            let run = HostSandbox.launch(on_host(launch, program)).await?;
            Ok(Box::new(FakeRun { run, id: format!("fake-vm-{n}"), link: self.link.clone(), changed: Arc::clone(&self.changed), images: Arc::clone(&self.images), swapped: Arc::clone(&self.swapped), components: self.components }) as Box<dyn Run>)
        })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["microvm"]
    }

    fn holds(&self) -> Holds {
        Holds { live: true, swaps: self.swaps, components: self.components, pools: self.pooling, ..super::host::HOLDS }
    }

    fn keep(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = if self.pooling {
            self.pool_asks.lock().push(("keep", pool.clone()));
            self.pools.lock().insert(pool);
            Ok(())
        } else {
            Err(Refusal::Unsupported(NO_POOLS.into()))
        };
        Box::pin(async move { out })
    }

    fn let_go(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = if self.pooling {
            self.pool_asks.lock().push(("let go", pool.clone()));
            self.pools.lock().remove(&pool);
            Ok(())
        } else {
            Err(Refusal::Unsupported(NO_POOLS.into()))
        };
        Box::pin(async move { out })
    }

    fn pools(&self) -> Vec<PoolHeld> {
        self.pools.lock().iter().map(|pool| PoolHeld { pool: pool.clone(), warm: 1, starting: 0 }).collect()
    }
}

/// A member's run in its stand-in microVM: the program's run, named as the microVM it stands in for.
struct FakeRun {
    run: Box<dyn Run>,
    id: String,
    link: Option<Link>,
    changed: Arc<Mutex<Vec<(String, Change)>>>,
    images: Arc<Vec<(String, String, PathBuf)>>,
    swapped: Arc<Mutex<Vec<(String, Launch)>>>,
    /// Its microVMs lay components (`Holds::components`).
    components: bool,
}

impl Run for FakeRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        self.run.stdin(bytes)
    }

    fn close_stdin(&self) {
        self.run.close_stdin();
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        self.run.next()
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        self.run.signal(signal)
    }

    fn kill(&self) {
        self.run.kill();
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        let link = self.link.clone();
        Box::pin(async move { link })
    }

    /// Taken and recorded, as Hex's wire calls will take one (P6b.3), a connection's grant checked as the seam has it
    /// (Shards' D285); a held run that ended refuses one before it comes here (`Held::change`).
    fn change(&self, change: Change) -> BoxFuture<'_, Result<(), Refusal>> {
        if let Change::Connect { grant, .. } | Change::Disconnect { grant, .. } = &change
            && let Err(e) = grant.check()
        {
            return Box::pin(async move { Err(e) });
        }
        if matches!(change, Change::Swap { .. } | Change::Restore { .. }) && !self.components {
            return Box::pin(async { Err(Refusal::Unsupported("a component swapped into a run while it runs".into())) });
        }
        self.changed.lock().push((self.id.clone(), change));
        Box::pin(async { Ok(()) })
    }

    /// Its member ends as its input does, and `launch`'s starts in the same microVM (the same name), as Hex's wire
    /// calls will swap one (P6c.2); recorded.
    fn swap(&self, launch: Launch) -> BoxFuture<'_, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a member swapped in from outside a microVM".into())) };
            if !launch.components.is_empty() && !self.components {
                return Err(Refusal::Unsupported("components laid over its image's own".into()));
            }
            let program = program_of(&self.images, image, &launch.program)?;
            self.swapped.lock().push((self.id.clone(), launch.clone()));
            self.run.close_stdin();
            let run = HostSandbox.launch(on_host(launch, program)).await?;
            Ok(Box::new(FakeRun { run, id: self.id.clone(), link: self.link.clone(), changed: Arc::clone(&self.changed), images: Arc::clone(&self.images), swapped: Arc::clone(&self.swapped), components: self.components }) as Box<dyn Run>)
        })
    }
}
