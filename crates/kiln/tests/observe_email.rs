//! Monitors' email, live (docs/observe.md §10, §13): a kiln server as a process of its own, and
//! Mailpit (an SMTP server, in Docker) as the relay. What a user relies on: a monitor that alerts
//! and recovers is said by email to every recipient, through a relay that requires STARTTLS and a
//! login, its certificate checked against the CA given; the alert and its recovery one thread; a
//! relay whose certificate is not the CA's, or a wrong password, is not tried again and again but
//! said, in the relay's words; a relay on its own network, plain, takes mail too.
//!
//! It needs Docker: `cargo test -p kiln --test observe_email -- --ignored`.
#![cfg(unix)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "axllent/mailpit:v1.27";

/// A CA and a certificate it signs for `localhost`: (the CA's PEM, the leaf's, its key's).
fn certificates() -> anyhow::Result<(String, String, String)> {
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "kiln test mail CA");
    let ca_pem = ca.self_signed(&ca_key)?.pem();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, ca_key)?;
    let key = rcgen::KeyPair::generate()?;
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])?.signed_by(&key, &issuer)?;
    Ok((ca_pem, leaf.pem(), key.serialize_pem()))
}

/// Mailpit in a container: its SMTP and HTTP ports.
struct Mailpit {
    name: String,
    smtp: u16,
    http: u16,
}

impl Mailpit {
    fn start(env: &[(&str, &str)], certs: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let (smtp, http) = (free_port()?, free_port()?);
        let name = format!("kiln-mail-{}-{smtp}", std::process::id());
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--rm", "--name", &name, "-p", &format!("127.0.0.1:{smtp}:1025"), "-p", &format!("127.0.0.1:{http}:8025")]);
        if let Some(dir) = certs {
            c.args(["-v", &format!("{}:/certs:ro", dir.display())]);
        }
        for (k, v) in env {
            c.args(["-e", &format!("{k}={v}")]);
        }
        let out = c.arg(IMAGE).output()?;
        anyhow::ensure!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
        let m = Self { name, smtp, http };
        let start = Instant::now();
        while m.messages().is_err() {
            anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "mailpit did not come up: {}", m.logs());
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(m)
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        let (code, body) = common::http(self.http, "GET", path, "", None)?;
        anyhow::ensure!(code == 200, "{path}: {code} {body}");
        Ok(serde_json::from_str(&body)?)
    }

    fn messages(&self) -> anyhow::Result<Vec<Value>> {
        Ok(at(&self.get("/api/v1/messages")?, &["messages"]).as_array().cloned().unwrap_or_default())
    }

    /// Waits for a message whose subject starts with `subject`: its summary, whole, and headers.
    fn wait_for(&self, subject: &str) -> anyhow::Result<(Value, Value, Value)> {
        let start = Instant::now();
        loop {
            if let Some(m) = self.messages()?.into_iter().find(|m| at(m, &["Subject"]).as_str().is_some_and(|s| s.starts_with(subject))) {
                let id = at(&m, &["ID"]).as_str().unwrap_or("").to_owned();
                return Ok((m, self.get(&format!("/api/v1/message/{id}"))?, self.get(&format!("/api/v1/message/{id}/headers"))?));
            }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "no message {subject:?}: {:?}", self.messages()?.iter().map(|m| at(m, &["Subject"]).clone()).collect::<Vec<_>>());
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn logs(&self) -> String {
        Command::new("docker").args(["logs", "--tail", "30", &self.name]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_default()
    }
}

impl Drop for Mailpit {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

/// The delivery events of integration `name` whose result is `result`.
fn deliveries(f: &Fleet, name: &str, result: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": format!("source:kiln.event @event.type:monitor.notification @integration:{name} @result:{result}"), "from": from, "to": now_ms() + 60_000, "limit": 20 })))?;
    Ok(at(&found, &["records"]).as_array().cloned().unwrap_or_default())
}

#[test]
#[ignore = "needs Docker: runs Mailpit, and a kiln server as a process"]
fn a_monitor_s_alert_and_recovery_are_mailed_through_a_relay_that_asks_for_tls_and_a_login() -> anyhow::Result<()> {
    let t0 = now_ms();
    let (ca, cert, key) = certificates()?;
    let certs = tempfile::tempdir()?;
    std::fs::write(certs.path().join("cert.pem"), &cert)?;
    std::fs::write(certs.path().join("key.pem"), &key)?;
    // Mailpit reads its files as its own user.
    {
        use std::os::unix::fs::PermissionsExt as _;
        for f in ["cert.pem", "key.pem"] {
            std::fs::set_permissions(certs.path().join(f), std::fs::Permissions::from_mode(0o644))?;
        }
        std::fs::set_permissions(certs.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    let secure = Mailpit::start(&[("MP_SMTP_TLS_CERT", "/certs/cert.pem"), ("MP_SMTP_TLS_KEY", "/certs/key.pem"), ("MP_SMTP_REQUIRE_STARTTLS", "true"), ("MP_SMTP_AUTH", "kiln:s3cret-pass")], Some(certs.path()))?;
    let plain = Mailpit::start(&[], None)?;
    let f = Fleet::new()?;
    f.until(Duration::from_secs(30), "the server leads", |s| at(s, &["leader"]).as_u64() == Some(1))?;
    let relay = |port: u16, security: &str, password: &str, ca: &str| json!({ "kind": "email", "to": ["oncall@example.com", "sre@example.com"], "from": "kiln@alerts.example.com", "host": "localhost", "port": port, "security": security, "username": "kiln", "password": password, "ca": ca });
    for (name, body) in [
        ("ops", relay(secure.smtp, "starttls", "s3cret-pass", &ca)),
        ("badca", relay(secure.smtp, "starttls", "s3cret-pass", &certificates()?.0)),
        ("badpass", relay(secure.smtp, "starttls", "wrong", &ca)),
        ("lan", json!({ "kind": "email", "to": ["lan@example.com"], "from": "kiln@alerts.example.com", "host": "127.0.0.1", "port": plain.smtp, "security": "none" })),
    ] {
        let (code, b) = f.request("PUT", &format!("/api/observe/v1/integrations/{name}"), Some(&body))?;
        anyhow::ensure!(code == 204, "integration {name}: {code} {}", String::from_utf8_lossy(&b));
    }
    let listed = f.call("GET", "/api/observe/v1/integrations", None)?;
    anyhow::ensure!(!listed.to_string().contains("s3cret-pass") && !listed.to_string().contains("BEGIN CERTIFICATE"), "a password or CA shown: {listed}");
    // A header's injection, refused when kept.
    let (code, _) = f.request("PUT", "/api/observe/v1/integrations/evil", Some(&json!({ "kind": "email", "to": ["a@example.com\r\nBcc: all@example.com"], "from": "kiln@alerts.example.com", "host": "localhost" })))?;
    anyhow::ensure!(code == 400, "an address with a line break: {code}");

    // A monitor that alerts and recovers, said to `ops`: two messages, one thread.
    let m = f.call(
        "POST",
        "/api/observe/v1/monitors",
        Some(&json!({
            "name": "mail temp <high>",
            "query": "last(last_1m):max:e2e.mail.temp{*} by {host} > 100",
            "message": "{{#is_alert}}temp on {{host.name}} is {{value}} & rising @email-ops{{/is_alert}}{{#is_recovery}}{{host.name}} recovered @email-ops{{/is_recovery}}",
            "options": { "every": 10, "thresholds": { "critical": 100 } },
        })),
    )?;
    let id = at(&m, &["id"]).as_str().unwrap_or("").to_owned();
    f.write("e2e.mail.temp", &["a"], now_ms() - 50_000, 6, |_, _| 150.0)?;
    let (summary, whole, headers) = secure.wait_for("[Triggered] mail temp <high> on host:a")?;
    let to: Vec<&str> = at(&summary, &["To"]).as_array().into_iter().flatten().filter_map(|t| at(t, &["Address"]).as_str()).collect();
    anyhow::ensure!(to == ["oncall@example.com", "sre@example.com"] && at(&summary, &["From", "Address"]) == "kiln@alerts.example.com", "{summary}");
    let text = at(&whole, &["Text"]).as_str().unwrap_or("");
    let html = at(&whole, &["HTML"]).as_str().unwrap_or("");
    anyhow::ensure!(text.contains("temp on a is 150 & rising") && !text.contains("@email-ops") && text.contains("Threshold: 100"), "text: {text}");
    anyhow::ensure!(html.contains("mail temp &lt;high&gt;") && html.contains("temp on a is 150 &amp; rising") && !html.contains("<high>"), "html: {html}");
    let header = |h: &Value, k: &str| at(h, &[k]).as_array().and_then(|v| v.first()).and_then(Value::as_str).unwrap_or("").to_owned();
    let thread = header(&headers, "References");
    anyhow::ensure!(header(&headers, "Auto-Submitted") == "auto-generated" && thread.starts_with(&format!("<kiln-{id}-")) && thread.ends_with("@alerts.example.com>"), "headers: {headers}");
    f.write("e2e.mail.temp", &["a"], now_ms() - 50_000, 6, |_, _| 50.0)?;
    let (_, recovered, rheaders) = secure.wait_for("[Recovered] mail temp <high> on host:a")?;
    anyhow::ensure!(at(&recovered, &["Text"]).as_str().is_some_and(|t| t.contains("a recovered")) && header(&rheaders, "References") == thread, "the recovery, in the alert's thread: {rheaders}");
    // Each delivery an event (written once the relay has said 250: searchable a moment later).
    let start = Instant::now();
    while deliveries(&f, "ops", "delivered", t0)?.len() < 2 {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(20), "two deliveries recorded: {:?}", deliveries(&f, "ops", "delivered", t0)?);
        std::thread::sleep(Duration::from_millis(250));
    }
    let said = deliveries(&f, "ops", "delivered", t0)?;
    anyhow::ensure!(said.len() == 2 && said.iter().all(|e| at(e, &["message"]).as_str().is_some_and(|m| m.contains(": delivered, 250"))), "{said:?}");

    // A CA that is not the relay's, a wrong password: said once each, not tried five times.
    for (name, why) in [("badca", "TLS"), ("badpass", "535")] {
        let started = Instant::now();
        f.call("POST", &format!("/api/observe/v1/integrations/{name}/test"), Some(&json!({})))?;
        let failed = loop {
            let d = deliveries(&f, name, "failed", t0)?;
            if !d.is_empty() {
                break d;
            }
            anyhow::ensure!(started.elapsed() < Duration::from_secs(30), "{name}: no failed delivery\n{}", f.logs());
            std::thread::sleep(Duration::from_millis(250));
        };
        let said = failed.first().map(|e| at(e, &["message"]).as_str().unwrap_or("").to_owned()).unwrap_or_default();
        anyhow::ensure!(said.contains(why), "{name}: {said}");
        anyhow::ensure!(started.elapsed() < Duration::from_secs(10), "{name} was tried again: {:?}", started.elapsed());
    }
    // A relay on its own network, plain, no login.
    f.call("POST", "/api/observe/v1/integrations/lan/test", Some(&json!({})))?;
    let (summary, _, _) = plain.wait_for("[Triggered] A test from kiln")?;
    let to = at(&summary, &["To"]).as_array().and_then(|t| t.first()).map(|t| at(t, &["Address"]).clone()).unwrap_or_default();
    anyhow::ensure!(to == "lan@example.com", "{summary}");
    Ok(())
}
