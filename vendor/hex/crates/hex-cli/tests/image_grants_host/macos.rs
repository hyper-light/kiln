//! macOS's reading of a run, with no unsafe of its own: `ps` for the process tree (each process's pid, parent and
//! accounting name, then its arguments) and `lsof` for the TCP and UDP sockets the run's processes hold, in its field
//! output (lsof(8), "OUTPUT FOR OTHER PROGRAMS"). A listing either can't give fails the reading.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;

use super::{Process, Socket};

/// How this reader reads (told with every reading).
pub const HOW: &str = "ps -axww -o pid=,ppid=,ucomm= and -o pid=,args= for the process tree; \
                       lsof -nP -a -i -T s -F pftPnT -p <pids> for the TCP and UDP sockets they hold";

/// `program`'s standard output, which it must give whole: a run that fails fails the reading.
fn listed(program: &str, args: &[&str]) -> String {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Every process `ps` lists now, from its two listings: pid, parent and accounting name (`ucomm`, the program's
/// file name as the kernel keeps it), then pid and arguments. One that ends between them is left out: it is no
/// running run's.
pub fn processes() -> Vec<Process> {
    read_processes(
        &listed("ps", &["-axww", "-o", "pid=,ppid=,ucomm="]),
        &listed("ps", &["-axww", "-o", "pid=,args="]),
    )
}

/// The processes `names` (lines of pid, parent pid and name) and `args` (lines of pid and command line) list both.
/// A command line is its words: `ps` joins a program's arguments with spaces, so a word of an argument holding one
/// stands alone.
fn read_processes(names: &str, args: &str) -> Vec<Process> {
    let lines: Vec<(u32, Vec<String>)> = args
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            Some((pid, words.map(str::to_owned).collect()))
        })
        .collect();
    names
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            let name = fields.collect::<Vec<_>>().join(" ");
            let (_, args) = lines.iter().find(|(p, _)| *p == pid)?;
            Some(Process {
                pid,
                parent,
                name,
                args: args.clone(),
            })
        })
        .collect()
}

/// The TCP and UDP sockets the processes `pids` hold, each once. lsof exits 1 having listed nothing when the
/// processes hold no internet socket; with words of its own on its error output, that is a reading it couldn't
/// make, which fails.
pub fn sockets(pids: &[u32]) -> Vec<Socket> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let out = Command::new("lsof")
        .args(["-nP", "-a", "-i", "-T", "s", "-F", "pftPnT", "-p", &list])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(0) => {}
        Some(1) if stdout.is_empty() && stderr.trim().is_empty() => return Vec::new(),
        _ => panic!("lsof -p {list}: {:?}: {stderr}", out.status),
    }
    read_lsof(&stdout)
}

/// The sockets lsof's field output lists, each once, with the processes holding it: per process (`p`, its pid), per
/// file, its family (`t`), protocol (`P`), name (`n`: its local address, then `->` and its peer's where it has one) and
/// TCP state (`T` `ST=`). A file of another protocol is left out; one listed before any process fails the reading.
fn read_lsof(text: &str) -> Vec<Socket> {
    #[derive(Default)]
    struct File {
        pid: u32,
        ipv6: bool,
        protocol: String,
        name: String,
        listening: bool,
    }
    let mut pid = None;
    let mut files: Vec<File> = Vec::new();
    for line in text.lines() {
        let Some(id) = line.chars().next() else {
            continue;
        };
        let value = line.get(id.len_utf8()..).unwrap_or_default();
        match id {
            'p' => {
                pid = Some(
                    value
                        .parse()
                        .unwrap_or_else(|_| panic!("lsof's process {value:?}")),
                );
            }
            'f' => files.push(File {
                pid: pid.unwrap_or_else(|| panic!("lsof listed a file before its process")),
                ..File::default()
            }),
            't' => {
                if let Some(file) = files.last_mut() {
                    file.ipv6 = value == "IPv6";
                }
            }
            'P' => {
                if let Some(file) = files.last_mut() {
                    file.protocol = value.to_owned();
                }
            }
            'n' => {
                if let Some(file) = files.last_mut() {
                    file.name = value.to_owned();
                }
            }
            'T' => {
                if let Some(file) = files.last_mut() {
                    file.listening |= value == "ST=LISTEN";
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<Socket> = Vec::new();
    for file in files {
        let tcp = match file.protocol.as_str() {
            "TCP" => true,
            "UDP" => false,
            _ => continue,
        };
        let (local, remote) = match file.name.split_once("->") {
            Some((local, remote)) => (local, Some(remote)),
            None => (file.name.as_str(), None),
        };
        super::add(
            &mut out,
            Socket {
                tcp,
                local: address(local, file.ipv6),
                remote: remote.map(|r| address(r, file.ipv6)),
                listening: tcp && file.listening,
                pids: vec![file.pid],
            },
        );
    }
    out
}

/// An address as lsof names it with -n and -P: `host:port`, the host `*` for any address of the file's family, an
/// IPv6 one in brackets (its zone, `%name`, dropped).
fn address(text: &str, ipv6: bool) -> SocketAddr {
    let (host, port) = text
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("lsof's address {text:?} has no port"));
    let port = port
        .parse()
        .unwrap_or_else(|_| panic!("lsof's address {text:?}: its port"));
    let ip = match host {
        "*" if ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        bracketed if bracketed.starts_with('[') => {
            let inner = bracketed.trim_start_matches('[').trim_end_matches(']');
            let inner = inner.split_once('%').map_or(inner, |(ip, _)| ip);
            IpAddr::V6(
                inner
                    .parse()
                    .unwrap_or_else(|_| panic!("lsof's address {text:?}")),
            )
        }
        plain => plain
            .parse()
            .unwrap_or_else(|_| panic!("lsof's address {text:?}")),
    };
    SocketAddr::new(ip, port)
}

/// lsof's field output read (no VM): do read a listing of a TCP listener, a UDP socket, a TCP connection both ways,
/// a wildcard IPv6 listener, a file of no internet protocol, and the UDP socket again under a third process it was
/// handed on to; expect each socket once, as it is, held by each process that lists it.
#[test]
fn lsofs_field_output_reads_as_it_names_sockets() {
    let listing = "p39261\nf3\ntIPv4\nPTCP\nn127.0.0.1:63822\nTST=LISTEN\nTQR=0\nTQS=0\nf4\ntIPv4\nPUDP\n\
                   n127.0.0.1:58890\nf5\ntIPv4\nPTCP\nn127.0.0.1:63823->127.0.0.1:63822\nTST=ESTABLISHED\n\
                   p39262\nf7\ntIPv6\nPTCP\nn*:7101\nTST=LISTEN\nf8\ntIPv6\nPTCP\n\
                   n[fe80::1%en0]:50000->[2001:db8::1]:443\nTST=ESTABLISHED\nf9\ntIPv4\nPICMP\nn*:*\n\
                   p39263\nf3\ntIPv4\nPUDP\nn127.0.0.1:58890\n";
    let read = read_lsof(listing);
    let socket = |tcp, local: &str, remote: Option<&str>, listening, pids: &[u32]| Socket {
        tcp,
        local: local.parse().unwrap(),
        remote: remote.map(|r| r.parse().unwrap()),
        listening,
        pids: pids.to_vec(),
    };
    assert_eq!(
        read,
        [
            socket(true, "127.0.0.1:63822", None, true, &[39261]),
            socket(false, "127.0.0.1:58890", None, false, &[39261, 39263]),
            socket(
                true,
                "127.0.0.1:63823",
                Some("127.0.0.1:63822"),
                false,
                &[39261]
            ),
            socket(true, "[::]:7101", None, true, &[39262]),
            socket(
                true,
                "[fe80::1]:50000",
                Some("[2001:db8::1]:443"),
                false,
                &[39262]
            ),
        ]
    );
}

/// `ps`'s listings read (no VM): do read a process both list, one one leaves out, and a name with a space; expect
/// the first and the third, each with its parent, name and words.
#[test]
fn pss_listings_read_as_processes() {
    let names = "    1     0 launchd         \n  700   650 shards          \n  701   700 my tool  \n  702     1 gone\n";
    let args =
        "    1 /sbin/launchd\n  700 /opt/shards/shards api-run --spec x\n  701 /bin/my tool\n";
    let read = read_processes(names, args);
    let process = |pid, parent, name: &str, args: &[&str]| Process {
        pid,
        parent,
        name: name.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    };
    assert_eq!(
        read,
        [
            process(1, 0, "launchd", &["/sbin/launchd"]),
            process(
                700,
                650,
                "shards",
                &["/opt/shards/shards", "api-run", "--spec", "x"]
            ),
            process(701, 700, "my tool", &["/bin/my", "tool"]),
        ]
    );
}
