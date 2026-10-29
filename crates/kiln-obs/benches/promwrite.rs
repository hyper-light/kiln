//! Prometheus's remote write decoded (`docs/observe.md` §8.5, §12.21): requests as Prometheus
//! sends them (2,000 samples, its `max_samples_per_send`, of node exporters' series), compressed
//! with Snappy; Snappy unzipped, then the request decoded into series. Time, allocations and page
//! faults a sample, the receiver's keys made by the requests before.
//!
//! `cargo bench -p kiln-obs --bench promwrite`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::{alloc, faults};
use kiln_obs::otlp::metrics::{Decoded, Keys};
use kiln_obs::otlp::pb::Writer;
use kiln_obs::promwrite::{Families, MAX_BODY, Version, decode_into, unsnappy};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Snappy's block format, compressed as Snappy does: four bytes hashed into a table of where they
/// were last, a match extended as far as it goes, literals between (what the decoder reads).
fn snappy(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    put_varint(&mut out, data.len() as u64);
    let mut table = vec![0usize; 1 << 14];
    let hash = |b: &[u8]| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]).wrapping_mul(0x1e35_a7bd) >> 18) as usize;
    let literal = |out: &mut Vec<u8>, lit: &[u8]| {
        for chunk in lit.chunks(1 << 16) {
            let n = chunk.len() - 1;
            if n < 60 {
                out.push((n as u8) << 2);
            } else if n < 256 {
                out.push(60 << 2);
                out.push(n as u8);
            } else {
                out.push(61 << 2);
                out.extend_from_slice(&(n as u16).to_le_bytes());
            }
            out.extend_from_slice(chunk);
        }
    };
    let (mut i, mut lit_start) = (0usize, 0usize);
    while i + 4 <= data.len() {
        let h = hash(&data[i..]);
        let cand = table[h];
        table[h] = i;
        if cand < i && i - cand <= 65_535 && data[cand..cand + 4] == data[i..i + 4] {
            let mut len = 4;
            while i + len < data.len() && data[cand + len] == data[i + len] && len < 64 {
                len += 1;
            }
            literal(&mut out, &data[lit_start..i]);
            out.push((((len - 1) as u8) << 2) | 2);
            out.extend_from_slice(&((i - cand) as u16).to_le_bytes());
            i += len;
            lit_start = i;
        } else {
            i += 1;
        }
    }
    literal(&mut out, &data[lit_start..]);
    out
}

/// A 1.0 request of `samples` samples: node exporters' series (`node_cpu_seconds_total` by cpu and
/// mode, `node_network_receive_bytes_total` by device, filesystem gauges), a host's labels each.
fn request(samples: usize, hosts: usize, at: i64) -> Vec<u8> {
    let mut w = Writer::default();
    let mut n = 0;
    'out: for h in 0..hosts {
        let instance = format!("10.0.{}.{}:9100", h / 250, h % 250);
        let mut series: Vec<(String, Vec<(&str, String)>)> = Vec::new();
        for cpu in 0..8 {
            for mode in ["user", "system", "idle", "iowait", "irq", "softirq", "steal", "nice"] {
                series.push(("node_cpu_seconds_total".into(), vec![("cpu", cpu.to_string()), ("mode", mode.into())]));
            }
        }
        for device in ["eth0", "eth1", "lo"] {
            series.push(("node_network_receive_bytes_total".into(), vec![("device", device.into())]));
            series.push(("node_network_transmit_bytes_total".into(), vec![("device", device.into())]));
        }
        for mount in ["/", "/boot", "/var", "/home"] {
            series.push(("node_filesystem_avail_bytes".into(), vec![("device", "/dev/nvme0n1p2".into()), ("fstype", "ext4".into()), ("mountpoint", mount.into())]));
        }
        for (name, labels) in series {
            w.message(1, |ts| {
                let mut all: Vec<(&str, String)> = vec![("__name__", name.clone()), ("instance", instance.clone()), ("job", "node".into())];
                all.extend(labels.iter().cloned());
                all.sort();
                for (k, v) in &all {
                    ts.message(1, |l| {
                        l.str(1, k);
                        l.str(2, v);
                    });
                }
                ts.message(2, |s| {
                    s.double(1, 12_345.678 + n as f64);
                    s.varint(2, at as u64);
                });
            });
            n += 1;
            if n == samples {
                break 'out;
            }
        }
    }
    w.buf
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    println!("{:<34} {:>8} {:>10} {:>10} {:>12} {:>12} {:>10} {:>8}", "request", "samples", "bytes", "snappy", "unzip ns/s", "decode ns/s", "allocs/s", "faults");
    for (name, samples, hosts) in [("2,000 samples, 50 node exporters", 2000, 50), ("10,000 samples, 500 node exporters", 10_000, 500)] {
        let plain = request(samples, hosts, 1_800_000_000_000);
        let zipped = snappy(&plain);
        let (keys, families) = (Keys::new(), Families::new());
        let (mut unzipped, mut d) = (Vec::new(), Decoded::default());
        unsnappy(&zipped, &mut unzipped, MAX_BODY).unwrap();
        assert_eq!(unzipped, plain, "the reference's Snappy reads back");
        // The receiver's keys made by the first; the rest found.
        let taken = decode_into(&unzipped, Version::V1, &keys, &families, &mut d).unwrap();
        assert_eq!(taken.samples as usize, samples);
        let unzip_ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    unsnappy(&zipped, &mut unzipped, MAX_BODY).unwrap();
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let decode_ns = median(
            (0..51)
                .map(|_| {
                    let t = Instant::now();
                    decode_into(&unzipped, Version::V1, &keys, &families, &mut d).unwrap();
                    std::hint::black_box(&d);
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        unsnappy(&zipped, &mut unzipped, MAX_BODY).unwrap();
        decode_into(&unzipped, Version::V1, &keys, &families, &mut d).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        let n = samples as f64;
        println!(
            "{:<34} {:>8} {:>10} {:>10} {:>12.1} {:>12.1} {:>10.4} {:>8}",
            name,
            samples,
            plain.len(),
            zipped.len(),
            unzip_ns as f64 / n,
            decode_ns as f64 / n,
            c.calls() as f64 / n,
            f1.since(&f0).total()
        );
    }
}
