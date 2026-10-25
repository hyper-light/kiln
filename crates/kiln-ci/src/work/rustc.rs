//! `kiln rustc` (docs/ci.md W24-R09): a rustc invocation as a unit of work. [`parse`] reads what
//! cargo passes; anything it doesn't understand, and a crate under edit (built with `-C
//! incremental`), runs rustc directly. [`weak`] keys an invocation on the toolchain, its arguments
//! with paths remapped (the output and incremental folders dropped), each `--extern`'s digest
//! rather than its path, and the variables cargo sets that rustc reads; [`strong`] adds what the
//! last run's dependency file named, each input's digest and each `env-dep`'s value (R04's two
//! phases). Pure: the wrapper hashes files and runs rustc.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// A rustc invocation `kiln rustc` caches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub crate_name: String,
    pub crate_types: Vec<String>,
    pub emit: Vec<String>,
    /// Where its outputs go, and the suffix cargo gives their names (`-C extra-filename`).
    pub out_dir: String,
    pub extra_filename: String,
    /// `--extern name=path`, each.
    pub externs: Vec<(String, String)>,
    /// Its source file.
    pub input: String,
    /// The arguments as keyed: paths remapped, the output folder and the externs' paths left out.
    pub keyed: Vec<String>,
}

/// What [`parse`] makes of an invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Cache(Invocation),
    /// Run rustc as it is, saying why.
    Direct(String),
}

/// `path` with the first of `remaps` (`from`, `to`) it starts with replaced.
fn remap(path: &str, remaps: &[(String, String)]) -> String {
    remaps.iter().find_map(|(from, to)| path.strip_prefix(from.as_str()).map(|rest| format!("{to}{rest}"))).unwrap_or_else(|| path.to_owned())
}

/// Reads rustc's arguments (`args`, the program's own left out), its paths remapped by `remaps`.
pub fn parse(args: &[String], remaps: &[(String, String)]) -> Parsed {
    let mut inv = Invocation::default();
    let mut it = args.iter().peekable();
    // A flag's value: `--flag=value` or `--flag value`.
    let value = |flag: &str, arg: &str, it: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| -> Option<String> {
        match arg.strip_prefix(flag) {
            Some(rest) if rest.starts_with('=') => Some(rest.get(1..).unwrap_or_default().to_owned()),
            Some("") => it.next().cloned(),
            _ => None,
        }
    };
    while let Some(arg) = it.next() {
        let a = arg.as_str();
        if a == "-" || a.starts_with('@') || a == "-vV" || a == "-V" || a.starts_with("--print") || a == "--version" {
            return Parsed::Direct(format!("`{a}` asks rustc something rather than builds"));
        }
        if let Some(v) = value("--crate-name", a, &mut it) {
            inv.keyed.extend(["--crate-name".to_owned(), v.clone()]);
            inv.crate_name = v;
        } else if let Some(v) = value("--crate-type", a, &mut it) {
            inv.keyed.extend(["--crate-type".to_owned(), v.clone()]);
            inv.crate_types.push(v);
        } else if let Some(v) = value("--emit", a, &mut it) {
            inv.emit = v.split(',').map(str::to_owned).collect();
            inv.keyed.push(format!("--emit={v}"));
        } else if let Some(v) = value("--out-dir", a, &mut it) {
            inv.out_dir = v;
        } else if let Some(v) = value("--extern", a, &mut it) {
            match v.split_once('=') {
                Some((name, path)) => inv.externs.push((name.to_owned(), path.to_owned())),
                None => inv.keyed.extend(["--extern".to_owned(), v]),
            }
        } else if let Some(v) = value("-L", a, &mut it) {
            // `-L kind=path`: the path remapped, its kind kept.
            let l = match v.split_once('=') {
                Some((kind, path)) => format!("{kind}={}", remap(path, remaps)),
                None => remap(&v, remaps),
            };
            inv.keyed.extend(["-L".to_owned(), l]);
        } else if a == "-C" || a.starts_with("-C") || a.starts_with("--codegen") {
            let v = if a == "-C" || a == "--codegen" { it.next().cloned().unwrap_or_default() } else { a.trim_start_matches("--codegen=").trim_start_matches("-C").to_owned() };
            if v.starts_with("incremental=") {
                return Parsed::Direct("a crate under edit (`-C incremental`) stays incremental".into());
            }
            if let Some(x) = v.strip_prefix("extra-filename=") {
                inv.extra_filename = x.to_owned();
            }
            inv.keyed.push(format!("-C{}", remap(&v, remaps)));
        } else if let Some(v) = ["--cfg", "--check-cfg", "--edition", "--target", "--cap-lints", "--error-format", "--json", "--diagnostic-width", "--color", "-W", "-A", "-D", "-F"].iter().find_map(|f| value(f, a, &mut it).map(|v| (f, v))) {
            inv.keyed.extend([v.0.to_string(), v.1]);
        } else if !a.starts_with('-') && (a.ends_with(".rs") || inv.input.is_empty()) {
            inv.input = a.to_owned();
            inv.keyed.push(remap(a, remaps));
        } else {
            return Parsed::Direct(format!("`{a}` isn't an argument kiln rustc understands"));
        }
    }
    if inv.crate_name.is_empty() || inv.out_dir.is_empty() || inv.input.is_empty() {
        return Parsed::Direct("no crate name, output folder or source".into());
    }
    Parsed::Cache(inv)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An invocation's weak key: the toolchain's identity, its keyed arguments, each extern's digest
/// by name (`digests`, by its path), and the variables cargo sets for it (`env`).
pub fn weak(toolchain: &str, inv: &Invocation, digests: &BTreeMap<String, String>, env: &BTreeMap<String, String>) -> String {
    let externs: BTreeMap<&str, &str> = inv.externs.iter().map(|(n, p)| (n.as_str(), digests.get(p).map_or("missing", String::as_str))).collect();
    let mut h = Sha256::new();
    let body = serde_json::json!({ "v": 1, "toolchain": toolchain, "args": inv.keyed, "externs": externs, "env": env });
    h.update(body.to_string().as_bytes());
    hex(&h.finalize())
}

/// The strong key: the weak one and what the last run read, each input's digest (by its path,
/// remapped) and each `env-dep`'s value.
pub fn strong(weak: &str, inputs: &BTreeMap<String, String>, env: &BTreeMap<String, Option<String>>) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::json!({ "weak": weak, "inputs": inputs, "env": env }).to_string().as_bytes());
    hex(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    /// W24-R09: cargo's invocation of a library is read and keyed; the same build from another
    /// checkout keys alike; an extern's digest counts, not its path; a crate under edit and a
    /// question for rustc run it directly.
    #[test]
    fn w24_r09_invocations_parse_and_key() -> Result<(), String> {
        let remaps = |root: &str| vec![(root.to_owned(), "/ws".to_owned())];
        let a = args("--crate-name alpha --edition=2021 alpha/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi --crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no -C debuginfo=2 -C metadata=1a2b -C extra-filename=-1a2b --out-dir /w1/target/debug/deps -L dependency=/w1/target/debug/deps --extern beta=/w1/target/debug/deps/libbeta-9.rmeta");
        let Parsed::Cache(one) = parse(&a, &remaps("/w1")) else { return Err("not cached".into()) };
        assert_eq!((one.crate_name.as_str(), one.extra_filename.as_str(), one.out_dir.as_str()), ("alpha", "-1a2b", "/w1/target/debug/deps"));
        assert_eq!(one.externs, vec![("beta".to_owned(), "/w1/target/debug/deps/libbeta-9.rmeta".to_owned())]);
        let b: Vec<String> = a.iter().map(|x| x.replace("/w1", "/w2")).collect();
        let Parsed::Cache(two) = parse(&b, &remaps("/w2")) else { return Err("not cached".into()) };
        let path = |i: &Invocation| i.externs.first().map(|e| e.1.clone()).unwrap_or_default();
        let digests = |d: &str| BTreeMap::from([(path(&one), d.to_owned()), (path(&two), d.to_owned())]);
        let env = BTreeMap::from([("CARGO_PKG_NAME".to_owned(), "alpha".to_owned())]);
        assert_eq!(weak("rustc 1.98", &one, &digests("d1"), &env), weak("rustc 1.98", &two, &digests("d1"), &env), "another checkout keys alike");
        assert_ne!(weak("rustc 1.98", &one, &digests("d1"), &env), weak("rustc 1.98", &one, &digests("d2"), &env), "a dependency's digest counts");
        assert_ne!(weak("rustc 1.98", &one, &digests("d1"), &env), weak("rustc 1.99", &one, &digests("d1"), &env), "so does the toolchain");
        let w = weak("rustc 1.98", &one, &digests("d1"), &env);
        assert_ne!(strong(&w, &BTreeMap::from([("/ws/alpha/src/lib.rs".to_owned(), "x".to_owned())]), &BTreeMap::new()), strong(&w, &BTreeMap::from([("/ws/alpha/src/lib.rs".to_owned(), "y".to_owned())]), &BTreeMap::new()));
        assert!(matches!(parse(&args("--crate-name a a.rs --out-dir o -C incremental=/w/inc"), &[]), Parsed::Direct(_)), "a crate under edit");
        assert!(matches!(parse(&args("-vV"), &[]), Parsed::Direct(_)));
        assert!(matches!(parse(&args("- --crate-name ___ --print=file-names"), &[]), Parsed::Direct(_)));
        assert!(matches!(parse(&args("--crate-name a a.rs --out-dir o -Z unstable-options"), &[]), Parsed::Direct(_)), "what it doesn't understand");
        Ok(())
    }
}
