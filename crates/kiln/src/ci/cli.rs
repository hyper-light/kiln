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
