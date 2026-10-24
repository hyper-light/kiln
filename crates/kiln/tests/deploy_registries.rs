//! An app's registries on kiln's own (docs/deploy-api.md §3.1, step 5): an app made from a template
//! with `registry: { at: kiln }` gets its release, snapshot and chart repositories; a release
//! image's tag, once pushed, never points at another image nor goes; a snapshot's moves; and a
//! template that makes registries is put only by someone who may.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};

use base64::Engine as _;
use common::{Options, http, start_cd};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// One request to the registry: its status and body.
fn send(port: u16, method: &str, path: &str, auth: &str, content_type: &str, body: &[u8]) -> anyhow::Result<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {auth}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Ok((head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0), rest.to_owned()))
}

fn digest(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

/// An image of one layer, its config `config`: the manifest, after its blobs are pushed to `repo`.
fn image(port: u16, auth: &str, repo: &str, config: &[u8]) -> anyhow::Result<Vec<u8>> {
    let layer = b"a layer";
    for blob in [config, layer.as_slice()] {
        let (st, body) = send(port, "POST", &format!("/v2/{repo}/blobs/uploads/?digest={}", digest(blob)), auth, "application/octet-stream", blob)?;
        anyhow::ensure!(st == 201, "a blob into {repo}: {st} {body}");
    }
    Ok(serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(config), "size": config.len() },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest(layer), "size": layer.len() }],
    }))?)
}

#[test]
fn p5_registries_on_kiln_s_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start_cd(dir.path(), &Options::default())?;
    let template = |name: &str, registry: Value| json!({ "name": name, "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "{app}" }, "registry": registry });

    // Put only by someone who may (settings.manage), and only where kiln can make them.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer", "environments.manage"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "ada", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let ada = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &ada, Some(&template("svc", json!({ "at": "kiln" }))))?;
    assert!(st == 403 && body.contains("makes registries"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &s.admin, Some(&template("elsewhere", json!({ "at": "no-such-integration" }))))?;
    assert!(st == 400 && body.contains("no such integration"), "{st} {body}");

    // An app from a template with a registry on kiln's own: its three repositories.
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &s.admin, Some(&template("svc", json!({ "at": "kiln", "name": "{project}/{app}", "keep_snapshots": 2 }))))?;
    anyhow::ensure!((200..300).contains(&st), "the template: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&json!({ "name": "web", "project": "shop", "template": "svc" })))?;
    anyhow::ensure!(st == 201, "the app: {st} {body}\n{}", s.log());
    let app: Value = serde_json::from_str(&body)?;
    let reg = app.get("registries").cloned().unwrap_or_default();
    assert_eq!(reg.get("at"), Some(&json!("kiln")), "{app}");
    assert!(reg.get("images").and_then(Value::as_str).is_some_and(|r| r.ends_with("/shop/web") && r.starts_with("127.0.0.1:")), "{reg}");
    assert!(reg.get("snapshots").and_then(Value::as_str).is_some_and(|r| r.ends_with("/shop/web/snapshots")), "{reg}");
    assert!(reg.get("chart").and_then(Value::as_str).is_some_and(|r| r.starts_with("oci://127.0.0.1:") && r.ends_with("/charts/shop/web")), "{reg}");

    // A registry token, as docker gets one.
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{}", s.admin)));
    let (st, body) = send(s.port, "GET", "/v2/token?service=kiln&scope=repository:shop/web:pull,push,delete&scope=repository:shop/web/snapshots:pull,push,delete", &basic, "text/plain", b"")?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let auth = format!("Bearer {}", serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default());
    const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

    // A release image's tag: pushed once, the same image again, never another, never deleted.
    let (a, b) = (image(s.port, &auth, "shop/web", b"{\"v\":1}")?, image(s.port, &auth, "shop/web", b"{\"v\":2}")?);
    assert_eq!(send(s.port, "PUT", "/v2/shop/web/manifests/1.0.0", &auth, MANIFEST, &a)?.0, 201);
    assert_eq!(send(s.port, "PUT", "/v2/shop/web/manifests/1.0.0", &auth, MANIFEST, &a)?.0, 201, "the same image again");
    let (st, body) = send(s.port, "PUT", "/v2/shop/web/manifests/1.0.0", &auth, MANIFEST, &b)?;
    assert!(st == 409 && body.contains("never move"), "another image under a release's tag: {st} {body}");
    let (st, body) = send(s.port, "DELETE", "/v2/shop/web/manifests/1.0.0", &auth, MANIFEST, b"")?;
    assert!(st == 409 && body.contains("never go"), "{st} {body}");
    let (st, body) = send(s.port, "GET", "/v2/shop/web/manifests/1.0.0", &auth, MANIFEST, b"")?;
    assert!(st == 200 && body.contains(&digest(b"{\"v\":1}")), "still the first: {st} {body}");
    assert_eq!(send(s.port, "PUT", "/v2/shop/web/manifests/1.0.1", &auth, MANIFEST, &b)?.0, 201, "another tag is another release");

    // A snapshot's tag moves.
    let (a, b) = (image(s.port, &auth, "shop/web/snapshots", b"{\"v\":3}")?, image(s.port, &auth, "shop/web/snapshots", b"{\"v\":4}")?);
    assert_eq!(send(s.port, "PUT", "/v2/shop/web/snapshots/manifests/main", &auth, MANIFEST, &a)?.0, 201);
    assert_eq!(send(s.port, "PUT", "/v2/shop/web/snapshots/manifests/main", &auth, MANIFEST, &b)?.0, 201, "a snapshot's tag moves");
    Ok(())
}
