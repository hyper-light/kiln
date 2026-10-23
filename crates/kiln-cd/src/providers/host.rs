//! `host` (docs/cd-targets.md §3): a service on the hosts kiln's runners run on (systemd on Linux,
//! launchd on macOS, a Windows service), each release's files installed beside the ones before,
//! the hosts taken a batch at a time.
//!
//! ```yaml
//! web:
//!   host:
//!     service: shop-web
//!     files: "${{ release.artifacts.dist.digest }}"   # a .tar.gz or .zip CI built
//!     start: bin/server --port 8080                  # run from the release's directory
//!     env: { MODE: prod }
//!     check: http://127.0.0.1:8080/healthz           # a URL each host's service answers, or a command
//!     batch: 2                                       # hosts at a time (1)
//!     keep: 3                                        # releases kept on each host, to go back to (3)
//! ```
//!
//! - **Service** (`shop-web`): `spec` is what each host runs: the files, how it starts, its
//!   environment and its check. On each host the target's selector picks, a job of the runner there
//!   fetches the files (a link kiln signs for an hour) into a directory of their own, points
//!   `current` at it, installs the service (a user unit, a launch agent, a Windows service) and
//!   restarts it, then waits for its check. A batch starts once the one before passed, so a release
//!   that breaks stops at its first batch. A rollback switches each host back to its release's
//!   directory, still there, without fetching it again.
//! - **Health** is the newest run's: every host on the release, under way, or the host that failed.

use serde_json::{Map, Value, json};

use super::runner::{self, Job, Step, plain, pq, sq};
use super::{RenderInput, object, text};
use crate::health::Status;

pub const GROUP: &str = "host.kiln.dev";

/// The files a render reads: none, the files are CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// A runner the target's selector picked: its name and its OS (`linux`, `darwin`, `windows`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub os: String,
}

fn number(spec: &Value, key: &str, default: u64) -> Result<u64, String> {
    match spec.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).filter(|n| *n >= 1).ok_or_else(|| format!("{key}: a whole number, 1 or more")),
    }
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let service = text(input.spec, "service").ok_or("host needs service: the service's name")?;
    if service.len() > 60 || !service.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) || service.starts_with('.') {
        return Err(format!("service `{service}`: letters, digits, -, _ and ."));
    }
    let files = text(input.spec, "files").ok_or("host needs files: what it runs, a .tar.gz or .zip CI built (${{ release.artifacts.<name>.digest }})")?;
    let hex = files.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("files `{files}`: a .tar.gz or .zip CI built"))?;
    let start = text(input.spec, "start").ok_or("host needs start: the command the service runs, from the release's directory")?;
    plain("start", start)?;
    let mut spec = Map::new();
    spec.insert("files".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    spec.insert("start".into(), json!(start));
    spec.insert("env".into(), Value::Object(runner::text_map(input.spec, "env")?));
    if let Some(c) = text(input.spec, "check") {
        plain("check", c)?;
        spec.insert("check".into(), json!(c));
    }
    spec.insert("batch".into(), json!(number(input.spec, "batch", 1)?));
    spec.insert("keep".into(), json!(number(input.spec, "keep", 3)?));
    Ok(vec![object(GROUP, "Service", service, None, Value::Object(spec))])
}

/// The release's directory's name on each host: its files' hash, short.
fn release_dir(o: &Value) -> String {
    let a = o.pointer("/spec/files/artifact").and_then(Value::as_str).unwrap_or_default().trim_start_matches("sha256:");
    a.get(..16).unwrap_or(a).to_owned()
}

/// A unix host's script: the files into the release's directory, `current` at it, the service
/// (systemd's user unit on Linux, a launch agent on macOS) restarted on it, its check waited for.
fn unix_script(o: &Value, os: &str, url: &str) -> String {
    let svc = crate::obj::name(o);
    let s = |k: &str| o.pointer(&format!("/spec/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let keep = o.pointer("/spec/keep").and_then(Value::as_u64).unwrap_or(3);
    let env: Vec<(String, String)> = o.pointer("/spec/env").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect();
    let mut sh = format!(
        "set -eu\nD=\"$HOME/.kiln/services/\"{svc}\nR=\"$D/releases/{rel}\"\nif [ ! -d \"$R\" ]; then\n  rm -rf \"$R.part\" && mkdir -p \"$R.part\"\n  curl -fsSL {url} -o \"$R.part/.files\"\n  if head -c 2 \"$R.part/.files\" | od -An -tx1 | grep -q '50 4b'; then (cd \"$R.part\" && unzip -q .files); else tar -xzf \"$R.part/.files\" -C \"$R.part\"; fi\n  rm -f \"$R.part/.files\" && mv \"$R.part\" \"$R\"\nfi\nln -sfn \"$R\" \"$D/current\"\n",
        svc = sq(svc),
        rel = release_dir(o),
        url = sq(url),
    );
    let start = s("start");
    if os == "darwin" {
        let label = format!("dev.kiln.{svc}");
        let vars: String = env.iter().map(|(k, v)| format!("    <key>{}</key><string>{}</string>\n", xml(k), xml(v))).collect();
        sh.push_str(&format!(
            "P=\"$HOME/Library/LaunchAgents/{label}.plist\"\nmkdir -p \"$HOME/Library/LaunchAgents\"\ncat > \"$P\" <<'KILN_PLIST'\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>cd \"$HOME/.kiln/services/{svcx}/current\" &amp;&amp; exec {startx}</string></array>\n  <key>EnvironmentVariables</key><dict>\n{vars}  </dict>\n  <key>KeepAlive</key><true/>\n  <key>RunAtLoad</key><true/>\n</dict></plist>\nKILN_PLIST\nlaunchctl bootout \"gui/$(id -u)/{label}\" 2>/dev/null || true\nlaunchctl bootstrap \"gui/$(id -u)\" \"$P\"\nsleep 2\nlaunchctl print \"gui/$(id -u)/{label}\" | grep -q 'state = running' || {{ echo 'the service did not start' >&2; exit 1; }}\n",
            svcx = xml(svc),
            startx = xml(&start),
        ));
    } else {
        let unit = format!("kiln-{svc}.service");
        let vars: String = env.iter().map(|(k, v)| format!("Environment={}\n", systemd_quote(&format!("{k}={v}")))).collect();
        sh.push_str(&format!(
            "mkdir -p \"$HOME/.config/systemd/user\"\ncat > \"$HOME/.config/systemd/user/{unit}\" <<'KILN_UNIT'\n[Unit]\nDescription={svc} (kiln)\n[Service]\nWorkingDirectory=%h/.kiln/services/{svc}/current\nExecStart=/bin/sh -c {start}\n{vars}Restart=on-failure\n[Install]\nWantedBy=default.target\nKILN_UNIT\nsystemctl --user daemon-reload\nsystemctl --user enable {unit}\nsystemctl --user restart {unit}\nsleep 2\nsystemctl --user is-active --quiet {unit} || {{ journalctl --user -u {unit} -n 30 --no-pager || true; exit 1; }}\n",
            start = systemd_quote(&start),
        ));
    }
    if let Some(check) = o.pointer("/spec/check").and_then(Value::as_str) {
        let probe = if check.starts_with("http://") || check.starts_with("https://") { format!("curl -fsS -o /dev/null {}", sq(check)) } else { check.to_owned() };
        sh.push_str(&format!("ok=\nfor i in $(seq 1 30); do if {probe}; then ok=1; break; fi; sleep 2; done\n[ -n \"$ok\" ] || {{ echo 'the check never passed: '{chk} >&2; exit 1; }}\n", chk = sq(check)));
    }
    sh.push_str(&format!("ls -1dt \"$D\"/releases/*/ 2>/dev/null | tail -n +{} | xargs -r rm -rf\n", keep + 1));
    sh
}

/// A Windows host's script, in PowerShell: the same, as a Windows service (the runner runs
/// elevated, as installing a service needs).
fn windows_script(o: &Value, url: &str) -> String {
    let svc = crate::obj::name(o);
    let start = o.pointer("/spec/start").and_then(Value::as_str).unwrap_or_default();
    let keep = o.pointer("/spec/keep").and_then(Value::as_u64).unwrap_or(3);
    let mut ps = format!(
        "$ErrorActionPreference = 'Stop'\n$D = Join-Path $env:ProgramData ('kiln\\services\\' + {svc})\n$R = Join-Path $D ('releases\\' + {rel})\nif (-not (Test-Path $R)) {{\n  New-Item -ItemType Directory -Force -Path \"$R.part\" | Out-Null\n  Invoke-WebRequest -UseBasicParsing -Uri {url} -OutFile \"$R.part\\.files\"\n  $magic = [System.IO.File]::ReadAllBytes(\"$R.part\\.files\")[0..1]\n  if ($magic[0] -eq 0x50 -and $magic[1] -eq 0x4b) {{ Expand-Archive -Path \"$R.part\\.files\" -DestinationPath \"$R.part\" }} else {{ tar -xzf \"$R.part\\.files\" -C \"$R.part\" }}\n  Remove-Item \"$R.part\\.files\"\n  Move-Item \"$R.part\" $R\n}}\n$name = 'kiln-' + {svc}\n$bin = Join-Path $R {start}\nif (Get-Service -Name $name -ErrorAction SilentlyContinue) {{ Stop-Service -Name $name -Force; sc.exe config $name binPath= $bin | Out-Null }} else {{ New-Service -Name $name -BinaryPathName $bin -StartupType Automatic | Out-Null }}\n",
        svc = pq(svc),
        rel = pq(&release_dir(o)),
        url = pq(url),
        start = pq(start),
    );
    for (k, v) in o.pointer("/spec/env").and_then(Value::as_object).into_iter().flatten() {
        ps.push_str(&format!("[Environment]::SetEnvironmentVariable({}, {}, 'Machine')\n", pq(k), pq(v.as_str().unwrap_or_default())));
    }
    ps.push_str("Start-Service -Name $name\nStart-Sleep -Seconds 2\nif ((Get-Service -Name $name).Status -ne 'Running') { throw 'the service did not start' }\n");
    if let Some(check) = o.pointer("/spec/check").and_then(Value::as_str) {
        let probe = if check.starts_with("http") { format!("(Invoke-WebRequest -UseBasicParsing -Uri {} -TimeoutSec 5).StatusCode -lt 400", pq(check)) } else { format!("& {{ {check} }}; $?") };
        ps.push_str(&format!("$ok = $false\nforeach ($i in 1..30) {{ try {{ if ({probe}) {{ $ok = $true; break }} }} catch {{ }}; Start-Sleep -Seconds 2 }}\nif (-not $ok) {{ throw ('the check never passed: ' + {}) }}\n", pq(check)));
    }
    ps.push_str(&format!("Get-ChildItem (Join-Path $D 'releases') -Directory | Sort-Object LastWriteTime -Descending | Select-Object -Skip {keep} | Remove-Item -Recurse -Force\n"));
    ps
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A value as systemd reads one: double-quoted, `\`, `"` and `%` escaped.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

/// The workflow that rolls a Service out across `hosts`, a batch at a time: each host's job
/// fetches its files from `url`.
pub fn workflow(o: &Value, hosts: &[Host], url: &str) -> Result<String, String> {
    if hosts.is_empty() {
        return Err("no runner the target's selector picks is connected".into());
    }
    let svc = crate::obj::name(o);
    let batch = o.pointer("/spec/batch").and_then(Value::as_u64).unwrap_or(1) as usize;
    let mut jobs = Vec::new();
    let mut before: Vec<String> = Vec::new();
    for group in runner::batches(hosts, batch) {
        let mut ids = Vec::new();
        for h in group {
            let id = runner::job_id("host", &h.name);
            let (shell, script) = if h.os == "windows" { ("pwsh", windows_script(o, url)) } else { ("bash", unix_script(o, &h.os, url)) };
            jobs.push(Job { needs: before.clone(), checkout: false, step: Step::Run { shell, script }, timeout_minutes: 30, ..Job::script(id.clone(), format!("{svc} on {}", h.name), json!(h.name), String::new()) });
            ids.push(id);
        }
        before = ids;
    }
    Ok(runner::workflow(&format!("kiln: {svc} on its hosts"), &jobs))
}

pub fn health(o: &Value) -> Option<Status> {
    runner::health(o)
}
