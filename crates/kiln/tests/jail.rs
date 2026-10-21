//! W13-R05's jail (docs/ci.md): what a VMM sees in its jail. `kiln vmm-jail` runs as the
//! `firecracker` executor runs it, with a shell in Firecracker's place: its root holds only what's
//! bound in, it's root there as this user, its network has only a loopback, its PID is 1, and
//! only its binds take writes. Skipped where this machine allows no unprivileged user namespaces.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The libraries `program` loads, as `ldd` lists them: each by the name it's asked for and where
/// that leads (its loader's name is the one its ELF header names).
fn libraries(program: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let o = Command::new("ldd").arg(program).output()?;
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .flat_map(|l| {
            let (name, to) = l.trim().split_once("=>").map_or((l.trim(), ""), |(n, t)| (n.trim(), t.trim()));
            [name, to].into_iter().filter_map(|p| p.split_whitespace().next()).filter(|p| p.starts_with('/')).map(PathBuf::from).collect::<Vec<_>>()
        })
        .collect())
}

/// Whether this machine allows the jail's namespaces.
fn namespaces() -> bool {
    Command::new("unshare").args(["-Urmpnf", "--mount-proc", "true"]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn w13_r05_a_vmm_sees_only_its_jail() -> anyhow::Result<()> {
    if !namespaces() {
        eprintln!("skipped: this machine allows no unprivileged user namespaces");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (root, disk) = (dir.path().join("root"), dir.path().join("disk"));
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&disk)?;
    let sh = std::fs::canonicalize("/bin/sh")?;
    let mut binds = vec![serde_json::json!({ "from": sh, "to": "/bin/sh", "rw": false }), serde_json::json!({ "from": disk, "to": "/drives", "rw": true })];
    for lib in libraries(&sh)? {
        binds.push(serde_json::json!({ "from": lib, "to": lib, "rw": false }));
    }
    // The shell's builtins only: nothing else is there.
    let script = [
        "echo \"root: $(echo /*)\"",
        "while read -r l; do echo \"map: $l\"; done < /proc/self/uid_map",
        "while read -r l; do case \"$l\" in *:*) echo \"net: ${l%%:*}\";; esac; done < /proc/net/dev",
        "read -r h < /proc/sys/kernel/hostname; echo \"host: $h\"",
        "echo \"pid: $$\"",
        "( : > /made ) 2>/dev/null && echo made-in-root || echo root-read-only",
        ": > /drives/made && echo made-in-drive",
    ]
    .join("\n");
    let spec = serde_json::json!({ "root": root, "binds": binds, "argv": ["/bin/sh", "-c", script] });
    let file = dir.path().join("jail.json");
    std::fs::write(&file, serde_json::to_vec(&spec)?)?;
    let o = Command::new(env!("CARGO_BIN_EXE_kiln")).arg("vmm-jail").arg(&file).output()?;
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    anyhow::ensure!(o.status.success(), "the jail: {out}\n{err}");
    let said = |name: &str| out.lines().filter_map(|l| l.strip_prefix(&format!("{name}: "))).map(str::trim).collect::<Vec<_>>();
    let seen = said("root").join(" ");
    assert!(seen.contains("/bin") && seen.contains("/drives") && seen.contains("/proc"), "its binds and its /proc: {seen}");
    for host in ["/home", "/etc", "/tmp", "/var", "/root"] {
        assert!(!seen.split_whitespace().any(|p| p == host), "none of the host's {host}: {seen}");
    }
    let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output()?.stdout).trim().to_owned();
    assert_eq!(said("map").first().map(|m| m.split_whitespace().collect::<Vec<_>>()), Some(vec!["0", uid.as_str(), "1"]), "root there, this user here: {out}");
    assert_eq!(said("net"), ["lo"], "a loopback, and no other network: {out}");
    assert_eq!(said("host"), ["kiln-vmm"], "{out}");
    assert_eq!(said("pid"), ["1"], "PID 1 of its own PID namespace: {out}");
    assert!(out.contains("root-read-only") && !out.contains("made-in-root"), "its root takes no writes: {out}");
    assert!(out.contains("made-in-drive") && disk.join("made").is_file(), "its writable bind does, as this user's: {out}");
    Ok(())
}
