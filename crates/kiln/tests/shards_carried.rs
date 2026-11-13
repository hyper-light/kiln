//! Shards as kiln carries it (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16; docs/shards.md): written out
//! beside each other, signed on a Mac as kiln signs them (Shards' entitlements, the Hardened Runtime, kiln's
//! identity or ad hoc), and its VM process booting a guest through Shards' own client (shards-api, as Hex boots
//! one) with the carried `shards` as its binary.
//!
//! It needs a kiln that carries Shards (ci/shards/pack.sh before the build) and a guest: `KILN_TEST_KERNEL`,
//! Shards' pinned kernel for this host, and `KILN_TEST_INIT`, a static Linux program its kernel runs as PID 1,
//! which powers the VM off (docs/research/s16-vendor/smoke.sh fetches the one and builds the other). Without
//! them it says SKIP and why.
//!
//!   ci/shards/pack.sh && KILN_TEST_KERNEL=… KILN_TEST_INIT=… cargo test -p kiln --test shards_carried -- --nocapture

use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure};
use shards_api::Guests as _;

/// How long a guest may take to boot and power off before the test says it didn't: a test's bound, far past
/// Shards' own records of a direct boot (its PM M463).
const WITHIN: Duration = Duration::from_secs(300);

#[test]
fn step16_kilns_carried_vm_process_boots_a_guest() -> anyhow::Result<()> {
    if !kiln::shards::carried() {
        eprintln!("SKIP: this kiln carries no Shards: ci/shards/pack.sh builds it, then build kiln");
        return Ok(());
    }
    let base = tempfile::tempdir()?;
    let held = kiln::shards::write_out(base.path())?;
    for f in [&held.shards, &held.vm, &held.net] {
        ensure!(f.is_file() && f.parent() == Some(held.dir.as_path()), "{} isn't beside the others in {}", f.display(), held.dir.display());
    }
    eprintln!("carried Shards in {} (signed: {:?})", held.dir.display(), held.signer);
    #[cfg(target_os = "macos")]
    signed_as_kiln_signs(&held)?;
    let (Some(kernel), Some(init)) = (std::env::var_os("KILN_TEST_KERNEL"), std::env::var_os("KILN_TEST_INIT")) else {
        eprintln!("SKIP: no guest to boot: KILN_TEST_KERNEL and KILN_TEST_INIT name it (docs/research/s16-vendor/smoke.sh)");
        return Ok(());
    };
    let spec = shards_api::GuestSpec {
        kernel: kernel.into(),
        init: shards_api::Init::Executable(init.into()),
        cmdline: "console=ttyS0 quiet panic=-1".into(),
        vcpus: 1,
        memory_mib: 256,
        gpu: false,
        gpu_devices: Vec::new(),
        disks: Vec::new(),
        shares: Vec::new(),
        ports: Vec::new(),
        binary: Some(held.shards.clone()),
    };
    let started = Instant::now();
    let guest = shards_api::Shards::new().boot(&spec).map_err(|e| anyhow!("the boot: {e:?}"))?;
    let end = guest.wait_for(WITHIN).map_err(|e| anyhow!("its end: {e:?}"))?;
    eprintln!("the guest ended {end:?} after {:?}", started.elapsed());
    ensure!(end == Some(shards_api::End::PowerOff), "the guest didn't power off: {end:?}");
    Ok(())
}

/// Each carried executable is signed, the VM process with the hypervisor's and App Sandbox's entitlements, the
/// network process with App Sandbox's and the network's, both under the Hardened Runtime; by kiln's identity
/// where kiln has one here, else ad hoc.
#[cfg(target_os = "macos")]
fn signed_as_kiln_signs(held: &kiln::shards::Held) -> anyhow::Result<()> {
    use std::process::Command;
    for (f, entitlements) in [
        (&held.shards, &[][..]),
        (&held.vm, &["com.apple.security.app-sandbox", "com.apple.security.hypervisor"][..]),
        (&held.net, &["com.apple.security.app-sandbox", "com.apple.security.network.client", "com.apple.security.network.server"][..]),
    ] {
        let verify = Command::new("/usr/bin/codesign").args(["--verify", "--strict"]).arg(f).output()?;
        ensure!(verify.status.success(), "{} isn't validly signed: {}", f.display(), String::from_utf8_lossy(&verify.stderr));
        let shown = Command::new("/usr/bin/codesign").args(["-d", "--verbose=2", "--entitlements", "-"]).arg(f).output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&shown.stdout), String::from_utf8_lossy(&shown.stderr));
        for e in entitlements {
            ensure!(said.contains(e), "{} lacks {e}: {said}", f.display());
        }
        if !entitlements.is_empty() {
            ensure!(said.contains("runtime"), "{} isn't under the Hardened Runtime: {said}", f.display());
        }
        match &held.signer {
            kiln::os::macsign::Signer::Kiln(_) => ensure!(said.contains("Authority=kiln code signing"), "{} isn't signed by kiln's identity: {said}", f.display()),
            _ => ensure!(said.contains("Signature=adhoc"), "{} isn't signed ad hoc: {said}", f.display()),
        }
    }
    Ok(())
}
