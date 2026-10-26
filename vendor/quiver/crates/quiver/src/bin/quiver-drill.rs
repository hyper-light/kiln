//! `quiver-drill`: one node of a persisted cluster, driven by lines on its standard input and
//! answering each on its standard output: the node the kill drills run as processes (kiln's
//! CP-T2.2, `tests/persisted_processes.rs`), each killed with SIGKILL at a persistence point and
//! started again on what its directory holds.
//!
//! `quiver-drill --id N --nodes 1=ADDRESS,2=ADDRESS,… --data DIR --identities DIR
//! [--identities-key FILE]`
//!
//! It says `ready RUN` once it has caught up with its catalog's group. Each line it reads is answered by one line:
//! `create NAME` by `ok` or `refused WHY`; `publish NAME PAYLOAD` by `acked OFFSET` or
//! `refused WHY`; `read NAME FROM` by `messages COUNT NEXT` and the messages, a line each
//! (`OFFSET PAYLOAD`); `figures NAME` by `figures LEADS TERM END`. Standard error carries
//! `frames N` each time the frames its logs wrote and flushed reach N: its persistence points.
//! It ends at the end of its input.

use std::future::Future;
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use quiver::{Node, NodeConfig, PersistedConfig};
use quiver_api::{Class, Topic, Topics};

/// How often the frames its logs wrote are looked at: a persistence point is told within this.
const WATCH: Duration = Duration::from_millis(1);

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` on this thread to its end.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut context) {
            return out;
        }
        std::thread::park();
    }
}

fn config(args: &[String]) -> Result<PersistedConfig, String> {
    let mut id = None;
    let mut nodes = Vec::new();
    let mut data = None;
    let mut identities = None;
    let mut identities_key = None;
    let mut at = args.iter();
    while let Some(flag) = at.next() {
        let value = at.next().ok_or_else(|| format!("{flag} takes a value"))?;
        match flag.as_str() {
            "--id" => id = Some(value.parse::<u64>().map_err(|e| format!("--id: {e}"))?),
            "--nodes" => {
                for node in value.split(',') {
                    let (n, address) = node.split_once('=').ok_or("--nodes: ID=ADDRESS,…")?;
                    let n = n.parse::<u64>().map_err(|e| format!("--nodes: {e}"))?;
                    let address = address
                        .parse::<SocketAddr>()
                        .map_err(|e| format!("--nodes: {e}"))?;
                    nodes.push((n, address));
                }
            }
            "--data" => data = Some(PathBuf::from(value)),
            "--identities" => identities = Some(PathBuf::from(value)),
            "--identities-key" => identities_key = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(PersistedConfig {
        id: id.ok_or("--id is needed")?,
        nodes,
        data: data.ok_or("--data is needed")?,
        identities: identities.ok_or("--identities is needed")?,
        identities_key,
        part_memory: PersistedConfig::PART_MEMORY_MIN,
        topics: 16,
        publishes: 4096,
        quorum_wait: Duration::from_secs(10),
    })
}

/// The answer to one line.
fn answer(node: &Node, line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let refused = |e: quiver_api::Refusal| format!("refused {e}");
    match words.as_slice() {
        ["create", name] => {
            let topic = Topic {
                class: Class::Persisted,
                retain_messages: 100_000,
                retain_bytes: 1 << 26,
            };
            block_on(node.create(name, topic)).map_or_else(refused, |()| "ok".to_owned())
        }
        ["publish", name, payload] => block_on(node.publish(name, None, payload.as_bytes()))
            .map_or_else(refused, |offset| format!("acked {offset}")),
        ["read", name, from] => {
            let Ok(from) = from.parse::<u64>() else {
                return "refused FROM is an offset".to_owned();
            };
            match block_on(node.read(name, from, quiver_api::READ_MAX)) {
                Ok(batch) => {
                    let mut out = format!("messages {} {}", batch.messages.len(), batch.next);
                    for m in batch.messages {
                        out.push_str(&format!(
                            "\n{} {}",
                            m.offset,
                            String::from_utf8_lossy(&m.payload)
                        ));
                    }
                    out
                }
                Err(e) => refused(e),
            }
        }
        ["figures", name] => {
            let stats = node.persisted_stats().unwrap_or_default();
            let topic = stats.topic(name).copied().unwrap_or_default();
            let end = block_on(node.bounds(name)).map_or(0, |(_, end)| end);
            format!("figures {} {} {end}", u8::from(topic.leads), topic.term)
        }
        _ => format!("refused unknown command {line:?}"),
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let config = config(args)?;
    let node = Node::with_persisted(
        NodeConfig {
            memory: 256 << 20,
            topics: 16,
            subscriptions: 16,
        },
        config,
    )
    .map_err(|e| e.to_string())?;
    // Ready once it caught up with its catalog's group: until then it refuses what it can't tell.
    while !node
        .persisted_stats()
        .is_some_and(|s| s.settled || s.ended.is_some())
    {
        std::thread::park_timeout(WATCH);
    }
    let run = node.persisted_stats().map_or(0, |s| s.run);
    let mut out = std::io::stdout().lock();
    writeln!(out, "ready {run}")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    let done = Arc::new(AtomicBool::new(false));
    let watcher = std::thread::spawn({
        let node = node.clone();
        let done = Arc::clone(&done);
        move || {
            let mut told = 0;
            let mut ended = false;
            while !done.load(Ordering::Relaxed) {
                let stats = node.persisted_stats().unwrap_or_default();
                if let (Some(why), false) = (&stats.ended, ended) {
                    ended = true;
                    let _ = writeln!(std::io::stderr().lock(), "ended {why}");
                }
                let frames = stats.frames();
                if frames != told {
                    told = frames;
                    let mut err = std::io::stderr().lock();
                    if writeln!(err, "frames {frames}").is_err() {
                        return;
                    }
                }
                std::thread::park_timeout(WATCH);
            }
        }
    });
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let reply = answer(&node, &line);
        writeln!(out, "{reply}")
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())?;
    }
    done.store(true, Ordering::Relaxed);
    watcher.thread().unpark();
    let _ = watcher.join();
    node.halt_persisted();
    Ok(())
}

fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string())
        .collect();
    let outcome = match args {
        Ok(args) => run(&args),
        Err(_) => Err("an argument isn't Unicode".to_owned()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(std::io::stderr().lock(), "quiver-drill: {why}");
            ExitCode::from(2)
        }
    }
}
