use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use serde_json::{Value, json};

use super::*;
use crate::agentfiles::builtin::ALL;

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A built-in for a test, of harness `claude`, with `pin` and `files`.
fn test_builtin(pin: String, files: &'static [(&'static str, &'static str)]) -> Builtin {
    Builtin { name: "harness-t", harness: "claude", text: "FROM scratch\n", pin: pin.leak(), files }
}

fn pin_of(builds: Value) -> String {
    json!({"version": "1.0.0", "base": "b@sha256:0000000000000000000000000000000000000000000000000000000000000000", "builds": builds}).to_string()
}

fn file(url: &str, body: &[u8], path: &str, mode: &str) -> Value {
    json!({"url": url, "sha256": sha256_hex(body), "size": body.len(), "path": path, "mode": mode})
}

fn ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
    r.extend_from_slice(body);
    r
}

/// A server on a thread of its own, answering each request by its path from what `routes` makes of the
/// server's base URL (404 for any other), for as long as the test runs. Returns the base URL.
fn serve(routes: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let routes = routes(&base);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match s.read(&mut byte) {
                    Ok(1) => {
                        let [b] = byte;
                        head.push(b);
                    }
                    _ => break,
                }
            }
            let line = String::from_utf8_lossy(&head);
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            let answer = routes.iter().find(|(p, _)| *p == path).map(|(_, a)| a.clone());
            let _ = s.write_all(&answer.unwrap_or_else(|| b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()));
        }
    });
    Ok(base)
}

/// What a folder holds, its own entries' names, sorted.
fn entries(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect::<Result<_, _>>()?;
    names.sort();
    Ok(names)
}

/// The harness's directory is named as its built-in's HARNESS line takes it for each build of its pin:
/// TARGETARCH, then the variant, put into the line's path.
#[test]
fn step19_harness_dirs_are_named_as_each_builtins_harness_line_takes_them() -> anyhow::Result<()> {
    anyhow::ensure!(harness_dir("claude", "linux/amd64", Some("gnu"))? == "claude-amd64-gnu");
    anyhow::ensure!(harness_dir("pi", "linux/arm64", None)? == "pi-arm64");
    anyhow::ensure!(harness_dir("x", "linux/arm64/v8", None)? == "x-arm64");
    for bad in ["darwin/arm64", "linux/", "linux", "linux/../x"] {
        anyhow::ensure!(harness_dir("x", bad, None).is_err(), "{bad} was taken");
    }
    for b in ALL {
        let line = b.text.lines().map(str::trim).find_map(|l| l.strip_prefix(&format!("HARNESS {} FROM ./", b.harness)).map(str::to_owned));
        let template = line.ok_or_else(|| anyhow::anyhow!("{}: no HARNESS line from a path", b.name))?;
        for build in &Pin::read(b.pin)?.builds {
            let arch = build.platform.split('/').nth(1).unwrap_or_default();
            let mut path = template.replace("${TARGETARCH}", arch);
            if let (Some(at), Some(v)) = (path.find("${"), build.variant.as_deref()) {
                let end = path.get(at..).and_then(|rest| rest.find('}')).map_or(path.len(), |e| at.saturating_add(e).saturating_add(1));
                path.replace_range(at..end, v);
            }
            let named = harness_dir(b.harness, &build.platform, build.variant.as_deref())?;
            anyhow::ensure!(path == named, "{}: its HARNESS line takes {path}, the fetcher makes {named}", b.name);
        }
    }
    Ok(())
}

/// A pin is read for its builds alone: its publisher's records, a file's other digests and an archive's
/// unpack, in the shapes steps 19 to 28 wrote them, don't stop it; a platform with two variants needs
/// one named; and every built-in's pin has each of its builds found.
#[test]
fn step19_pins_are_read_for_their_builds_alone() -> anyhow::Result<()> {
    let text = r#"{"version":"1.5.6","base":{"ref":"buildpack-deps:trixie-scm","digest":"sha256:4f34"},"verified":["a","b"],
        "builds":[{"platform":"linux/amd64","variant":"gnu","files":[{"url":"https://x/a","sha256":"aa","sha512":"bb","size":1,"path":"bin/a","mode":"0755","unpack":{"format":"tar.gz","member":"a","sha256":"cc","size":2}}]},
                  {"platform":"linux/amd64","variant":"musl","files":[{"url":"https://x/b","sha256":"dd","size":3,"path":".","unpack":{"format":"tar.gz","strip":1}}]}]}"#;
    let pin = Pin::read(text)?;
    anyhow::ensure!(pin.build("linux/amd64", Some("musl"))?.files.len() == 1);
    anyhow::ensure!(matches!(pin.build("linux/amd64", None), Err(Error::Platform { .. })), "two variants and none asked");
    match pin.build("linux/arm64", Some("gnu")) {
        Err(Error::Platform { asked, available }) => anyhow::ensure!(asked == "linux/arm64 gnu" && available == ["linux/amd64 gnu", "linux/amd64 musl"], "{asked} {available:?}"),
        other => anyhow::bail!("a missing platform: {other:?}"),
    }
    for bad in ["{", "{\"version\":\"1\"}", "{\"builds\":[{\"platform\":\"linux/amd64\"}]}"] {
        anyhow::ensure!(matches!(Pin::read(bad), Err(Error::Pin(_))), "{bad} was read");
    }
    for b in ALL {
        let pin = Pin::read(b.pin)?;
        for build in &pin.builds {
            pin.build(&build.platform, build.variant.as_deref())?;
        }
    }
    Ok(())
}

/// A build of a variant gets that variant's harness file as its harness.json, and no other variant's.
#[test]
fn step19_a_variants_build_gets_its_own_harness_file() -> anyhow::Result<()> {
    let claude = ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no claude built-in"))?;
    let text = |name: &str| claude.files.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).unwrap_or_default();
    anyhow::ensure!(files_for(claude, Some("gnu")) == [("harness.json", text("harness.json"))]);
    anyhow::ensure!(files_for(claude, Some("musl")) == [("harness.json", text("harness.musl.json"))]);
    anyhow::ensure!(files_for(claude, None) == [("harness.json", text("harness.json"))]);
    let one = test_builtin(pin_of(json!([])), &[("harness.json", "{}"), ("launch.sh", "#!/bin/sh\n")]);
    anyhow::ensure!(files_for(&one, Some("musl")) == [("harness.json", "{}"), ("launch.sh", "#!/bin/sh\n")]);
    Ok(())
}

/// A recipe is the same for the same build and changes with each thing a build takes: its Agentfile, the
/// variant's harness file, its pin, its base and its variant.
#[test]
fn step19_a_recipe_changes_exactly_with_what_a_build_takes() -> anyhow::Result<()> {
    let files: &'static [(&'static str, &'static str)] = &[("harness.json", "{\"a\":1}"), ("harness.musl.json", "{\"a\":2}")];
    let b = test_builtin(pin_of(json!([])), files);
    let base = "b@sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let r = recipe(&b, base, Some("gnu"));
    anyhow::ensure!(r.len() == 64 && r == recipe(&test_builtin(pin_of(json!([])), files), base, Some("gnu")));
    let other_text = Builtin { text: "FROM scratch\nARG X\n", ..test_builtin(pin_of(json!([])), files) };
    let other_file = test_builtin(pin_of(json!([])), &[("harness.json", "{\"a\":3}"), ("harness.musl.json", "{\"a\":2}")]);
    let other_pin = test_builtin(pin_of(json!([{"platform": "linux/amd64", "files": []}])), files);
    for (what, other) in [
        ("the Agentfile", recipe(&other_text, base, Some("gnu"))),
        ("the harness file", recipe(&other_file, base, Some("gnu"))),
        ("the pin", recipe(&other_pin, base, Some("gnu"))),
        ("the base", recipe(&b, "c@sha256:0000000000000000000000000000000000000000000000000000000000000000", Some("gnu"))),
        ("the variant", recipe(&b, base, Some("musl"))),
        ("no variant", recipe(&b, base, None)),
    ] {
        anyhow::ensure!(other != r, "{what} changed and the recipe didn't");
    }
    Ok(())
}

/// Modes are octal permissions with no set-ID or sticky bit, and paths stay inside the harness's directory.
#[test]
fn step19_modes_are_permissions_and_paths_stay_inside() -> anyhow::Result<()> {
    anyhow::ensure!(octal("0755")? == 0o755 && octal("0644")? == 0o644 && octal("755")? == 0o755);
    for bad in ["04755", "1777", "0800", "7", "x", "07555"] {
        anyhow::ensure!(octal(bad).is_err(), "{bad} was taken");
    }
    anyhow::ensure!(inside("bin/claude").is_ok() && inside("node/bin/node").is_ok());
    for bad in ["/etc/passwd", "../x", "bin/../../x", "a//b", "./a", "", "."] {
        anyhow::ensure!(inside(bad).is_err(), "{bad} was taken");
    }
    Ok(())
}

/// Light integration, against a server of the test's own: a context is written whole (through a redirect,
/// each file at its path with its mode, the harness file beside them, the Agentfile last); a wrong digest,
/// a short body, a long body, a 404, an archive and a second platform's failure each return their error and
/// leave nothing in the folder.
#[test]
fn step19_a_context_is_written_whole_or_not_at_all() -> anyhow::Result<()> {
    let claude: &[u8] = b"a claude binary, as pinned";
    let tool: &[u8] = b"a tool";
    let mut long = claude.to_vec();
    long.extend_from_slice(b" and then some");
    let mut wrong = claude.to_vec();
    if let Some(last) = wrong.last_mut() {
        *last ^= 1;
    }
    let short = claude.get(..5).unwrap_or_default().to_vec();
    let base = serve(|base| {
        vec![
            ("/claude".to_owned(), ok(claude)),
            ("/tool".to_owned(), ok(tool)),
            ("/redirect".to_owned(), format!("HTTP/1.1 302 Found\r\nlocation: {base}/tool\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()),
            ("/wrong".to_owned(), ok(&wrong)),
            ("/short".to_owned(), ok(&short)),
            ("/long".to_owned(), ok(&long)),
        ]
    })?;
    let at = move |p: &str| format!("{base}{p}");
    let good = json!([
        {"platform": "linux/amd64", "files": [file(&at("/claude"), claude, "bin/claude", "0755"), file(&at("/redirect"), tool, "share/tool", "0644")]},
        {"platform": "linux/arm64", "files": [file(&at("/claude"), claude, "bin/claude", "0755")]},
    ]);
    let platforms = ["linux/amd64".to_owned(), "linux/arm64".to_owned()];
    crate::rt::block_on(async move {
        let files: &'static [(&'static str, &'static str)] = &[("harness.json", "{\"schemaVersion\":1}")];
        let dir = tempfile::tempdir()?;
        let b = test_builtin(pin_of(good.clone()), files);
        let made = materialize(&b, &platforms, None, dir.path()).await?;
        anyhow::ensure!(made == [dir.path().join("claude-amd64"), dir.path().join("claude-arm64")], "{made:?}");
        anyhow::ensure!(entries(dir.path())? == ["Agentfile", "claude-amd64", "claude-arm64"], "{:?}", entries(dir.path())?);
        anyhow::ensure!(std::fs::read(dir.path().join("Agentfile"))? == b.text.as_bytes());
        let amd = dir.path().join("claude-amd64");
        anyhow::ensure!(std::fs::read(amd.join("bin/claude"))? == claude && std::fs::read(amd.join("share/tool"))? == tool);
        anyhow::ensure!(std::fs::read(amd.join("harness.json"))? == b"{\"schemaVersion\":1}");
        anyhow::ensure!(entries(&amd)? == ["bin", "harness.json", "share"], "{:?}", entries(&amd)?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o7777);
            anyhow::ensure!(mode(&amd.join("bin/claude"))? == 0o755 && mode(&amd.join("share/tool"))? == 0o644);
        }
        // A second run into the same folder replaces nothing.
        anyhow::ensure!(materialize(&b, &platforms, None, dir.path()).await.is_err(), "a context was written over another");

        let swap = |path: &str, to: Value| -> anyhow::Result<String> {
            let mut builds = good.clone();
            let f = builds.pointer_mut(path).ok_or_else(|| anyhow::anyhow!("{path}"))?;
            *f = to;
            Ok(pin_of(builds))
        };
        let cases = [
            ("a wrong digest", swap("/0/files/0", json!({"url": at("/wrong"), "sha256": sha256_hex(claude), "size": claude.len(), "path": "bin/claude", "mode": "0755"}))?),
            ("a short body", swap("/0/files/0", json!({"url": at("/short"), "sha256": sha256_hex(claude), "size": claude.len(), "path": "bin/claude", "mode": "0755"}))?),
            ("a long body", swap("/0/files/0", json!({"url": at("/long"), "sha256": sha256_hex(claude), "size": claude.len(), "path": "bin/claude", "mode": "0755"}))?),
            ("a 404", swap("/0/files/0", file(&at("/missing"), claude, "bin/claude", "0755"))?),
            ("an archive", swap("/0/files/0", json!({"url": at("/claude"), "sha256": sha256_hex(claude), "size": claude.len(), "path": ".", "unpack": {"format": "tar.gz"}}))?),
            ("the second platform's failure", swap("/1/files/0", file(&at("/missing"), claude, "bin/claude", "0755"))?),
        ];
        for (what, pin) in cases {
            let dir = tempfile::tempdir()?;
            let result = materialize(&test_builtin(pin, files), &platforms, None, dir.path()).await;
            let fits = match (&result, what) {
                (Err(Error::Digest { observed, .. }), "a wrong digest") => *observed == sha256_hex(&wrong),
                (Err(Error::Size { observed, .. }), "a short body") => *observed == 5,
                (Err(Error::Size { expected, observed, .. }), "a long body") => observed > expected,
                (Err(Error::Status { status: 404, .. }), "a 404" | "the second platform's failure") => true,
                (Err(Error::Unpack { .. }), "an archive") => true,
                _ => false,
            };
            anyhow::ensure!(fits, "{what}: {result:?}");
            anyhow::ensure!(entries(dir.path())?.is_empty(), "{what} left {:?}", entries(dir.path())?);
        }
        Ok(())
    })
}

/// A server answering `/f` with `body`: its first answer cut halfway (its content-length the whole, its connection
/// closed after half, as a network drops one); each later one asked from a byte (`Range: bytes=<n>-`) answered with
/// the rest (206 and its `Content-Range`) where it takes `ranges`, else whole (200). Each request's `Range`, in order.
fn cutting(body: Vec<u8>, ranges: bool) -> anyhow::Result<(String, std::sync::Arc<parking_lot::Mutex<Vec<Option<String>>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let asked = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&asked);
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut s) = stream else { continue };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match s.read(&mut byte) {
                    Ok(1) => {
                        let [b] = byte;
                        head.push(b);
                    }
                    _ => break,
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let range = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("range: ").map(|r| r.trim().to_owned()));
            kept.lock().push(range.clone());
            let from = range.as_deref().and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse::<usize>().ok());
            let answer = match (n, from) {
                (0, _) => {
                    let mut r = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
                    r.extend_from_slice(body.get(..body.len() / 2).unwrap_or_default());
                    r
                }
                (_, Some(from)) if ranges => {
                    let rest = body.get(from..).unwrap_or_default();
                    let mut r = format!("HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {from}-{}/{}\r\nconnection: close\r\n\r\n", rest.len(), body.len().saturating_sub(1), body.len()).into_bytes();
                    r.extend_from_slice(rest);
                    r
                }
                _ => ok(&body),
            };
            let _ = s.write_all(&answer);
        }
    });
    Ok((base, asked))
}

/// What the network drops of a pinned file's download is fetched again from where it got, and it arrives whole, its
/// pin's digest: its first answer cut mid-body, the next asked from that byte (`Range`) and given the rest (206);
/// or, where its server takes no ranges, given whole again (200) and taken from its start. Before, the cut ended the
/// build (`Error::Fetch`: "request body: connection closed", antigravity's image, Adas-Mac-Studio-3, 2026-11-14).
#[test]
fn a_dropped_download_is_fetched_again_from_where_it_got() -> anyhow::Result<()> {
    let body: Vec<u8> = (0..4096_u32).map(|i| u8::try_from(i % 251).unwrap_or_default()).collect();
    for ranges in [true, false] {
        let (base, asked) = cutting(body.clone(), ranges)?;
        let pin = pin_of(json!([{"platform": "linux/arm64", "files": [file(&format!("{base}/f"), &body, "bin/f", "0755")]}]));
        let fetched = crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let made = materialize(&test_builtin(pin, &[]), &["linux/arm64".to_owned()], None, dir.path()).await?;
            let at = made.first().ok_or_else(|| anyhow::anyhow!("nothing made"))?.join("bin/f");
            anyhow::Ok(std::fs::read(at)?)
        })?;
        anyhow::ensure!(fetched == body, "ranges {ranges}: it arrived as {} bytes, not its pin's", fetched.len());
        let asked = asked.lock().clone();
        anyhow::ensure!(asked == [None, Some("bytes=2048-".to_owned())], "ranges {ranges}: asked {asked:?}");
    }
    Ok(())
}

/// A fetch asks again as TCP sends again (RFC 6298): 1 s at first, doubled after each attempt that brought nothing,
/// at most 60 s, and 1 s again after one that brought bytes.
#[test]
fn a_refetch_backs_off_as_tcps_retransmission_timer_does() {
    let mut pause = Pause::default();
    let waits: Vec<u64> = (0..8).map(|_| pause.after(false).as_secs()).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
    assert_eq!(pause.after(true).as_secs(), 1, "an attempt that brought bytes starts it again");
}
