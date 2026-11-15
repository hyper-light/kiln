//! Linux's reading of a run, from /proc: each process's `stat`, `comm` and `cmdline`; the sockets its descriptors
//! hold (`fd`'s `socket:[inode]` links), each matched by its inode in its network namespace's tables
//! (`net/tcp`, `tcp6`, `udp`, `udp6`). A descriptor list this test can't read fails it.

use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};

use super::{Process, Socket};

/// How this reader reads (told with every reading).
pub const HOW: &str = "/proc: each process's stat, comm and cmdline, and its descriptors' socket inodes found in its \
                       network namespace's tcp, tcp6, udp and udp6 tables";

/// TCP_LISTEN, a listening TCP socket's state in /proc/net's tables (include/net/tcp_states.h).
const TCP_LISTEN: u8 = 10;

/// Every process /proc lists now. One that ends while this reads it is left out: it is no running run's.
pub fn processes() -> Vec<Process> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc").unwrap() {
        let entry = entry.unwrap();
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let (Ok(stat), Ok(name), Ok(args)) = (
            fs::read_to_string(format!("/proc/{pid}/stat")),
            fs::read_to_string(format!("/proc/{pid}/comm")),
            fs::read(format!("/proc/{pid}/cmdline")),
        ) else {
            continue;
        };
        // The name sits in parentheses and may hold any byte, so the state, then the parent, follow the last ')'.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("/proc/{pid}/stat: {stat}"));
        out.push(Process {
            pid,
            parent,
            name: name.trim_end().to_owned(),
            args: args
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        });
    }
    out
}

/// A TCP or UDP socket as its network namespace's table shows it.
#[derive(Debug)]
struct Row {
    tcp: bool,
    local: SocketAddr,
    remote: SocketAddr,
    /// Its state number in the table (include/net/tcp_states.h).
    state: u8,
    inode: u64,
}

/// An address as /proc/net's tables write it: the address's bytes as the host's 32-bit words in hex, then the
/// port in hex (net/ipv4/tcp_ipv4.c `get_tcp4_sock`, net/ipv6/tcp_ipv6.c `get_tcp6_sock`).
fn address(text: &str) -> SocketAddr {
    let (ip, port) = text.split_once(':').unwrap();
    let bytes: Vec<u8> = ip
        .as_bytes()
        .chunks(8)
        .flat_map(|word| {
            u32::from_str_radix(std::str::from_utf8(word).unwrap(), 16)
                .unwrap()
                .to_ne_bytes()
        })
        .collect();
    let ip = match bytes.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(bytes).unwrap()),
        16 => IpAddr::from(<[u8; 16]>::try_from(bytes).unwrap()),
        n => panic!("an address of {n} bytes: {text}"),
    };
    SocketAddr::new(ip, u16::from_str_radix(port, 16).unwrap())
}

/// The TCP and UDP sockets of the network namespace the process `pid` is in, from its tables. A kernel with no
/// IPv6 has no IPv6 tables, and so no IPv6 socket.
fn rows_seen_by(pid: u32) -> Vec<Row> {
    let mut out = Vec::new();
    for (table, tcp) in [
        ("tcp", true),
        ("tcp6", true),
        ("udp", false),
        ("udp6", false),
    ] {
        let path = format!("/proc/{pid}/net/{table}");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound && table.ends_with('6') => continue,
            Err(e) => panic!("{path}: {e}"),
        };
        for row in text.lines().skip(1) {
            let fields: Vec<&str> = row.split_whitespace().collect();
            // sl, local, remote, state, tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout, inode, ...
            let [_, local, remote, state, _, _, _, _, _, inode, ..] = fields.as_slice() else {
                panic!("{path}: a row of {} fields: {row}", fields.len());
            };
            out.push(Row {
                tcp,
                local: address(local),
                remote: address(remote),
                state: u8::from_str_radix(state, 16).unwrap(),
                inode: inode.parse().unwrap(),
            });
        }
    }
    out
}

/// The inodes of the sockets the process `pid`'s descriptors hold.
fn socket_inodes(pid: u32) -> Vec<u64> {
    let dir = format!("/proc/{pid}/fd");
    let mut out = Vec::new();
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{dir}: {e}: a run's descriptors this test can't read"));
    for entry in entries {
        // A descriptor closed while this reads the list is no longer held.
        let Ok(target) = fs::read_link(entry.unwrap().path()) else {
            continue;
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|t| t.strip_prefix("socket:["))
            .and_then(|t| t.strip_suffix(']'))
        {
            out.push(inode.parse().unwrap());
        }
    }
    out
}

/// The TCP and UDP sockets the processes `pids` hold, each once, by its inode, with every one of them holding it.
pub fn sockets(pids: &[u32]) -> Vec<Socket> {
    let mut found: Vec<(u64, Socket)> = Vec::new();
    for &pid in pids {
        let held = socket_inodes(pid);
        if held.is_empty() {
            continue;
        }
        for row in rows_seen_by(pid) {
            if !held.contains(&row.inode) {
                continue;
            }
            if let Some((_, known)) = found.iter_mut().find(|(inode, _)| *inode == row.inode) {
                if !known.pids.contains(&pid) {
                    known.pids.push(pid);
                }
                continue;
            }
            let peer = if row.remote.ip().is_unspecified() && row.remote.port() == 0 {
                None
            } else {
                Some(row.remote)
            };
            found.push((
                row.inode,
                Socket {
                    tcp: row.tcp,
                    local: row.local,
                    remote: peer,
                    listening: row.tcp && row.state == TCP_LISTEN,
                    pids: vec![pid],
                },
            ));
        }
    }
    found.into_iter().map(|(_, socket)| socket).collect()
}

/// The /proc reading's address form (no VM): do read addresses as a little-endian kernel writes them, an IPv4
/// loopback port, IPv6's loopback, and an IPv4-mapped one; expect each as it is.
#[cfg(target_endian = "little")]
#[test]
fn proc_net_addresses_read_as_the_kernel_writes_them() {
    assert_eq!(address("0100007F:1F90"), "127.0.0.1:8080".parse().unwrap());
    assert_eq!(
        address("00000000000000000000000001000000:01BB"),
        "[::1]:443".parse().unwrap()
    );
    assert_eq!(
        address("0000000000000000FFFF00000100007F:0035"),
        "[::ffff:127.0.0.1]:53".parse().unwrap()
    );
}
