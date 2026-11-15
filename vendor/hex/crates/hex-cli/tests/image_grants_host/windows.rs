//! Windows' reading of a run, with no unsafe of its own: one `powershell` for each reading, its script passed encoded
//! (`-EncodedCommand`, so no quoting stands between it and its words) and its answer JSON. The process tree is CIM's
//! `Win32_Process` (each process's id, its parent's, its image name, command line and creation time); the sockets
//! are the NetTCPIP module's tables, `Get-NetTCPConnection` and `Get-NetUDPEndpoint`, each row by its owning
//! process. A reading powershell can't make fails.

use std::net::{IpAddr, SocketAddr};
use std::process::Command;

use super::{Process, Socket};

/// How this reader reads (told with every reading).
pub const HOW: &str = "powershell: Get-CimInstance Win32_Process for the process tree (a parent created after its \
                       child is another process, its pid reused); Get-NetTCPConnection and Get-NetUDPEndpoint \
                       (NetTCPIP) for the TCP and UDP sockets, each by its owning process. Windows' UDP table names \
                       no peer, so every UDP socket reads as taking what comes in";

/// `MSFT_NetTCPConnection.State`'s name for a listening socket, as the NetTCPIP module's State enumeration names it.
const LISTEN: &str = "Listen";

/// Base64 (RFC 4648 §4, padded) of `bytes`: `-EncodedCommand`'s form.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(
                    ALPHABET[usize::try_from((n >> shift) & 63).unwrap()],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The JSON `script` writes, run by one `powershell`, its output UTF-8 with no byte-order mark (one written anyway is
/// read past).
fn powershell(script: &str) -> serde_json::Value {
    let script =
        format!("[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false\n{script}");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &base64(&utf16),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "powershell: {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let json = out
        .stdout
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(&out.stdout);
    serde_json::from_slice(json).unwrap_or_else(|e| {
        panic!(
            "powershell's JSON: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// A command line's words, as Windows' C runtime splits them (CommandLineToArgvW's rules): whitespace between words,
/// a quote toggling a span where it is none, `2n` backslashes before a quote `n` of them and the quote a toggle,
/// `2n + 1` backslashes before one `n` of them and a quote, other backslashes as they are.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut backslashes = 0usize;
    for c in line.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                in_word = true;
            }
            '"' => {
                word.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    word.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                word.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                word.push(c);
                in_word = true;
            }
        }
    }
    word.extend(std::iter::repeat_n('\\', backslashes));
    if in_word {
        out.push(word);
    }
    out
}

/// Every process CIM lists now, each named by its image name less its `.exe`. A parent created after its child is
/// no parent of it (the pid was reused), so the child is taken for an orphan.
pub fn processes() -> Vec<Process> {
    let listed = powershell(
        "$all = @(Get-CimInstance Win32_Process -ErrorAction Stop | ForEach-Object {\n\
         [pscustomobject]@{ pid = [uint32]$_.ProcessId; parent = [uint32]$_.ParentProcessId; name = [string]$_.Name;\n\
         line = [string]$_.CommandLine; created = $(if ($_.CreationDate) { $_.CreationDate.ToFileTimeUtc() } else { 0 }) } })\n\
         ConvertTo-Json -InputObject $all -Compress -Depth 3",
    );
    read_processes(&listed)
}

/// The processes `listed` (CIM's rows, as [`processes`] writes them) names.
fn read_processes(listed: &serde_json::Value) -> Vec<Process> {
    let rows = listed.as_array().unwrap();
    let number = |row: &serde_json::Value, key: &str| row[key].as_u64().unwrap();
    let created: Vec<(u32, u64)> = rows
        .iter()
        .map(|row| {
            (
                u32::try_from(number(row, "pid")).unwrap(),
                number(row, "created"),
            )
        })
        .collect();
    let born = |pid: u32| created.iter().find(|(p, _)| *p == pid).map(|(_, c)| *c);
    rows.iter()
        .map(|row| {
            let pid = u32::try_from(number(row, "pid")).unwrap();
            let named = u32::try_from(number(row, "parent")).unwrap();
            let parent = match (born(named), born(pid)) {
                (Some(parent), Some(child)) if parent <= child => named,
                _ => 0,
            };
            let name = row["name"].as_str().unwrap();
            let name = name
                .len()
                .checked_sub(4)
                .filter(|&at| {
                    name.get(at..)
                        .is_some_and(|ext| ext.eq_ignore_ascii_case(".exe"))
                })
                .and_then(|at| name.get(..at))
                .unwrap_or(name);
            Process {
                pid,
                parent,
                name: name.to_owned(),
                args: words(row["line"].as_str().unwrap_or_default()),
            }
        })
        .collect()
}

/// The TCP and UDP sockets the processes `pids` own, each once. The tables are read whole and their rows chosen by
/// owner here, so a table that can't be read fails the reading rather than reading as empty.
pub fn sockets(pids: &[u32]) -> Vec<Socket> {
    let tables = powershell(
        "$tcp = @(Get-NetTCPConnection -ErrorAction Stop | ForEach-Object {\n\
         [pscustomobject]@{ owner = [uint32]$_.OwningProcess; local = [string]$_.LocalAddress; lport = [int]$_.LocalPort;\n\
         remote = [string]$_.RemoteAddress; rport = [int]$_.RemotePort; state = [string]$_.State } })\n\
         $udp = @(Get-NetUDPEndpoint -ErrorAction Stop | ForEach-Object {\n\
         [pscustomobject]@{ owner = [uint32]$_.OwningProcess; local = [string]$_.LocalAddress; lport = [int]$_.LocalPort } })\n\
         ConvertTo-Json -InputObject ([pscustomobject]@{ tcp = $tcp; udp = $udp }) -Compress -Depth 4",
    );
    read_sockets(&tables, pids)
}

/// The sockets `tables` (the TCP and UDP rows, as [`sockets`] writes them) gives the owners `pids`, each held by its
/// row's owner.
fn read_sockets(tables: &serde_json::Value, pids: &[u32]) -> Vec<Socket> {
    let address = |ip: &serde_json::Value, port: &serde_json::Value| {
        let ip: IpAddr = ip.as_str().unwrap().parse().unwrap();
        SocketAddr::new(ip, u16::try_from(port.as_u64().unwrap()).unwrap())
    };
    let owner = |row: &serde_json::Value| u32::try_from(row["owner"].as_u64().unwrap()).unwrap();
    let owned = |row: &&serde_json::Value| pids.contains(&owner(row));
    let mut out: Vec<Socket> = Vec::new();
    for row in tables["tcp"].as_array().unwrap().iter().filter(owned) {
        let remote = address(&row["remote"], &row["rport"]);
        super::add(
            &mut out,
            Socket {
                tcp: true,
                local: address(&row["local"], &row["lport"]),
                remote: if remote.ip().is_unspecified() && remote.port() == 0 {
                    None
                } else {
                    Some(remote)
                },
                listening: row["state"].as_str() == Some(LISTEN),
                pids: vec![owner(row)],
            },
        );
    }
    for row in tables["udp"].as_array().unwrap().iter().filter(owned) {
        super::add(
            &mut out,
            Socket {
                tcp: false,
                local: address(&row["local"], &row["lport"]),
                remote: None,
                listening: false,
                pids: vec![owner(row)],
            },
        );
    }
    out
}

/// The encoding powershell is given (no VM): do encode the empty input and inputs of one, two and three bytes; expect
/// RFC 4648's test vectors.
#[test]
fn base64_is_rfc_4648s() {
    for (input, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), encoded, "{input:?}");
    }
}

/// A command line's words (no VM): do split a quoted program path with a space, plain words, a quoted word holding
/// a quote by backslash, and trailing backslashes; expect each word as the C runtime gives it.
#[test]
fn a_command_line_splits_as_the_c_runtime_splits_it() {
    assert_eq!(
        words(r#""C:\Program Files\shards\shards.exe" api-run --spec x"#),
        [
            r"C:\Program Files\shards\shards.exe",
            "api-run",
            "--spec",
            "x"
        ]
    );
    assert_eq!(
        words(r#"a "b \"c\" d" e\\ f\\\"g"#),
        ["a", r#"b "c" d"#, r"e\\", r#"f\"g"#]
    );
    assert_eq!(words("   "), Vec::<String>::new());
}

/// The tables read (no VM): do read CIM's rows of a parent and child, a child whose parent pid was reused by a later
/// process, and a socket table of a listener, a connection, a socket of another owner and a UDP endpoint; expect the
/// tree and the sockets as they are, each held by its row's owner, the reused parent no parent.
#[test]
fn cims_and_nettcpips_rows_read_as_processes_and_sockets() {
    let listed = serde_json::json!([
        {"pid": 700, "parent": 4, "name": "shards.exe", "line": "\"C:\\s\\shards.exe\" api-run", "created": 100},
        {"pid": 701, "parent": 700, "name": "shards-net.EXE", "line": null, "created": 200},
        {"pid": 702, "parent": 703, "name": "orphan.exe", "line": "orphan", "created": 300},
        {"pid": 703, "parent": 1, "name": "later.exe", "line": "later", "created": 400},
    ]);
    let read = read_processes(&listed);
    let parents: Vec<(u32, u32, &str)> = read
        .iter()
        .map(|p| (p.pid, p.parent, p.name.as_str()))
        .collect();
    assert_eq!(
        parents,
        [
            (700, 0, "shards"),
            (701, 700, "shards-net"),
            (702, 0, "orphan"),
            (703, 0, "later")
        ]
    );
    assert_eq!(read[0].args, [r"C:\s\shards.exe", "api-run"]);
    let tables = serde_json::json!({
        "tcp": [
            {"owner": 701, "local": "127.0.0.1", "lport": 7101, "remote": "0.0.0.0", "rport": 0, "state": "Listen"},
            {"owner": 701, "local": "10.0.0.2", "lport": 50000, "remote": "192.0.2.1", "rport": 443, "state": "Established"},
            {"owner": 9, "local": "0.0.0.0", "lport": 445, "remote": "0.0.0.0", "rport": 0, "state": "Listen"},
        ],
        "udp": [{"owner": 700, "local": "::", "lport": 5353}],
    });
    let socket = |tcp, local: &str, remote: Option<&str>, listening, owner| Socket {
        tcp,
        local: local.parse().unwrap(),
        remote: remote.map(|r| r.parse().unwrap()),
        listening,
        pids: vec![owner],
    };
    assert_eq!(
        read_sockets(&tables, &[700, 701]),
        [
            socket(true, "127.0.0.1:7101", None, true, 701),
            socket(true, "10.0.0.2:50000", Some("192.0.2.1:443"), false, 701),
            socket(false, "[::]:5353", None, false, 700),
        ]
    );
}
