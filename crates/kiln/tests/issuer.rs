//! W10-R12 (docs/ci.md): kiln's OIDC issuer on an address of its own (`[ci.oidc] listen`),
//! serving its two documents and nothing else, each client rate-limited, so it can face the
//! internet without the rest of kiln; and Settings → OIDC's account of how it's reached.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{Options, free_port, http, start};
use serde_json::{Value, json};

#[test]
fn w10_r12_the_issuers_own_address() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let s = start(dir.path(), &Options { ci: &format!("[ci.oidc]\nlisten = [\"127.0.0.1:{port}\"]\n"), ..Options::default() })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let discovery = loop {
        if let Ok((200, body)) = http(port, "GET", "/oidc/.well-known/openid-configuration", "", None) {
            break serde_json::from_str::<Value>(&body)?;
        }
        anyhow::ensure!(Instant::now() < deadline, "the issuer's address never answered");
        std::thread::sleep(Duration::from_millis(200));
    };
    let issuer = discovery.get("issuer").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(issuer.ends_with("/oidc") && discovery.get("jwks_uri") == Some(&json!(format!("{issuer}/jwks"))), "{discovery}");
    let (st, body) = http(port, "GET", "/oidc/jwks", "", None)?;
    assert!(st == 200 && serde_json::from_str::<Value>(&body)?.pointer("/keys/0/kid").is_some(), "{st} {body}");

    // Nothing else of kiln's, whatever the token.
    for path in ["/api/ci/runs", "/", "/api/ci/oidc", "/git/acme/app.git/info/refs"] {
        let (st, body) = http(port, "GET", path, &s.admin, None)?;
        assert!(st == 404 && body.contains("only kiln's OIDC issuer is here"), "{path}: {st} {body}");
    }
    assert_eq!(http(port, "POST", "/oidc/jwks", "", Some(&json!({})))?.0, 405);

    // How it's reached, as Settings → OIDC says it: this address, and a URL the internet can't.
    let (st, body) = http(s.port, "GET", "/api/ci/oidc", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    assert!(st == 200 && v.pointer("/reach/listen/0") == Some(&json!(format!("127.0.0.1:{port}"))) && v.pointer("/reach/public") == Some(&json!(false)), "{st} {body}");

    // A client asking too fast is held back; the main address isn't.
    let limited = (0..40).filter(|_| http(port, "GET", "/oidc/jwks", "", None).is_ok_and(|(st, _)| st == 429)).count();
    assert!(limited > 0, "none of 40 quick requests was held back");
    assert_eq!(http(s.port, "GET", "/oidc/jwks", "", None)?.0, 200);
    Ok(())
}
