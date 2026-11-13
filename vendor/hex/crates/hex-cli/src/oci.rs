//! `hex oci <verb>`: the OCI runtime command line, driving `hex_runtime`'s `oci` class. It is
//! runtime-spec compatible, so opencontainers/runtime-tools drives it as it drives runc or crun
//! (AC-T7.1.4). `hex __oci-init` is the re-exec'd container init, and `hex __oci-exec` the helper
//! `hex oci exec` becomes; both are handled in `main`.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use hex_runtime::linux::oci::OciError;
use hex_runtime::linux::oci::run::Runtime;

/// Runs an `oci` subcommand (`args` is everything after `oci`); returns the process exit code.
pub fn main(args: &[String]) -> u8 {
    // `exec` becomes its helper and so returns only on failure; the program's status is the helper's.
    if let Some((verb, rest)) = args.split_first()
        && verb == "exec"
    {
        let e = match exec(rest) {
            Ok(mut cmd) => OciError::os("starting hex __oci-exec", cmd.exec()),
            Err(e) => e,
        };
        let _ = writeln!(std::io::stderr().lock(), "hex oci: {e}");
        return 126;
    }
    match run(args) {
        Ok(output) => {
            if !output.is_empty() {
                let _ = std::io::stdout().lock().write_all(output.as_bytes());
            }
            0
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "hex oci: {e}");
            1
        }
    }
}

fn run(args: &[String]) -> Result<String, OciError> {
    let rt = Runtime::new(Runtime::default_root());
    let (verb, rest) = args.split_first().ok_or_else(usage)?;
    match verb.as_str() {
        "create" => {
            let (id, bundle, pid_file) = parse_create(rest)?;
            // What a label whose module the host's kernel doesn't run does: its owner's word, else skipped
            // with a warning (lsm::ENV_POLICY).
            let policy = hex_runtime::linux::oci::lsm::Policy::from_env()?;
            Runtime::new(Runtime::default_root())
                .with_lsm(policy)
                .create(&id, &bundle, pid_file.as_deref())?;
            Ok(String::new())
        }
        "start" => {
            rt.start(&one_id(rest)?)?;
            Ok(String::new())
        }
        "state" => {
            let state = rt.state(&one_id(rest)?)?;
            String::from_utf8(state.to_json()?)
                .map_err(|e| OciError::Broken(format!("state isn't utf-8: {e}")))
        }
        "kill" => {
            let (id, sig, all) = parse_kill(rest)?;
            rt.kill(&id, sig, all)?;
            Ok(String::new())
        }
        "delete" => {
            let (id, force) = parse_delete(rest)?;
            rt.delete(&id, force)?;
            Ok(String::new())
        }
        other => Err(OciError::Config(format!(
            "unknown oci command {other:?}; {}",
            usage()
        ))),
    }
}

fn usage() -> OciError {
    OciError::Config(
        "usage: hex oci <create|start|state|kill|delete> [flags] <id>, or hex oci exec [--cwd dir] \
         [--env K=V]... [--user uid[:gid]] <id> [--] <program> [args]..."
            .to_owned(),
    )
}

/// The command `hex oci exec` becomes (`Runtime::exec_command`).
fn exec(rest: &[String]) -> Result<std::process::Command, OciError> {
    let e = parse_exec(rest)?;
    Runtime::new(Runtime::default_root()).exec_command(&e.id, e.args, e.cwd, e.env, e.user, false)
}

/// `hex oci exec`'s command line.
#[derive(Debug, PartialEq, Eq)]
pub struct ExecArgs {
    pub id: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<String>,
    pub user: Option<(u32, Option<u32>)>,
}

/// Parses `[--cwd dir] [--env K=V]... [--user uid[:gid]] <id> [--] <program> [args]...`, as runc's
/// `exec` takes them (`-e` and `-u` too). Flags come before the id; everything after it is the
/// program's, flags included.
pub fn parse_exec(rest: &[String]) -> Result<ExecArgs, OciError> {
    let mut cwd = None;
    let mut env = Vec::new();
    let mut user = None;
    let mut i = 0usize;
    while let Some(arg) = rest.get(i) {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let flag = match name {
            "--cwd" => "cwd",
            "--env" | "-e" => "env",
            "--user" | "-u" => "user",
            "--" => {
                return Err(OciError::Config(
                    "exec: `--` comes after the container id, before the program".to_owned(),
                ));
            }
            other if other.starts_with('-') => {
                return Err(OciError::Config(format!("exec: unknown flag {other:?}")));
            }
            _ => break,
        };
        let value = match inline {
            Some(v) => v,
            None => {
                i = i.saturating_add(1);
                rest.get(i)
                    .cloned()
                    .ok_or_else(|| OciError::Config(format!("exec: {name} needs a value")))?
            }
        };
        match flag {
            "cwd" => cwd = Some(value),
            "env" => {
                if !value.contains('=') {
                    return Err(OciError::Config(format!(
                        "exec: --env takes K=V, not {value:?}"
                    )));
                }
                env.push(value);
            }
            _ => user = Some(parse_user(&value)?),
        }
        i = i.saturating_add(1);
    }
    let id = rest
        .get(i)
        .cloned()
        .ok_or_else(|| OciError::Config("exec needs a container id".to_owned()))?;
    let mut program = rest.get(i.saturating_add(1)..).unwrap_or_default();
    if let Some((first, after)) = program.split_first()
        && first == "--"
    {
        program = after;
    }
    if program.is_empty() {
        return Err(OciError::Config("exec needs a program".to_owned()));
    }
    Ok(ExecArgs {
        id,
        args: program.to_vec(),
        cwd,
        env,
        user,
    })
}

/// `uid` or `uid:gid`, both numbers: a name would need the container's own user database.
fn parse_user(s: &str) -> Result<(u32, Option<u32>), OciError> {
    let number = |part: &str| {
        part.parse::<u32>().map_err(|_| {
            OciError::Config(format!(
                "exec: --user takes uid[:gid] as numbers, not {s:?}"
            ))
        })
    };
    match s.split_once(':') {
        Some((u, g)) => Ok((number(u)?, Some(number(g)?))),
        None => Ok((number(s)?, None)),
    }
}

/// Splits `rest` into flags (a leading run of `--flag [value]`) and positionals, taking the value
/// of each flag in `with_value`.
fn parse_flags<'a>(
    rest: &'a [String],
    with_value: &[&str],
) -> (Vec<(String, Option<String>)>, Vec<&'a String>) {
    let mut flags = Vec::new();
    let mut positionals = Vec::new();
    let mut i = 0;
    while let Some(arg) = rest.get(i) {
        if let Some(name) = arg.strip_prefix("--") {
            if let Some((k, v)) = name.split_once('=') {
                flags.push((k.to_owned(), Some(v.to_owned())));
            } else if with_value.contains(&name) {
                let v = rest.get(i.saturating_add(1)).cloned();
                flags.push((name.to_owned(), v));
                i = i.saturating_add(1);
            } else {
                flags.push((name.to_owned(), None));
            }
        } else {
            positionals.push(arg);
        }
        i = i.saturating_add(1);
    }
    (flags, positionals)
}

fn parse_create(rest: &[String]) -> Result<(String, PathBuf, Option<PathBuf>), OciError> {
    let (flags, positionals) = parse_flags(rest, &["bundle", "pid-file", "console-socket"]);
    let value = |name: &str| {
        flags
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.clone())
    };
    let bundle = value("bundle")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let pid_file = value("pid-file").map(PathBuf::from);
    let id = positionals
        .first()
        .ok_or_else(|| OciError::Config("create needs a container id".to_owned()))?;
    Ok(((*id).clone(), bundle, pid_file))
}

fn one_id(rest: &[String]) -> Result<String, OciError> {
    let (_, positionals) = parse_flags(rest, &[]);
    positionals
        .first()
        .map(|s| (*s).clone())
        .ok_or_else(|| OciError::Config("a container id is needed".to_owned()))
}

fn parse_kill(rest: &[String]) -> Result<(String, i32, bool), OciError> {
    let (flags, positionals) = parse_flags(rest, &[]);
    let all = flags.iter().any(|(k, _)| k == "all");
    let id = positionals
        .first()
        .map(|s| (*s).clone())
        .ok_or_else(|| OciError::Config("kill needs a container id".to_owned()))?;
    // The signal defaults to TERM, as runc's does; a second positional overrides it.
    let sig = match positionals.get(1) {
        Some(s) => signal_number(s)?,
        None => 15,
    };
    Ok((id, sig, all))
}

fn parse_delete(rest: &[String]) -> Result<(String, bool), OciError> {
    let (flags, positionals) = parse_flags(rest, &[]);
    let force = flags.iter().any(|(k, _)| k == "force");
    let id = positionals
        .first()
        .map(|s| (*s).clone())
        .ok_or_else(|| OciError::Config("delete needs a container id".to_owned()))?;
    Ok((id, force))
}

/// A signal's number from its name (`KILL`, `SIGKILL`) or a decimal number.
fn signal_number(s: &str) -> Result<i32, OciError> {
    if let Ok(n) = s.parse::<i32>() {
        return Ok(n);
    }
    let name = s.strip_prefix("SIG").unwrap_or(s).to_ascii_uppercase();
    let n = match name.as_str() {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        "STOP" => 19,
        "CONT" => 18,
        other => return Err(OciError::Config(format!("unknown signal {other:?}"))),
    };
    Ok(n)
}

/// Runs the container init (`hex __oci-init`). Only reached on Linux.
pub fn init() -> u8 {
    hex_runtime::linux::oci::init::run()
}

/// Runs `hex oci exec`'s helper (`hex __oci-exec`). Only reached on Linux.
pub fn exec_helper() -> u8 {
    hex_runtime::linux::oci::exec::run()
}

/// Runs an exec seed (`hex __oci-seed`), the node agent's reusable helper for exec through the seam.
/// Only reached on Linux.
pub fn seed() -> u8 {
    hex_runtime::linux::oci::seed::run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| (*a).to_owned()).collect()
    }

    /// `hex oci exec <id> <program>` is runc's shape, as oci_bench.py calls it, and everything after
    /// the id is the program's, its own flags included (docs/design/runtime.md, `hex oci exec`; AC-T7.1.4).
    #[test]
    fn exec_takes_flags_before_the_id_and_the_rest_as_the_program() {
        let e = parse_exec(&args(&["c1", "/usr/bin/true"])).unwrap();
        assert_eq!(
            (e.id.as_str(), e.args.clone()),
            ("c1", args(&["/usr/bin/true"]))
        );
        let e = parse_exec(&args(&[
            "--cwd",
            "/w",
            "-e",
            "A=1",
            "--env=B=2",
            "--user",
            "1000:100",
            "c1",
            "--",
            "sh",
            "-c",
            "echo --cwd",
        ]))
        .unwrap();
        assert_eq!(e.cwd.as_deref(), Some("/w"));
        assert_eq!(e.env, args(&["A=1", "B=2"]));
        assert_eq!(e.user, Some((1000, Some(100))));
        assert_eq!(e.args, args(&["sh", "-c", "echo --cwd"]));
        assert_eq!(
            parse_exec(&args(&["-u", "7", "c1", "id"])).unwrap().user,
            Some((7, None))
        );
    }

    /// What exec can't run is refused by name, before anything starts.
    #[test]
    fn exec_refuses_a_missing_id_program_or_value_and_bad_flags() {
        for bad in [
            &["--cwd"][..],
            &["c1"],
            &["c1", "--"],
            &["--env", "noequals", "c1", "true"],
            &["--user", "root", "c1", "true"],
            &["--tty", "c1", "true"],
            &["--", "c1", "true"],
            &[],
        ] {
            assert!(parse_exec(&args(bad)).is_err(), "{bad:?} was accepted");
        }
    }
}
