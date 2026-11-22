//! AL.1's measurement (docs/design/audit.md): a hash chain against RFC 6962's Merkle tree, the two
//! tamper-evident log designs Crosby and Wallach compare (USENIX Security 2009), for the cost to
//! append an event and to audit: show one event is in the head, show a head extends an earlier one,
//! and read every event verified. Both run in memory over the same events, so the numbers are the
//! designs' hashing and the rows each would read or write, not a store's.
//!
//! cargo run --release -p amphora --example audit_cost -- --events N --bytes B --samples S --repeats R
//!
//! The per-event operations run for S event indices (and earlier sizes) spread evenly over the
//! log, as a chain's are quadratic over all of them. One JSON line a measurement: the design, the operation, rows read and written, hashes (where the
//! design's code counts them), and the best wall time of R repeats in nanoseconds an operation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use amphora::audit::tree::{self, Frontier, Hash, Node, TreeError};
use sha2::{Digest, Sha256};

struct Args {
    events: usize,
    bytes: usize,
    samples: usize,
    repeats: usize,
}

fn args() -> Args {
    let mut found: HashMap<String, usize> = HashMap::new();
    let mut it = std::env::args_os().skip(1);
    while let Some(flag) = it.next() {
        let flag = flag.into_string().expect("arguments are Unicode");
        let value = it
            .next()
            .and_then(|v| v.into_string().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{flag} takes a number"));
        found.insert(flag, value);
    }
    let need = |name: &str| {
        *found.get(name).unwrap_or_else(|| {
            panic!(
                "usage: audit_cost --events N --bytes B --samples S --repeats R (missing {name})"
            )
        })
    };
    Args {
        events: need("--events"),
        bytes: need("--bytes"),
        samples: need("--samples"),
        repeats: need("--repeats"),
    }
}

/// Events of `bytes` bytes each, every one different.
fn events(n: usize, bytes: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut e = (i as u64).to_be_bytes().repeat(bytes / 8 + 1);
            e.truncate(bytes);
            e
        })
        .collect()
}

/// `samples` indices spread evenly over 0..n.
fn spread(n: usize, samples: usize) -> Vec<usize> {
    let s = samples.clamp(1, n.max(1));
    (0..s).map(|k| k * n / s).collect()
}

/// A lookup of perfect subtrees' hashes that counts the rows it reads.
fn lookup<'a>(
    nodes: &'a HashMap<Node, Hash>,
    read: &'a mut usize,
) -> impl FnMut(Node) -> Result<Hash, TreeError> + 'a {
    move |node| {
        *read += 1;
        nodes
            .get(&node)
            .copied()
            .ok_or(TreeError::Shape { start: 0, end: 0 })
    }
}

/// The best of `repeats` runs of `f`, in nanoseconds a run divided by `ops`.
fn best(repeats: usize, ops: usize, mut f: impl FnMut()) -> f64 {
    (0..repeats)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_nanos() as f64 / ops.max(1) as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn line(
    design: &str,
    op: &str,
    events: usize,
    read: f64,
    written: f64,
    hashes: Option<f64>,
    ns: f64,
) {
    let hashes = hashes.map_or("null".to_owned(), |h| format!("{h:.2}"));
    println!(
        "{{\"design\":\"{design}\",\"op\":\"{op}\",\"events\":{events},\"rows_read\":{read:.2},\"rows_written\":{written:.2},\"hashes\":{hashes},\"ns_per_op\":{ns:.1}}}"
    );
}

/// The hash chain: link i = SHA-256(link i-1 || event i), the head the last link. Each append
/// writes its event and the head; showing event i is in the head means hashing on from link i-1
/// (kept beside the events, as a chain's checkpoints are) through every later event.
fn chain(a: &Args, events: &[Vec<u8>]) {
    let n = events.len();
    let link = |prev: &Hash, e: &[u8]| -> Hash {
        let mut h = Sha256::new();
        h.update(prev);
        h.update(e);
        h.finalize().into()
    };
    let mut links = vec![[0u8; 32]; n + 1];
    let ns = best(a.repeats, n, || {
        for (i, e) in events.iter().enumerate() {
            links[i + 1] = link(&links[i], e);
        }
        black_box(&links);
    });
    line("hash_chain", "append", n, 0.0, 2.0, Some(1.0), ns);

    // Event i against the head: from link i-1, hash events i..n and compare with the head.
    let head = links[n];
    let at = spread(n, a.samples);
    let mut read = 0usize;
    let ns = best(a.repeats, at.len(), || {
        read = 0;
        for &i in &at {
            let mut h = links[i];
            read += 1;
            for e in &events[i..] {
                h = link(&h, e);
                read += 1;
            }
            assert_eq!(h, head);
        }
    });
    let per = read as f64 / at.len() as f64;
    line(
        "hash_chain",
        "prove_one_event",
        n,
        per,
        0.0,
        Some(per - 1.0),
        ns,
    );

    // Head n extends head m: hash events m..n on from link m.
    let mut read = 0usize;
    let ns = best(a.repeats, at.len(), || {
        read = 0;
        for m in at.iter().map(|i| i + 1) {
            let mut h = links[m];
            for e in &events[m..] {
                h = link(&h, e);
                read += 1;
            }
            assert_eq!(h, head);
        }
    });
    let per = read as f64 / at.len() as f64;
    line("hash_chain", "prove_extends", n, per, 0.0, Some(per), ns);

    // Every event verified: one pass from the start.
    let ns = best(a.repeats, 1, || {
        let mut h = [0u8; 32];
        for e in events {
            h = link(&h, e);
        }
        assert_eq!(h, head);
    });
    line(
        "hash_chain",
        "audit_all",
        n,
        n as f64,
        0.0,
        Some(n as f64),
        ns,
    );
}

/// RFC 6962's tree as `AuditLog` keeps it: each append writes its event, its leaf hash, the
/// subtrees it completes and the head; showing an event or an extension reads a proof's subtrees.
fn merkle(a: &Args, events: &[Vec<u8>]) {
    let n = events.len();
    let mut nodes: HashMap<Node, Hash> = HashMap::with_capacity(2 * n);
    let mut written = 0usize;
    let ns = best(a.repeats, n, || {
        nodes.clear();
        written = 0;
        let mut frontier = Frontier::new(0, &[]).unwrap();
        let mut completed = Vec::new();
        for e in events {
            completed.clear();
            frontier.push(tree::leaf_hash(e), &mut completed).unwrap();
            // The event, the subtrees completed (its leaf hash first) and the head.
            written += 2 + completed.len();
            nodes.extend(completed.iter().copied());
            black_box(frontier.root());
        }
    });
    line(
        "merkle_rfc6962",
        "append",
        n,
        0.0,
        written as f64 / n as f64,
        None,
        ns,
    );

    let size = n as u64;
    let head = tree::root(size, lookup(&nodes, &mut 0)).unwrap();
    let at = spread(n, a.samples);

    // Event i against the head: the event, its leaf hash and its inclusion proof's subtrees.
    let mut read = 0usize;
    let mut proof_hashes = 0usize;
    let ns = best(a.repeats, at.len(), || {
        read = 0;
        proof_hashes = 0;
        for &i in &at {
            let leaf = tree::leaf_hash(&events[i]);
            read += 2;
            let proof = tree::inclusion_proof(i as u64, size, lookup(&nodes, &mut read)).unwrap();
            proof_hashes += proof.len();
            assert!(tree::verify_inclusion(i as u64, size, &leaf, &proof, &head));
        }
    });
    let k = at.len() as f64;
    line(
        "merkle_rfc6962",
        "prove_one_event",
        n,
        read as f64 / k,
        0.0,
        Some(1.0 + proof_hashes as f64 / k),
        ns,
    );

    // Head n extends head m: the consistency proof's subtrees.
    let olds: Vec<(u64, Hash)> = at
        .iter()
        .map(|&i| {
            let m = i as u64 + 1;
            (m, tree::root(m, lookup(&nodes, &mut 0)).unwrap())
        })
        .collect();
    let mut read = 0usize;
    let mut proof_hashes = 0usize;
    let ns = best(a.repeats, olds.len(), || {
        read = 0;
        proof_hashes = 0;
        for (m, old_root) in &olds {
            let proof = tree::consistency_proof(*m, size, lookup(&nodes, &mut read)).unwrap();
            proof_hashes += proof.len();
            assert!(tree::verify_consistency(*m, size, old_root, &head, &proof));
        }
    });
    line(
        "merkle_rfc6962",
        "prove_extends",
        n,
        read as f64 / k,
        0.0,
        Some(proof_hashes as f64 / k),
        ns,
    );

    // Every event verified as AuditLog reads them: each with its own inclusion proof.
    let mut read = 0usize;
    let ns = best(a.repeats, 1, || {
        read = 0;
        for (i, e) in events.iter().enumerate() {
            let leaf = tree::leaf_hash(e);
            read += 2;
            let proof = tree::inclusion_proof(i as u64, size, lookup(&nodes, &mut read)).unwrap();
            assert!(tree::verify_inclusion(i as u64, size, &leaf, &proof, &head));
        }
    });
    line("merkle_rfc6962", "audit_all", n, read as f64, 0.0, None, ns);
}

fn main() {
    let a = args();
    let events = events(a.events, a.bytes);
    chain(&a, &events);
    merkle(&a, &events);
}
