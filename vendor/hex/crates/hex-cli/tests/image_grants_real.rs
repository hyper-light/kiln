//! Joint plan step 15, AC-T7.7.2 and T-T7.7.1 (docs/design/network-grants.md N4 and N6): a Shards image VM's network
//! is its grants alone, on VMs a real `hex node` boots: on Linux KVM, macOS HVF and Windows WHP alike. These are the
//! booted-VM checks Hex's lead runs beside goal 12's driver of the class's cases (`image_real.rs`), whose node and
//! helpers they share (`image_node`). Every VM boots the probe image, whose one harness is the grants probe
//! (`image_grants_probe/probe.rs`; its context made by `scripts/grants-probe/build.sh`, whose header says what its
//! Agentfile grants).
//!
//! - **A destination its image grants not refuses the boot.** The probe granted a name its image doesn't grant is made,
//!   and its start refused `Invalid`, past the image's grants: Shards' run host refused it before anything started.
//! - **No grant, nothing past the microVM.** A VM of its members alone granted nothing, its probe driven by this test
//!   (`ShardsImage::members`), is asked on the probe's stdin: its provider's name and its ungranted name unresolved
//!   (no DNS answer, T-T7.7.1) and none of its targets connected, the host's service and the image's port egress
//!   among them; inspect shows no grant and nothing published, and the host's service took nothing. Its image links
//!   its harness past the microVM, so its run keeps its network, whose network process (Shards' `shards-net`, D36),
//!   told nothing kept, lets nothing past (D250): one network process, no process of its run holding a socket that
//!   takes what comes in (a listening TCP socket, a UDP socket with no peer) but its run's agent-control endpoint's,
//!   all three held (Hex's own, on the host's loopback, where inspect says: `BoxInfo.control` and `control_wake`, its
//!   served socket, its waker and its wake, which Windows' table shows with no peer), and no flow past the host's
//!   loopback.
//! - **One publish, published alone, to its member.** A VM of its members alone is granted one publish: the probe's
//!   port at the host's loopback, on a host port the OS picks. Inspect shows that one grant, and its one published
//!   port at 127.0.0.1 reaching the probe at that port, as Shards names it. Its run listens there alone, past its
//!   agent-control endpoint: at no wildcard address, and not on the image's other port coming in. A connection there
//!   reaches the probe, which says it came in on that port and that none of its own egress connected: its provider and
//!   an ungranted name, the host's service, and the image's port egress, every one its image grants dropped by Hex's
//!   narrowing. No process of the run holds a flow past the host's loopback, and the host's service, a listener of
//!   this test's, took nothing.
//! - **Its granted destinations alone, reached.** A VM of its members alone is granted that publish and, by TCP, the
//!   probe's provider by name and the host's service (kiln's endpoint, `host.docker.internal`): the probe says it
//!   reached both, its ungranted name and the image's port egress nowhere, and the host's service, this test's
//!   listener, took its connection (N4: the run host holds every member to its reach of the names kept, D246).
//! - **A publish reaching the run's own command stops it.** A VM with a command of its own is granted a publish of a
//!   port its image gives no member, which Shards gives that command (D122): its start is refused `Invalid`, naming
//!   the run's own command, the VM is stopped, and no run host is left.
//!
//! What the host shows of a run (its processes, under its run host `shards api-run`, which the node starts, D231; and
//! the TCP and UDP sockets they hold) is read by one reader for each OS (`image_grants_host`), each saying how it reads:
//! /proc on Linux, `ps` and `lsof` on macOS, CIM's `Win32_Process` and the NetTCPIP module's tables on Windows. What a
//! reader can't read fails the test: what can't be seen isn't passed.
//!
//! Every wait measures as the run goes, with no deadline picked for it: each waits for its fact while its party lives
//! (the node's ready line while the node runs; the probe's hand-over, asked again while its box answers Pending as its
//! run settles, and its answer at a port, while its VM runs; its report on its stdout until that ends), the person's
//! rule of 2026-11-13.
//!
//! Explicit inputs only, each by its own variable; a missing one fails by name. Run alone, `--ignored`: on Linux
//! inside a cgroup delegated to this binary (`HEX_DELEGATED`); with image_real's node inputs (`SHARDS_BIN`, Hex's
//! `shards`, `scripts/shards-gpu/build.sh`'s at the Shards revision vendor/shards pins, whose run host speaks the image
//! channel the node's vendored API writes; `SHARDS_KERNEL` and `SHARDS_INIT`, the kernel the run hosts boot and the
//! init beside it, which Shards takes together, the init that same `shards` build carries (build.sh's
//! `shards-init`); the node's `HEX_GUEST_*`, `HEX_IMAGE_CPU_MILLI`, `HEX_IMAGE_MEMORY_BYTES`, `HEX_IMAGE_WORK`, a
//! short path where the node's socket lies, and `HEX_WAITS_CENSUS_MAX`), and the probe's:
//! - the probe image, in the node's Shards home before the node starts: `HEX_GRANTS_CONTEXT`, the context
//!   `scripts/grants-probe/build.sh` made, built there; or else `HEX_GRANTS_IMAGE`, `NAME@sha256:...`, pulled there;
//! - the recipe's own inputs, as it was given them: `HEX_GRANTS_MEMBER`, `HEX_GRANTS_PORT`,
//!   `HEX_GRANTS_UNGRANTED_PORT`, `HEX_GRANTS_PROVIDER` (a provider this host reaches at TCP 443),
//!   `HEX_GRANTS_UNGRANTED`, `HEX_GRANTS_HOST_PORT` (where this test listens on the host's loopback for the probe's
//!   try) and `HEX_GRANTS_OPEN`;
//! - `HEX_GRANTS_COMMAND_PORT`: a TCP port the image gives no member, nor declares as egress.
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_grants_real -- --ignored --nocapture` with those set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;

use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, BoxState, Class, ClassSpec, Destination, Exit, Grant,
    ImageMember, LifecycleCause, Member, MemberKind, Ports, Proto, Receiver, Refusal, Runtime,
    ShardsImage, Signal,
};
use hex_client::Client;
use hex_core::patience::Patience;

#[path = "image_node/mod.rs"]
mod image_node;
use image_node::{Node, allocation, built, input, number, pull, records, step};

#[path = "image_grants_host/mod.rs"]
mod host;
#[path = "image_grants_probe/probe.rs"]
mod probe;
use probe::{Ingress, Outcome, Report};

/// The probe's answer at a published port, which no recorded operation makes: it measures, for its fact alone
/// (hex_testkit::waits), with no deadline, while the VM runs ([`answered_at`]).
static MEMBER_ANSWERED: Patience = Patience::measuring("test.hex_cli.image_grants_member_answered");
/// The probe's hand-over to its caller, while its box settles ([`handed_over`]): measuring alike.
static MEMBER_HANDED_OVER: Patience =
    Patience::measuring("test.hex_cli.image_grants_member_handed_over");
/// A box's end after a KILL whose stop outlasted its W* ([`stop_and_delete`]): measuring alike.
static KILLED: Patience = Patience::measuring("test.hex_cli.image_grants_killed");

/// The name the probe image is tagged with where this test builds it into the node's Shards home.
const BUILT: &str = "hex-image-grants/probe";

/// The probe image, its harness and the recipe's inputs, as the run was given them.
struct Probe {
    image: String,
    member: Member,
    port: u16,
    ungranted_port: u16,
    provider: String,
    ungranted: String,
    host_port: u16,
    open: String,
    command_port: u16,
}

fn port(name: &str) -> u16 {
    u16::try_from(number(name)).unwrap_or_else(|_| panic!("{name} is no TCP port"))
}

/// A Shards image VM of `image`, admitted for the run's workload, with its run's own command where it has one, and
/// its grants.
fn image_box(
    name: &str,
    image: &str,
    allocation: Allocation,
    command: Option<Vec<String>>,
    grants: Vec<Grant>,
) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            command,
            grants,
            ..ShardsImage::new(image)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation),
        route: None,
        band: 0,
    }
}

/// The probe's TCP port `port`, published at the host's loopback on a port the OS picks.
fn publish(probe: &Probe, port: u16) -> Grant {
    Grant::Publish {
        member: probe.member.clone(),
        proto: Proto::Tcp,
        port,
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        host_port: None,
    }
}

/// Starts box `id`, its boot followed to its answer: a start past its boot's W* answers `Lifecycle { Pending }`, the
/// boot going on, and `Runtime::started` follows it to its run (decision 9, the person's ruling of 2026-11-13; kiln's
/// HexVms adapter waits the same way). Says the start's refusal, or `started`'s.
fn start(client: &Client, id: &BoxId) -> Result<(), Refusal> {
    match client.start(id) {
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        }) => client.started(id).map(drop),
        started => started,
    }
}

/// Every socket running box `id`'s run's agent-control endpoint holds, as its inspect `shown` says (`BoxInfo.control`
/// and `control_wake`, Shards' `ControlAt::sockets`, image channel v30): its served address, its waker's and its wake's,
/// told on stderr. Hex asks every image run for an endpoint, so a running VM that says none, or its address alone, fails.
fn agent_control(id: &BoxId, shown: &hex_api::BoxInfo) -> [SocketAddr; 3] {
    let control = shown.control_sockets().unwrap_or_else(|| {
        panic!("{id} runs and says no agent-control endpoint's sockets: {shown:?}")
    });
    let [address, waker, wake] = control;
    eprintln!(
        "GRANTS {id}'s agent-control endpoint at {address}, its waker at {waker}, its wake at {wake}"
    );
    control
}

/// Stops box `id` with KILL, Hex's stop of an image VM, and deletes it; expects it ended by that signal. A KILL whose
/// stop outlasts its W* answers `Lifecycle { Pending }`, the stop going on (the person's ruling of 2026-11-13): the box
/// is then looked at, each look the node's own answer, until it shows its end.
fn stop_and_delete(client: &Client, id: &BoxId) {
    match client.signal(id, Signal::KILL) {
        Ok(())
        | Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        }) => {}
        Err(why) => panic!("{id}'s KILL: {why:?}"),
    }
    let mut ended = None;
    // A look: the node tells no event as a stop ends. Its party, the box, ends the wait in the fact itself.
    hex_testkit::waits::follow::until(
        &KILLED,
        &format!("{id} ended by its KILL"),
        || hex_testkit::waits::follow::Party::Live,
        || {
            let shown = client.inspect(id).unwrap();
            let reached = shown.state == BoxState::Empty;
            ended = Some(shown);
            reached
        },
    )
    .unwrap();
    let ended = ended.unwrap();
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    client.delete(id).unwrap();
}

/// Joint plan step 15 (N4): do create, then start, the probe granted by TCP a name its image grants not (its ungranted
/// name). Expect it made, and its start refused Invalid, past the image's grants: Shards' run host refused it before
/// anything started, so the box ended and holds nothing.
fn a_destination_its_image_grants_not_refuses_the_boot(
    client: &Client,
    probe: &Probe,
    allocation: Allocation,
) {
    let spec = image_box(
        "grants-wider",
        &probe.image,
        allocation,
        None,
        vec![Grant::Egress {
            member: probe.member.clone(),
            to: Destination::Name(probe.ungranted.clone()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    // The node's first boot: its start follows the boot to its answer, no W* learned yet (decision 9).
    let refused = client.start(&id);
    eprintln!("GRANTS-CHECK-1 {id} start: {refused:?}");
    assert!(
        matches!(&refused, Err(Refusal::Invalid { what: "shards image", why }) if why.contains("past the image's grants")),
        "{refused:?}"
    );
    let ended = client.inspect(&id).unwrap();
    assert_eq!(ended.state, BoxState::Empty);
    assert!(ended.published.is_empty());
    client.delete(&id).unwrap();
}

/// What `ask` hands over (`Runtime::member` of box `id`), asked again while it answers `Lifecycle { Pending }`, as a
/// box whose run is settling answers it, and `shown` (the box's state, its exit and a failed run's words,
/// `BoxInfo.ended`) says the box lives: each ask the node's own answer, with no timer of the test's (R03), the wait
/// measuring (`MEMBER_HANDED_OVER`). A box that ends first ends the wait, refused naming its end; any other refusal
/// ends it at once. Says it and how many asks it took.
fn handed_over<P>(
    id: &BoxId,
    mut ask: impl FnMut() -> Result<P, Refusal>,
    mut shown: impl FnMut() -> Result<(BoxState, Option<Exit>, Option<String>), Refusal>,
) -> Result<(P, u64), String> {
    let mut answer = None;
    // A look: the node tells no event as a member is handed over, asked again. Its party, the box, ends the wait in
    // the fact itself (a box that ends first, or any other refusal).
    let asks = hex_testkit::waits::follow::until(
        &MEMBER_HANDED_OVER,
        &format!("{id}'s member handed over"),
        || hex_testkit::waits::follow::Party::Live,
        || {
            answer = Some(match ask() {
                Err(Refusal::Lifecycle {
                    cause: LifecycleCause::Pending,
                    ..
                }) => match shown() {
                    Ok((BoxState::Empty, exit, ended)) => Err(format!(
                        "{id} ended before its member was handed over: {exit:?}, {ended:?}"
                    )),
                    Ok(_) => return false,
                    Err(why) => Err(format!("{id}, its member pending: {why:?}")),
                },
                given => given.map_err(|why| format!("{id}'s member: {why:?}")),
            });
            true
        },
    )
    .map_err(|waited| waited.to_string())?;
    let given = answer.ok_or_else(|| format!("{id}'s member: no answer"))??;
    Ok((given, asks))
}

/// The probe's report, asked on its stdin, as its caller drives it (`ShardsImage::members`): the probe handed over
/// ([`handed_over`]), one line written, its answer read from its stdout up to the empty line that ends it. The read
/// waits for the probe's answer with no deadline of its own; a probe that ends first ends its stdout, which refuses
/// the read.
fn asked_on_stdin(client: &Client, id: &BoxId, probe: &Probe) -> Report {
    let (mut member, asks) = handed_over(
        id,
        || client.member(id, &probe.member),
        || {
            client
                .inspect(id)
                .map(|shown| (shown.state, shown.exit, shown.ended))
        },
    )
    .unwrap();
    eprintln!("GRANTS-CHECK-2 {id}: its probe handed over at ask {asks}");
    let given = member.streams().unwrap();
    let mut stdin = hex_api::OwnedStream::from(given.stdin.unwrap());
    let stdout = hex_api::OwnedStream::from(given.stdout.unwrap());
    stdin.write_all(b"\n").unwrap();
    let mut text = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if line.is_empty() {
            return Report::parse(&text).unwrap();
        }
        text.push_str(&line);
        text.push('\n');
    }
    panic!("the probe's stdout ended before its report's end: {text:?}");
}

/// Joint plan step 15, T-T7.7.1 (N4, D250): do run a VM of its members alone granted nothing, its probe driven by this
/// test, while this test listens on the host's loopback at the service port the image grants as
/// `host.docker.internal`, and ask the probe on its stdin. Expect: inspect shows no grant and nothing published, and
/// where its run's agent-control endpoint is bound; its run holds its VM process and one network process (D250: its
/// image links its harness past the microVM, so the run keeps its network, whose network process, told nothing kept,
/// lets nothing past), no socket taking what comes in but that endpoint, and no flow past the host's loopback; the
/// probe says it was asked on its stdin, its provider's name and its ungranted name unresolved (no DNS answer), and
/// none of its targets connected (no packet past the microVM: the host's service by the name its image grants it, and
/// the image's port egress, among them); and the host's service took nothing.
fn no_grant_lets_nothing_past(client: &Client, node: u32, probe: &Probe, allocation: Allocation) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, probe.host_port)).unwrap();
    service.set_nonblocking(true).unwrap();
    let mut spec = image_box("grants-none", &probe.image, allocation, None, Vec::new());
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.members = vec![ImageMember::new(probe.member.clone())];
    }
    let id = spec.id.clone();
    client.create(spec).unwrap();
    start(client, &id).unwrap();
    let shown = client.inspect(&id).unwrap();
    assert!(
        shown.grants.is_empty() && shown.published.is_empty(),
        "{shown:?}"
    );
    let control = agent_control(&id, &shown);
    host::granted_nothing(node, control);
    let report = asked_on_stdin(client, &id, probe);
    eprintln!("GRANTS-CHECK-2 {id}: the probe said {report:?}");
    assert_eq!(report.ingress, Ingress::Stdin);
    assert_eq!(
        targets_of(&report),
        recipe_targets(probe),
        "the probe's targets: its context isn't the recipe's of these inputs"
    );
    for label in ["provider", "ungranted"] {
        assert_eq!(
            outcome_of(&report, label),
            Outcome::Unresolved,
            "{label}: a DNS answer in a VM granted nothing"
        );
    }
    for (target, outcome) in &report.egress {
        assert_ne!(
            *outcome,
            Outcome::Connected,
            "the probe of a VM granted nothing reached {} ({})",
            target.at,
            target.label
        );
    }
    host::nothing_leaves(node);
    match service.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        taken => panic!("the host's service at {}: {taken:?}", probe.host_port),
    }
    eprintln!("GRANTS-CHECK-2 {id}: no DNS answer, no packet past the microVM");
    stop_and_delete(client, &id);
}

/// Each target of a report, as `(label, host:port)`.
fn targets_of(report: &Report) -> Vec<(String, String)> {
    report
        .egress
        .iter()
        .map(|(target, _)| (target.label.clone(), target.at.clone()))
        .collect()
}

/// The targets the recipe gives the probe of these inputs, in its order (scripts/grants-probe/build.sh).
fn recipe_targets(probe: &Probe) -> Vec<(String, String)> {
    [
        ("provider", format!("{}:443", probe.provider)),
        ("ungranted", format!("{}:443", probe.ungranted)),
        ("host", format!("host.docker.internal:{}", probe.host_port)),
        ("open", format!("{}:80", probe.open)),
    ]
    .map(|(label, at)| (label.to_owned(), at))
    .to_vec()
}

/// The outcome of the target labelled `label` in `report`.
fn outcome_of(report: &Report, label: &str) -> Outcome {
    report
        .egress
        .iter()
        .find(|(target, _)| target.label == label)
        .map(|(_, outcome)| outcome.clone())
        .unwrap_or_else(|| panic!("no {label} target in {report:?}"))
}

/// The probe's answer at the published socket `at`, waited for while its VM runs: each look is one connection read to
/// its end, which Shards closes where the probe doesn't take it yet, and a VM that ends first ends the wait, refused
/// naming its end. It measures, with no deadline of its own (the person's rule of 2026-11-13: a wait follows its
/// answering party while that lives); says the answer and how many looks it took.
fn answered_at(client: &Client, id: &BoxId, at: SocketAddr) -> (String, u64) {
    let mut answer = String::new();
    // A look: each is one connection read to its end. Its party, the VM, ends the wait in the fact itself.
    let looks = hex_testkit::waits::follow::until(
        &MEMBER_ANSWERED,
        &format!("the probe's answer at {at}"),
        || hex_testkit::waits::follow::Party::Live,
        || {
            answer.clear();
            if TcpStream::connect(at)
                .and_then(|mut c| c.read_to_string(&mut answer))
                .is_ok()
                && !answer.is_empty()
            {
                return true;
            }
            let shown = client.inspect(id).unwrap();
            assert_eq!(
                shown.state,
                BoxState::Running,
                "{id} ended before its probe answered at {at}: {shown:?}"
            );
            false
        },
    )
    .unwrap();
    (answer, looks)
}

/// Joint plan step 15, AC-T7.7.2: do run a VM of its members alone granted one publish, the probe's port at the host's
/// loopback, the OS picking the host's port, while this test listens on the host's loopback at the service port the
/// image grants as `host.docker.internal`. Expect: inspect shows that one grant and its one published port at
/// 127.0.0.1, reaching the probe at that port; the run listens there alone past its agent-control endpoint, at no
/// wildcard address and not on the image's other port coming in; a connection there reaches the probe, which says it
/// came in on that port and that none of its egress connected (its provider, an ungranted name, the host's service,
/// the image's port egress), each outcome typed; the run holds no flow past the host's loopback; and the host's service
/// took nothing.
fn one_publish_published_alone(client: &Client, node: u32, probe: &Probe, allocation: Allocation) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, probe.host_port)).unwrap();
    service.set_nonblocking(true).unwrap();
    let grant = publish(probe, probe.port);
    let spec = image_box(
        "grants-publish",
        &probe.image,
        allocation,
        None,
        vec![grant.clone()],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    start(client, &id).unwrap();
    let shown = client.inspect(&id).unwrap();
    eprintln!(
        "GRANTS-CHECK-3 {id} granted {:?}, published {:?}",
        shown.grants, shown.published
    );
    assert_eq!(shown.grants, vec![grant]);
    let [published] = shown.published.as_slice() else {
        panic!("published ports: {:?}", shown.published);
    };
    // Its receiver as Shards names it (D231's `Bound::to`): the probe, at its own port.
    assert_eq!(
        (published.grant, published.at.ip(), published.receiver),
        (
            0,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Receiver::Named { port: probe.port }
        )
    );
    let at = published.at;
    host::listens_alone(node, at, probe.ungranted_port, agent_control(&id, &shown));
    let (answer, looks) = answered_at(client, &id, at);
    eprintln!("GRANTS-CHECK-3 the probe said {answer:?} after {looks} looks");
    let report = Report::parse(&answer).unwrap();
    assert_eq!(
        report.ingress,
        Ingress::Port(probe.port),
        "the port it came in on"
    );
    assert_eq!(
        targets_of(&report),
        recipe_targets(probe),
        "the probe's targets: its context isn't the recipe's of these inputs"
    );
    for (target, outcome) in &report.egress {
        assert_ne!(
            *outcome,
            Outcome::Connected,
            "the probe reached {} ({}), which Hex's narrowing drops",
            target.at,
            target.label
        );
    }
    host::nothing_leaves(node);
    match service.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        taken => panic!("the host's service at {}: {taken:?}", probe.host_port),
    }
    eprintln!(
        "GRANTS-CHECK-3 {id}: published at {at} alone, to the probe; its egress reached nothing"
    );
    stop_and_delete(client, &id);
}

/// Joint plan step 15, AC-T7.7.2 (N4): do run a VM of its members alone granted the probe's publish and, by TCP, its
/// provider by name and the host's service at the port its image grants as `host.docker.internal`, while this test
/// listens there on the host's loopback. Expect: inspect shows the three grants and the publish reaching the probe;
/// the probe says it came in on its port, reached its provider and the host's service, and its ungranted name and the
/// image's port egress nowhere; and the host's service took the probe's connection. So a harness reaches exactly its
/// granted provider and kiln's endpoint.
fn granted_destinations_are_reached_alone(client: &Client, probe: &Probe, allocation: Allocation) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, probe.host_port)).unwrap();
    service.set_nonblocking(true).unwrap();
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    let grants = vec![
        publish(probe, probe.port),
        Grant::Egress {
            member: probe.member.clone(),
            to: Destination::Name(probe.provider.clone()),
            ports: tcp(443),
        },
        Grant::Host {
            member: probe.member.clone(),
            ports: tcp(probe.host_port),
        },
    ];
    let spec = image_box(
        "grants-reach",
        &probe.image,
        allocation,
        None,
        grants.clone(),
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    start(client, &id).unwrap();
    let shown = client.inspect(&id).unwrap();
    eprintln!(
        "GRANTS-CHECK-4 {id} granted {:?}, published {:?}",
        shown.grants, shown.published
    );
    assert_eq!(shown.grants, grants);
    let [published] = shown.published.as_slice() else {
        panic!("published ports: {:?}", shown.published);
    };
    assert_eq!(published.receiver, Receiver::Named { port: probe.port });
    let at = published.at;
    let (answer, looks) = answered_at(client, &id, at);
    eprintln!("GRANTS-CHECK-4 the probe said {answer:?} after {looks} looks");
    let report = Report::parse(&answer).unwrap();
    assert_eq!(report.ingress, Ingress::Port(probe.port));
    let outcome = |label: &str| outcome_of(&report, label);
    assert_eq!(
        outcome("provider"),
        Outcome::Connected,
        "its granted provider"
    );
    assert_eq!(
        outcome("host"),
        Outcome::Connected,
        "the host's service, kiln's endpoint"
    );
    assert_ne!(
        outcome("ungranted"),
        Outcome::Connected,
        "a name no grant names"
    );
    assert_ne!(
        outcome("open"),
        Outcome::Connected,
        "the image's port egress, dropped"
    );
    assert!(
        service.accept().is_ok(),
        "the host's service at {} took no connection",
        probe.host_port
    );
    eprintln!("GRANTS-CHECK-4 {id}: its provider and the host's service reached, nothing else");
    stop_and_delete(client, &id);
}

/// Joint plan step 15 (N6): do create, then start, a VM with a command of its own granted a publish of a port its
/// image gives no member, which Shards gives that command (D122). Expect: the start refused Invalid naming the run's
/// own command (beside its run's stop still pending, Uncertain, where that stop outlasts its W*), the box followed to
/// its end by KILL with nothing published, and no run host left under the node.
fn a_publish_reaching_the_runs_own_command_stops_it(
    client: &Client,
    node: u32,
    probe: &Probe,
    allocation: Allocation,
) {
    let command = ["/bin/sh", "-c", "exec tail -f /dev/null"]
        .map(str::to_owned)
        .to_vec();
    let spec = image_box(
        "grants-command",
        &probe.image,
        allocation,
        Some(command),
        vec![publish(probe, probe.command_port)],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    let refused = start(client, &id);
    eprintln!("GRANTS-CHECK-5 {id} start: {refused:?}");
    // The grants' refusal, its run's stop begun: where that stop outlasts its W* the start says so beside it (Uncertain,
    // its cleanup Lifecycle Pending) and the stop goes on (decision 9; B-4's stop answers once its run is released).
    let grants = match &refused {
        Err(Refusal::Uncertain {
            refusal, cleanup, ..
        }) if matches!(
            cleanup.as_ref(),
            Refusal::Lifecycle {
                cause: LifecycleCause::Pending,
                ..
            }
        ) =>
        {
            refusal.as_ref()
        }
        Err(why) => why,
        Ok(()) => panic!("{id} started past its grants"),
    };
    assert!(
        matches!(grants, Refusal::Invalid { what: "grants", why } if why.contains("the run's own command")),
        "{refused:?}"
    );
    // Its end followed: the box looked at, each look the node's own answer, until it shows it.
    let mut ended = None;
    hex_testkit::waits::follow::until(
        &KILLED,
        &format!("{id} ended by its grants' stop"),
        || hex_testkit::waits::follow::Party::Live,
        || {
            let shown = client.inspect(&id).unwrap();
            let reached = shown.state == BoxState::Empty;
            ended = Some(shown);
            reached
        },
    )
    .unwrap();
    let ended = ended.unwrap();
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(ended.published.is_empty());
    host::no_run_left(node);
    client.delete(&id).unwrap();
}

/// The probe's arguments for three targets: a listener of this test's, a port nothing holds and a name RFC 6761 §6.4
/// reserves as never resolving, the probe listening at the host's loopback on a port the OS picks.
/// The closed target's port: a listener of this test's on a port below the range this host gives a connection's source
/// port and a bind to port 0 from ([`ephemeral_first`]), held while the probe binds its own listener and let go
/// (dropped) just before the probe connects to its targets. No bind to port 0 is given it, the probe's own listener's
/// among them, and no connection takes it as its source port and meets itself: a port from bind(0) let go could be
/// both (goal 5's r878 on Linux; ROOT's macOS gate at 45fc1605, where the probe's connection was given the port just let
/// go, the one after its open target's, and met itself: "closed connected"). Held without listening it would do no
/// better: macOS drops a connection to a port bound but not listening, so it times out, never refused.
fn closed_port() -> TcpListener {
    // Each port picked once in this process, so a test running beside another never binds the port the other let go
    // for its probe to find closed.
    static PICKED: std::sync::Mutex<std::collections::BTreeSet<u16>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let first = ephemeral_first();
    let mut picked = PICKED.lock().unwrap();
    let listener = (1..first)
        .rev()
        .filter(|port| !picked.contains(port))
        .find_map(|port| TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok())
        .unwrap_or_else(|| panic!("no port below this host's ephemeral range, {first}, is free"));
    picked.insert(listener.local_addr().unwrap().port());
    listener
}

/// The first port of the range this host gives a connection's source port and a bind to port 0 from, as its OS is
/// configured: Linux's `net.ipv4.ip_local_port_range`, macOS's `net.inet.ip.portrange.first`, Windows' TCP dynamic port
/// range.
fn ephemeral_first() -> u16 {
    #[cfg(target_os = "linux")]
    let said = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    #[cfg(target_os = "macos")]
    let said = String::from_utf8(
        std::process::Command::new("sysctl")
            .args(["-n", "net.inet.ip.portrange.first"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    #[cfg(windows)]
    let said = {
        let shown = String::from_utf8(
            std::process::Command::new("netsh")
                .args(["int", "ipv4", "show", "dynamicport", "tcp"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        shown
            .lines()
            .find_map(|line| line.trim().strip_prefix("Start Port"))
            .and_then(|rest| rest.split(':').nth(1))
            .unwrap_or_else(|| panic!("netsh shows no TCP start port: {shown}"))
            .to_owned()
    };
    said.split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .unwrap_or_else(|| panic!("no first ephemeral port in {said:?}"))
}

fn three_targets(open: &TcpListener, closed: &TcpListener) -> Vec<std::ffi::OsString> {
    let closed = closed.local_addr().unwrap();
    [
        "--address".to_owned(),
        "127.0.0.1".to_owned(),
        "--listen".to_owned(),
        "0".to_owned(),
        "--egress".to_owned(),
        format!("open={}", open.local_addr().unwrap()),
        "--egress".to_owned(),
        format!("closed={closed}"),
        "--egress".to_owned(),
        "nowhere=nowhere.invalid:80".to_owned(),
    ]
    .map(Into::into)
    .to_vec()
}

/// Joint plan step 15 (the probe, no VM): do start the probe at the host's loopback with three targets, a listener of
/// this test's, a port nothing holds and a name RFC 6761 §6.4 reserves as never resolving, then connect to it. Expect
/// its answer to read back typed: the port it came in on, then each target in turn, connected, refused and
/// unresolved. Each target is answered by this host's own TCP stack and resolver (a listener, a closed port's reset,
/// the resolver's answer), which the probe waits for, with no wait of its own: the test waits on those facts.
#[test]
fn the_probe_answers_each_connection_with_its_targets_outcomes() {
    let open = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = closed_port();
    let told = probe::Told::read(&three_targets(&open, &closed)).unwrap();
    let started = probe::start(&told).unwrap();
    let [at] = started.at.as_slice() else {
        panic!("{:?}", started.at);
    };
    // The probe's listener is bound: the closed target's port is let go now, before the probe connects to it.
    drop(closed);
    let mut answer = String::new();
    TcpStream::connect(at)
        .unwrap()
        .read_to_string(&mut answer)
        .unwrap();
    let report = Report::parse(&answer).unwrap();
    assert_eq!(report.ingress, Ingress::Port(at.port()));
    let outcomes: Vec<(&str, &Outcome)> = report
        .egress
        .iter()
        .map(|(target, outcome)| (target.label.as_str(), outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("open", &Outcome::Connected),
            ("closed", &Outcome::Refused),
            ("nowhere", &Outcome::Unresolved)
        ],
        "{answer}"
    );
    // Its servers run on for the VM's life; this test's end ends them.
    drop(started.servers);
}

/// Joint plan step 15 (the hand-over, no VM; R03): do ask a hand-over of a fake that answers `Lifecycle { Pending }`
/// twice, its box running, as a box settling does, then hands its member over. Expect it handed over at the third ask.
/// Do ask one that answers Pending while its box has ended; expect the wait refused naming the box's end, its exit and
/// its run's words (`BoxInfo.ended`). Do ask one that refuses otherwise; expect that refusal, at the first ask.
#[test]
fn a_members_hand_over_is_asked_again_while_its_box_settles() {
    let id = BoxId::new("grants-handover").unwrap();
    let pending = || Refusal::Lifecycle {
        id: id.clone(),
        cause: LifecycleCause::Pending,
        why: None,
    };
    let mut answers = vec![Err(pending()), Err(pending()), Ok("the probe")].into_iter();
    let handed = handed_over(
        &id,
        || answers.next().unwrap(),
        || Ok((BoxState::Running, None, None)),
    );
    assert_eq!(handed, Ok(("the probe", 3)));
    let ended = handed_over(
        &id,
        || Err::<(), _>(pending()),
        || {
            Ok((
                BoxState::Empty,
                Some(Exit::Code(1)),
                Some("its run host's words".to_owned()),
            ))
        },
    );
    assert!(
        matches!(&ended, Err(why) if why.contains("ended")
            && why.contains("Code(1)")
            && why.contains("its run host's words")),
        "{ended:?}"
    );
    let mut asked = 0;
    let other = handed_over(
        &id,
        || {
            asked += 1;
            Err::<(), _>(Refusal::NotRunning(id.clone(), "member"))
        },
        || Ok((BoxState::Running, None, None)),
    );
    assert!(
        matches!(&other, Err(why) if why.contains("NotRunning")),
        "{other:?}"
    );
    assert_eq!(asked, 1);
}

/// Joint plan step 15, T-T7.7.1 (the probe, no VM): do ask the probe on its stdin, as its harness is asked where its
/// caller drives it (`ShardsImage::members`), two lines, then end its stdin. Expect two reports on its stdout, each
/// ended by an empty line and read back typed: asked on its stdin, then each of the three targets in turn, connected,
/// refused and unresolved; and the answering done at its stdin's end.
#[test]
fn the_probe_answers_each_line_on_its_stdin_with_its_targets_outcomes() {
    let open = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = closed_port();
    let told = probe::Told::read(&three_targets(&open, &closed)).unwrap();
    drop(closed);
    let mut out = Vec::new();
    probe::answer("\nagain\n".as_bytes(), &mut out, &told.targets).unwrap();
    let text = String::from_utf8(out).unwrap();
    let reports: Vec<Report> = text
        .split_terminator("\n\n")
        .map(|report| Report::parse(report).unwrap())
        .collect();
    assert_eq!(reports.len(), 2, "{text:?}");
    for report in reports {
        assert_eq!(report.ingress, Ingress::Stdin);
        let outcomes: Vec<(&str, &Outcome)> = report
            .egress
            .iter()
            .map(|(target, outcome)| (target.label.as_str(), outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("open", &Outcome::Connected),
                ("closed", &Outcome::Refused),
                ("nowhere", &Outcome::Unresolved)
            ],
            "{text}"
        );
    }
}

/// Joint plan step 15 (the probe, no VM): do write a report of every outcome, asked on a port and on the probe's
/// stdin, and read each back; expect each whole. Do read reports with no ingress line first, an ingress that is no
/// port and not stdin, an egress line short of a word, and an outcome no outcome is; expect each refused, never read
/// as another. Do read the probe's arguments short of each one it needs, and with a flag it has no use for, a wait of
/// its own among them (it waits for each try's answer); expect each refused by name.
#[test]
fn the_probes_report_and_arguments_read_back_typed() {
    let target = |label: &str| probe::Target {
        label: label.to_owned(),
        at: "api.example:443".to_owned(),
    };
    let egress: Vec<(probe::Target, Outcome)> = [
        Outcome::Connected,
        Outcome::Refused,
        Outcome::Unresolved,
        Outcome::Unreachable,
        Outcome::TimedOut,
        Outcome::Failed("PermissionDenied".to_owned()),
    ]
    .into_iter()
    .enumerate()
    .map(|(n, outcome)| (target(&format!("t{n}")), outcome))
    .collect();
    for (ingress, line) in [
        (Ingress::Port(7101), "ingress 7101\n"),
        (Ingress::Stdin, "ingress stdin\n"),
    ] {
        let every = Report {
            ingress,
            egress: egress.clone(),
        };
        let text = every.egress.iter().fold(line.to_owned(), |text, (t, o)| {
            text + &format!("egress {} {} {o}\n", t.label, t.at)
        });
        assert_eq!(Report::parse(&text).unwrap(), every);
    }
    for bad in [
        "",
        "egress a api.example:443 connected\n",
        "ingress stdout\negress a api.example:443 connected\n",
        "ingress 7101\negress a api.example:443\n",
        "ingress 7101\negress a api.example:443 connected-ish\n",
        "ingress 7101\negress a api.example:443 failed:\n",
    ] {
        assert!(Report::parse(bad).is_err(), "{bad:?}");
    }
    let read = |args: &[&str]| {
        let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        probe::Told::read(&args)
    };
    let whole = [
        "--address",
        "0.0.0.0",
        "--listen",
        "7101",
        "--egress",
        "provider=api.example:443",
    ];
    let told = read(&whole).unwrap();
    assert_eq!(
        (told.address, told.listen, told.targets.len()),
        (IpAddr::V4(Ipv4Addr::UNSPECIFIED), vec![7101], 1)
    );
    let no_label = [&whole[..], &["--egress", "nolabel"]].concat();
    let unknown = [&whole[..], &["--shout", "x"]].concat();
    let waits = [&whole[..], &["--wait-ms", "1000"]].concat();
    for (args, says) in [
        (&whole[2..], "--address"),
        (&whole[..2], "--listen"),
        (&no_label[..], "--egress"),
        (&unknown[..], "--shout"),
        (&waits[..], "--wait-ms"),
    ] {
        let refused = read(args).unwrap_err();
        assert!(refused.contains(says), "{args:?}: {refused}");
    }
}

#[test]
#[ignore = "native: KVM, HVF or WHP, a Shards guest configuration and the probe image (HEX_GRANTS_CONTEXT or HEX_GRANTS_IMAGE); on Linux a delegated cgroup"]
fn a_vms_network_is_its_grants_alone_on_a_real_node() {
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let kernel = PathBuf::from(input("SHARDS_KERNEL"));
    assert!(kernel.is_file(), "SHARDS_KERNEL {kernel:?} is no file");
    // A kernel goes with its init in Shards (`--kernel` and `--init` together); the node's run hosts inherit both.
    let init = PathBuf::from(input("SHARDS_INIT"));
    assert!(init.is_file(), "SHARDS_INIT {init:?} is no file");
    let work = PathBuf::from(input("HEX_IMAGE_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    let image = match std::env::var_os("HEX_GRANTS_CONTEXT") {
        Some(context) => built(&shards, &PathBuf::from(context), &work, BUILT),
        None => {
            let image = input("HEX_GRANTS_IMAGE");
            ShardsImage::new(&image).check().unwrap();
            pull(&shards, &image, &work.join("state"));
            image
        }
    };
    ShardsImage::new(&image).check().unwrap();
    let probe = Probe {
        image,
        member: Member {
            kind: MemberKind::Harness,
            name: input("HEX_GRANTS_MEMBER"),
        },
        port: port("HEX_GRANTS_PORT"),
        ungranted_port: port("HEX_GRANTS_UNGRANTED_PORT"),
        provider: input("HEX_GRANTS_PROVIDER"),
        ungranted: input("HEX_GRANTS_UNGRANTED"),
        host_port: port("HEX_GRANTS_HOST_PORT"),
        open: input("HEX_GRANTS_OPEN"),
        command_port: port("HEX_GRANTS_COMMAND_PORT"),
    };
    // A VM's start boots it: each wait is learned as the run goes, and past its W* tells its run-out and waits for its
    // answer while its party lives (the person's rule of 2026-11-13), the run's whole with no deadline of its own.
    assert!(
        hex_core::patience::census(step),
        "a census sink was installed before this one"
    );
    let allocation = allocation();
    eprintln!("GRANTS each VM {:?}", allocation.vm_size().unwrap());
    // On Linux the node's boxes go in a cgroup delegated to this binary, as image_real's do.
    #[cfg(target_os = "linux")]
    let boxes = {
        assert!(
            std::env::var_os("HEX_DELEGATED").is_some(),
            "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
        );
        Some(image_node::own_cgroup("hex-image-grants-real").join("hex-image-grants-node"))
    };
    #[cfg(not(target_os = "linux"))]
    let boxes: Option<PathBuf> = None;
    let node = Node::start(&work, boxes.as_deref());
    let (socket, pid) = (node.socket.clone(), node.child.id());
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let client = Client::connect(&socket).unwrap();
        // P2.2: a node that boots image VMs lists their class (its host check said it runs microVMs).
        assert!(
            client.classes().contains(&Class::ShardsImage),
            "{:?}",
            client.classes()
        );
        a_destination_its_image_grants_not_refuses_the_boot(&client, &probe, allocation);
        no_grant_lets_nothing_past(&client, pid, &probe, allocation);
        one_publish_published_alone(&client, pid, &probe, allocation);
        granted_destinations_are_reached_alone(&client, &probe, allocation);
        a_publish_reaching_the_runs_own_command_stops_it(&client, pid, &probe, allocation);
        let _ = done.send(client.list().unwrap());
    });
    let left = match finished.recv() {
        Ok(left) => left,
        Err(mpsc::RecvError) => panic!("a check failed: its panic is above"),
    };
    drop(node);
    records(&work.join("census.txt"));
    assert!(left.is_empty(), "a box is left: {left:?}");
}
