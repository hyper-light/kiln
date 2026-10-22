//! kiln's base chart as an OCI chart (docs/deploy-api.md §3.4): a server publishes the kiln-base it
//! ships in its own registry (`kiln/charts/kiln-base`, at its version, with Helm's media types),
//! anyone pulls it and nobody pushes over it; `kiln app init --url` writes an app chart that
//! depends on it there, and helm builds that dependency from the server, lints the chart and
//! renders it (where helm is installed).

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::Value;

/// A GET without credentials: its status and body.
fn anonymous(port: u16, path: &str) -> anyhow::Result<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/vnd.oci.image.manifest.v1+json\r\nConnection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body))
}

/// kiln-base's version, as the chart in kiln's source says.
fn version() -> String {
    include_str!("../assets/charts/kiln-base/Chart.yaml").lines().find_map(|l| l.strip_prefix("version:")).map(|v| v.trim().to_owned()).unwrap_or_default()
}

fn have(bin: &str) -> bool {
    Command::new(bin).arg("version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn p5_kiln_base_as_an_oci_chart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let version = version();

    // Published at start, and anyone pulls it.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, body) = anonymous(s.port, "/v2/kiln/charts/kiln-base/tags/list")?;
        if st == 200 && body.contains(&format!("\"{version}\"")) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "kiln-base {version} isn't published: {st} {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let (st, body) = anonymous(s.port, &format!("/v2/kiln/charts/kiln-base/manifests/{version}"))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let manifest: Value = serde_json::from_str(&body)?;
    assert_eq!(manifest.pointer("/config/mediaType").and_then(Value::as_str), Some("application/vnd.cncf.helm.config.v1+json"), "{manifest}");
    assert_eq!(manifest.pointer("/layers/0/mediaType").and_then(Value::as_str), Some("application/vnd.cncf.helm.chart.content.v1.tar+gzip"), "{manifest}");
    // Nobody pushes over it, the server's own administrator neither.
    let (st, body) = http(s.port, "PUT", &format!("/v2/kiln/charts/kiln-base/manifests/{version}"), &s.admin, Some(&manifest))?;
    assert!(st == 401 || st == 403, "{st} {body}");
    let (st, body) = http(s.port, "DELETE", &format!("/v2/kiln/charts/kiln-base/manifests/{version}"), &s.admin, None)?;
    assert!(st == 401 || st == 403, "{st} {body}");
    let (st, _) = anonymous(s.port, &format!("/v2/kiln/charts/kiln-base/manifests/{version}"))?;
    assert_eq!(st, 200, "still there");

    // An app's chart that depends on it there.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    let out = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["app", "init", "web", "--image", "ghcr.io/acme/web"])
        .arg("--dir")
        .arg(&repo)
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("RUST_MIN_STACK", "8388608")
        .output()?;
    anyhow::ensure!(out.status.success(), "kiln app init: {}", String::from_utf8_lossy(&out.stderr));
    let chart = std::fs::read_to_string(repo.join("charts/web/Chart.yaml"))?;
    assert!(chart.contains(&format!("repository: oci://127.0.0.1:{}/kiln/charts", s.port)) && chart.contains(&format!("version: {version}")), "{chart}");
    assert!(!repo.join("charts/web/charts").exists(), "nothing vendored");

    // helm fetches it from the server, and the chart lints and renders.
    if have("helm") {
        let homes = dir.path().join("helm");
        let helm = |args: &[&str], at: &Path| -> anyhow::Result<String> {
            let o = Command::new("helm")
                .args(args)
                .arg(at)
                .env("HELM_CACHE_HOME", homes.join("cache"))
                .env("HELM_CONFIG_HOME", homes.join("config"))
                .env("HELM_DATA_HOME", homes.join("data"))
                .output()?;
            anyhow::ensure!(o.status.success(), "helm {args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let web = repo.join("charts/web");
        helm(&["dependency", "build", "--plain-http"], &web)?;
        assert!(web.join(format!("charts/kiln-base-{version}.tgz")).is_file(), "the dependency is in charts/");
        helm(&["lint"], &web)?;
        let rendered = helm(&["template", "web"], &web)?;
        assert!(rendered.contains("kind: Deployment") && rendered.contains("image: ghcr.io/acme/web:0.1.0"), "{rendered}");
        // Where Argo CD deploys it, a release says its secrets' versions (`kiln.secrets`), and
        // the pods are marked with them, so new ones roll them (§3.5).
        assert!(!rendered.contains("kiln.dev/secrets"), "{rendered}");
        let with = helm(&["template", "web", "--set", "kiln.secrets=DB_PASSWORD@1 API_KEY@2"], &web)?;
        assert!(with.contains("kiln.dev/secrets: \"DB_PASSWORD@1 API_KEY@2\""), "{with}");
        eprintln!("helm built kiln-base {version} from the server, and linted and rendered the app's chart");
    } else {
        eprintln!("no helm here: the chart wasn't built, linted or rendered");
    }
    Ok(())
}
