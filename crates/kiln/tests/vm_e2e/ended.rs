//! `HexVms::finish` on real microVMs (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b, a9fbded98): a box whose run
//! ended on its own before its delete is deleted, charged the use last read. Hex alone, asked to delete such a box with
//! its final use, answers `NotRunning` and leaves the box as it was (its image class's `ended_first`), where kiln's
//! delete stopped before a9fbded98.
//!
//! - **A replay's box** (`HexVms::boot_command`): its microVM's own command ended.
//! - **A session's box** (`HexVms::launch_as`, as an isolated session launches): its harness member's microVM failed
//!   under it, its VM process killed here as a VM that dies is. Its end is said once its box is gone. A session whose
//!   harness ends on its own leaves its run up (Shards' `ATTEND` holds it until it's stopped), so its delete reads its
//!   final use as before.
//!
//! Each is held to its box gone from Hex's list, no VM process of it left, and its charge in kiln's records. The
//! stand-in harness (`tests/fixtures/vm-stub/harness`) is built from this checkout's build context into the test's own
//! Shards home, on kiln's images' base (`VM_E2E_SHARDS_STORE`'s copy where it's given, else pulled once). It skips
//! where this kiln carries no Shards or this host runs no microVMs; `VM_E2E=1` makes that a failure.
//!
//!   cargo test -p kiln --test vm_harnesses ended:: -- --nocapture --test-threads 1

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hex_api::{BoxState, Refusal as H, Runtime as _};
use hex_node::images::ImageRuntime;
use kiln::sandbox::hexvm::{Charge, Charged, HexVms, USE};
use kiln_ci::place::holder::BoxName;
use kiln_seams::Records as _;
use kiln_seams::records::{Consistency, SimRecords};
use kiln_seams::sandbox::{Boundary, Egress, Launch, Limits, Output};

use crate::vm_e2e;

/// The epoch the test's boxes are numbered in, as a runner process numbers those of its runs that reserve nothing.
const EPOCH: u64 = 7;
/// The stand-in's harness member, and its image's name.
const MEMBER: &str = "stub";
const IMAGE: &str = "kiln-ended";

/// Box `n` of the test's.
fn named(n: u64) -> BoxName {
    BoxName::Unreserved { epoch: EPOCH, n }
}

fn hex(e: H) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Skips the test, saying why, unless `VM_E2E=1` asks this host to run it.
fn skip(why: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!vm_e2e::isolation_required(), "VM_E2E=1, and {why}");
    eprintln!("SKIP: {why}");
    Ok(())
}

/// The stand-in as a Shards image with its harness member, built from this checkout into Shards home `home` (its
/// base taken from the home's store where it holds it), named as Hex boots it: `NAME@sha256:…`.
fn stand_in(shards: &Path, home: &Path, at: &Path) -> anyhow::Result<String> {
    let (fixture, to) = vm_e2e::STUB_HARNESS;
    let (context, config, iid) = (at.join("context"), at.join("docker-config"), at.join("iid"));
    let harness = context.join("harness");
    std::fs::create_dir_all(&harness)?;
    std::fs::create_dir_all(&config)?;
    for f in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(fixture))? {
        let f = f?;
        std::fs::copy(f.path(), harness.join(f.file_name()))?;
    }
    let text = format!("{}\nFROM ${{BASE}}\nENV VM_STUB_PROTOCOL=claude\nHARNESS {MEMBER} FROM ./harness TO {to}\n", vm_e2e::kiln_base()?);
    std::fs::write(context.join("Agentfile"), text)?;
    let out = std::process::Command::new(shards)
        .args(["build", "--progress=quiet", "--platform", crate::narrowed::platform()?, "--file"])
        .arg(context.join("Agentfile"))
        .arg("--tag")
        .arg(format!("{IMAGE}:1"))
        .arg("--iidfile")
        .arg(&iid)
        .arg(&context)
        .env("SHARDS_HOME", home)
        .env("DOCKER_CONFIG", &config)
        .output()?;
    anyhow::ensure!(out.status.success(), "the stand-in's image wasn't built: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(format!("{IMAGE}@{}", std::fs::read_to_string(&iid)?.trim()))
}

/// The pids of the VM processes running carried VM binary `vm`.
fn vm_processes(vm: &Path) -> anyhow::Result<Vec<u32>> {
    let out = std::process::Command::new("ps").args(["-axww", "-o", "pid=,args="]).output()?;
    anyhow::ensure!(out.status.success(), "ps: {}", String::from_utf8_lossy(&out.stderr));
    let vm = vm.to_string_lossy();
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, args) = l.trim().split_once(' ')?;
            args.trim_start().starts_with(vm.as_ref()).then(|| pid.parse().ok()).flatten()
        })
        .collect())
}

/// Whether Hex lists box `name`.
async fn listed(runtime: &Arc<ImageRuntime>, name: BoxName) -> anyhow::Result<bool> {
    let r = Arc::clone(runtime);
    let boxes = kiln::rt::blocking(move || r.list()).await?.map_err(hex)?;
    let id = name.id().map_err(hex)?.to_string();
    Ok(boxes.iter().any(|b| b.id.to_string() == id))
}

/// Box `name` once its run has ended, as Hex keeps that in its record (`BoxState::Empty`). Hex has no call that waits
/// on an image box's own end, so its record is read until it says so, each read due once as long as the last took has
/// passed (at most half the time reading, as Hex's conformance reads a box's state and a replay its steps' ends), the
/// test's own bound ending a run that never does.
async fn ended(runtime: &Arc<ImageRuntime>, name: BoxName) -> anyhow::Result<hex_api::BoxInfo> {
    loop {
        let r = Arc::clone(runtime);
        let read = Instant::now();
        let info = kiln::rt::blocking(move || r.inspect(&name.id()?)).await?.map_err(hex)?;
        if info.state == BoxState::Empty {
            return Ok(info);
        }
        kiln::rt::sleep_until(kiln_seams::clock::later(Instant::now(), read.elapsed())).await;
    }
}

/// The charge kept for box `name` in `records`, if any.
async fn charged(records: &SimRecords, name: BoxName) -> anyhow::Result<Option<Charged>> {
    let row = records.get(USE, name.id().map_err(hex)?.to_string().as_bytes(), Consistency::Linearizable).await?;
    Ok(row.map(|r| serde_json::from_slice(&r.value)).transpose()?)
}

#[test]
fn p7_1b_a_box_whose_run_ended_on_its_own_is_deleted() -> anyhow::Result<()> {
    if !kiln::shards::carried() {
        return skip("this kiln carries no Shards: ci/shards/pack.sh builds it (docs/shards.md)");
    }
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("s");
    let held = kiln::shards::write_out(&state)?;
    match shards_api::host_check(Some(held.shards.as_path())) {
        Ok(shards_api::HostCheck::Usable(_)) => {}
        Ok(shards_api::HostCheck::Unusable { why, .. }) => return skip(&format!("this host runs no microVMs: {why}")),
        Err(e) => return skip(&format!("Shards couldn't say whether this host runs microVMs: {e}")),
    }
    let home = kiln::sandbox::shards_home(&state);
    if let Some(seed) = std::env::var_os("VM_E2E_SHARDS_STORE") {
        vm_e2e::seed_store_from(Path::new(&seed), &state)?;
    }
    let built = stand_in(&held.shards, &home, dir.path());
    let checked = built.and_then(|image| {
        let paths = shards_api::RuntimePaths::new(state.join("shards-cache"), home.clone()).map_err(|r| anyhow::anyhow!("{r:?}"))?;
        let runtime = ImageRuntime::new(held.shards.clone(), paths, &state.join("hex-records")).map_err(hex)?;
        kiln::rt::block_on(check(image, runtime, held.vm.clone()))
    });
    // What Shards started for the test's home (its daemon, where a build or pull started one) ends with it.
    let _ = std::process::Command::new(&held.shards).args(["stop", "daemon"]).env("SHARDS_HOME", &home).output();
    checked
}

async fn check(image: String, runtime: Arc<ImageRuntime>, vm: std::path::PathBuf) -> anyhow::Result<()> {
    let records = Arc::new(SimRecords::new());
    let n = AtomicU64::new(0);
    let hex_vms =
        HexVms::new(Arc::clone(&runtime) as Arc<dyn hex_api::Runtime>, move || Some(named(n.fetch_add(1, Ordering::Relaxed)))).with_ledger(Arc::clone(&records) as Arc<dyn kiln_seams::Records>);

    // A replay's box: its own command ends at once.
    let replay = hex_vms.boot_command(&image, vec!["/bin/sh".into(), "-c".into(), "exit 0".into()], &[], None).await.map_err(|e| anyhow::anyhow!("a replay's microVM didn't start: {e}"))?;
    let info = ended(&runtime, named(0)).await?;
    eprintln!("replay's box ended on its own: {:?}, {:?}", info.exit, info.ended);
    let r = Arc::clone(&runtime);
    let alone = kiln::rt::blocking(move || r.delete_with_host_use(&named(0).id()?)).await?;
    assert!(matches!(alone, Err(H::NotRunning(..))), "Hex reads no final use of a run that ended: {alone:?}");
    assert!(listed(&runtime, named(0)).await?, "and leaves its box as it was");
    let lived = replay.end().await.map_err(|e| anyhow::anyhow!("its end: {e}"))?;
    assert!(!listed(&runtime, named(0)).await?, "kiln's finish deleted the box whose run ended on its own");
    assert_eq!((lived, charged(&records, named(0)).await?), (None, None), "never read, charged nothing");
    assert_eq!(vm_processes(&vm)?, Vec::<u32>::new(), "no VM process of it left");

    // A session's box: its harness member's microVM dies under it.
    let launch = Launch { limits: Limits::NONE, egress: Egress::Only(Vec::new()), boundary: Boundary::MicroVm { image: image.clone() }, ..Launch::of(MEMBER, &[]) };
    let run = hex_vms.launch_as(named(1), launch, None, &[]).await.map_err(|e| anyhow::anyhow!("a session's microVM didn't start: {e}"))?;
    let meter = hex_vms.meter(&named(1).id().map_err(hex)?.to_string()).ok_or_else(|| anyhow::anyhow!("a live run has its meter"))?;
    let read = kiln::rt::blocking(move || meter.read()).await?.map_err(hex)?;
    let vms = vm_processes(&vm)?;
    let [pid] = vms.as_slice() else { anyhow::bail!("one VM process runs, the session's: {vms:?}") };
    let killed = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status()?;
    anyhow::ensure!(killed.success(), "its VM process {pid} wasn't killed");
    let end = loop {
        match run.next().await {
            Some(Output::Exited(end)) => break end,
            Some(_) => {}
            None => anyhow::bail!("its run ended with no end said"),
        }
    };
    eprintln!("session's box, its VM killed: its member's end {end:?}, read before {read:?}");
    assert!(!listed(&runtime, named(1)).await?, "its box is gone by the time its end is said");
    let kept = charged(&records, named(1)).await?;
    assert!(kept.as_ref().is_some_and(|c| c.of == Charge::LastRead && c.footprint_bytes == read.footprint_bytes), "charged the use last read, its run ended before its delete: {kept:?}");
    drop(run);
    assert_eq!(vm_processes(&vm)?, Vec::<u32>::new(), "no VM process left");
    Ok(())
}
