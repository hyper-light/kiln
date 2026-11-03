//! CP-T10.2 and W27-R08 (docs/blobs.md, "The Mantle store"; the Transfers plan's step 12):
//! kiln's Mantle store against a cell of three real Mantle processes on loopback, never
//! production's. The tests are ignored: they start the processes of the executable
//! `KILN_TEST_MANTLE_BIN` names, Mantle's release build from `vendor/mantle` (`cargo build
//! --release -p mantle` there; a debug cell is too slow for the suite's clause of 1001 objects).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use kiln::blobs::mantle::{Config, MantleBlobs};
use kiln_seams::blobs::{self as seam, Blobs, BlobsFixture, Method, Put, Refusal};
use serde_json::Value;

/// The largest object the tests' store takes, as the simulator's fixture's.
const OBJECT: u64 = 24 << 20;

struct Node { id: u64, child: Child }
struct Cell {
    root: tempfile::TempDir,
    binary: PathBuf,
    nodes: Vec<Node>,
    bound: BTreeMap<u64, (SocketAddr, SocketAddr)>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl Drop for Cell {
    fn drop(&mut self) {
        // Each Child is a PID this fixture started and retained, never a process-name search.
        for node in &mut self.nodes { let _ = node.child.kill(); let _ = node.child.wait(); }
        for thread in self.threads.drain(..) { let _ = thread.join(); }
    }
}
impl Cell {
    fn new() -> anyhow::Result<Self> {
        let binary = std::env::var_os("KILN_TEST_MANTLE_BIN").map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("set KILN_TEST_MANTLE_BIN to the vendored mantle executable"))?;
        let mut cell = Self { root: tempfile::Builder::new().prefix("kiln-mantle-").tempdir()?, binary, nodes: Vec::new(), bound: BTreeMap::new(), threads: Vec::new() };
        let founders = cell.root.path().join("founders");
        let mut text = String::from("max_nodes = 3\nmax_volumes = 3\n");
        for n in 0..3 {
            let device = cell.root.path().join(format!("device-{n}"));
            std::fs::create_dir(&device)?;
            text.push_str(&format!("[node]\nquic = 127.0.0.1:0\nplane = 127.0.0.1:0\ndomain = rack-{n}\nvolume = {} 2G\nmemory = 1536M\n", device.display()));
        }
        std::fs::write(&founders, text)?;
        let out = cell.root.path().join("cell");
        let init = Command::new(&cell.binary).arg("init").arg(&out).arg("--founders").arg(&founders).output()?;
        anyhow::ensure!(init.status.success(), "mantle init exited {}", init.status);
        let mut made = Vec::new();
        for line in String::from_utf8(init.stdout)?.lines() {
            if let Some(rest) = line.strip_prefix("node ") {
                let (id, rest) = rest.split_once(' ').ok_or_else(|| anyhow::anyhow!("node report has no directory"))?;
                let (dir, _) = rest.split_once(": ").ok_or_else(|| anyhow::anyhow!("node report has no directory end"))?;
                made.push((id.parse::<u64>()?, PathBuf::from(dir)));
            }
        }
        anyhow::ensure!(made.len() == 3, "mantle init did not make three founders");
        let (tx, rx) = mpsc::sync_channel(16);
        for (id, dir) in made {
            let err = std::fs::File::create(cell.root.path().join(format!("node-{id}.stderr")))?;
            let mut child = Command::new(&cell.binary).arg("serve").arg(dir.join("config")).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(err).spawn()?;
            let pid = child.id();
            let output = child.stdout.take();
            cell.nodes.push(Node { id, child });
            std::fs::write(cell.root.path().join(format!("node-{id}.pid")), pid.to_string())?;
            let output = output.ok_or_else(|| anyhow::anyhow!("node has no output pipe"))?;
            let mut log = std::fs::File::create(cell.root.path().join(format!("node-{id}.stdout")))?;
            let tx = tx.clone();
            cell.threads.push(std::thread::spawn(move || {
                for line in std::io::BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    let _ = writeln!(log, "{line}");
                    if line.starts_with("bound ") || line == "enlisted" { let _ = tx.try_send((id, line)); }
                }
            }));
        }
        drop(tx);
        if let Err(e) = cell.ready(&rx) {
            anyhow::bail!("{e:#}\n{}", cell.said());
        }
        Ok(cell)
    }
    /// Waits until each node listens, tells each where the others do, and waits until each has
    /// enlisted its volume.
    fn ready(&mut self, rx: &mpsc::Receiver<(u64, String)>) -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(180);
        let mut enlisted = BTreeSet::new();
        let mut introduced = false;
        while self.bound.len() != 3 || enlisted.len() != 3 {
            let remaining = until.saturating_duration_since(Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "Mantle cell did not become ready ({} bound, {} enlisted)", self.bound.len(), enlisted.len());
            let (id, line) = rx.recv_timeout(remaining).map_err(|e| anyhow::anyhow!("Mantle startup: {e}; {} bound, {} enlisted", self.bound.len(), enlisted.len()))?;
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["bound", quic, plane] => {
                    self.bound.insert(id, (quic.parse()?, plane.parse()?));
                }
                ["enlisted"] => {
                    enlisted.insert(id);
                }
                _ => {}
            }
            if self.bound.len() == 3 && !introduced {
                for node in &mut self.nodes {
                    let input = node.child.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("node has no input pipe"))?;
                    for (peer, (quic, plane)) in &self.bound {
                        if *peer != node.id {
                            writeln!(input, "learn {peer} {quic} {plane}")?;
                        }
                    }
                    input.flush()?;
                }
                introduced = true;
            }
        }
        Ok(())
    }
    /// What each node said last, on standard error and output, and whether it ended: a cell that
    /// didn't start says why.
    fn said(&mut self) -> String {
        let root = self.root.path().to_owned();
        let tail = |file: &str| {
            let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            lines.get(lines.len().saturating_sub(12)..).unwrap_or_default().join("\n    ")
        };
        let mut out = String::new();
        for node in &mut self.nodes {
            let status = node.child.try_wait().ok().flatten().map_or_else(|| "running".to_owned(), |s| s.to_string());
            out.push_str(&format!("node {} ({status}):\n  stderr:\n    {}\n  stdout:\n    {}\n", node.id, tail(&format!("node-{}.stderr", node.id)), tail(&format!("node-{}.stdout", node.id))));
        }
        out
    }
    fn client(&self, name: &str) -> anyhow::Result<PathBuf> {
        let dir = self.root.path().join(name);
        let result = Command::new(&self.binary).args(["init", "client"]).arg(&dir).arg("--cell").arg(self.root.path().join("cell")).output()?;
        anyhow::ensure!(result.status.success(), "mantle init client exited {}", result.status);
        let path = dir.join("config");
        let mut text: String = std::fs::read_to_string(&path)?.lines().filter(|l| !l.starts_with("node = ")).map(|l| format!("{l}\n")).collect();
        for (id, (quic, _)) in &self.bound { text.push_str(&format!("node = {id} {quic}\n")); }
        std::fs::write(path, text)?;
        Ok(dir)
    }
    fn blobs(&self, prefix: &str) -> anyhow::Result<MantleBlobs> {
        let native = Config { client: self.client("kiln-client")?, prefix: prefix.into(), create_bucket: true, ..Config::default() };
        // CP-T10.2: exercise the actual kiln.toml selection and startup path too.
        let text = format!("[blobs]\nstore = \"mantle\"\nobject_max = {OBJECT}\n[blobs.mantle]\n{}", toml::to_string(&native)?);
        let mut config: kiln::config::Config = toml::from_str(&text)?;
        config.server.data_dir = self.root.path().join("kiln");
        config.server.public_url = "http://kiln.test".into();
        let opened = kiln::blobs::open(&config, &kiln::store::Store::in_memory()?, kiln::rt::clock())?;
        opened.mantle.map(|b| b.as_ref().clone()).ok_or_else(|| anyhow::anyhow!("config did not open Mantle"))
    }
    /// The uploads the cell holds open in the bucket, as `mantle multipart uploads` lists them
    /// (`<date> <upload> <key>`), through a client of its own.
    #[cfg(feature = "chaos")]
    fn uploads(&self) -> anyhow::Result<Vec<String>> {
        let client = self.client("lister-client")?;
        let result = Command::new(&self.binary).arg("--client").arg(client).args(["multipart", "uploads", "s3://kiln"]).output()?;
        anyhow::ensure!(result.status.success(), "mantle multipart uploads exited {}: {}", result.status, String::from_utf8_lossy(&result.stderr));
        Ok(String::from_utf8(result.stdout)?.lines().map(str::to_owned).collect())
    }
    /// Alter an actual cell object through a separately issued client, with valid new native
    /// checksums. kiln's pointer still names the SHA-256 it originally handed on.
    fn alter(&self, prefix: &str, sha: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let client = self.client("other-client")?;
        let file = self.root.path().join("changed");
        std::fs::write(&file, bytes)?;
        let result = Command::new(&self.binary).arg("--client").arg(client).arg("cp").arg(file).arg(format!("s3://kiln/{prefix}/d/{sha}")).output()?;
        anyhow::ensure!(result.status.success(), "native overwrite exited {}: {}", result.status, String::from_utf8_lossy(&result.stderr));
        Ok(())
    }
}
struct Fixture { blobs: MantleBlobs }
impl BlobsFixture for Fixture {
    fn blobs(&self) -> &dyn Blobs { &self.blobs }
    fn part_min(&self) -> usize { seam::PART_MIN }
    fn object_max(&self) -> u64 { OBJECT }
    fn resolution(&self) -> Duration { Duration::from_secs(1) }
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()> { Box::pin(kiln::rt::sleep(d)) }
    fn redeem<'a>(&'a self, url: &'a str, method: Method, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>> { Box::pin(self.blobs.redeem(url, method, body)) }
}
/// Runs kiln-seams' Blobs suite against `blobs`, each clause on a prefix of its own under `under`,
/// within 300 s as the S3 store's suite runs (`KILN_TEST_BLOBS_DEADLINE_S` says otherwise), saying
/// how long each took; `KILN_TEST_BLOBS_CLAUSES` (`BLOBS-90,BLOBS-91`) runs those alone. How many
/// clauses passed, every one asked, or each that didn't and why.
fn suite(blobs: MantleBlobs, under: &'static str) -> anyhow::Result<usize> {
    let deadline = Duration::from_secs(std::env::var("KILN_TEST_BLOBS_DEADLINE_S").ok().and_then(|s| s.parse().ok()).unwrap_or(300));
    let only: Option<BTreeSet<String>> = std::env::var("KILN_TEST_BLOBS_CLAUSES").ok().map(|s| s.split(',').map(|c| c.trim().to_owned()).collect());
    let cases: Vec<_> = kiln_seams::blobs::cases::<Fixture>().into_iter().enumerate().filter(|(_, c)| only.as_ref().is_none_or(|o| o.contains(c.id))).collect();
    let asked = cases.len();
    let report = kiln::rt::block_on(async move {
        let mut report = kiln_seams::conformance::Report { seam: "Blobs", ..Default::default() };
        for (n, case) in cases {
            let fixture = Fixture { blobs: blobs.with_prefix(&format!("{under}/{}", n.saturating_add(1)))? };
            let began = Instant::now();
            match kiln::rt::timeout(deadline, (case.check)(&fixture)).await {
                Ok(Ok(())) => {
                    report.passed.push(case.id);
                    println!("{} passed in {:.1} s", case.id, began.elapsed().as_secs_f64());
                }
                Ok(Err(why)) => {
                    println!("{} failed in {:.1} s: {why}", case.id, began.elapsed().as_secs_f64());
                    report.failed.push((case.id, why));
                }
                Err(_) => {
                    println!("{} timed out at {} s", case.id, deadline.as_secs());
                    report.failed.push((case.id, format!("deadline of {} s", deadline.as_secs())));
                }
            }
        }
        Ok(report)
    })?;
    let passed = report.into_result().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(passed == asked, "{passed} of the {asked} clauses asked ran");
    Ok(passed)
}

/// CP-T10.2: kiln-seams' Blobs suite, every clause, over Mantle's native protocol on a cell of
/// three.
#[test]
#[ignore = "starts three vendored Mantle processes; KILN_TEST_MANTLE_BIN required"]
fn cp_t10_2_native_mantle_passes_the_blobs_suite() -> anyhow::Result<()> {
    let cell = Cell::new()?;
    let passed = suite(cell.blobs("suite")?, "suite")?;
    println!("CP-T10.2: {passed} Blobs clauses passed over native QUIC on three nodes");
    Ok(())
}

/// CP-T10.2: the suite again, the cell's first answer to each of kiln's mutations lost on its way
/// back after the cell applied it (chaos `mantle_lost`): a put, a pointer's write, a delete, and
/// the last step of each large write (a native upload's Finish), asked again, read as done and
/// change nothing more; and the cell is left no upload of its own open, as a lost Finish begun
/// again as a new upload would leave one.
#[cfg(feature = "chaos")]
#[test]
#[ignore = "starts three vendored Mantle processes; KILN_TEST_MANTLE_BIN and --features chaos required"]
fn cp_t10_2_native_mantle_answers_a_lost_answer_from_what_it_made() -> anyhow::Result<()> {
    kiln::runner::chaos::set(kiln::runner::chaos::parse("mantle_lost")?);
    let cell = Cell::new()?;
    let passed = suite(cell.blobs("lost")?, "lost");
    // Looked for whatever the clauses came to.
    let open: Vec<String> = cell.uploads()?.into_iter().filter(|line| line.contains(" lost/")).collect();
    println!("CP-T10.2: uploads the cell holds open under lost/: {}", open.len());
    let passed = passed?;
    anyhow::ensure!(open.is_empty(), "uploads the cell holds open: {open:?}");
    println!("CP-T10.2: {passed} Blobs clauses passed with every mutation's first answer lost; no upload left open");
    Ok(())
}
/// W27-R08: kiln's Mantle hand-on and receipts walk by SHA-256; changed cell bytes are refused
/// and recorded even though the backend never gives the corrupt body to the custody wrapper.
#[test]
#[ignore = "starts three vendored Mantle processes; KILN_TEST_MANTLE_BIN required"]
fn w27_r08_native_mantle_signs_put_and_read_and_records_changed_bytes() -> anyhow::Result<()> {
    let cell = Cell::new()?;
    let native = cell.blobs("custody")?;
    let root = tempfile::tempdir()?;
    let store = kiln::store::Store::in_memory()?;
    let pki = Arc::new(kiln::pki::Pki::open(&root.path().join("pki"))?);
    let etag = kiln::util::sha256_hex(b"the app, built");
    let key = "artifacts/7/app";
    let blobs: Arc<dyn Blobs> = Arc::new(kiln::custody::blobs::Attested::new(Arc::new(native), "mantle", store.clone(), Arc::clone(&pki)));
    let (kept, identity, reader) = (store.clone(), Arc::clone(&pki), Arc::clone(&blobs));
    let expected = etag.clone();
    kiln::rt::block_on(async move {
        kiln::identities::own_identities(&kept, &identity).await?;
        let meta = reader.put(key, b"the app, built".to_vec(), Put::default()).await?;
        anyhow::ensure!(meta.etag == expected, "put did not answer the content's SHA-256");
        let (_, bytes) = reader.get(key, None).await?;
        anyhow::ensure!(bytes == b"the app, built", "whole read changed");
        let chain = kiln::custody::chain(&kept, &identity.ca_der, &identity.organization(), &format!("sha256:{expected}"), kiln::util::now_unix()).await?.ok_or_else(|| anyhow::anyhow!("no custody chain"))?;
        anyhow::ensure!(chain.get("unbroken") == Some(&Value::Bool(true)), "{chain}");
        let hops: Vec<_> = chain.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (h.get("kind").and_then(Value::as_str).unwrap_or_default().to_owned(), h.get("taken_for").and_then(Value::as_str).map(str::to_owned))).collect();
        let uri = format!("kiln://{}/blobs/mantle", identity.organization());
        anyhow::ensure!(hops == vec![("received".into(), None), ("taken".into(), Some(uri.clone())), ("received".into(), Some(uri)), ("handed-on".into(), None)], "{chain}");
        Ok::<_, anyhow::Error>(())
    })?;
    cell.alter("custody", &etag, b"the app, wrong")?;
    kiln::rt::block_on(async move {
        let result = blobs.get(key, None).await;
        anyhow::ensure!(matches!(result, Err(Refusal::BadChecksum { .. })), "changed object was not refused: {result:?}");
        let breaks = kiln::custody::breaks(&store, 10).await?;
        anyhow::ensure!(breaks.iter().any(|b| b.get("kind").and_then(Value::as_str) == Some("changed") && b.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains(key))), "no recorded object break: {breaks:?}");
        Ok(())
    })
}
