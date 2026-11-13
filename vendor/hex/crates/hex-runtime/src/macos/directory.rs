//! The box helper's directory work for a pool grown on demand (the lead's call, 2026-11-06): it lists the
//! pool's users, reads the ids held, makes an account from a fixed template, and keeps the peak of
//! concurrent boxes. Every tool runs by absolute path with a fixed argv and no shell; nothing a caller sends
//! reaches an argument but the account `hex_core::pool` named. No `unsafe`: these are subprocesses, started
//! through [`super::command`], so each holds only its stdio. What a tool prints is read a line at a time,
//! each line within the most its call can print (a record's name, the attribute asked for, a u32's digits),
//! and only what the pool needs is kept: nothing a tool prints accumulates (CLAUDE.md §2). A tool's stderr
//! is the helper's own (launchd's log).
//!
//! Each account is written to the installer's manifest before it is made, so uninstall removes it even if
//! the helper stops part-way, and logged on the helper's stderr (launchd's log file).

use super::command::{Command, Stdio};
use super::sys;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;

use hex_api::Refusal;
use hex_core::pool::{Account, ROLE_UIDS, pool_index};

const DSCL: &str = "/usr/bin/dscl";
const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";
const PS: &str = "/bin/ps";
/// What a line past its bound is refused as (`Refusal::Exhausted`), each naming its tool.
const DSCL_LINE: &str = "bytes in a line /usr/bin/dscl prints";
const PS_LINE: &str = "bytes in a line /bin/ps prints";

/// Where the helper grows its pool: the pool's group, the installer's manifest, and the file it keeps the
/// peak in.
#[derive(Debug, Clone)]
pub struct Directory {
    /// The pool group's name and gid (`hexbox`, the installer's).
    pub group: String,
    pub gid: u32,
    /// The installer's manifest (`/var/db/hex-install/manifest`): each account made is a `user <name>` line.
    pub manifest: PathBuf,
    /// The peak of concurrent box-user boxes, one decimal line, kept across restarts.
    pub peak_file: PathBuf,
}

/// The decimal digits of the widest u32: the most a uid or gid prints in.
fn id_digits() -> usize {
    u32::MAX.to_string().len()
}

/// Runs `tool` with `args` (no shell) to its end, handing `each` every line it prints as it comes, each at
/// most `line_max` bytes (past it, `Exhausted` with `what`); an exit but 0 is an error naming the tool and
/// its arguments.
fn lines(
    tool: &str,
    what: &'static str,
    args: &[&str],
    line_max: usize,
    mut each: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let status = Command::new(tool)
        .args(args)
        .lines(what, line_max, |line| {
            each(
                std::str::from_utf8(line)
                    .map_err(|e| io::Error::other(format!("{tool} printed non-UTF-8: {e}")))?,
            )
        })?;
    exited(tool, args, status)
}

/// Runs `tool` with `args` (no shell) for what it does: none of its stdout is read (it goes to the null
/// device); an exit but 0 is an error naming the tool and its arguments.
fn run(tool: &str, args: &[&str]) -> io::Result<()> {
    let status = Command::new(tool)
        .args(args)
        .stdout(Stdio::null())
        .status()?;
    exited(tool, args, status)
}

fn exited(tool: &str, args: &[&str], status: std::process::ExitStatus) -> io::Result<()> {
    if !status.success() {
        return Err(io::Error::other(format!(
            "{tool} {}: {status} (its stderr is in the helper's log)",
            args.join(" ")
        )));
    }
    Ok(())
}

/// The longest line `dscl . -list` prints for a numbered record: its name (at most the longest login name,
/// `sysconf(_SC_LOGIN_NAME_MAX)`, 255), padded to dscl's column (23 wide, far narrower), a space, and a
/// u32's digits.
fn list_line() -> io::Result<usize> {
    sys::login_name_max()?
        .checked_add(1)
        .and_then(|length| length.checked_add(id_digits()))
        .ok_or_else(|| io::Error::other("a dscl listing's line bound isn't representable"))
}

/// The longest line `dscl . -search /Users UniqueID <uid>` prints: a holder's name (at most the longest
/// login name), two tabs and the attribute asked for (`UniqueID = (`); the value's lines are shorter.
fn search_line() -> io::Result<usize> {
    sys::login_name_max()?
        .checked_add("\t\tUniqueID = (".len())
        .ok_or_else(|| io::Error::other("a dscl search's line bound isn't representable"))
}

/// `dscl . -list <path> <attribute>`'s numbered records, each handed to `keep` as its line comes as (name,
/// number); a line that isn't one is skipped as not a numbered record (dscl lists one record a line).
fn listed(
    path: &str,
    attribute: &str,
    mut keep: impl FnMut(&str, u32) -> io::Result<()>,
) -> io::Result<()> {
    lines(
        DSCL,
        DSCL_LINE,
        &[".", "-list", path, attribute],
        list_line()?,
        |line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next().and_then(|n| n.parse().ok())) {
                (Some(name), Some(number)) => keep(name, number),
                _ => Ok(()),
            }
        },
    )
}

/// Past `bound` of what the pool keeps: the directory holds more of it than the role range can.
fn past(what: &'static str, bound: usize) -> io::Error {
    io::Error::other(Refusal::Exhausted { what, bound })
}

impl Directory {
    /// The pool's users: those `hex_core::pool` names, at a role uid, whose primary group is the pool's. At
    /// most one a role uid: the directory read past that holds more than the pool can, and is refused.
    pub fn pool_users(&self) -> io::Result<Vec<(String, u32)>> {
        let bound = ROLE_UIDS.clone().count();
        let mut members = BTreeSet::new();
        listed("/Users", "PrimaryGroupID", |name, gid| {
            if gid == self.gid && pool_index(name).is_some() {
                if members.len() >= bound {
                    return Err(past("pool users in the pool's group", bound));
                }
                members.insert(name.to_owned());
            }
            Ok(())
        })?;
        let mut users = Vec::new();
        listed("/Users", "UniqueID", |name, uid| {
            if ROLE_UIDS.contains(&uid) && members.contains(name) {
                if users.len() >= bound {
                    return Err(past("pool users at a role uid", bound));
                }
                users.push((name.to_owned(), uid));
            }
            Ok(())
        })?;
        Ok(users)
    }

    /// Every role id held now (`ROLE_UIDS`, the only ids `hex_core::pool::next_account` weighs): users'
    /// uids, groups' gids and running processes' uids.
    pub fn taken_ids(&self) -> io::Result<BTreeSet<u32>> {
        let mut taken = BTreeSet::new();
        let mut hold = |id: u32| {
            if ROLE_UIDS.contains(&id) {
                taken.insert(id);
            }
            Ok(())
        };
        listed("/Users", "UniqueID", |_, uid| hold(uid))?;
        listed("/Groups", "PrimaryGroupID", |_, gid| hold(gid))?;
        // `ps -o uid=` prints each uid right-aligned in a column narrower than a u32's digits.
        lines(
            PS,
            PS_LINE,
            &["-axo", "uid="],
            id_digits(),
            |line| match line.trim().parse::<u32>() {
                Ok(uid) => hold(uid),
                Err(_) => Ok(()),
            },
        )?;
        Ok(taken)
    }

    /// Makes `account` from the template: hidden, shell `/usr/bin/false`, home `/var/empty`, no password
    /// (`*`), primary group and member of the pool's group. Checks its uid is still no one's first (another
    /// installer may have taken it since install), and writes it to the manifest before making it.
    pub fn make_user(&self, account: &Account) -> io::Result<()> {
        if pool_index(&account.name).is_none() || !ROLE_UIDS.contains(&account.uid) {
            return Err(io::Error::other(format!(
                "{} at {} isn't a pool account",
                account.name, account.uid
            )));
        }
        let uid = account.uid.to_string();
        // Any record dscl finds holds the uid; the first one's name is kept to say which.
        let mut holder = None;
        lines(
            DSCL,
            DSCL_LINE,
            &[".", "-search", "/Users", "UniqueID", &uid],
            search_line()?,
            |line| {
                if holder.is_none() {
                    holder = line.split_whitespace().next().map(str::to_owned);
                }
                Ok(())
            },
        )?;
        if let Some(holder) = holder {
            return Err(io::Error::other(format!("uid {uid} is taken: {holder}")));
        }
        let mut manifest = OpenOptions::new().append(true).open(&self.manifest)?;
        writeln!(manifest, "user {}", account.name)?;
        manifest.sync_all()?;
        let record = format!("/Users/{}", account.name);
        let gid = self.gid.to_string();
        let real_name = format!("Hex box user {}", account.name);
        run(DSCL, &[".", "-create", &record])?;
        for (attribute, value) in [
            ("UniqueID", uid.as_str()),
            ("PrimaryGroupID", gid.as_str()),
            ("UserShell", "/usr/bin/false"),
            ("NFSHomeDirectory", "/var/empty"),
            ("IsHidden", "1"),
            ("Password", "*"),
            ("RealName", real_name.as_str()),
        ] {
            run(DSCL, &[".", "-create", &record, attribute, value])?;
        }
        run(
            DSEDITGROUP,
            &["-o", "edit", "-t", "user", "-a", &account.name, &self.group],
        )?;
        let _ = writeln!(
            io::stderr().lock(),
            "hex __box-helper: made box user {} (uid {uid}) for the pool",
            account.name
        );
        Ok(())
    }

    /// The kept peak, 0 when none is kept yet.
    pub fn peak(&self) -> io::Result<u32> {
        match std::fs::read_to_string(&self.peak_file) {
            Ok(text) => text
                .trim()
                .parse()
                .map_err(|e| io::Error::other(format!("{}: {e}", self.peak_file.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Keeps `peak`, replacing the file whole (written beside it, then renamed).
    pub fn keep_peak(&self, peak: u32) -> io::Result<()> {
        let next = self.peak_file.with_extension("next");
        std::fs::write(&next, format!("{peak}\n"))?;
        std::fs::rename(&next, &self.peak_file)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The argument that turns `descriptor_probe` on: a filter no test is named, so the probe's run
    /// runs it alone.
    const PROBE: &str = "hex-descriptor-probe";

    /// The arguments that run this binary as the descriptor probe, alone.
    pub(crate) fn probe_args() -> [&'static str; 5] {
        let probe = concat!(module_path!(), "::descriptor_probe");
        let probe = probe.split_once("::").map_or(probe, |(_, test)| test);
        ["--exact", probe, "--nocapture", "--test-threads=1", PROBE]
    }

    /// The descriptors the probe printed it holds (on libtest's line for it, after the test's name).
    pub(crate) fn held(printed: &str) -> Vec<i32> {
        printed
            .lines()
            .find_map(|line| line.split_once("HEX_FDS ").map(|(_, held)| held))
            .unwrap_or_else(|| panic!("the probe printed no descriptors: {printed}"))
            .split_whitespace()
            .map(|fd| fd.parse().unwrap())
            .collect()
    }

    /// AC-T7.4.1 (CLAUDE.md §2, every buffer has a bound): a directory tool's output is read within the
    /// bound its call derives, never whole. Do: run a tool that prints one line of 4096 bytes where its
    /// call allows 64. Expect: a typed refusal, Exhausted naming that tool's lines and the bound, and no
    /// line past the bound handed on.
    #[test]
    fn a_tool_line_past_its_bound_is_refused_by_name() {
        const WHAT: &str = "bytes in a line the test's shell prints";
        let mut past = 0usize;
        let read = lines(
            "/bin/sh",
            WHAT,
            &["-c", "head -c 4096 /dev/zero | tr '\\000' x; echo"],
            64,
            |line| {
                if line.len() > 64 {
                    past += 1;
                }
                Ok(())
            },
        );
        assert_eq!(past, 0, "a line past its bound was handed on");
        let refused = read.expect_err("a line past its bound was read whole");
        assert!(
            matches!(
                refused
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<hex_api::Refusal>()),
                Some(hex_api::Refusal::Exhausted {
                    what: WHAT,
                    bound: 64
                })
            ),
            "{refused}"
        );
    }

    /// AC-T7.4.1 (CLAUDE.md §2): this Mac's directory reads within the bounds each call derives. Do: read
    /// the role ids held, and the users of a pool whose group no record has, through the real dscl and ps.
    /// Expect: both read to their end, no line past its bound, and only role ids kept.
    #[test]
    fn this_macs_directory_reads_within_its_bounds() {
        let directory = Directory {
            group: "hex-test-no-group".to_owned(),
            gid: u32::MAX,
            manifest: PathBuf::new(),
            peak_file: PathBuf::new(),
        };
        let taken = directory.taken_ids().unwrap();
        assert!(taken.iter().all(|id| ROLE_UIDS.contains(id)), "{taken:?}");
        let users = directory.pool_users().unwrap();
        assert!(users.is_empty(), "{users:?}");
    }

    /// Not a test of its own: the tool the descriptor tests start (this binary, named so), which
    /// prints the descriptors it holds.
    #[test]
    fn descriptor_probe() {
        if !std::env::args_os().any(|arg| arg == PROBE) {
            return;
        }
        let pid = rustix::process::getpid().as_raw_nonzero().get();
        let held = crate::macos::sys::descriptors(pid).unwrap();
        let held: Vec<String> = held.iter().map(i32::to_string).collect();
        println!("HEX_FDS {}", held.join(" "));
    }

    /// AC-T7.4.1 (CLAUDE.md §12, CVE-2024-21626's class): a tool the box helper runs holds only the
    /// descriptors it is given, its stdio. Do: hold a descriptor without close-on-exec, as a pipe
    /// another thread has just made holds its ends until its fcntl (macOS's pipe(2) takes no flag),
    /// and run a tool through the directory's runner: this binary's descriptor probe, which lists
    /// what it holds (proc_pidinfo). Expect: its stdio, 0, 1 and 2, and nothing else.
    #[test]
    fn a_tool_the_directory_runs_holds_only_its_stdio() {
        let (reader, _writer) = std::io::pipe().unwrap();
        // dup(2) leaves the copy without close-on-exec: the state of a fresh pipe's end before its fcntl.
        let open = rustix::io::dup(&reader).unwrap();
        assert!(
            !rustix::io::fcntl_getfd(&open)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        let exe = std::env::current_exe().unwrap();
        let mut printed = String::new();
        lines(
            exe.to_str().unwrap(),
            "bytes in a line the probe prints",
            &probe_args(),
            4096,
            |line| {
                printed.push_str(line);
                printed.push('\n');
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            held(&printed),
            [0, 1, 2],
            "the tool held descriptors it wasn't given (this process's {} without close-on-exec among them)",
            std::os::fd::AsRawFd::as_raw_fd(&open)
        );
    }
}
