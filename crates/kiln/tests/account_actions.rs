//! W07-AC01, W22-AC02 (docs/ci.md): F-21's actions that need a third-party account, run unmodified
//! on kiln in configurations that need none, each against a stand-in for its provider where the
//! action lets one take the provider's place:
//!
//! - `aws-actions/configure-aws-credentials@v4` and `amazon-ecr-login@v2`, the AWS SDK's own
//!   `AWS_ENDPOINT_URL_STS` and `AWS_ENDPOINT_URL_ECR` naming a stand-in for STS and ECR (and a
//!   registry for `docker login`): kiln's ID token for a role, then a registry login;
//! - `google-github-actions/auth@v2`, its `universe` a stand-in for Google's STS over TLS
//!   (`sts.localhost`, its certificate the job's `NODE_EXTRA_CA_CERTS`): kiln's ID token exchanged
//!   for a federated token and a credentials file;
//! - `codecov/codecov-action@v5` with `dry_run`, its `url` a stand-in for Codecov's API.
//!
//! `azure/login` has no such configuration: it signs in with the Azure CLI against Microsoft
//! Entra, so it waits for a test tenant (W11-AC03). The actions come from github.com, so this runs
//! only when github.com answers; the Google part only where `sts.localhost` is this machine's.

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Whether github.com answers here.
fn github() -> bool {
    Command::new("curl").args(["-fsSI", "--max-time", "10", "https://github.com/actions/checkout"]).output().is_ok_and(|o| o.status.success())
}

/// Whether `sts.localhost` is this machine (RFC 6761, as systemd-resolved answers it).
fn localhost_names() -> bool {
    Command::new("getent").args(["hosts", "sts.localhost"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("::1"))
}

/// A request as a stand-in saw it.
#[derive(Debug, Clone, Default)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> &str {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map_or("", |(_, v)| v.as_str())
    }

    fn form(&self, key: &str) -> String {
        self.body.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v.replace('+', " ")).map(|v| percent_decode(&v)).unwrap_or_default()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while let Some(&c) = b.get(i) {
        match (c, b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An HTTP stand-in: each request kept, each answered by `answer` (status, content type, body).
fn stand_in(seen: Arc<Mutex<Vec<Seen>>>, answer: impl Fn(&Seen) -> (u16, &'static str, String) + Send + 'static) -> anyhow::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            let req = loop {
                let Ok(n) = s.read(&mut chunk) else { break None };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(buf.get(..at).unwrap_or_default()).into_owned();
                let headers: Vec<(String, String)> = head.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
                let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
                if buf.len() >= at + 4 + len {
                    let mut words = head.split_whitespace();
                    let (method, path) = (words.next().unwrap_or_default().to_owned(), words.next().unwrap_or_default().to_owned());
                    let body = String::from_utf8_lossy(buf.get(at + 4..at + 4 + len).unwrap_or_default()).into_owned();
                    break Some(Seen { method, path, headers, body });
                }
            };
            let Some(req) = req else { continue };
            let (status, kind, body) = answer(&req);
            if let Ok(mut all) = seen.lock() {
                all.push(req);
            }
            let _ = s.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        }
    });
    Ok(port)
}

/// Where no Docker is (a CI pod), a stand-in for the `docker login` the ECR action runs: what a login
/// asks of the registry (its `/v2/`, with the credentials ECR gave), first on the `PATH` the
/// server's jobs run with. Where Docker is, it's Docker's own.
fn docker_path(dir: &Path) -> anyhow::Result<Option<String>> {
    if Command::new("sh").args(["-c", "command -v docker"]).output().is_ok_and(|o| o.status.success()) {
        return Ok(None);
    }
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let docker = bin.join("docker");
    std::fs::write(&docker, STAND_IN_DOCKER)?;
    std::fs::set_permissions(&docker, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755))?;
    Ok(Some(format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())))
}

/// `docker login` as a registry sees it: `GET /v2/` with the credentials given.
const STAND_IN_DOCKER: &str = r#"#!/bin/sh
[ "$1" = login ] || exit 0
shift
user= pass= server=
while [ $# -gt 0 ]; do
  case "$1" in
    -u|--username) user=$2; shift 2 ;;
    -p|--password) pass=$2; shift 2 ;;
    --password-stdin) pass=$(cat); shift ;;
    *) server=$1; shift ;;
  esac
done
curl -fsS -u "$user:$pass" "http://${server#*://}/v2/" >/dev/null && echo "Login Succeeded"
"#;

/// STS, ECR and a registry as AWS answers them: credentials for a web identity, the caller's
/// identity, an ECR authorization token naming this stand-in (the host it was asked at) as the
/// registry, and `/v2/` for `docker login`.
fn aws_answer(r: &Seen) -> (u16, &'static str, String) {
    let target = r.header("x-amz-target");
    if target.ends_with("GetAuthorizationToken") {
        let token = base64::engine::general_purpose::STANDARD.encode("AWS:kiln-ecr-password");
        let data = json!({ "authorizationData": [{ "authorizationToken": token, "expiresAt": 1_900_000_000, "proxyEndpoint": format!("http://{}", r.header("host")) }] });
        return (200, "application/x-amz-json-1.1", data.to_string());
    }
    if r.path.starts_with("/v2") {
        return (200, "application/json", "{}".into());
    }
    match r.form("Action").as_str() {
        "AssumeRoleWithWebIdentity" => (
            200,
            "text/xml",
            "<AssumeRoleWithWebIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIAKILNTEST</AccessKeyId><SecretAccessKey>kiln-secret</SecretAccessKey><SessionToken>kiln-session</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/kiln/session</Arn><AssumedRoleId>AROAKILN:session</AssumedRoleId></AssumedRoleUser><SubjectFromWebIdentityToken>repo:acme/app:ref:refs/heads/main</SubjectFromWebIdentityToken></AssumeRoleWithWebIdentityResult><ResponseMetadata><RequestId>kiln</RequestId></ResponseMetadata></AssumeRoleWithWebIdentityResponse>".into(),
        ),
        "GetCallerIdentity" => (
            200,
            "text/xml",
            "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>arn:aws:sts::123456789012:assumed-role/kiln/session</Arn><UserId>AROAKILN:session</UserId><Account>123456789012</Account></GetCallerIdentityResult><ResponseMetadata><RequestId>kiln</RequestId></ResponseMetadata></GetCallerIdentityResponse>".into(),
        ),
        other => (400, "text/xml", format!("<ErrorResponse><Error><Code>InvalidAction</Code><Message>{other}</Message></Error></ErrorResponse>")),
    }
}

/// Google's STS over TLS at `sts.localhost`: a federated token for a subject token, which it keeps
/// in `got`. Its certificate is signed by a CA of this test's own, in `ca`.
struct GoogleSts {
    child: Child,
    port: u16,
}

impl Drop for GoogleSts {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const GOOGLE_STS: &str = r#"
import http.server, json, socket, ssl, sys
cert, key, port_file, got = sys.argv[1:5]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0"))).decode()
        open(got, "a").write(body + "\n")
        try:
            asked = json.loads(body)
        except ValueError:
            asked = {}
        if self.path == "/v1/token" and asked.get("subjectToken"):
            out, status = json.dumps({"access_token": "ya29.kiln", "issued_token_type": "urn:ietf:params:oauth:token-type:access_token", "token_type": "Bearer", "expires_in": 3600}), 200
        else:
            out, status = json.dumps({"error": "invalid_request"}), 400
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out.encode())
    def log_message(self, *a):
        pass
class S(http.server.HTTPServer):
    address_family = socket.AF_INET6
srv = S(("::1", 0), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
open(port_file, "w").write(str(srv.server_address[1]))
srv.serve_forever()
"#;

fn google_sts(dir: &Path) -> anyhow::Result<GoogleSts> {
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "kiln test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key)?;
    let key = rcgen::KeyPair::generate()?;
    let cert = rcgen::CertificateParams::new(vec!["sts.localhost".to_owned()])?.signed_by(&key, &ca)?;
    std::fs::write(dir.join("ca.pem"), ca.pem())?;
    std::fs::write(dir.join("sts.pem"), cert.pem())?;
    std::fs::write(dir.join("sts.key"), key.serialize_pem())?;
    std::fs::write(dir.join("sts.py"), GOOGLE_STS)?;
    let port_file = dir.join("sts.port");
    let child = Command::new("python3")
        .arg("-I")
        .arg(dir.join("sts.py"))
        .args([dir.join("sts.pem"), dir.join("sts.key"), port_file.clone(), dir.join("sts.got")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut sts = GoogleSts { child, port: 0 };
    let deadline = Instant::now() + Duration::from_secs(20);
    while sts.port == 0 {
        anyhow::ensure!(Instant::now() < deadline, "Google's stand-in never listened");
        sts.port = std::fs::read_to_string(&port_file).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(sts)
}

/// A JWT's claims.
fn claims(jwt: &str) -> Value {
    jwt.split('.').nth(1).and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

fn workflow(aws: u16, codecov: u16, google: Option<(u16, &Path)>) -> String {
    let google = google.map_or_else(String::new, |(port, ca)| {
        format!(
            r#"  google:
    runs-on: local
    env:
      NODE_EXTRA_CA_CERTS: {}
    steps:
      - run: echo "a workspace" > here.txt
      - id: auth
        uses: google-github-actions/auth@v2
        with:
          workload_identity_provider: projects/123456789/locations/global/workloadIdentityPools/kiln/providers/kiln
          universe: localhost:{port}
          create_credentials_file: true
      - run: |
          test -f "${{{{ steps.auth.outputs.credentials_file_path }}}}"
          grep -q 'sts.localhost:{port}' "$GOOGLE_APPLICATION_CREDENTIALS"
          echo "google: credentials at ${{{{ steps.auth.outputs.credentials_file_path }}}}"
"#,
            ca.display()
        )
    });
    format!(
        r#"on: push
permissions:
  id-token: write
  contents: read
jobs:
  aws:
    runs-on: local
    env:
      AWS_ENDPOINT_URL_STS: http://127.0.0.1:{aws}
      AWS_ENDPOINT_URL_ECR: http://127.0.0.1:{aws}
    steps:
      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: arn:aws:iam::123456789012:role/kiln
          aws-region: us-east-1
      - id: ecr
        uses: aws-actions/amazon-ecr-login@v2
      - run: |
          test "$AWS_ACCESS_KEY_ID" = ASIAKILNTEST
          echo "aws: signed in as $AWS_ACCESS_KEY_ID, the registry ${{{{ steps.ecr.outputs.registry }}}}"
  codecov:
    runs-on: local
    steps:
      - run: |
          cat > coverage.xml <<'XML'
          <?xml version="1.0" ?>
          <coverage line-rate="1" branch-rate="1" version="1" timestamp="1792500000">
            <packages><package name="app" line-rate="1"><classes><class name="app" filename="app.py" line-rate="1"><lines><line number="1" hits="1"/></lines></class></classes></package></packages>
          </coverage>
          XML
          echo "print('app')" > app.py
      - uses: codecov/codecov-action@v5
        with:
          url: http://127.0.0.1:{codecov}
          dry_run: true
          fail_ci_if_error: true
          files: coverage.xml
          disable_search: true
          token: kiln-dry-run
{google}"#
    )
}

/// W07-AC01, W22-AC02: each passes, having asked its stand-in what it would ask its provider.
#[test]
fn w22_ac02_account_actions_in_dry_configurations() -> anyhow::Result<()> {
    if !github() {
        eprintln!("skipped: github.com doesn't answer here, and the actions come from it");
        return Ok(());
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let (aws_seen, codecov_seen) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    let aws = stand_in(Arc::clone(&aws_seen), aws_answer)?;
    let codecov = stand_in(Arc::clone(&codecov_seen), |r| {
        // The CLI's upload asks where to put the report, then puts it there.
        if r.method == "PUT" {
            return (200, "application/json", "{}".into());
        }
        (200, "application/json", json!({ "raw_upload_location": "http://127.0.0.1:1/storage", "url": "http://127.0.0.1:1/report" }).to_string())
    })?;
    let google = if localhost_names() { Some(google_sts(dir.path())?) } else { None };
    let path = docker_path(dir.path())?;
    let env: Vec<(&str, &str)> = path.as_deref().map(|p| ("PATH", p)).into_iter().collect();
    let s = start_env(dir.path(), &Options { agent: true, ..Options::default() }, false, &env)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let ca = dir.path().join("ca.pem");
    std::fs::write(work.join(".github/workflows/accounts.yml"), workflow(aws, codecov, google.as_ref().map(|g| (g.port, ca.as_path()))))?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the account actions"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 900, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut logs = std::collections::BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let mut log = String::new();
        for x in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
        }
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        logs.insert(key, (about.pointer("/leg/conclusion").cloned().unwrap_or_default(), log));
    }
    for (key, (conclusion, log)) in &logs {
        assert_eq!(conclusion, &json!("success"), "{key}:\n{log}\n{}", s.log());
    }
    // AWS: kiln's ID token for the role, and a registry login with what ECR gave.
    let asked = aws_seen.lock().map(|a| a.clone()).unwrap_or_default();
    let assume = asked.iter().find(|r| r.form("Action") == "AssumeRoleWithWebIdentity").cloned().unwrap_or_default();
    assert_eq!(assume.form("RoleArn"), "arn:aws:iam::123456789012:role/kiln", "{asked:?}");
    let token = claims(&assume.form("WebIdentityToken"));
    assert_eq!((token.get("aud"), token.get("sub")), (Some(&json!("sts.amazonaws.com")), Some(&json!("repo:acme/app:ref:refs/heads/main"))), "kiln's ID token: {token}");
    assert!(asked.iter().any(|r| r.header("x-amz-target").ends_with("GetAuthorizationToken")), "ECR asked: {asked:?}");
    // The key the step compared, masked in the log: the action says it's a secret.
    assert!(logs.get("aws").is_some_and(|(_, l)| l.contains("aws: signed in as ***") && l.contains("Logging into registry 127.0.0.1:")), "{logs:?}");
    // Codecov: its dry run, against the stand-in.
    assert!(logs.get("codecov").is_some_and(|(_, l)| l.contains("dry") || l.contains("Dry")), "{logs:?}");
    // Google: kiln's ID token for the pool's provider, exchanged; a credentials file made.
    if google.is_some() {
        let got = std::fs::read_to_string(dir.path().join("sts.got")).unwrap_or_default();
        // What it asked, as it sent it (JSON, over lines).
        let asked: Value = serde_json::Deserializer::from_str(&got).into_iter::<Value>().next().and_then(Result::ok).unwrap_or(Value::Null);
        let token = claims(asked.get("subjectToken").and_then(Value::as_str).unwrap_or_default());
        // Google's own audience for the provider, whatever the universe.
        assert_eq!(
            token.get("aud"),
            Some(&json!("https://iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/kiln/providers/kiln")),
            "kiln's ID token, for the pool's provider: {token}\n{got}"
        );
        assert!(logs.get("google").is_some_and(|(_, l)| l.contains("google: credentials at")), "{logs:?}");
    } else {
        eprintln!("google-github-actions/auth not run: sts.localhost isn't this machine here");
    }
    Ok(())
}
