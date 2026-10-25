//! `kiln rustc` (docs/ci.md W24-R09): a `RUSTC_WRAPPER` that gives back rustc's outputs for an
//! invocation whose inputs are unchanged. cargo runs it as `<wrapper> <rustc> <args>`. What the
//! invocation is and how it's keyed are `kiln_ci::work::rustc`'s; this runs rustc, hashes files and
//! keeps entries in a folder of its own (`$KILN_RUSTC_CACHE`, or `~/.cache/kiln/rustc`): under the
//! weak key, what the last run read; under the strong key, its outputs and what rustc said. A hit
//! puts the outputs back under the names cargo expects and replays rustc's diagnostics; a miss runs
//! rustc and keeps a success. `$KILN_RUSTC_LOG`, when set, gets a line for each invocation.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use kiln_ci::work::rustc::{self as key, Invocation, Parsed};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// What the last run of a weak key read: its inputs (paths remapped) and its `env-dep`s.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Reads {
    inputs: Vec<String>,
    env: BTreeMap<String, Option<String>>,
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

fn log(line: &str) {
    if let Some(p) = std::env::var_os("KILN_RUSTC_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn code(status: Option<i32>) -> ExitCode {
    ExitCode::from(u8::try_from(status.unwrap_or(1)).unwrap_or(1))
}

/// Runs rustc as it is.
fn direct(rustc: &OsString, args: &[OsString]) -> ExitCode {
    match Command::new(rustc).args(args).status() {
        Ok(s) => code(s.code()),
        Err(e) => {
            eprintln!("kiln rustc: running {}: {e}", rustc.to_string_lossy());
            ExitCode::FAILURE
        }
    }
}

/// `kiln rustc <rustc> <args>`.
pub fn main(argv: &[OsString]) -> ExitCode {
    let Some((rustc, rest)) = argv.split_first() else {
        eprintln!("kiln rustc: usage: kiln rustc <rustc> <args>");
        return ExitCode::FAILURE;
    };
    let Some(args) = rest.iter().map(|a| a.to_str().map(str::to_owned)).collect::<Option<Vec<String>>>() else {
        return direct(rustc, rest);
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut remaps = vec![(cwd.to_string_lossy().into_owned(), "/ws".to_owned())];
    if let Some(home) = std::env::var_os("CARGO_HOME").or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo").into_os_string())) {
        remaps.push((home.to_string_lossy().into_owned(), "/cargo".to_owned()));
    }
    // The build's own folder (`target/debug`), wherever the target is: `--out-dir` is its `deps`, or
    // a build script's `build/<package>`.
    let base = args.iter().position(|a| a == "--out-dir").and_then(|i| args.get(i + 1)).and_then(|o| {
        o.rsplit_once("/build/").map(|(b, _)| b.to_owned()).or_else(|| o.strip_suffix("/deps").map(str::to_owned)).or_else(|| Path::new(o).parent().map(|p| p.to_string_lossy().into_owned()))
    });
    if let Some(b) = base {
        remaps.insert(0, (b, "/out".to_owned()));
    }
    let inv = match key::parse(&args, &remaps) {
        Parsed::Cache(inv) => inv,
        Parsed::Direct(why) => {
            log(&format!("direct {}: {why}", args.iter().position(|a| a == "--crate-name").and_then(|i| args.get(i + 1)).map_or("-", String::as_str)));
            return direct(rustc, rest);
        }
    };
    // A build script's probe (`autocfg`'s, `proc-macro2`'s): no unit of cargo's, and nothing kept.
    if inv.extra_filename.is_empty() && inv.out_dir.contains("/build/") {
        log(&format!("direct {}: a build script's probe", inv.crate_name));
        return direct(rustc, rest);
    }
    match cached(rustc, &args, &inv, &remaps) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("direct {}: {e:#}", inv.crate_name));
            direct(rustc, rest)
        }
    }
}

fn root() -> PathBuf {
    std::env::var_os("KILN_RUSTC_CACHE").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/kiln/rustc"))).unwrap_or_else(|| PathBuf::from(".kiln-rustc"))
}

/// A remapped path back in this build.
fn unmap(path: &str, remaps: &[(String, String)]) -> String {
    remaps.iter().find_map(|(from, to)| path.strip_prefix(to.as_str()).filter(|r| r.is_empty() || r.starts_with('/')).map(|r| format!("{from}{r}"))).unwrap_or_else(|| path.to_owned())
}

/// A path remapped, as keys keep it.
fn map(path: &str, remaps: &[(String, String)]) -> String {
    remaps.iter().find_map(|(from, to)| path.strip_prefix(from.as_str()).filter(|r| r.is_empty() || r.starts_with('/')).map(|r| format!("{to}{r}"))).unwrap_or_else(|| path.to_owned())
}

/// The digests of `inputs` (remapped) as they are now: a missing one is `absent`.
fn now(inputs: &[String], remaps: &[(String, String)]) -> BTreeMap<String, String> {
    inputs.iter().map(|p| (p.clone(), sha256_file(Path::new(&unmap(p, remaps))).unwrap_or_else(|| "absent".into()))).collect()
}

fn cached(rustc: &OsString, args: &[String], inv: &Invocation, remaps: &[(String, String)]) -> anyhow::Result<ExitCode> {
    let toolchain = Command::new(rustc).arg("-vV").output()?;
    anyhow::ensure!(toolchain.status.success(), "`rustc -vV` failed");
    let toolchain = String::from_utf8_lossy(&toolchain.stdout).into_owned();
    let digests: BTreeMap<String, String> = inv.externs.iter().map(|(_, p)| (p.clone(), sha256_file(Path::new(p)).unwrap_or_else(|| "absent".into()))).collect();
    let env: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CARGO_PKG_") || matches!(k.as_str(), "CARGO_CRATE_NAME" | "CARGO_MANIFEST_DIR" | "CARGO_PRIMARY_PACKAGE" | "OUT_DIR"))
        .map(|(k, v)| (k, map(&v, remaps)))
        .collect();
    let weak = key::weak(&toolchain, inv, &digests, &env);
    let dir = root().join(&weak);
    let stem = format!("{}{}", inv.crate_name, inv.extra_filename);
    // A hit: what the last run read is as it was.
    if let Ok(text) = std::fs::read(dir.join("reads.json"))
        && let Ok(reads) = serde_json::from_slice::<Reads>(&text)
    {
        let env_now: BTreeMap<String, Option<String>> = reads.env.keys().map(|k| (k.clone(), std::env::var(k).ok().map(|v| map(&v, remaps)))).collect();
        let strong = key::strong(&weak, &now(&reads.inputs, remaps), &env_now);
        let entry = dir.join(&strong);
        if entry.join("done").is_file() {
            for e in std::fs::read_dir(entry.join("files"))? {
                let e = e?;
                let name = e.file_name();
                let to = Path::new(&inv.out_dir).join(&name);
                if name.to_string_lossy().ends_with(".d") {
                    let text = std::fs::read_to_string(e.path())?;
                    let mut back = text.clone();
                    for (from, to) in remaps {
                        back = back.replace(&format!("{to}/"), &format!("{from}/"));
                    }
                    std::fs::write(&to, back)?;
                } else {
                    let _ = std::fs::remove_file(&to);
                    std::fs::copy(e.path(), &to)?;
                }
            }
            if let Ok(said) = std::fs::read(entry.join("stderr")) {
                let _ = std::io::stderr().write_all(&said);
            }
            log(&format!("hit {stem}"));
            return Ok(ExitCode::SUCCESS);
        }
    }
    // A miss: rustc runs, its paths remapped as the key's are so its outputs are the same from any
    // checkout or target folder, its diagnostics passed on as they come and kept.
    let mut child = Command::new(rustc).args(args).args(remaps.iter().map(|(from, to)| format!("--remap-path-prefix={from}={to}"))).stderr(Stdio::piped()).spawn()?;
    let mut said = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let mut buf = [0u8; 8192];
        loop {
            let n = std::io::Read::read(&mut err, &mut buf)?;
            if n == 0 {
                break;
            }
            let got = buf.get(..n).unwrap_or_default();
            let _ = std::io::stderr().write_all(got);
            said.extend_from_slice(got);
        }
    }
    let status = child.wait()?;
    log(&format!("miss {stem}"));
    if !status.success() {
        return Ok(code(status.code()));
    }
    let Ok(d) = std::fs::read_to_string(Path::new(&inv.out_dir).join(format!("{stem}.d"))) else { return Ok(ExitCode::SUCCESS) };
    let Ok(dep) = kiln_ci::work::pathset::dep_info(&d) else { return Ok(ExitCode::SUCCESS) };
    let cwd = std::env::current_dir().unwrap_or_default();
    let inputs: Vec<String> = dep.inputs.iter().map(|p| map(&cwd.join(p).to_string_lossy(), remaps)).collect();
    // A variable's value as keys keep it: a path in it remapped (`OUT_DIR`, `CARGO_MANIFEST_DIR`).
    let env = dep.env.iter().map(|(k, v)| (k.clone(), v.as_ref().map(|v| map(v, remaps)))).collect();
    let reads = Reads { inputs, env };
    let strong = key::strong(&weak, &now(&reads.inputs, remaps), &reads.env);
    let entry = dir.join(&strong);
    let files = entry.join("files");
    std::fs::create_dir_all(&files)?;
    for e in std::fs::read_dir(&inv.out_dir)? {
        let e = e?;
        let name = e.file_name();
        let n = name.to_string_lossy();
        if !(n.starts_with(&stem) || n.starts_with(&format!("lib{stem}"))) || !e.file_type()?.is_file() {
            continue;
        }
        if n.ends_with(".d") {
            let mut text = std::fs::read_to_string(e.path())?;
            for (from, to) in remaps {
                text = text.replace(&format!("{from}/"), &format!("{to}/"));
            }
            std::fs::write(files.join(&name), text)?;
        } else {
            std::fs::copy(e.path(), files.join(&name))?;
        }
    }
    std::fs::write(entry.join("stderr"), &said)?;
    std::fs::write(entry.join("done"), b"")?;
    std::fs::write(dir.join("reads.json"), serde_json::to_vec(&reads)?)?;
    Ok(ExitCode::SUCCESS)
}
