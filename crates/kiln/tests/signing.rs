//! How kiln signs attestations (docs/ci.md W18-R02), each way end to end: a run's `kiln/attest@v1`
//! signed with cosign's own key file (made by cosign, and verified by it with the key's public
//! half), and keyless against a fake Fulcio and Rekor that check kiln's identity token and the
//! proof of possession, the bundle carrying the certificate and the log entry.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::{Options, http, start};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const COSIGN: &str = "ghcr.io/sigstore/cosign/cosign:v2.4.1";

fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// A server with `ci` (more of `[ci]`) and `extra`, a repository whose workflow attests a file the
/// job writes; the run's attestations, once it has them, and the server.
fn attested(dir: &std::path::Path, ci: &str, extra: &str) -> anyhow::Result<(common::Server, Vec<Value>, String)> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "signed\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("{extra}\n[[repos]]\nname = \"acme/sig\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir, &Options { agent: true, ci: &format!("kiln_workflows = true\n{ci}"), extra: &extra, terminal: false })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/sig") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        "name: sig\non: { push: {} }\njobs:\n  sig:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: printf 'the artifact\\n' > artifact.txt\n      - uses: kiln/attest@v1\n        with: { subject-path: artifact.txt }\n",
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "sig"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/sig&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(run.get("conclusion").and_then(Value::as_str) == Some("success"), "the run: {run}\n{}", s.log());
    let (_, body) = http(s.port, "GET", &format!("/api/ci/runs/{id}/attestations"), &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    let list = v.get("attestations").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok((s, list, id))
}

/// cosign's own key pair, made by cosign (its container): the key file and the public key.
fn cosign_keys(dir: &std::path::Path) -> anyhow::Result<Option<(std::path::PathBuf, std::path::PathBuf)>> {
    if !std::process::Command::new("docker").args(["image", "inspect", COSIGN]).output().is_ok_and(|o| o.status.success()) {
        return Ok(None);
    }
    let keys = dir.join("keys");
    std::fs::create_dir_all(&keys)?;
    let o = std::process::Command::new("docker")
        .args(["run", "--rm", "-e", "COSIGN_PASSWORD=hunter2", "-v"])
        .arg(format!("{}:/w", keys.display()))
        .args(["-w", "/w", "--user", &format!("{}:{}", uid("-u"), uid("-g")), COSIGN, "generate-key-pair"])
        .output()?;
    anyhow::ensure!(o.status.success(), "cosign generate-key-pair: {}", String::from_utf8_lossy(&o.stderr));
    std::fs::write(keys.join("password"), "hunter2\n")?;
    Ok(Some((keys.join("cosign.key"), keys.join("cosign.pub"))))
}

fn uid(flag: &str) -> String {
    std::process::Command::new("id").arg(flag).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_else(|_| "0".into())
}

#[test]
fn w18_r02_cosign_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((key, public)) = cosign_keys(dir.path())? else {
        eprintln!("no cosign image here: cosign's key format is checked by the unit test only");
        return Ok(());
    };
    let ci = format!("[ci.attest]\nsigning = \"key\"\nkey_file = \"{}\"\nkey_password_file = \"{}\"\n", key.display(), dir.path().join("keys/password").display());
    let (s, list, _) = attested(dir.path(), "", &ci)?;
    let bundle = list.iter().find(|a| a.get("predicate_type").and_then(Value::as_str) == Some("https://slsa.dev/provenance/v1")).and_then(|a| a.get("bundle")).cloned().ok_or_else(|| anyhow::anyhow!("no provenance: {list:?}"))?;
    // The served key is the key file's.
    let (_, served) = http(s.port, "GET", "/_kiln/attestation-key.pem", &s.admin, None)?;
    let normalize = |p: &str| p.lines().filter(|l| !l.starts_with("-----")).collect::<String>();
    assert_eq!(normalize(&served), normalize(&std::fs::read_to_string(&public)?), "the public key served is cosign.pub");
    // cosign verifies the provenance about the artifact with cosign.pub.
    let w = dir.path().join("verify");
    std::fs::create_dir_all(&w)?;
    std::fs::write(w.join("bundle.json"), bundle.to_string())?;
    std::fs::write(w.join("artifact.txt"), "the artifact\n")?;
    std::fs::copy(&public, w.join("cosign.pub"))?;
    std::fs::write(w.join("root.json"), "{\"mediaType\":\"application/vnd.dev.sigstore.trustedroot+json;version=0.1\"}")?;
    let o = std::process::Command::new("docker")
        .args(["run", "--rm", "-v"])
        .arg(format!("{}:/w", w.display()))
        .args([COSIGN, "verify-blob-attestation", "--key", "/w/cosign.pub", "--bundle", "/w/bundle.json", "--new-bundle-format", "--insecure-ignore-tlog", "--trusted-root", "/w/root.json", "--type", "slsaprovenance1", "/w/artifact.txt"])
        .output()?;
    assert!(o.status.success(), "cosign: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    // CD's `require-provenance` (W18-R04): the artifact's provenance meets a requirement of its
    // workflow and branch, and not one of another branch.
    let digest = format!("sha256:{}", hex::encode(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b"the artifact\n").as_ref()));
    let (st, body) = http(s.port, "POST", "/api/ci/attestations/verify", &s.admin, Some(&json!({ "digest": digest, "workflows": [".kiln/*.yaml"], "branches": ["main"] })))?;
    let v: Value = serde_json::from_str(&body)?;
    assert!(st == 200 && v.get("verified") == Some(&json!(true)), "{st} {body}");
    let (_, body) = http(s.port, "POST", "/api/ci/attestations/verify", &s.admin, Some(&json!({ "digest": digest, "branches": ["release/*"] })))?;
    let v: Value = serde_json::from_str(&body)?;
    assert!(v.get("verified") == Some(&json!(false)) && v.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("not release/*")), "{body}");
    let (_, body) = http(s.port, "POST", "/api/ci/attestations/verify", &s.admin, Some(&json!({ "digest": format!("sha256:{}", "0".repeat(64)) })))?;
    assert!(body.contains("\"verified\":false") && body.contains("no provenance"), "{body}");
    Ok(())
}

/// What the fake Fulcio and Rekor were sent.
type Sent = Arc<Mutex<Vec<(String, Value)>>>;

fn sigstore() -> anyhow::Result<(u16, Sent)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let sent: Sent = Arc::default();
    let seen = sent.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Ok(mut s) = seen.lock() {
                s.push((path.clone(), v));
            }
            let (status, reply) = match path.as_str() {
                "/fulcio/api/v2/signingCert" => (201, json!({ "signedCertificateEmbeddedSct": { "chain": { "certificates": ["-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n", "-----BEGIN CERTIFICATE-----\nMIIBroot\n-----END CERTIFICATE-----\n"] } } })),
                "/rekor/api/v1/log/entries" => (201, json!({ "24296fb24b8ad77a": { "body": "eyJ9", "integratedTime": 1792300000, "logID": "c0d23d6ad406973f", "logIndex": 42,
                    "verification": { "signedEntryTimestamp": "MEUC", "inclusionProof": { "logIndex": 42, "rootHash": "ab", "treeSize": 43, "hashes": ["cd"], "checkpoint": "rekor.example - 1\n43\n" } } } })),
                _ => (404, json!({})),
            };
            let text = reply.to_string();
            let mut s = reader.into_inner();
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
    });
    Ok((port, sent))
}

#[test]
fn w18_r02_keyless() -> anyhow::Result<()> {
    let (port, sent) = sigstore()?;
    let dir = tempfile::tempdir()?;
    let ci = format!("[ci.attest]\nsigning = \"keyless\"\nfulcio_url = \"http://127.0.0.1:{port}/fulcio\"\nrekor_url = \"http://127.0.0.1:{port}/rekor\"\n");
    let (s, list, _) = attested(dir.path(), "", &ci)?;
    let bundle = list.iter().find(|a| a.get("predicate_type").and_then(Value::as_str) == Some("https://slsa.dev/provenance/v1")).and_then(|a| a.get("bundle")).cloned().ok_or_else(|| anyhow::anyhow!("no provenance: {list:?}\n{}", s.log()))?;
    // The bundle: Fulcio's certificate, Rekor's entry, the envelope.
    assert_eq!(bundle.pointer("/verificationMaterial/certificate/rawBytes").and_then(Value::as_str), Some(B64.encode(B64.decode("MIIBfake").unwrap_or_default()).as_str()), "{bundle}");
    assert_eq!(bundle.pointer("/verificationMaterial/tlogEntries/0/logIndex").and_then(Value::as_str), Some("42"), "{bundle}");
    assert!(bundle.pointer("/dsseEnvelope/signatures/0/sig").is_some());
    // What Fulcio was asked: kiln's identity token for the build, and the key, proven.
    let calls = sent.lock().map(|c| c.clone()).unwrap_or_default();
    let ask = calls.iter().find(|(p, _)| p.ends_with("signingCert")).map(|(_, v)| v.clone()).ok_or_else(|| anyhow::anyhow!("Fulcio was never asked"))?;
    let token = ask.pointer("/credentials/oidcIdentityToken").and_then(Value::as_str).unwrap_or_default();
    let claims: Value = token.split('.').nth(1).and_then(|c| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(c).ok()).and_then(|c| serde_json::from_slice(&c).ok()).unwrap_or(Value::Null);
    assert_eq!(claims.get("aud").and_then(Value::as_str), Some("sigstore"), "{claims}");
    assert!(claims.get("sub").and_then(Value::as_str).is_some_and(|s| s.contains("acme/sig")), "{claims}");
    let pem = ask.pointer("/publicKeyRequest/publicKey/content").and_then(Value::as_str).unwrap_or_default();
    let der = B64.decode(pem.lines().filter(|l| !l.starts_with("-----")).collect::<String>())?;
    let proof = B64.decode(ask.pointer("/publicKeyRequest/proofOfPossession").and_then(Value::as_str).unwrap_or_default())?;
    let sub = claims.get("sub").and_then(Value::as_str).unwrap_or_default();
    // The SPKI's last 65 bytes are the uncompressed point.
    let point = der.get(der.len().saturating_sub(65)..).unwrap_or_default();
    let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1, point);
    assert!(key.verify(sub.as_bytes(), &proof).is_ok(), "the proof of possession signs the subject");
    // Rekor got the envelope with Fulcio's certificate as its verifier.
    let entry = calls.iter().find(|(p, _)| p.ends_with("entries")).map(|(_, v)| v.clone()).ok_or_else(|| anyhow::anyhow!("Rekor was never asked"))?;
    assert_eq!(entry.get("kind").and_then(Value::as_str), Some("dsse"));
    let verifier = entry.pointer("/spec/proposedContent/verifiers/0").and_then(Value::as_str).and_then(|v| B64.decode(v).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    assert!(verifier.contains("MIIBfake"), "{verifier}");
    // No key of kiln's to verify with: keyless.
    let (st, _) = http(s.port, "GET", "/_kiln/attestation-key.pem", &s.admin, None)?;
    assert_eq!(st, 404);
    Ok(())
}
