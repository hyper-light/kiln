//! W24-AC08 with the tools' real clients (docs/ci.md W24-R08): each builds a fixture twice
//! against a runner's cache service, the second build on a fresh machine's disk, so what it
//! doesn't rebuild came from the service. A client this machine lacks is skipped, saying so; the
//! protocols of the others are checked against their recorded requests in
//! `runner::work::tools` and `runner::work::gocacheprog`.
//!
//! - Go: `go` on `PATH`, with `GOCACHEPROG="kiln gocacheprog"`.
//! - Turborepo: `KILN_TEST_TURBO` (a `turbo` binary, e.g. npm's `turbo-linux-64/bin/turbo`), else
//!   `turbo` on `PATH`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kiln::runner::work::service::{Access, Service};

fn access(repo: &str) -> Access {
    Access { repo: repo.into(), domain: "trusted".into(), read: true, write: true, ttl: Duration::from_secs(3600) }
}

/// `f` on a thread of its own, awaited: a build run from a test mustn't take one of the blocking
/// pool's few places, which the cache service it talks to needs.
async fn on_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> anyhow::Result<R> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.map_err(|_| anyhow::anyhow!("the build's thread ended without an answer"))
}

fn write(root: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    for (path, text) in files {
        let p = root.join(path);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, text)?;
    }
    Ok(())
}

/// W24-AC08 (Go, the real client): `go build` twice, each with a fresh `GOCACHE` and a fresh
/// helper folder; the second compiles nothing, every package served from the cache service
/// through `kiln gocacheprog`.
#[test]
fn w24_ac08_go_builds_twice_the_second_from_the_cache() -> anyhow::Result<()> {
    let Some(go) = kiln::os::which("go") else {
        eprintln!("w24_ac08 (go): skipped, no go on this machine");
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let src = root.join("src");
    write(
        &src,
        &[
            ("go.mod", "module example.com/fixture\n\ngo 1.24\n"),
            ("lib/lib.go", "package lib\n\nimport \"strings\"\n\n// Shout says it louder.\nfunc Shout(s string) string { return strings.ToUpper(s) + \"!\" }\n"),
            ("main.go", "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/fixture/lib\"\n)\n\nfunc main() { fmt.Println(lib.Shout(\"kiln\")) }\n"),
        ],
    )?;
    kiln::rt::block_on(async move {
        let svc = Service::open(&root.join("runner"))?;
        let at = svc.listen_tcp(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let token = svc.mint(&access("acme/app"))?;
        let r = root.clone();
        let (first, second) = on_thread(move || -> anyhow::Result<(String, String)> {
            let build = |n: &str| -> anyhow::Result<String> {
                let out = Command::new(&go)
                    .args(["build", "-x", "-o", &format!("out{n}"), "."])
                    .current_dir(&src)
                    .env("GOCACHEPROG", format!("{} gocacheprog", env!("CARGO_BIN_EXE_kiln")))
                    .env("KILN_CACHE_URL", format!("http://{at}"))
                    .env("KILN_CACHE_TOKEN", &token)
                    .env("KILN_GOCACHEPROG_DIR", r.join(format!("disk{n}")))
                    .env("GOCACHE", r.join(format!("gocache{n}")))
                    .env("GOPATH", r.join("gopath"))
                    .env("GOENV", "off")
                    .env("GOFLAGS", "")
                    .env("GOTOOLCHAIN", "local")
                    .env("GOPROXY", "off")
                    .env("CGO_ENABLED", "0")
                    .output()?;
                let log = String::from_utf8_lossy(&out.stderr).into_owned();
                anyhow::ensure!(out.status.success(), "go build {n} failed: {log}");
                let ran = Command::new(src.join(format!("out{n}"))).output()?;
                anyhow::ensure!(String::from_utf8_lossy(&ran.stdout).trim() == "KILN!", "the build runs");
                Ok(log)
            };
            Ok((build("1")?, build("2")?))
        })
        .await??;
        let compiles = |log: &str| log.lines().filter(|l| l.contains("/compile -o") || l.contains("/compile\" -o")).count();
        let (cold, warm) = (compiles(&first), compiles(&second));
        println!("w24_ac08 (go): the first build compiled {cold} packages, the second {warm}");
        anyhow::ensure!(cold > 0, "the first build compiled: {first}");
        anyhow::ensure!(warm == 0, "the second build was served from the cache, compiling nothing: {second}");
        Ok(())
    })
}

/// W24-AC08 (Turborepo, the real client): `turbo run build` twice against the remote cache API,
/// the local cache and the outputs removed in between; the second replays the first's outputs
/// from the cache service.
#[test]
fn w24_ac08_turborepo_builds_twice_the_second_from_the_cache() -> anyhow::Result<()> {
    let turbo = std::env::var_os("KILN_TEST_TURBO").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| kiln::os::which("turbo"));
    let Some(turbo) = turbo else {
        eprintln!("w24_ac08 (turbo): skipped, no turbo on this machine (KILN_TEST_TURBO)");
        return Ok(());
    };
    if kiln::os::which("npm").is_none() {
        eprintln!("w24_ac08 (turbo): skipped, turbo runs its tasks with npm, which this machine lacks");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let repo = root.join("repo");
    write(
        &repo,
        &[
            ("package.json", r#"{"name":"fixture","private":true,"packageManager":"npm@10.9.2","workspaces":["packages/*"]}"#),
            (
                "package-lock.json",
                r#"{"name":"fixture","lockfileVersion":3,"requires":true,"packages":{"":{"name":"fixture","workspaces":["packages/*"]},"node_modules/a":{"resolved":"packages/a","link":true},"packages/a":{"name":"a","version":"1.0.0"}}}"#,
            ),
            ("turbo.json", r#"{"$schema":"https://turbo.build/schema.json","tasks":{"build":{"outputs":["dist/**"]}}}"#),
            ("packages/a/package.json", r#"{"name":"a","version":"1.0.0","scripts":{"build":"node build.js"}}"#),
            ("packages/a/build.js", "const fs = require('fs');\nfs.mkdirSync('dist', { recursive: true });\nfs.writeFileSync('dist/out.txt', 'built at ' + Date.now() + '\\n');\nconsole.log('building a');\n"),
            (".gitignore", "node_modules\n.turbo\ndist\n"),
        ],
    )?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()?
            .status
            .success();
        anyhow::ensure!(ok, "git {args:?}");
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-qm", "fixture"])?;
    kiln::rt::block_on(async move {
        let svc = Service::open(&root.join("runner"))?;
        let at = svc.listen_tcp(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let token = svc.mint(&access("acme/app"))?;
        let (first, second, out1, out2) = on_thread(move || -> anyhow::Result<(String, String, String, String)> {
            let run = || -> anyhow::Result<String> {
                let out = Command::new(&turbo)
                    .args(["run", "build", "--api", &format!("http://{at}"), "--token", &token, "--team", "kiln"])
                    .current_dir(&repo)
                    .env("TURBO_TELEMETRY_DISABLED", "1")
                    .env("TURBO_NO_UPDATE_NOTIFIER", "1")
                    .env("DO_NOT_TRACK", "1")
                    .env("TURBO_UI", "false")
                    .env("TURBO_PRINT_VERSION_DISABLED", "1")
                    .output()?;
                let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                anyhow::ensure!(out.status.success(), "turbo failed: {log}");
                Ok(log)
            };
            let first = run()?;
            let out1 = std::fs::read_to_string(repo.join("packages/a/dist/out.txt"))?;
            // Another machine's disk: no local cache, no outputs.
            for gone in [repo.join(".turbo"), repo.join("node_modules/.cache"), repo.join("packages/a/.turbo"), repo.join("packages/a/dist")] {
                let _ = std::fs::remove_dir_all(gone);
            }
            let second = run()?;
            let out2 = std::fs::read_to_string(repo.join("packages/a/dist/out.txt"))?;
            Ok((first, second, out1, out2))
        })
        .await??;
        println!("w24_ac08 (turbo): first run:\n{first}\nsecond run:\n{second}");
        anyhow::ensure!(first.contains("building a"), "the first run built: {first}");
        anyhow::ensure!(second.to_lowercase().contains("cache hit") && second.contains("FULL TURBO"), "the second run was served from the cache: {second}");
        anyhow::ensure!(out1 == out2, "the outputs came back as the first run made them ({out1:?} then {out2:?})");
        Ok(())
    })
}
