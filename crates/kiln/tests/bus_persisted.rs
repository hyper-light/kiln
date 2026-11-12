//! CP-T2.2 step 8, its first step (AC-T2.2.1): the server's bus keeps a persisted topic's every
//! acknowledged message across a kill -9. A process holding the bus as the server opens it
//! (`QuiverBus::open_server`: the persisted class, a cluster of one, in a data directory) publishes
//! to `kiln.events` and says each acknowledgement as it comes; it's killed -9 near the end, and a process
//! started on the same directory reads the topic back: every acknowledged message at its offset,
//! none twice. The messages fill most of what the topic keeps (`[bus] persisted_bytes`), more than
//! a day of kiln's events (docs/bus.md, "The persisted class"), so the restart's view is held to
//! keeping them all; and how long the restarted bus took to serve them is said.
//!
//! Each process is this test's binary again, told its part by `KILN_BUS_CHILD`.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use kiln::bus::{Config, QuiverBus};
use kiln_seams::bus::{Bus, Class, Refusal, Topic};

const CHILD: &str = "KILN_BUS_CHILD";
const TEST: &str = "cp_t2_2_a_persisted_topic_keeps_every_acknowledged_message_across_kill_9";
const TOPIC: &str = "kiln.events";
/// Messages published, and each one's bytes: 42 MB of the topic's 64 MiB.
const MESSAGES: usize = 30_000;
const SIZE: usize = 1400;
/// Acknowledgements the parent waits for before it kills the publisher: most of them, so the
/// restart is held to nearly all it published (39 MB).
const KILL_AFTER: usize = MESSAGES - 2000;
/// Publishes in flight at once.
const IN_FLIGHT: usize = 64;
/// How long a process may take to open its bus and serve the topic.
const OPEN_WITHIN: Duration = Duration::from_secs(120);

/// The message numbered `i`: its number, then filler to `SIZE` bytes.
fn payload(i: usize) -> Vec<u8> {
    let mut p = format!("{i:08}").into_bytes();
    p.resize(SIZE, b'x');
    p
}

fn number(payload: &[u8]) -> Option<usize> {
    std::str::from_utf8(payload.get(..8)?).ok()?.parse().ok()
}

/// The bus as the server opens it, on `dir`, once its persisted class has caught up with its
/// catalog: and how long that took.
fn open(dir: &Path) -> anyhow::Result<(QuiverBus, Duration)> {
    let began = Instant::now();
    let bus = QuiverBus::open_server(&Config::default(), dir, kiln::rt::clock())?;
    anyhow::ensure!(bus.persisted_refused().is_none(), "the persisted class didn't open: {:?}", bus.persisted_refused());
    while !bus.persisted_stats().is_some_and(|s| s.settled) {
        anyhow::ensure!(began.elapsed() < OPEN_WITHIN, "the bus never caught up with its catalog");
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok((bus, began.elapsed()))
}

/// The publisher: makes the topic, publishes every message, `IN_FLIGHT` at once, and prints
/// `acked <offset> <number>` for each acknowledgement as it comes.
fn publish(dir: &Path) -> anyhow::Result<()> {
    let (bus, _) = open(dir)?;
    match futures::executor::block_on(bus.create(TOPIC, Topic { class: Class::Persisted, retain: 1_000_000 })) {
        Ok(()) | Err(Refusal::Exists(_)) => {}
        Err(e) => anyhow::bail!("{TOPIC}: {e:?}"),
    }
    let mut out = std::io::stdout().lock();
    // The test harness said `test … …` without ending its line.
    writeln!(out)?;
    let acks = futures::stream::iter(0..MESSAGES)
        .map(|i| {
            let bus = &bus;
            async move { (i, bus.publish(TOPIC, None, payload(i)).await) }
        })
        .buffer_unordered(IN_FLIGHT);
    futures::executor::block_on(acks.for_each(|(i, acked)| {
        match acked {
            Ok(offset) => {
                let _ = writeln!(out, "acked {offset} {i}");
                let _ = out.flush();
            }
            Err(e) => {
                let _ = writeln!(out, "refused {i} {e:?}");
            }
        }
        futures::future::ready(())
    }));
    Ok(())
}

/// The reader: opens the bus on what the publisher left, waits until the topic holds at least
/// `least` messages, and prints `open <ms>` and then `held <offset> <number>` for each.
fn read(dir: &Path, least: u64) -> anyhow::Result<()> {
    let (bus, took) = open(dir)?;
    let began = Instant::now();
    let (first, end) = loop {
        match futures::executor::block_on(bus.bounds(TOPIC)) {
            Ok((first, end)) if end >= least => break (first, end),
            Ok(_) | Err(Refusal::NoTopic(_) | Refusal::Unavailable(_)) => {}
            Err(e) => anyhow::bail!("{TOPIC}'s bounds: {e:?}"),
        }
        anyhow::ensure!(began.elapsed() < OPEN_WITHIN, "{TOPIC} never held {least} messages again");
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut out = std::io::stdout().lock();
    writeln!(out)?;
    writeln!(out, "open {} {}", took.as_millis(), began.elapsed().as_millis())?;
    let mut at = first;
    while at < end {
        let batch = futures::executor::block_on(bus.read(TOPIC, at, 1000)).map_err(|e| anyhow::anyhow!("read at {at}: {e:?}"))?;
        anyhow::ensure!(!batch.messages.is_empty(), "nothing read at {at} of {end}");
        for m in &batch.messages {
            writeln!(out, "held {} {}", m.offset, number(&m.payload).map_or_else(|| "?".to_owned(), |n| n.to_string()))?;
        }
        at = batch.next;
    }
    out.flush()?;
    Ok(())
}

/// This test's binary again, as `role` on `dir`.
fn child(role: &str, dir: &Path) -> anyhow::Result<std::process::Child> {
    Ok(Command::new(std::env::current_exe()?)
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, role)
        .env("KILN_BUS_DIR", dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?)
}

/// CP-T2.2 step 8 (AC-T2.2.1): do publish 30,000 messages of 1,400 bytes to the server bus's
/// persisted `kiln.events`, kill -9 the publishing process once 28,000 are acknowledged, and open the
/// bus again on its directory; expect every acknowledged message read back at the offset it was
/// acknowledged at, and no message twice.
#[test]
fn cp_t2_2_a_persisted_topic_keeps_every_acknowledged_message_across_kill_9() -> anyhow::Result<()> {
    if let Ok(role) = std::env::var(CHILD) {
        let dir = std::env::var_os("KILN_BUS_DIR").ok_or_else(|| anyhow::anyhow!("no KILN_BUS_DIR"))?;
        let dir = Path::new(&dir);
        return match role.split_once(' ') {
            None if role == "publish" => publish(dir),
            Some(("read", least)) => read(dir, least.parse()?),
            _ => anyhow::bail!("no role {role}"),
        };
    }
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("bus");

    let mut publisher = child("publish", &dir)?;
    let lines = BufReader::new(publisher.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?);
    let began = Instant::now();
    let mut acked: BTreeMap<u64, usize> = BTreeMap::new();
    for line in lines.lines() {
        let line = line?;
        let mut words = line.split(' ');
        match (words.next(), words.next(), words.next()) {
            (Some("acked"), Some(offset), Some(i)) => {
                acked.insert(offset.parse()?, i.parse()?);
            }
            (Some("refused"), ..) => anyhow::bail!("a publish was refused: {line}"),
            _ => continue,
        }
        if acked.len() >= KILL_AFTER {
            break;
        }
    }
    publisher.kill()?;
    publisher.wait()?;
    let published = began.elapsed();
    anyhow::ensure!(acked.len() >= KILL_AFTER, "the publisher ended after {} acknowledgements", acked.len());

    let least = acked.keys().next_back().map_or(0, |last| last + 1);
    let reader = child(&format!("read {least}"), &dir)?.wait_with_output()?;
    anyhow::ensure!(reader.status.success(), "the reader failed: {}", reader.status);
    let mut held: BTreeMap<u64, usize> = BTreeMap::new();
    let mut numbers = std::collections::HashSet::new();
    let mut opened = None;
    for line in String::from_utf8(reader.stdout)?.lines() {
        let mut words = line.split(' ');
        match (words.next(), words.next(), words.next()) {
            (Some("held"), Some(offset), Some(i)) => {
                let i: usize = i.parse()?;
                anyhow::ensure!(numbers.insert(i), "message {i} is held twice");
                held.insert(offset.parse()?, i);
            }
            (Some("open"), Some(bus), Some(topic)) => opened = Some((bus.parse::<u64>()?, topic.parse::<u64>()?)),
            _ => {}
        }
    }
    let lost: Vec<(u64, usize)> = acked.iter().filter(|(offset, i)| held.get(offset) != Some(i)).map(|(o, i)| (*o, *i)).collect();
    assert!(lost.is_empty(), "{} of {} acknowledged messages not at their offsets after the kill, the first {:?}", lost.len(), acked.len(), lost.first());
    let (bus_ms, topic_ms) = opened.ok_or_else(|| anyhow::anyhow!("the reader never said how long it took"))?;
    println!(
        "{} acknowledged in {:.1} s before the kill -9, {} held after it (bytes {}); the restarted bus caught up in {bus_ms} ms and served the topic {topic_ms} ms later",
        acked.len(),
        published.as_secs_f64(),
        held.len(),
        held.len() * SIZE
    );
    Ok(())
}
