//! The floor over TCP (bench/README.md, "The comparison"): what any broker reached over loopback
//! pays before its own work, a hop to a server and on to each subscriber. A relay that does nothing
//! but forward, as a server of its own (`fanout relay`), on the brokers' CPUs as `bench/compare.sh`
//! starts it: every frame the publisher writes (its length, 4 bytes little-endian, then its payload)
//! written on to every subscriber of its topic, whole frames only, as many as one read took in one
//! write each. No queue but the sockets', nothing parsed but the lengths, and no sleep while a topic
//! is published: the publisher's thread polls its socket and the writers poll what it hands them, so
//! the relay adds the kernel's sends and receives over loopback and no wake-up of its own. At one
//! subscriber, one read and one write a frame, no broker a hop away costs less; at fan-out it writes a
//! share of subscribers at a time, so a broker can beat it at the tail (NATS did at 100 polling
//! subscribers, bench/records/2026-10-24-fedora-quiet), and there it bounds the hop from above. Its
//! CPU is its time, not its work: it polls its socket by reading it.
//!
//! A connection says what it is in its first line: `P <topic>` (the publisher) or `S <topic>` (a
//! subscriber). A topic's subscribers are shared out over `--threads` writers (by default the CPUs
//! it may use): the publisher's own thread writes to the first share, and a writer of its own to
//! each other share once it has a subscriber, handed what was read; with one subscriber nothing is
//! handed on. When the publisher's connection closes, the writers finish what they were handed, the
//! relay prints what the topic took (frames, deliveries, its CPU in user space and in the kernel),
//! and lets the topic's subscribers go.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Reads handed to a writer and not yet written, at most: past it the publisher's thread waits, and
/// the publisher's socket fills behind it.
const HANDED: usize = 1024;

/// What one read took, handed to a writer: whole frames, and how many.
type Handed = (Arc<[u8]>, u64);

/// A writer: what hands it reads, and its thread, which ends with the deliveries it made.
type Writer = (SyncSender<Handed>, JoinHandle<u64>);

/// One writer's subscribers.
#[derive(Default)]
struct Share {
    subscribers: Mutex<Vec<TcpStream>>,
    /// How many it has, read without its lock (the writer holds that while it writes).
    count: AtomicUsize,
}

struct Topic {
    shares: Vec<Arc<Share>>,
    joined: usize,
}

type Topics = Arc<Mutex<HashMap<String, Topic>>>;

fn topic<'a>(all: &'a mut HashMap<String, Topic>, name: &str, threads: usize) -> &'a mut Topic {
    all.entry(name.to_owned()).or_insert_with(|| Topic {
        shares: (0..threads).map(|_| Arc::default()).collect(),
        joined: 0,
    })
}

/// Serves `listen` until the process is stopped.
pub fn serve(listen: &str, threads: usize) {
    let threads = threads.max(1);
    let listener =
        TcpListener::bind(listen).unwrap_or_else(|e| panic!("relay: binding {listen}: {e}"));
    println!("relay: on {listen}, a topic's subscribers over {threads} writers");
    let topics: Topics = Arc::default();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let topics = Arc::clone(&topics);
        std::thread::spawn(move || connection(stream, &topics, threads));
    }
}

fn connection(stream: TcpStream, topics: &Topics, threads: usize) {
    if stream.set_nodelay(true).is_err() {
        return;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Some((kind, name)) = line.trim_end().split_once(' ') else {
        return;
    };
    match kind {
        // A subscriber sends nothing after its line: what the reader holds is all it sent.
        "S" => {
            let mut all = topics.lock().unwrap();
            let topic = topic(&mut all, name, threads);
            let share = &topic.shares[topic.joined % threads];
            topic.joined += 1;
            let mut subscribers = share.subscribers.lock().unwrap();
            subscribers.push(reader.into_inner());
            share.count.store(subscribers.len(), Ordering::Release);
        }
        "P" => publisher(reader, topics, name, threads),
        _ => {}
    }
}

/// Writes `frames` to each subscriber of `share`, letting go of any whose connection failed;
/// returns the deliveries, `n` frames to each subscriber written to.
fn write(share: &Share, frames: &[u8], n: u64) -> u64 {
    let mut subscribers = share.subscribers.lock().unwrap();
    subscribers.retain_mut(|s| s.write_all(frames).is_ok());
    share.count.store(subscribers.len(), Ordering::Release);
    n * subscribers.len() as u64
}

/// A writer for `share`: it polls for what it's handed until the publisher's thread lets it go.
fn writer(share: Arc<Share>) -> Writer {
    let (tx, rx) = sync_channel::<Handed>(HANDED);
    let thread = std::thread::spawn(move || {
        let rx: Receiver<Handed> = rx;
        let mut deliveries = 0;
        loop {
            match rx.try_recv() {
                Ok((frames, n)) => deliveries += write(&share, &frames, n),
                Err(TryRecvError::Empty) => std::hint::spin_loop(),
                Err(TryRecvError::Disconnected) => return deliveries,
            }
        }
    });
    (tx, thread)
}

fn publisher(reader: BufReader<TcpStream>, topics: &Topics, name: &str, threads: usize) {
    let shares = topic(&mut topics.lock().unwrap(), name, threads)
        .shares
        .clone();
    let before = hyper_measure::usage::this().ok();
    let mut writers: Vec<Option<Writer>> = (1..threads).map(|_| None).collect();
    let mut buf = vec![0u8; 1 << 20];
    // What the line's reader took past the line comes first.
    let held = reader.buffer().len();
    buf[..held].copy_from_slice(reader.buffer());
    let mut have = held;
    let mut stream = reader.into_inner();
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let (mut frames_in, mut bytes_in, mut deliveries) = (0u64, 0u64, 0u64);
    loop {
        // The whole frames at the front of what's been read.
        let (mut end, mut n) = (0, 0u64);
        while have - end >= 4 {
            let len = u32::from_le_bytes(buf[end..end + 4].try_into().unwrap()) as usize;
            if have - end - 4 < len {
                break;
            }
            end += 4 + len;
            n += 1;
        }
        if end > 0 {
            frames_in += n;
            bytes_in += end as u64;
            let mut handed: Option<Arc<[u8]>> = None;
            for (share, w) in shares[1..].iter().zip(&mut writers) {
                if share.count.load(Ordering::Acquire) == 0 {
                    continue;
                }
                let (tx, _) = w.get_or_insert_with(|| writer(Arc::clone(share)));
                let frames = handed.get_or_insert_with(|| Arc::from(&buf[..end]));
                // A writer gone (it panicked) delivers nothing more, which the count shows.
                let _ = tx.send((Arc::clone(frames), n));
            }
            deliveries += write(&shares[0], &buf[..end], n);
            buf.copy_within(end..have, 0);
            have -= end;
        }
        if have == buf.len() {
            // A frame longer than the buffer: room for it.
            buf.resize(buf.len() * 2, 0);
        }
        match stream.read(&mut buf[have..]) {
            Ok(0) => break,
            Ok(read) => have += read,
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(_) => break,
        }
    }
    for (tx, thread) in writers.into_iter().flatten() {
        drop(tx);
        deliveries += thread.join().unwrap_or(0);
    }
    let used = match (before, hyper_measure::usage::this().ok()) {
        (Some(b), Some(a)) => Some(a.since(&b)),
        _ => None,
    };
    let subscribers: usize = shares.iter().map(|s| s.count.load(Ordering::Acquire)).sum();
    println!(
        "relay: {name}: {frames_in} frames ({bytes_in} bytes) in, {deliveries} deliveries to {subscribers} subscribers; {}",
        used.map_or_else(
            || "its CPU unread".to_owned(),
            |u| format!(
                "CPU {:.3} s user, {:.3} s system (its time, polling)",
                u.user_ns as f64 / 1e9,
                u.system_ns as f64 / 1e9
            )
        )
    );
    // The topic's subscribers let go: their connections close.
    topics.lock().unwrap().remove(name);
}
