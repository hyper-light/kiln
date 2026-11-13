//! The machine's capacity view end to end (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1): `hex node` and
//! `hex schedule serve` keeping the view on its QUIC lane, each a process of its own, the scheduler enrolled as
//! production enrolls it (`hex node enroll`, a CA's answer kept), Hex's own boxes placed through the scheduler beside
//! the reservations of a stand-in for kiln's runner (`capacity_view/stand_in.rs`, the holder contract driven over
//! `hex_quic::capacity::Link`).
//!
//! HEX-CV-L1 ([`hex_cv_l1`]): one host, over loopback QUIC, a normal test on every OS: the scheduler killed and started
//! again, a link dropped, and the stand-in's kiln-shaped work, every word checked.
//!
//! HEX-CV-2H ([`two_hosts`]): the same across two hosts in phases the lead runs (every other run ignores them), the
//! keeper's host and the holder's on any of Linux, macOS and Windows: the scheduler killed and started again, then the
//! lane's UDP port dropped and let through again.
//!
//! Linux needs a delegated cgroup v2 subtree for the node's boxes, as `quic_conformance.rs` does: a test re-runs once
//! in a `systemd-run --user --scope -p Delegate=yes` where its own cgroup isn't one, and records the skip where neither
//! is. macOS and Windows run their `process` class rootless.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use hex_api::capacity::Why;
use hex_core::patience::Patience;
use hex_quic::Refused;
use hex_quic::enroll::sim::SimCa;
use hex_quic::enroll::{Ca, Request, store};
use hex_testkit::waits::within;

#[path = "capacity_view/machine.rs"]
mod machine;
#[path = "capacity_view/stand_in.rs"]
mod stand_in;

use stand_in::{Asked, Rng, StandIn, run_of, workload};

/// The scheduler's name: the DNS name its certificate carries, which the holder checks it for.
const KEEPER: &str = "scheduler.capacity-view.test";
/// The stand-in's name, which the lane serves.
const HOLDER: &str = "kiln.capacity-view.test";
/// A peer kiln's CA enrolled that the lane doesn't name.
const STRANGER: &str = "stranger.capacity-view.test";

/// A refused dial's answer: the keeper's endpoint turning a peer it doesn't name away at its handshake.
static REFUSAL: Patience = Patience::measuring("test.hex_cli.capacity_view.refusal");

/// A folder of the test's own: under ~/.cache on Unix (the hazards' rule: never /tmp), the system's temp folder on
/// Windows, as Hex's Windows tests take.
fn scratch(prefix: &str) -> tempfile::TempDir {
    #[cfg(unix)]
    {
        let cache = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".cache/hex-t7/capacity");
        std::fs::create_dir_all(&cache).expect("cache dir");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&cache)
            .unwrap()
    }
    #[cfg(windows)]
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

/// `name` enrolled in `dir` with `ca` as a holder enrolls: its key made there (`store::begin`), its request answered.
fn enroll_holder(ca: &SimCa, dir: &Path, name: &str, grant: &[u8]) {
    let credentials = stand_in::credentials(dir);
    store::begin(&credentials, name).unwrap();
    ca.admit(name, grant);
    let issued = ca
        .enroll(&store::request(&credentials, grant.to_vec()).unwrap())
        .unwrap();
    store::finish(&credentials, &issued).unwrap();
}

/// A dial by the peer enrolled in `dir`, which the lane at `address` doesn't name: refused at its handshake, typed
/// Identity, as for a certificate kiln's CA didn't issue (hex-quic.md Q10).
fn a_stranger_is_turned_away(dir: &Path, address: SocketAddr) {
    let end = stand_in::endpoint(&stand_in::credentials(dir), address);
    let dialed = within(&REFUSAL, |by| end.dial(KEEPER, address, by));
    assert!(
        matches!(dialed, Err(Refused::Identity)),
        "a peer the lane doesn't name: {dialed:?}"
    );
}

/// The boxes of Hex's own HEX-CV-L1 places on the machine in `dir`, each a quarter of its CPUs: two where its scheduler
/// places a `process` box under its `oci` placement class ([`machine::places_process_boxes`]), as on Linux; none where
/// it doesn't, as on a Mac, which places Shards microVMs alone, or on a Windows node that runs containers, whose `oci`
/// class places those alone. This test boots neither: there the stand-in meets the machine's room alone.
pub fn hex_boxes(dir: &Path) -> u64 {
    if machine::places_process_boxes(dir) {
        2
    } else {
        0
    }
}

/// What a kiln-shaped workload's run of HEX-CV-L1 takes: steps enough to fill the room and empty it again several times
/// over, with the runs' shares of 2 to 8 of the machine (`stand_in::workload`). A test generator's parameter.
const L1_STEPS: u64 = 64;

/// HEX-CV-L1's body: the machine in `dir`'s `a`, its boxes under `box_root` on Linux; the stand-in in `b`, a stranger in
/// `c`.
fn light(dir: &Path, box_root: Option<&Path>) {
    let (machine_dir, holder_dir, stranger_dir) = (dir.join("a"), dir.join("b"), dir.join("c"));
    for d in [&machine_dir, &holder_dir, &stranger_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    let ca = SimCa::new().unwrap();
    let request = machine::enroll(&machine_dir, KEEPER);
    ca.admit(KEEPER, b"scheduler");
    let issued = ca
        .enroll(&Request {
            grant: b"scheduler".to_vec(),
            ..request
        })
        .unwrap();
    machine::keep(&machine_dir, &issued);
    enroll_holder(&ca, &holder_dir, HOLDER, b"holder");
    enroll_holder(&ca, &stranger_dir, STRANGER, b"stranger");

    let node = machine::node(&machine_dir, box_root);
    machine::configure(
        &machine_dir,
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        &[HOLDER],
    );
    let (mut scheduler, address) = machine::scheduler(&machine_dir);
    a_stranger_is_turned_away(&stranger_dir, address);

    let mut kiln = StandIn::start(&holder_dir, HOLDER, KEEPER, address);
    assert!(kiln.join().unwrap(), "the first sync's counts disagree");
    let room = kiln.view().unwrap().room;
    // Hex's own: two boxes of a quarter of the machine's CPUs each, placed through the scheduler, where it places them.
    let boxes = hex_boxes(&machine_dir);
    eprintln!("HEX-CV-L1: Hex's boxes {boxes}");
    for n in 0..boxes {
        let placed = machine::place(&machine_dir, &format!("cv-box-{n}"), room.cpu_milli / 4);
        assert!(
            placed.created && placed.refusals.is_empty(),
            "Hex's box {n}: {placed:?}\n{}",
            scheduler.said()
        );
    }
    // Runs of an eighth of the machine until the room refuses one: what Hex's boxes leave, and no more.
    let left = 8 - 2 * boxes;
    let mut granted = 0u64;
    loop {
        match kiln.reserve(run_of(room, 8)).unwrap() {
            Asked::Granted => granted += 1,
            Asked::Refused(why) => {
                assert_eq!(why, Why::Room);
                break;
            }
        }
        assert!(
            granted <= left,
            "granted past the {left} eighths Hex's boxes leave"
        );
    }
    assert!(granted >= 1, "nothing granted beside Hex's boxes");
    let boxed: u64 = machine::boxes(&machine_dir)
        .iter()
        .map(|b| u64::from(b.bounds.cpu_max_milli.unwrap()))
        .sum();
    assert!(
        boxed + machine::sum(&kiln.held()).cpu_milli <= room.cpu_milli,
        "Hex's boxes ({boxed}) and the holder's runs ({:?}) past the machine's {} CPU",
        kiln.held(),
        room.cpu_milli
    );

    // The scheduler killed and started again over the same state: the stand-in's link goes with it, told once the
    // transport's idle timeout passes, and its sync agrees with the keeper's books, read from its records.
    scheduler.kill();
    drop(scheduler);
    let (mut scheduler, address) = machine::scheduler(&machine_dir);
    kiln.moved(address);
    let lost = kiln.reserve(run_of(room, 8));
    assert!(lost.is_err(), "a word to the killed scheduler: {lost:?}");
    assert!(
        kiln.join().unwrap(),
        "the counts disagree after the restart"
    );
    // A link dropped at the stand-in's end, as a changed path drops it, and opened again.
    kiln.drop_link();
    assert!(
        kiln.join().unwrap(),
        "the counts disagree after a dropped link"
    );
    assert!(kiln.is_up());

    let seed = kiln.epoch();
    workload(&mut kiln, &mut Rng::new(seed), L1_STEPS).unwrap();
    let tally = kiln.tally;
    eprintln!("HEX-CV-L1: {tally:?}");
    // Non-vacuity: the work met the room's edge and came back from it, Hex's boxes were hinted where it placed them,
    // the reserve said to the killed scheduler was abandoned, and every join synced.
    assert!(
        tally.granted > granted
            && tally.released > 0
            && tally.refused_room > 0
            && tally.refused_other == 0
            && tally.unanswered > 0
            && tally.synced == 3
            && tally.links_lost == 2
            && (boxes == 0 || tally.hints > 0)
            && tally.views > tally.granted,
        "{tally:?}"
    );

    // Once the scheduler has ended, its books of the holder are the holder's own.
    scheduler.kill();
    drop(scheduler);
    assert_eq!(machine::books(&machine_dir, 1, HOLDER), kiln.held());
    machine::delete_boxes(&machine_dir);
    drop(node);
}

/// HEX-CV-L1 (joint plan step 13; AC-T7.4.1; CV1-CV11 over CV9's lane): do run `hex node` and `hex schedule serve` with
/// a capacity lane on loopback, its enrollment begun by `hex node enroll` and answered by the simulator CA; have a peer
/// the lane doesn't name dial it; have the stand-in join, Hex place two boxes of a quarter of the CPUs (where it places
/// a `process` box: `hex_boxes`), and the stand-in reserve eighths until refused; kill the scheduler and start it
/// again; drop the stand-in's link; then run 64 steps of kiln-shaped work. Expect the stranger refused at its handshake
/// (Identity); every view's CPU held within the machine's and every grant within its room; the runs refused for room
/// past what the boxes leave, the boxes and the runs within the machine's CPUs; the sync agreeing after the restart and
/// after the dropped link; and the keeper's books, read from its records once it has ended, the stand-in's own.
#[cfg(not(target_os = "linux"))]
#[test]
fn hex_cv_l1() {
    let dir = scratch("cv-");
    light(dir.path(), None);
}

/// The two-host run (HEX-CV-2H; the joint plan's step 13 on two real machines; docs/design/capacity-view.md CV1-CV11),
/// the lead's to run. On host A (the keeper's: any of Linux, macOS and Windows) `hex node` and `hex schedule serve`
/// keep the machine's view, its lane bound on A's address; on host B (the holder's: any of the three) the stand-in
/// reserves through it across the hosts. Both are enrolled under one simulator CA, which lives only in B's phase B1:
/// A's request goes to B, and the chain and roots come back; every private key stays on the host that made it. Then the
/// scheduler is killed by its pid and started again, and the lane's UDP port is dropped and let through again.
///
/// Each phase is a test every other run ignores, run by name on its host:
/// `cargo test --locked -p hex-cli --test capacity_view -- --ignored --nocapture --test-threads=1 hex_cv_2h_<phase>`.
/// Inputs come from the environment; outputs go to stdout as `HEX_CV_<NAME>=<value>` lines for the next phase. Each
/// host takes `HEX_CV_DIR` (a fresh folder per run), `HEX_CV_KEEPER` (the scheduler's name) and `HEX_CV_HOLDER` (the
/// holder's). In order:
/// 1. `a1`, on A: says `HEX_CV_REQUEST`.
/// 2. `b1`, on B, with `HEX_CV_REQUEST`: says `HEX_CV_CHAIN` and `HEX_CV_ROOTS`.
/// 3. `a2`, on A, with `HEX_CV_CHAIN` and `HEX_CV_ROOTS`.
/// 4. `a3`, on A, in the background, with `HEX_CV_BIND` (`<A's address>:<port>`): says `HEX_CV_SCHEDULER_PID` and
///    `HEX_CV_CAPACITY` at each start of the scheduler (after the first, `HEX_CV_HEX_BOXES`: how many boxes of Hex's
///    own it placed), and starts it again each time it ends, until it ends with `$HEX_CV_DIR/stop` there: then says
///    `HEX_CV_BOOKS` and `HEX_CV_LEFT=none`.
/// 5. `b2`, on B, with `HEX_CV_ADDR` (A's `HEX_CV_BIND`): says `HEX_CV_TALLY` and `HEX_CV_HELD`.
/// 6. `b3`, on B, in the background, with `HEX_CV_ADDR` and `HEX_CV_LOSS=kill`: once it says `HEX_CV_READY`, kill the
///    scheduler on A by its last `HEX_CV_SCHEDULER_PID` (`kill -9 <pid>`; Windows: `taskkill /F /PID <pid>`); A3 starts
///    it again. Once B3 says `HEX_CV_LOST`, `touch $HEX_CV_DIR/healed` on B: it joins again and says `HEX_CV_HEALED`.
/// 7. Phase 6 again with `HEX_CV_LOSS=partition`: once B3 says `HEX_CV_READY`, drop the lane's UDP port on A; once it
///    says `HEX_CV_LOST`, let it through again, then `touch $HEX_CV_DIR/healed` on B. The port, by A's OS (as root):
///    Linux `iptables -I INPUT -p udp --dport <port> -j DROP`, healed with `-D` for `-I`; macOS
///    `echo 'block drop in quick proto udp to any port <port>' | pfctl -a com.apple/hex-cv -f - && pfctl -E` (macOS's
///    pf.conf loads `com.apple/*` anchors alone), healed with `pfctl -a com.apple/hex-cv -F rules`; Windows
///    `New-NetFirewallRule -DisplayName hex-cv -Direction Inbound -Protocol UDP -LocalPort <port> -Action Block`,
///    healed with `Remove-NetFirewallRule -DisplayName hex-cv`. Where A's firewall admits no port the lane could bind,
///    bind it on loopback behind a UDP relay on A whose address is B's `HEX_CV_ADDR`, and drop the holder's datagrams
///    there instead (capacity-view.md's run notes).
/// 8. On A, `touch $HEX_CV_DIR/stop`, then kill the scheduler as in 6: A3 says `HEX_CV_BOOKS` and `HEX_CV_LEFT=none`.
/// 9. `b4`, on B, with `HEX_CV_BOOKS`.
mod two_hosts {
    use std::io::Write;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};

    use hex_api::capacity::Held;
    use hex_core::patience::Patience;
    use hex_quic::enroll::sim::SimCa;
    use hex_quic::enroll::{Ca, Issued, Request};
    use hex_testkit::waits::until;

    use super::stand_in::{Rng, StandIn, workload};
    use super::{a_stranger_is_turned_away, enroll_holder, machine};

    /// The grants the simulator CA admits each name with, each taken once (HEX-ENROLL-5).
    const KEEPER_GRANT: &[u8] = b"hex-cv-2h scheduler";
    const HOLDER_GRANT: &[u8] = b"hex-cv-2h holder";
    const STRANGER_GRANT: &[u8] = b"hex-cv-2h stranger";

    /// The operator's word that the keeper is back: what B3 waits for once its link has gone.
    static HEALED: Patience = Patience::measuring("test.hex_cli.capacity_view.healed");

    /// The steps of kiln-shaped work a phase runs (`HEX_CV_STEPS`, else HEX-CV-L1's), a test generator's parameter.
    fn steps() -> u64 {
        std::env::var("HEX_CV_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(super::L1_STEPS)
    }

    /// The run's variable `name`, which the phase needs.
    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("HEX-CV-2H's phase needs {name}"))
    }

    /// This host's folder for the run (`HEX_CV_DIR`, a fresh one each run: an enrollment begins once).
    fn dir() -> PathBuf {
        let dir = PathBuf::from(var("HEX_CV_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The holder's folder on B, and the stranger's.
    fn holder_dir() -> PathBuf {
        let dir = dir().join("holder");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stranger_dir() -> PathBuf {
        let dir = dir().join("stranger");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The stranger's name: the holder's, the lane naming only the holder.
    fn stranger() -> String {
        format!("stranger-{}", var("HEX_CV_HOLDER"))
    }

    /// Says `name=value` for the next phase, at once, on a line of its own: libtest's `test <name> ... ` comes first
    /// on the line it begins.
    fn say(name: &str, value: impl std::fmt::Display) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "\n{name}={value}").unwrap();
        out.flush().unwrap();
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn from_hex(text: &str) -> Vec<u8> {
        let text = text.trim().as_bytes();
        assert!(text.len().is_multiple_of(2), "hex of an odd length");
        text.chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Certificates as the run carries them: each one's DER in hex, joined by commas.
    fn certs_hex<C: AsRef<[u8]>>(certs: &[C]) -> String {
        certs
            .iter()
            .map(|c| to_hex(c.as_ref()))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn certs_der(text: &str) -> Vec<Vec<u8>> {
        text.split(',')
            .filter(|c| !c.is_empty())
            .map(from_hex)
            .collect()
    }

    /// Books as the run carries them: their JSON, in hex.
    fn books_hex(held: &[Held]) -> String {
        to_hex(&serde_json::to_vec(held).unwrap())
    }

    fn addr() -> SocketAddr {
        var("HEX_CV_ADDR").parse().unwrap()
    }

    /// HEX-CV-2H A1, on the keeper's host. Do: `hex node enroll` the scheduler `HEX_CV_KEEPER` in `$HEX_CV_DIR/keeper`.
    /// Expect: its request for the CA said (`HEX_CV_REQUEST`), its key kept here alone.
    #[test]
    #[ignore = "HEX-CV-2H's phase A1, on the keeper's host: the lead's two-host run"]
    fn hex_cv_2h_a1_the_scheduler_enrolls_here() {
        let request = machine::enroll(&dir(), &var("HEX_CV_KEEPER"));
        say("HEX_CV_REQUEST", to_hex(&request.csr));
    }

    /// HEX-CV-2H B1, on the holder's host. Do: make the simulator CA here, admit `HEX_CV_KEEPER` and answer its request
    /// (`HEX_CV_REQUEST`), then enroll the holder `HEX_CV_HOLDER` in `$HEX_CV_DIR/holder` and a stranger the lane won't
    /// name in `$HEX_CV_DIR/stranger`, each key made here. Expect: the scheduler's chain and roots said
    /// (`HEX_CV_CHAIN`, `HEX_CV_ROOTS`). The CA's key ends with this process.
    #[test]
    #[ignore = "HEX-CV-2H's phase B1, on the holder's host: the lead's two-host run"]
    fn hex_cv_2h_b1_the_ca_answers_here_and_the_holder_enrolls() {
        let ca = SimCa::new().unwrap();
        let keeper = var("HEX_CV_KEEPER");
        ca.admit(&keeper, KEEPER_GRANT);
        let issued = ca
            .enroll(&Request {
                name: keeper,
                csr: from_hex(&var("HEX_CV_REQUEST")),
                grant: KEEPER_GRANT.to_vec(),
            })
            .unwrap();
        enroll_holder(&ca, &holder_dir(), &var("HEX_CV_HOLDER"), HOLDER_GRANT);
        enroll_holder(&ca, &stranger_dir(), &stranger(), STRANGER_GRANT);
        say("HEX_CV_CHAIN", certs_hex(&issued.chain));
        say("HEX_CV_ROOTS", certs_hex(&issued.roots));
    }

    /// HEX-CV-2H A2, on the keeper's host. Do: keep the CA's answer (`HEX_CV_CHAIN`, `HEX_CV_ROOTS`) for the
    /// scheduler's enrollment. Expect: taken, being for the key made here and the name asked, and the credentials load.
    #[test]
    #[ignore = "HEX-CV-2H's phase A2, on the keeper's host: the lead's two-host run"]
    fn hex_cv_2h_a2_the_scheduler_keeps_the_cas_answer() {
        let issued = Issued {
            chain: certs_der(&var("HEX_CV_CHAIN"))
                .into_iter()
                .map(Into::into)
                .collect(),
            roots: certs_der(&var("HEX_CV_ROOTS"))
                .into_iter()
                .map(Into::into)
                .collect(),
        };
        machine::keep(&dir(), &issued);
        say("HEX_CV_KEPT", machine::keeper_credentials(&dir()).display());
    }

    /// A3's body over the machine in `HEX_CV_DIR`, its boxes under `box_root` on Linux.
    fn serve(box_root: Option<&Path>) {
        let dir = dir();
        let node = machine::node(&dir, box_root);
        let bind: SocketAddr = var("HEX_CV_BIND").parse().unwrap();
        machine::configure(&dir, bind, &[&var("HEX_CV_HOLDER")]);
        let (mut scheduler, address) = machine::scheduler(&dir);
        say("HEX_CV_SCHEDULER_PID", scheduler.child.id());
        say("HEX_CV_CAPACITY", address);
        // Hex's own: two boxes of a quarter of the machine's CPUs each (the node's CPUs are its hardware threads, as
        // the standard library counts them within the cgroup's quota), kept through every restart; none where its
        // scheduler places no `process` box (`hex_boxes`).
        let threads = u64::try_from(std::thread::available_parallelism().unwrap().get()).unwrap();
        let boxes = super::hex_boxes(&dir);
        say("HEX_CV_HEX_BOXES", boxes);
        for n in 0..boxes {
            let placed = machine::place(&dir, &format!("cv-2h-box-{n}"), threads * 1000 / 4);
            assert!(
                placed.created && placed.refusals.is_empty(),
                "Hex's box {n}: {placed:?}"
            );
        }
        loop {
            let ended = scheduler.child.wait().unwrap();
            say("HEX_CV_SCHEDULER_ENDED", ended);
            if dir.join("stop").exists() {
                break;
            }
            drop(scheduler);
            let (next, address) = machine::scheduler(&dir);
            scheduler = next;
            say("HEX_CV_SCHEDULER_PID", scheduler.child.id());
            say("HEX_CV_CAPACITY", address);
        }
        drop(scheduler);
        say(
            "HEX_CV_BOOKS",
            books_hex(&machine::books(&dir, 1, &var("HEX_CV_HOLDER"))),
        );
        machine::delete_boxes(&dir);
        drop(node);
        say("HEX_CV_LEFT", "none");
    }

    /// HEX-CV-2H A3, on the keeper's host, in the background. Do: run `hex node` and `hex schedule serve` with its lane
    /// at `HEX_CV_BIND` serving `HEX_CV_HOLDER` alone, and place two boxes of Hex's own of a quarter of the machine's
    /// CPUs each where its scheduler places a `process` box (`HEX_CV_HEX_BOXES` says how many); each time the
    /// scheduler ends, start it again over the same state and address, until it ends with `$HEX_CV_DIR/stop` there.
    /// Expect: each start said (`HEX_CV_SCHEDULER_PID`, `HEX_CV_CAPACITY`), Hex's boxes placed, and once stopped the
    /// keeper's books of the holder said from its records (`HEX_CV_BOOKS`) and the boxes deleted (`HEX_CV_LEFT=none`).
    /// A Linux host that can't delegate a cgroup records the skip and serves nothing.
    #[test]
    #[ignore = "HEX-CV-2H's phase A3, on the keeper's host: the lead's two-host run"]
    fn hex_cv_2h_a3_the_machine_keeps_its_view_through_its_schedulers_end() {
        #[cfg(target_os = "linux")]
        super::linux::delegated(
            "two_hosts::hex_cv_2h_a3_the_machine_keeps_its_view_through_its_schedulers_end",
            |own| serve(Some(&own.join("hex-boxes-cv-2h"))),
        );
        #[cfg(not(target_os = "linux"))]
        serve(None);
    }

    /// HEX-CV-2H B2, on the holder's host. Do: have the stranger dial the keeper at `HEX_CV_ADDR`; start the stand-in
    /// and join; run `HEX_CV_STEPS` steps (else 64) of kiln-shaped work. Expect: the stranger refused at its handshake
    /// (Identity), the sync agreeing, every view's CPU held within the machine's and every grant within its room.
    /// Says `HEX_CV_TALLY` and `HEX_CV_HELD`.
    #[test]
    #[ignore = "HEX-CV-2H's phase B2, on the holder's host: the lead's two-host run"]
    fn hex_cv_2h_b2_the_holder_reserves_across_the_hosts() {
        a_stranger_is_turned_away(&stranger_dir(), addr());
        let mut kiln = StandIn::start(
            &holder_dir(),
            &var("HEX_CV_HOLDER"),
            &var("HEX_CV_KEEPER"),
            addr(),
        );
        assert!(kiln.join().unwrap(), "the first sync's counts disagree");
        let seed = kiln.epoch();
        workload(&mut kiln, &mut Rng::new(seed), steps()).unwrap();
        say("HEX_CV_TALLY", format!("{:?}", kiln.tally));
        say("HEX_CV_HELD", books_hex(&kiln.held()));
    }

    /// HEX-CV-2H B3, on the holder's host, in the background, with `HEX_CV_LOSS` (`kill` or `partition`). Do: start the
    /// stand-in again (its epoch one more, its ledger's runs listed), join, run kiln-shaped work, say `HEX_CV_READY`
    /// and hear the keeper until the link goes, the keeper lost (killed, or its port dropped); say `HEX_CV_LOST`, wait
    /// for the operator's `$HEX_CV_DIR/healed`, join again and work on. Expect: each sync agreeing (`HEX_CV_HEALED`
    /// after the loss), the loss said by the transport, and every view and grant within the room. Says `HEX_CV_TALLY`
    /// and `HEX_CV_HELD`.
    #[test]
    #[ignore = "HEX-CV-2H's phase B3, on the holder's host: the lead's two-host run"]
    fn hex_cv_2h_b3_a_lost_keeper_reconciles() {
        let loss = var("HEX_CV_LOSS");
        assert!(
            loss == "kill" || loss == "partition",
            "HEX_CV_LOSS is kill or partition, not {loss:?}"
        );
        let healed = dir().join("healed");
        let _ = std::fs::remove_file(&healed);
        let mut kiln = StandIn::start(
            &holder_dir(),
            &var("HEX_CV_HOLDER"),
            &var("HEX_CV_KEEPER"),
            addr(),
        );
        assert!(kiln.join().unwrap(), "the sync before the loss disagrees");
        let seed = kiln.epoch();
        workload(&mut kiln, &mut Rng::new(seed), steps()).unwrap();
        say(
            "HEX_CV_READY",
            if loss == "kill" {
                "kill the scheduler on its host by its last HEX_CV_SCHEDULER_PID now"
            } else {
                "drop the lane's UDP port on its host now"
            },
        );
        let lost = kiln.until_lost();
        say("HEX_CV_LOST", lost.0);
        until(
            &HEALED,
            "the operator's word that the keeper is back",
            || healed.exists(),
        )
        .unwrap();
        assert!(kiln.join().unwrap(), "the sync after the loss disagrees");
        say("HEX_CV_HEALED", "agree");
        workload(&mut kiln, &mut Rng::new(seed + 1), steps()).unwrap();
        say("HEX_CV_TALLY", format!("{:?}", kiln.tally));
        say("HEX_CV_HELD", books_hex(&kiln.held()));
    }

    /// HEX-CV-2H B4, on the holder's host, once A3 has said `HEX_CV_BOOKS`. Do: compare the keeper's books of the
    /// holder, read from its records once its scheduler ended, with the stand-in's ledger. Expect: the same.
    #[test]
    #[ignore = "HEX-CV-2H's phase B4, on the holder's host: the lead's two-host run"]
    fn hex_cv_2h_b4_the_keepers_books_are_the_holders() {
        let books: Vec<Held> = serde_json::from_slice(&from_hex(&var("HEX_CV_BOOKS"))).unwrap();
        let kiln = StandIn::start(
            &holder_dir(),
            &var("HEX_CV_HOLDER"),
            &var("HEX_CV_KEEPER"),
            addr(),
        );
        assert_eq!(
            books,
            kiln.held(),
            "the keeper's books and the holder's differ"
        );
        say(
            "HEX_CV_BOOKS_AGREE",
            format!("{} reservations", books.len()),
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// This process's cgroup directory under /sys/fs/cgroup.
    fn own_cgroup() -> PathBuf {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
        let rel = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .expect("a 0:: cgroup line")
            .trim_start_matches('/');
        PathBuf::from("/sys/fs/cgroup").join(rel)
    }

    /// This process in a leaf of its own cgroup, which hands every controller down (process_box.rs's).
    fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
        let own = own_cgroup();
        let leaf = own.join("hex-agent");
        std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the agent: {e}"))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| format!("moving self into a leaf: {e}"))?;
        hex_runtime::linux::cgroup::BoxRoot::open(
            &own.join("hex-probe"),
            hex_runtime::linux::disks::under(disk).unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        Ok(own)
    }

    /// Runs `body` with this process's cgroup, delegated: here where it is, else once more as `test` alone in a
    /// `systemd-run --user --scope -p Delegate=yes` (an ignored test too, as the two-host run's are), else the skip
    /// recorded.
    pub(super) fn delegated(test: &str, body: impl FnOnce(&Path)) {
        let disk = super::scratch("disk-");
        match prepare_cgroup(disk.path()) {
            Ok(own) => body(&own),
            Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
                let has_systemd = Command::new("systemd-run")
                    .arg("--version")
                    .output()
                    .is_ok_and(|o| o.status.success());
                if !has_systemd {
                    hex_testkit::skip(
                        test,
                        "cgroup v2 delegation",
                        &format!(
                            "own cgroup isn't ours to subdivide ({direct}) and no systemd --user"
                        ),
                    )
                    .expect("recording the skip");
                    return;
                }
                let status = Command::new("systemd-run")
                    .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                    .arg(std::env::current_exe().expect("test binary path"))
                    .args([
                        test,
                        "--exact",
                        "--include-ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("HEX_DELEGATED", "1")
                    .status()
                    .expect("run the test in a delegated scope");
                assert!(status.success(), "the delegated re-run of {test} failed");
            }
            Err(direct) => {
                hex_testkit::skip(
                    test,
                    "cgroup v2 delegation",
                    &format!(
                        "none in the delegated scope either ({direct}); run on a cgroup-v2 host"
                    ),
                )
                .expect("recording the skip");
            }
        }
    }

    /// HEX-CV-L1 on Linux, its node's boxes in a delegated cgroup subtree (the crate's docs).
    #[test]
    fn hex_cv_l1() {
        delegated("linux::hex_cv_l1", |own| {
            let dir = super::scratch("cv-");
            super::light(dir.path(), Some(&own.join("hex-boxes-cv")));
        });
    }
}
