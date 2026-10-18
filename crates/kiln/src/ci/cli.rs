//! `kiln secrets` and `kiln vars` (docs/ci.md W09-R11), over the server's API. A scope is
//! written `server` (the default), `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>`
//! or `env:<repo>:<environment>`.

use std::io::Read as _;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::client::{Client, Method};

#[derive(clap::Args)]
pub struct SecretsArgs {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: SecretsCmd,
}

#[derive(clap::Subcommand)]
enum SecretsCmd {
    /// The secrets of a scope: names, sizes and last use, never values.
    List {
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Set a secret from a file, or from stdin (one trailing newline is dropped).
    Set {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
        #[arg(long)]
        from_file: Option<std::path::PathBuf>,
    },
    Delete {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Set every `NAME=value` of a dotenv file.
    Import {
        #[arg(long)]
        dotenv: std::path::PathBuf,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    /// Re-seal every secret under a new data key.
    RotateKey,
}

#[derive(clap::Args)]
pub struct VarsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: VarsCmd,
}

#[derive(clap::Subcommand)]
enum VarsCmd {
    List {
        #[arg(long, default_value = "server")]
        scope: String,
    },
    Set {
        name: String,
        value: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
    Delete {
        name: String,
        #[arg(long, default_value = "server")]
        scope: String,
    },
}

struct Api {
    client: Client,
    url: String,
    token: String,
}

impl Api {
    async fn connect(cfg: &Config, url: Option<&str>, token: Option<&str>) -> Result<Api> {
        let t = crate::access::resolve(cfg, url, token).await?;
        Ok(Api { client: Client::public()?, url: t.url, token: t.token })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.client.request(method, &format!("{}/api/ci{path}", self.url)).bearer(&self.token).timeout(Duration::from_secs(60));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes(16 << 20).await?;
        let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        if !(200..300).contains(&status) {
            bail!("{}", v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}")));
        }
        Ok(v)
    }
}

#[derive(clap::Args)]
pub struct OidcArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: OidcCmd,
}

#[derive(clap::Subcommand)]
enum OidcCmd {
    /// The issuer, its keys and which one signs.
    Status,
    /// The published key set (JWKS), for providers that take keys pasted in (Google Cloud, …).
    Jwks,
    /// Write the discovery document and keys to a directory for a static host (W10-R12): serve it
    /// at the issuer URL (`[ci.oidc] issuer`). Run it again within 7 days of each new key (`kiln
    /// oidc status`), or set `[ci.oidc] publish_dir` and the server keeps it written.
    Publish {
        #[arg(long)]
        to: std::path::PathBuf,
    },
    /// Make a new signing key: it signs in 7 days, or at once with `--now`.
    Rotate {
        #[arg(long)]
        now: bool,
    },
}

/// `kiln oidc`.
pub async fn oidc(args: OidcArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        OidcCmd::Status => {
            let v = api.call(Method::Get, "/oidc", None).await?;
            println!("issuer {}", v.get("issuer").and_then(Value::as_str).unwrap_or_default());
            for k in v.get("keys").and_then(Value::as_array).cloned().unwrap_or_default() {
                let flag = |n: &str| k.get(n).and_then(Value::as_bool).unwrap_or(false);
                println!("{}  signs from {}{}{}", k.get("kid").and_then(Value::as_str).unwrap_or_default(), when(k.get("sign_from").and_then(Value::as_i64)), if flag("signing") { "  signing" } else { "" }, if flag("published") { "  published" } else { "" });
            }
        }
        OidcCmd::Jwks => {
            let v = api.call(Method::Get, "/oidc/jwks-export", None).await?;
            println!("{}", serde_json::to_string_pretty(v.get("jwks").unwrap_or(&Value::Null))?);
        }
        OidcCmd::Publish { to } => {
            let v = api.call(Method::Get, "/oidc/jwks-export", None).await?;
            let (Some(discovery), Some(jwks)) = (v.get("discovery"), v.get("jwks")) else { bail!("the server sent no documents") };
            super::oidc::write_published(&to, discovery, jwks)?;
            println!("wrote the discovery document and keys to {}; serve it at {}", to.display(), v.get("issuer").and_then(Value::as_str).unwrap_or_default());
        }
        OidcCmd::Rotate { now } => {
            let v = api.call(Method::Post, "/oidc/rotate", Some(json!({ "now": now }))).await?;
            println!("a new signing key, signing from {}", when(v.get("sign_from").and_then(Value::as_i64)));
        }
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ActionsArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: ActionsCmd,
}

#[derive(clap::Subcommand)]
enum ActionsCmd {
    /// The mirrored actions whose name has QUERY, with their newest versions.
    List { query: Option<String> },
    /// Mirror `[ci.actions] prefetch` now, for a server that will run without reaching GitHub.
    Sync,
    /// The action policy: the server's, and a repository's with `--repo`. `--set` replaces the
    /// repository's with the flags given; `--clear` removes it.
    Policy {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        set: bool,
        #[arg(long)]
        clear: bool,
        /// `owner/repo[/path][@ref]` globs a run may use (repeatable).
        #[arg(long)]
        allow: Vec<String>,
        #[arg(long)]
        deny: Vec<String>,
        #[arg(long)]
        require_sha_pins: bool,
    },
}

/// `kiln actions`.
pub async fn actions(args: ActionsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        ActionsCmd::List { query } => {
            let v = api.call(Method::Get, &format!("/actions?q={}", enc(query.as_deref().unwrap_or_default())), None).await?;
            for a in v.get("actions").and_then(Value::as_array).cloned().unwrap_or_default() {
                let versions: Vec<String> = a.get("versions").and_then(Value::as_array).cloned().unwrap_or_default().iter().take(5).filter_map(|x| x.get("ref").and_then(Value::as_str).map(str::to_owned)).collect();
                let about = a.pointer("/action/description").and_then(Value::as_str).unwrap_or_default().lines().next().unwrap_or_default().to_owned();
                println!("{:<44} {:<28} {about}", a.get("uses").and_then(Value::as_str).unwrap_or_default(), versions.join(" "));
            }
        }
        ActionsCmd::Sync => {
            let v = api.call(Method::Post, "/actions/sync", Some(json!({}))).await?;
            let list = v.get("synced").and_then(Value::as_array).cloned().unwrap_or_default();
            if list.is_empty() {
                println!("nothing to mirror: `[ci.actions] prefetch` is empty");
            }
            for s in list {
                let uses = s.get("uses").and_then(Value::as_str).unwrap_or_default();
                match (s.get("sha").and_then(Value::as_str), s.get("error").and_then(Value::as_str)) {
                    (Some(sha), _) => println!("{uses} → {sha}"),
                    (_, e) => println!("{uses}: {}", e.unwrap_or("failed")),
                }
            }
        }
        ActionsCmd::Policy { repo, set, clear, allow, deny, require_sha_pins } => {
            let q = repo.as_deref().map(|r| format!("?repo={}", enc(r))).unwrap_or_default();
            let v = match (set, clear) {
                (true, true) => bail!("--set or --clear, not both"),
                (true, false) if repo.is_some() => api.call(Method::Put, &format!("/actions/policy{q}"), Some(json!({ "allow": allow, "deny": deny, "require_sha_pins": require_sha_pins }))).await?,
                (false, true) if repo.is_some() => api.call(Method::Delete, &format!("/actions/policy{q}"), None).await?,
                (false, false) => api.call(Method::Get, &format!("/actions/policy{q}"), None).await?,
                _ => bail!("the server's policy is `[ci.actions]` in its config; --set and --clear take a --repo"),
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// Percent-encodes a query value.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn when(t: Option<i64>) -> String {
    t.and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok()).and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_else(|| "never".into())
}

/// `NAME=value` lines of a dotenv file (comments, blank lines and `export` skipped; quotes removed).
fn dotenv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

/// `kiln secrets`.
pub async fn secrets(args: SecretsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        SecretsCmd::List { scope } => {
            let v = api.call(Method::Get, &format!("/secrets?scope={}", enc(&scope)), None).await?;
            for s in v.get("secrets").and_then(Value::as_array).cloned().unwrap_or_default() {
                let name = s.get("name").and_then(Value::as_str).unwrap_or_default();
                let size = s.get("size").and_then(Value::as_i64).unwrap_or(0);
                println!("{name:<32} {size:>6} B  updated {}  used {}", when(s.get("updated_at").and_then(Value::as_i64)), when(s.get("last_used_at").and_then(Value::as_i64)));
            }
        }
        SecretsCmd::Set { name, scope, from_file } => {
            let value = match from_file {
                Some(p) => std::fs::read_to_string(&p)?,
                None => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s.strip_suffix('\n').map(|x| x.strip_suffix('\r').unwrap_or(x)).unwrap_or(&s).to_owned()
                }
            };
            api.call(Method::Put, &format!("/secrets/{}?scope={}", enc(&name), enc(&scope)), Some(json!({ "value": value }))).await?;
            println!("set {} in {scope}", name.to_ascii_uppercase());
        }
        SecretsCmd::Delete { name, scope } => {
            api.call(Method::Delete, &format!("/secrets/{}?scope={}", enc(&name), enc(&scope)), None).await?;
            println!("deleted {} from {scope}", name.to_ascii_uppercase());
        }
        SecretsCmd::Import { dotenv: path, scope } => {
            let pairs = dotenv(&std::fs::read_to_string(&path)?);
            for (k, v) in &pairs {
                api.call(Method::Put, &format!("/secrets/{}?scope={}", enc(k), enc(&scope)), Some(json!({ "value": v }))).await?;
            }
            println!("set {} secrets in {scope}", pairs.len());
        }
        SecretsCmd::RotateKey => {
            let v = api.call(Method::Post, "/secrets/rotate", None).await?;
            println!("re-sealed {} secrets under a new data key", v.get("resealed").and_then(Value::as_u64).unwrap_or(0));
        }
    }
    Ok(())
}

/// `kiln vars`.
pub async fn vars(args: VarsArgs, cfg: &Config) -> Result<()> {
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        VarsCmd::List { scope } => {
            let v = api.call(Method::Get, &format!("/variables?scope={}", enc(&scope)), None).await?;
            for s in v.get("variables").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{}={}", s.get("name").and_then(Value::as_str).unwrap_or_default(), s.get("value").and_then(Value::as_str).unwrap_or_default());
            }
        }
        VarsCmd::Set { name, value, scope } => {
            api.call(Method::Put, &format!("/variables/{}?scope={}", enc(&name), enc(&scope)), Some(json!({ "value": value }))).await?;
            println!("set {} in {scope}", name.to_ascii_uppercase());
        }
        VarsCmd::Delete { name, scope } => {
            api.call(Method::Delete, &format!("/variables/{}?scope={}", enc(&name), enc(&scope)), None).await?;
            println!("deleted {} from {scope}", name.to_ascii_uppercase());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_files() {
        let got = dotenv("# c\n\nexport A=1\nB=\"two words\"\nC='x=y'\nbad line\n");
        assert_eq!(got, vec![("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "x=y".into())]);
        assert_eq!(enc("repo:acme/app"), "repo%3Aacme%2Fapp");
    }
}
