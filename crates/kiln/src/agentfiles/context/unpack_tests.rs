//! Archives through the one unpacker (kiln plan 01a1ce705e62732b83f16295d74ee263, step 3). Contexts of
//! pi's shape are fetched from kiln's own HTTP server in the test: pi's tree with `pi/` stripped at the
//! harness directory's root, then ripgrep's and fd's members at bin/. So are agy's (plan
//! 01a1ce705e55753182217afdfe607de9, step 3): each archive's one entry, the member antigravity held
//! alone, at bin/agy. And Codex's (plan 01a1ce705d7076648c1d9f9ee26214d3, step 3): OpenAI's package at
//! the root, arm64's skipping its codex-path/rg for ripgrep's member at that path. Each archive the
//! unpacker refuses comes back as its typed error, naming its URL and its entry, and leaves nothing
//! behind.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tar::EntryType;

use super::super::builtin::Builtin;
use super::super::unpack::Why;
use super::{Error, materialize};
use crate::http::server::{self as srv, Request, Response};

const AGENTFILE: &str = "FROM scratch\nARG TARGETARCH\nHARNESS pi FROM ./pi-${TARGETARCH}\n";
const HARNESS_JSON: &str = "{\"schemaVersion\":1,\"name\":\"pi\"}\n";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// An entry of a fixture archive: its name's bytes as they are (tar's own setters refuse `/x` and
/// `../x`), its kind, its mode, its data, and the path a link names.
struct E {
    name: Vec<u8>,
    kind: EntryType,
    mode: u32,
    data: Vec<u8>,
    link: Option<&'static str>,
}

fn dir(name: impl Into<Vec<u8>>) -> E {
    E { name: name.into(), kind: EntryType::Directory, mode: 0o755, data: Vec::new(), link: None }
}

fn file(name: impl Into<Vec<u8>>, mode: u32, data: &[u8]) -> E {
    E { name: name.into(), kind: EntryType::Regular, mode, data: data.to_vec(), link: None }
}

fn link(name: &str, kind: EntryType, to: &'static str) -> E {
    E { name: name.into(), kind, mode: 0o777, data: Vec::new(), link: Some(to) }
}

/// A gzip tar of `entries`, in order.
fn targz(entries: &[E]) -> anyhow::Result<Vec<u8>> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    for e in entries {
        let mut h = tar::Header::new_gnu();
        let gnu = h.as_gnu_mut().ok_or_else(|| anyhow::anyhow!("not a GNU header"))?;
        gnu.name.get_mut(..e.name.len()).ok_or_else(|| anyhow::anyhow!("a name past 100 bytes"))?.copy_from_slice(&e.name);
        h.set_entry_type(e.kind);
        h.set_mode(e.mode);
        h.set_size(u64::try_from(e.data.len())?);
        if let Some(to) = e.link {
            h.set_link_name_literal(to)?;
        }
        h.set_cksum();
        b.append(&h, e.data.as_slice())?;
    }
    Ok(b.into_inner()?.finish()?)
}

/// A tree's totals as pin.sh pins them: its directories and regular files as `tar -t` lists them, and
/// the regular files' bytes as `tar -tv` does.
fn totals(entries: &[E]) -> (u64, u64) {
    let taken = entries.iter().filter(|e| matches!(e.kind, EntryType::Directory | EntryType::Regular));
    taken.fold((0, 0), |(n, bytes), e| (n + 1, bytes + if e.kind == EntryType::Regular { e.data.len() as u64 } else { 0 }))
}

/// pi's tree as its release archives hold it, in small: `pi/` leading the executable, its package file
/// and its directories, the executables 0755 and the rest 0644 (as pi's v1.5.2 tarballs have them:
/// evidence e_step3_pi_modes.txt).
fn pi_tree(arch: &str) -> Vec<E> {
    vec![
        dir("pi/"),
        file("pi/pi", 0o755, format!("#!/bin/sh\necho pi {arch}\n").as_bytes()),
        file("pi/package.json", 0o644, b"{\"version\":\"1.5.2\"}\n"),
        dir("pi/theme/"),
        file("pi/theme/dark.json", 0o644, b"{}\n"),
        dir("pi/native/"),
        dir("pi/native/linux/"),
        file("pi/native/linux/helper.node", 0o755, b"\x7fELF helper\n"),
    ]
}

/// A tool's bytes for `arch`, as its archive holds it.
fn tool(name: &str, arch: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {name} {arch}\n").into_bytes()
}

/// One platform of pi's shape, as the test's server serves it and as its pin names it:
/// - `tree` at `.`, stripped of `pi/`, its totals `totals`;
/// - ripgrep's and fd's archives, each with a README and its binary, which ripgrep's archive holds as
///   `rg_in_archive` (the pin always names `rg`), at bin/ with mode 0755.
fn build(origin: &str, arch: &str, tree: &[E], totals: (u64, u64), rg_in_archive: &str) -> anyhow::Result<(Vec<(String, Vec<u8>)>, Value)> {
    let gz = targz(tree)?;
    let url = format!("{origin}/{arch}/pi.tar.gz");
    let mut files = vec![json!({ "url": url, "sha256": sha(&gz), "size": gz.len(), "path": ".", "unpack": { "format": "tar.gz", "strip": 1, "unpacked_bytes": totals.1, "entries": totals.0 } })];
    let mut routes = vec![(format!("/{arch}/pi.tar.gz"), gz)];
    for name in ["rg", "fd"] {
        let lead = format!("{name}-1.0.0-{arch}-unknown-linux-musl");
        let held = if name == "rg" { rg_in_archive } else { name };
        let body = tool(name, arch);
        let gz = targz(&[dir(format!("{lead}/")), file(format!("{lead}/README.md"), 0o644, b"readme\n"), file(format!("{lead}/{held}"), 0o755, &body)])?;
        let url = format!("{origin}/{arch}/{name}.tar.gz");
        files.push(json!({ "url": url, "sha256": sha(&gz), "size": gz.len(), "path": format!("bin/{name}"), "mode": "0755",
            "unpack": { "format": "tar.gz", "member": format!("{lead}/{name}"), "sha256": sha(&body), "size": body.len() } }));
        routes.push((format!("/{arch}/{name}.tar.gz"), gz));
    }
    Ok((routes, json!({ "platform": format!("linux/{arch}"), "files": files })))
}

/// pi's built-in for a test: the pin of `builds`.
fn pi(builds: Vec<Value>) -> Builtin {
    let pin = json!({ "version": "1.5.2", "verified": "by the test", "builds": builds }).to_string();
    Builtin { name: "harness-pi", harness: "pi", text: AGENTFILE, pin: pin.leak(), files: &[("harness.json", HARNESS_JSON)] }
}

/// What kiln's HTTP server in the test answers: by path, a body with 200, else 404.
struct Routes(Vec<(String, Vec<u8>)>);

impl srv::Handler for Routes {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        match self.0.iter().find(|(p, _)| p == req.uri().path()) {
            Some((_, body)) => srv::response(http::StatusCode::OK, "application/gzip", srv::full(body.clone())),
            None => srv::text(http::StatusCode::NOT_FOUND, "missing"),
        }
    }
}

/// kiln's own HTTP server on a port of its own, answering as `routes` (given its origin) says; its origin.
async fn serve(routes: impl FnOnce(&str) -> anyhow::Result<Vec<(String, Vec<u8>)>>) -> anyhow::Result<String> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let origin = format!("http://{}", tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?);
    let host = Arc::new(Routes(routes(&origin)?));
    crate::rt::spawn(async move {
        while let Ok(stream) = tcp.accept().await {
            let (h, peer) = (Arc::clone(&host), stream.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(stream), h, peer));
        }
    });
    Ok(origin)
}

fn names(dir: &Path) -> std::io::Result<BTreeSet<String>> {
    std::fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect()
}

fn set<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
    names.into_iter().map(str::to_owned).collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o7777)
}

/// A context of pi's shape for linux/amd64 and linux/arm64 holds the Agentfile and pi-amd64/ and
/// pi-arm64/, and nothing else: no staging directory, no archive. Each holds harness.json, pi's whole
/// tree at its root (`pi/` stripped, its archived modes kept) and bin/rg and bin/fd at 0755, the bytes
/// of its own arch.
#[test]
fn step28_a_pi_shaped_context_unpacks_as_pinned() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let mut builds = Vec::new();
        let origin = serve(|o| {
            let mut routes = Vec::new();
            for arch in ["amd64", "arm64"] {
                let tree = pi_tree(arch);
                let (r, b) = build(o, arch, &tree, totals(&tree), "rg")?;
                routes.extend(r);
                builds.push(b);
            }
            Ok(routes)
        })
        .await?;
        let b = pi(builds);
        let tmp = tempfile::tempdir()?;
        let ctx = tmp.path().join("ctx");

        let made = materialize(&b, &["linux/amd64".to_owned(), "linux/arm64".to_owned()], None, &ctx).await?;

        assert_eq!(made, [ctx.join("pi-amd64"), ctx.join("pi-arm64")], "from {origin}");
        assert_eq!(names(&ctx)?, set(["Agentfile", "pi-amd64", "pi-arm64"]));
        for arch in ["amd64", "arm64"] {
            let d = ctx.join(format!("pi-{arch}"));
            assert_eq!(names(&d)?, set(["bin", "harness.json", "native", "package.json", "pi", "theme"]), "{arch}");
            assert_eq!(names(&d.join("bin"))?, set(["fd", "rg"]), "{arch}");
            assert_eq!(std::fs::read_to_string(d.join("harness.json"))?, HARNESS_JSON);
            assert_eq!(std::fs::read(d.join("pi"))?, format!("#!/bin/sh\necho pi {arch}\n").into_bytes());
            assert_eq!(std::fs::read(d.join("bin/rg"))?, tool("rg", arch));
            assert_eq!(std::fs::read(d.join("bin/fd"))?, tool("fd", arch));
            #[cfg(unix)]
            {
                for (path, bits) in [("pi", 0o755), ("package.json", 0o644), ("theme", 0o755), ("theme/dark.json", 0o644), ("native/linux/helper.node", 0o755), ("bin/rg", 0o755), ("bin/fd", 0o755)] {
                    assert_eq!(mode(&d.join(path))?, bits, "{arch}: {path}");
                }
            }
        }
        Ok(())
    })
}

/// Each archive the unpacker refuses comes back as its typed error, naming the pinned URL and the entry
/// at fault: a symlink, a hard link, an absolute path, a `..` path, the same path twice, a member the
/// archive doesn't hold, and a tree of other totals than its pin's. Each is arm64's, after amd64's
/// directory was made, and leaves nothing in the context's directory: no harness directory, no staging
/// directory (the fetcher's or the unpacker's), no archive, no Agentfile.
#[test]
fn step28_an_archive_the_unpacker_refuses_leaves_nothing() -> anyhow::Result<()> {
    type Is = fn(&Why) -> bool;
    let with = |extra: E| {
        let mut t = pi_tree("arm64");
        t.insert(2, extra);
        t
    };
    let rg_missing = Some("rg-1.0.0-arm64-unknown-linux-musl/rg");
    // A directory its owner may not search (0644), which `remove_dir_all` can't empty once the tree is in.
    let mut locked = pi_tree("arm64");
    locked.extend([E { name: b"pi/locked/".to_vec(), kind: EntryType::Directory, mode: 0o644, data: Vec::new(), link: None }, file("pi/locked/f", 0o644, b"f\n")]);
    // Each case: what it is, arm64's tree, its pinned totals where they aren't the tree's, what ripgrep's
    // archive holds its binary as, the archive refused, the entry it names, and why.
    let cases: Vec<(&str, Vec<E>, Option<(u64, u64)>, &str, &str, Option<&str>, Is)> = vec![
        ("a symlink", with(link("pi/link", EntryType::Symlink, "pi")), None, "rg", "pi", Some("pi/link"), |w| matches!(w, Why::Link)),
        ("a hard link", with(link("pi/hard", EntryType::Link, "pi/pi")), None, "rg", "pi", Some("pi/hard"), |w| matches!(w, Why::Link)),
        ("an absolute path", with(file("/pi/abs", 0o644, b"x\n")), None, "rg", "pi", Some("/pi/abs"), |w| matches!(w, Why::Escapes)),
        ("a .. path", with(file("pi/../../x", 0o644, b"x\n")), None, "rg", "pi", Some("pi/../../x"), |w| matches!(w, Why::Escapes)),
        ("a duplicate", with(file("pi/package.json", 0o644, b"{}\n")), None, "rg", "pi", Some("pi/package.json"), |w| matches!(w, Why::Duplicate)),
        ("a missing member", pi_tree("arm64"), None, "rg-not-it", "rg", rg_missing, |w| matches!(w, Why::Missing)),
        ("a wrong count", pi_tree("arm64"), Some((9, 0)), "rg", "pi", None, |w| matches!(w, Why::Entries { expected: 9, got: 8 })),
        ("a missing member after a locked directory", locked, None, "rg-not-it", "rg", rg_missing, |w| matches!(w, Why::Missing)),
    ];
    for (what, tree, wrong, rg, fails, entry, is) in cases {
        crate::rt::block_on(async move {
            let mut builds = Vec::new();
            let origin = serve(|o| {
                let good = pi_tree("amd64");
                let (mut routes, b) = build(o, "amd64", &good, totals(&good), "rg")?;
                builds.push(b);
                let (r, b) = build(o, "arm64", &tree, wrong.map_or_else(|| totals(&tree), |(n, _)| (n, totals(&tree).1)), rg)?;
                routes.extend(r);
                builds.push(b);
                Ok(routes)
            })
            .await?;
            let b = pi(builds);
            let url = format!("{origin}/arm64/{fails}.tar.gz");
            let tmp = tempfile::tempdir()?;
            let (absent, empty) = (tmp.path().join("absent"), tmp.path().join("empty"));
            std::fs::create_dir(&empty)?;
            for ctx in [&absent, &empty] {
                let refused = materialize(&b, &["linux/amd64".to_owned(), "linux/arm64".to_owned()], None, ctx).await;
                let Err(Error::Unpack(u)) = refused else {
                    anyhow::bail!("{what}: {:?}, not the unpacker's refusal", refused.map(|m| m.len()));
                };
                assert_eq!((u.url.as_str(), u.entry.as_deref()), (url.as_str(), entry), "{what}: {u}");
                assert!(is(&u.why), "{what}: {:?}", u.why);
                let left = if ctx.exists() { names(ctx)? } else { BTreeSet::new() };
                assert!(left.is_empty(), "{what}: left {left:?}");
            }
            assert!(!absent.exists() || names(&absent)?.is_empty(), "{what}: the directory it made");
            Ok(())
        })?;
    }
    Ok(())
}

/// agy's harness file and Agentfile for a test: its HARNESS line takes `./antigravity-${TARGETARCH}`.
const AGY_AGENTFILE: &str = "FROM scratch\nARG TARGETARCH\nHARNESS antigravity FROM ./antigravity-${TARGETARCH}\n";
const AGY_HARNESS_JSON: &str = "{\"schemaVersion\":1,\"name\":\"antigravity\"}\n";

/// agy's binary for `arch`, as Google's archive holds it, in small.
fn agy(arch: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho 1.5.7 {arch}\n").into_bytes()
}

/// The URL path of agy's archive for `arch` on the test's server.
fn agy_path(arch: &str) -> String {
    format!("/{arch}/cli_linux_{arch}_musl.tar.gz")
}

/// One platform of agy's shape (kiln plan 01a1ce705e55753182217afdfe607de9, step 3), as the test's server
/// serves it and as agy's pin.sh pins Google's archive: `gz` by its own SHA-256 and size, unpacked from
/// its one entry, the member antigravity held alone, of `member`'s SHA-256 and size, to bin/agy at 0755.
fn agy_build(origin: &str, arch: &str, gz: Vec<u8>, member: &[u8]) -> (Vec<(String, Vec<u8>)>, Value) {
    let file = json!({ "url": format!("{origin}{}", agy_path(arch)), "sha256": sha(&gz), "size": gz.len(), "path": "bin/agy", "mode": "0755",
        "unpack": { "format": "tar.gz", "member": "antigravity", "alone": true, "sha256": sha(member), "size": member.len() } });
    (vec![(agy_path(arch), gz)], json!({ "platform": format!("linux/{arch}"), "files": [file] }))
}

/// agy's built-in for a test: the pin of `builds`.
fn antigravity(builds: Vec<Value>) -> Builtin {
    let pin = json!({ "version": "1.5.7", "verified": "by the test", "builds": builds }).to_string();
    Builtin { name: "harness-antigravity", harness: "antigravity", text: AGY_AGENTFILE, pin: pin.leak(), files: &[("harness.json", AGY_HARNESS_JSON)] }
}

fn both() -> [String; 2] {
    ["linux/amd64".to_owned(), "linux/arm64".to_owned()]
}

/// A context of agy's shape for linux/amd64 and linux/arm64 holds the Agentfile and antigravity-amd64/
/// and antigravity-arm64/, and nothing else: no staging directory, no archive, no temporary file. Each
/// holds harness.json and bin/agy, its own arch's bytes at the pin's 0755 (the archive's entry is 0644).
#[test]
fn step26_an_agy_shaped_context_holds_bin_agy_as_pinned() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let mut builds = Vec::new();
        let origin = serve(|o| {
            let mut routes = Vec::new();
            for arch in ["amd64", "arm64"] {
                let (r, b) = agy_build(o, arch, targz(&[file("antigravity", 0o644, &agy(arch))])?, &agy(arch));
                routes.extend(r);
                builds.push(b);
            }
            Ok(routes)
        })
        .await?;
        let tmp = tempfile::tempdir()?;
        let ctx = tmp.path().join("ctx");

        let made = materialize(&antigravity(builds), &both(), None, &ctx).await?;

        assert_eq!(made, [ctx.join("antigravity-amd64"), ctx.join("antigravity-arm64")], "from {origin}");
        assert_eq!(names(&ctx)?, set(["Agentfile", "antigravity-amd64", "antigravity-arm64"]));
        assert_eq!(std::fs::read_to_string(ctx.join("Agentfile"))?, AGY_AGENTFILE);
        for arch in ["amd64", "arm64"] {
            let d = ctx.join(format!("antigravity-{arch}"));
            assert_eq!(names(&d)?, set(["bin", "harness.json"]), "{arch}");
            assert_eq!(names(&d.join("bin"))?, set(["agy"]), "{arch}");
            assert_eq!(std::fs::read(d.join("bin/agy"))?, agy(arch), "{arch}");
            assert_eq!(std::fs::read_to_string(d.join("harness.json"))?, AGY_HARNESS_JSON, "{arch}");
            #[cfg(unix)]
            assert_eq!((mode(&d.join("bin/agy"))?, mode(&d.join("bin"))?), (0o755, 0o755), "{arch}");
        }
        Ok(())
    })
}

/// Each agy archive failing a check comes back as the unpacker's typed error naming the pinned URL, the
/// entry at fault and, where there are two, the values pinned and found: its member of another SHA-256,
/// or larger than pinned; no member; an entry beside the member; the member a symlink; a `../` path; a
/// corrupt gzip stream. Each is arm64's, after amd64's directory was made, and leaves nothing in the
/// context's directory: no harness directory, no bin/agy, no staging directory, no archive, no
/// Agentfile.
#[test]
fn step26_an_agy_archive_failing_a_check_leaves_nothing() -> anyhow::Result<()> {
    type Is = Box<dyn Fn(&Why) -> bool + Send>;
    let good = agy("arm64");
    let mut flipped = good.clone();
    if let Some(last) = flipped.last_mut() {
        *last ^= 1;
    }
    let shorter = good.get(..good.len().saturating_sub(1)).unwrap_or_default().to_vec();
    let one = targz(&[file("antigravity", 0o755, &good)])?;
    // A gzip header, then a deflate block of the reserved type (BTYPE 11): no gzip reader gets past it.
    let mut corrupt = one.clone();
    for b in corrupt.iter_mut().skip(10).take(16) {
        *b = 0xff;
    }
    let (want_digest, got_digest, len) = (sha(&flipped), sha(&good), good.len() as u64);
    let cases: Vec<(&str, Vec<u8>, Vec<u8>, Option<&str>, Is)> = vec![
        ("a member of another SHA-256", one.clone(), flipped, Some("antigravity"), Box::new(move |w: &Why| matches!(w, Why::Digest { expected, got } if *expected == want_digest && *got == got_digest))),
        ("a member larger than pinned", one.clone(), shorter, Some("antigravity"), Box::new(move |w: &Why| matches!(w, Why::Size { expected, got } if *expected == len - 1 && *got == len))),
        ("no member", targz(&[file("agy", 0o755, &good)])?, good.clone(), Some("antigravity"), Box::new(|w: &Why| matches!(w, Why::Missing))),
        ("an extra entry", targz(&[file("antigravity", 0o755, &good), file("README.md", 0o644, b"# agy\n")])?, good.clone(), Some("README.md"), Box::new(|w: &Why| matches!(w, Why::Extra { member } if member == "antigravity"))),
        ("the member a symlink", targz(&[link("antigravity", EntryType::Symlink, "/bin/sh")])?, good.clone(), Some("antigravity"), Box::new(|w: &Why| matches!(w, Why::Link))),
        ("a ../ path", targz(&[file("antigravity", 0o755, &good), file("../antigravity", 0o755, &good)])?, good.clone(), Some("../antigravity"), Box::new(|w: &Why| matches!(w, Why::Escapes))),
        ("a corrupt gzip stream", corrupt, good.clone(), None, Box::new(|w: &Why| matches!(w, Why::Io(_)))),
    ];
    for (what, gz, member, entry, is) in cases {
        crate::rt::block_on(async move {
            let mut builds = Vec::new();
            let origin = serve(|o| {
                let (mut routes, b) = agy_build(o, "amd64", targz(&[file("antigravity", 0o755, &agy("amd64"))])?, &agy("amd64"));
                builds.push(b);
                let (r, b) = agy_build(o, "arm64", gz, &member);
                routes.extend(r);
                builds.push(b);
                Ok(routes)
            })
            .await?;
            let b = antigravity(builds);
            let url = format!("{origin}{}", agy_path("arm64"));
            let tmp = tempfile::tempdir()?;
            let (absent, empty) = (tmp.path().join("absent"), tmp.path().join("empty"));
            std::fs::create_dir(&empty)?;
            for ctx in [&absent, &empty] {
                let refused = materialize(&b, &both(), None, ctx).await;
                let Err(Error::Unpack(u)) = refused else {
                    anyhow::bail!("{what}: {:?}, not the unpacker's refusal", refused.map(|m| m.len()));
                };
                assert_eq!((u.url.as_str(), u.entry.as_deref()), (url.as_str(), entry), "{what}: {u}");
                assert!(is(&u.why), "{what}: {:?}", u.why);
                let shown = u.to_string();
                assert!(shown.starts_with(&url) && entry.is_none_or(|e| shown.contains(e)), "{what}: {shown}");
                let left = if ctx.exists() { names(ctx)? } else { BTreeSet::new() };
                assert!(left.is_empty(), "{what}: left {left:?}");
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Codex's harness file and Agentfile for a test: its HARNESS line takes `./codex-${TARGETARCH}`.
const CODEX_AGENTFILE: &str = "FROM scratch\nARG TARGETARCH\nHARNESS codex FROM ./codex-${TARGETARCH}\n";
const CODEX_HARNESS_JSON: &str = "{\"schemaVersion\":1,\"name\":\"codex\"}\n";
/// The member Codex's arm64 pin takes from ripgrep's archive, under the directory that archive leads with.
const RG_MEMBER: &str = "ripgrep-15.2.0-aarch64-unknown-linux-musl/rg";
/// Each rg, in small: amd64's package's own (a static build, which stays), arm64's package's (a glibc
/// build, which its tree skips) and ripgrep's static build, the member that takes arm64's place.
const AMD64_RG: &[u8] = b"\x7fELF rg, the amd64 package's static build\n";
const GLIBC_RG: &[u8] = b"\x7fELF rg, the arm64 package's glibc build\n";
const STATIC_RG: &[u8] = b"\x7fELF rg, ripgrep's static aarch64 build\n";

/// OpenAI's package for `arch`, in small, as Codex's pin takes it whole at strip 0 (no leading
/// directory): bin/codex, bwrap in codex-resources/, the package's own `rg` at codex-path/rg, and its
/// layout file.
fn codex_package(arch: &str, rg: &[u8]) -> Vec<E> {
    vec![
        dir("bin/"),
        file("bin/codex", 0o755, format!("\x7fELF codex {arch}\n").as_bytes()),
        dir("codex-resources/"),
        file("codex-resources/bwrap", 0o755, b"\x7fELF bwrap\n"),
        dir("codex-path/"),
        file("codex-path/rg", 0o755, rg),
        file("codex-package.json", 0o644, b"{\"layoutVersion\":1}\n"),
    ]
}

/// One platform of Codex's shape, as the test's server serves it and as its pin names it: `package` a
/// tree at `.`, its totals the archive's own, with `skip` where it's given one; on arm64, ripgrep's
/// archive too (a README and its rg), whose member is written at codex-path/rg, 0755.
fn codex_build(origin: &str, arch: &str, package: &[E], skip: &[&str]) -> anyhow::Result<(Vec<(String, Vec<u8>)>, Value)> {
    let gz = targz(package)?;
    let (entries, bytes) = totals(package);
    let unpack = match skip {
        [] => json!({ "format": "tar.gz", "strip": 0, "unpacked_bytes": bytes, "entries": entries }),
        _ => json!({ "format": "tar.gz", "strip": 0, "unpacked_bytes": bytes, "entries": entries, "skip": skip }),
    };
    let mut files = vec![json!({ "url": format!("{origin}/{arch}/codex-package.tar.gz"), "sha256": sha(&gz), "size": gz.len(), "path": ".", "unpack": unpack })];
    let mut routes = vec![(format!("/{arch}/codex-package.tar.gz"), gz)];
    if arch == "arm64" {
        let lead = RG_MEMBER.rsplit_once('/').map_or(RG_MEMBER, |(lead, _)| lead);
        let rg = targz(&[dir(format!("{lead}/")), file(format!("{lead}/README.md"), 0o644, b"# ripgrep\n"), file(RG_MEMBER, 0o755, STATIC_RG)])?;
        files.push(json!({ "url": format!("{origin}/arm64/ripgrep.tar.gz"), "sha256": sha(&rg), "size": rg.len(), "path": "codex-path/rg", "mode": "0755",
            "unpack": { "format": "tar.gz", "member": RG_MEMBER, "sha256": sha(STATIC_RG), "size": STATIC_RG.len() } }));
        routes.push(("/arm64/ripgrep.tar.gz".to_owned(), rg));
    }
    Ok((routes, json!({ "platform": format!("linux/{arch}"), "files": files })))
}

/// Codex's built-in for a test: the pin of `builds`.
fn codex(builds: Vec<Value>) -> Builtin {
    let pin = json!({ "version": "0.167.2", "verified": "by the test", "builds": builds }).to_string();
    Builtin { name: "harness-codex", harness: "codex", text: CODEX_AGENTFILE, pin: pin.leak(), files: &[("harness.json", CODEX_HARNESS_JSON)] }
}

/// A context of Codex's shape for linux/amd64 and linux/arm64 holds the Agentfile and codex-amd64/ and
/// codex-arm64/, and nothing else: no staging directory, no archive. Each holds harness.json and its
/// package's tree at its root, its archived modes kept. amd64's codex-path/rg is its package's own;
/// arm64's package skips its glibc rg, and ripgrep's static rg, the member, is written there at 0755.
#[test]
fn step20_a_codex_shaped_context_holds_ripgreps_rg_in_place_of_arm64s() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let mut builds = Vec::new();
        let origin = serve(|o| {
            let (mut routes, amd64) = codex_build(o, "amd64", &codex_package("amd64", AMD64_RG), &[])?;
            let (arm, arm64) = codex_build(o, "arm64", &codex_package("arm64", GLIBC_RG), &["codex-path/rg"])?;
            routes.extend(arm);
            builds.extend([amd64, arm64]);
            Ok(routes)
        })
        .await?;
        let tmp = tempfile::tempdir()?;
        let ctx = tmp.path().join("ctx");

        let made = materialize(&codex(builds), &both(), None, &ctx).await?;

        assert_eq!(made, [ctx.join("codex-amd64"), ctx.join("codex-arm64")], "from {origin}");
        assert_eq!(names(&ctx)?, set(["Agentfile", "codex-amd64", "codex-arm64"]));
        assert_eq!(std::fs::read_to_string(ctx.join("Agentfile"))?, CODEX_AGENTFILE);
        for (arch, rg) in [("amd64", AMD64_RG), ("arm64", STATIC_RG)] {
            let d = ctx.join(format!("codex-{arch}"));
            assert_eq!(names(&d)?, set(["bin", "codex-package.json", "codex-path", "codex-resources", "harness.json"]), "{arch}");
            assert_eq!(names(&d.join("codex-path"))?, set(["rg"]), "{arch}");
            assert_eq!(std::fs::read(d.join("codex-path/rg"))?, rg, "{arch}");
            assert_eq!(std::fs::read(d.join("bin/codex"))?, format!("\x7fELF codex {arch}\n").into_bytes(), "{arch}");
            assert_eq!(std::fs::read_to_string(d.join("harness.json"))?, CODEX_HARNESS_JSON, "{arch}");
            #[cfg(unix)]
            {
                for (path, bits) in [("bin/codex", 0o755), ("codex-resources/bwrap", 0o755), ("codex-path", 0o755), ("codex-path/rg", 0o755), ("codex-package.json", 0o644)] {
                    assert_eq!(mode(&d.join(path))?, bits, "{arch}: {path}");
                }
            }
        }
        Ok(())
    })
}

/// Each way arm64's skip can fail comes back as the unpacker's typed error, naming the pinned URL and
/// the entry at fault: a skip path the package doesn't hold; a skipped path that's a directory, or a
/// symlink (a skipped entry is checked as every entry is, though never written); and no skip at all,
/// where ripgrep's rg finds the package's there already, since nothing is overwritten. Each is arm64's,
/// after amd64's directory was made, and leaves nothing in the context's directory.
#[test]
fn step20_a_codex_skip_that_fails_leaves_nothing() -> anyhow::Result<()> {
    type Is = fn(&Why) -> bool;
    let package = || codex_package("arm64", GLIBC_RG);
    let mut linked = package();
    linked.retain(|e| e.name != b"codex-path/rg");
    linked.push(link("codex-path/rg", EntryType::Symlink, "/usr/bin/rg"));
    // Each case: what it is, arm64's package, its skip, the archive refused, the entry it names, and why.
    let cases: [(&str, Vec<E>, &[&str], &str, &str, Is); 4] = [
        ("a skip path the package lacks", package(), &["codex-path/fd"], "codex-package", "codex-path/fd", |w| matches!(w, Why::Missing)),
        ("a skipped directory", package(), &["codex-path"], "codex-package", "codex-path/", |w| matches!(w, Why::Kind(b'5'))),
        ("a skipped symlink", linked, &["codex-path/rg"], "codex-package", "codex-path/rg", |w| matches!(w, Why::Link)),
        ("no skip", package(), &[], "ripgrep", RG_MEMBER, |w| matches!(w, Why::Exists(at) if at == Path::new("codex-path/rg"))),
    ];
    for (what, arm64, skip, fails, entry, is) in cases {
        crate::rt::block_on(async move {
            let mut builds = Vec::new();
            let origin = serve(|o| {
                let (mut routes, b) = codex_build(o, "amd64", &codex_package("amd64", AMD64_RG), &[])?;
                builds.push(b);
                let (r, b) = codex_build(o, "arm64", &arm64, skip)?;
                routes.extend(r);
                builds.push(b);
                Ok(routes)
            })
            .await?;
            let b = codex(builds);
            let url = format!("{origin}/arm64/{fails}.tar.gz");
            let tmp = tempfile::tempdir()?;
            let (absent, empty) = (tmp.path().join("absent"), tmp.path().join("empty"));
            std::fs::create_dir(&empty)?;
            for ctx in [&absent, &empty] {
                let refused = materialize(&b, &both(), None, ctx).await;
                let Err(Error::Unpack(u)) = refused else {
                    anyhow::bail!("{what}: {:?}, not the unpacker's refusal", refused.map(|m| m.len()));
                };
                assert_eq!((u.url.as_str(), u.entry.as_deref()), (url.as_str(), Some(entry)), "{what}: {u}");
                assert!(is(&u.why), "{what}: {:?}", u.why);
                let left = if ctx.exists() { names(ctx)? } else { BTreeSet::new() };
                assert!(left.is_empty(), "{what}: left {left:?}");
            }
            Ok(())
        })?;
    }
    Ok(())
}
