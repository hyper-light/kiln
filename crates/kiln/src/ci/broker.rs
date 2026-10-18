//! Brokered secrets (docs/ci.md W09-R08): a secret defined as coming from somewhere instead of a
//! stored value, made at leg start for the leg's workload and never stored.
//!
//! - `integration:<name>`: the integration's credential. A stored token as it is; AWS's
//!   short-lived credentials as `credential_process` JSON (`{"Version":1,"AccessKeyId":…}`);
//!   Google's and Azure's an access token (Azure's for `?scope=`, Azure Resource Manager by
//!   default).
//! - `aws-sm:<arn>`: an AWS Secrets Manager secret's string.
//! - `gcp-sm:projects/<p>/secrets/<s>[/versions/<v>]`: a Google Secret Manager version's payload
//!   (`latest` when none is named).
//! - `azure-kv:<vault>/<secret>`: an Azure Key Vault secret's value.
//!
//! The stores are read with the repository's integration of their cloud (`@<integration>` names
//! one), which exchanges kiln's identity for the leg (W11). Vault, Infisical, Doppler and
//! 1Password wait for their integration kinds.

use base64::Engine as _;
use serde_json::{Value, json};

use super::integrations::{self, Integration};
use crate::hub::Hub;
use crate::store::Repo;

/// Where a brokered secret comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Integration { name: String, scope: Option<String> },
    AwsSm { arn: String, via: Option<String> },
    GcpSm { name: String, via: Option<String> },
    AzureKv { vault: String, secret: String, via: Option<String> },
}

/// `x@integration` → (`x`, the integration).
fn via(s: &str) -> (String, Option<String>) {
    match s.rsplit_once('@') {
        Some((a, b)) if !b.is_empty() && !b.contains('/') && !b.contains(':') => (a.to_owned(), Some(b.to_owned())),
        _ => (s.to_owned(), None),
    }
}

/// A secret's `from`, read and checked.
pub fn parse(from: &str) -> Result<Source, String> {
    let from = from.trim();
    let (kind, rest) = from.split_once(':').ok_or_else(|| format!("`{from}`: say where it comes from, as `integration:<name>`, `aws-sm:<arn>`, `gcp-sm:projects/…/secrets/…`, or `azure-kv:<vault>/<secret>`"))?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("`{from}` names nothing"));
    }
    match kind {
        "integration" => {
            let (name, scope) = match rest.split_once("?scope=") {
                Some((n, s)) => (n.to_owned(), Some(s.to_owned())),
                None => (rest.to_owned(), None),
            };
            Ok(Source::Integration { name, scope })
        }
        "aws-sm" => {
            let (arn, via) = via(rest);
            if !arn.starts_with("arn:aws") || !arn.contains(":secretsmanager:") {
                return Err(format!("`{arn}` isn't a Secrets Manager secret's ARN (arn:aws:secretsmanager:<region>:<account>:secret:<name>)"));
            }
            Ok(Source::AwsSm { arn, via })
        }
        "gcp-sm" => {
            let (name, via) = via(rest);
            let parts: Vec<&str> = name.split('/').collect();
            let ok = matches!(parts.as_slice(), ["projects", _, "secrets", _] | ["projects", _, "secrets", _, "versions", _]);
            if !ok {
                return Err(format!("`{name}` isn't a Secret Manager secret (projects/<project>/secrets/<secret>[/versions/<version>])"));
            }
            let name = if parts.len() == 4 { format!("{name}/versions/latest") } else { name };
            Ok(Source::GcpSm { name, via })
        }
        "azure-kv" => {
            let (path, via) = via(rest);
            let Some((vault, secret)) = path.split_once('/').filter(|(v, s)| !v.is_empty() && !s.is_empty() && !s.contains('/')) else {
                return Err(format!("`{path}` isn't a Key Vault secret (<vault>/<secret>)"));
            };
            if !vault.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(format!("`{vault}` isn't a vault's name"));
            }
            Ok(Source::AzureKv { vault: vault.to_owned(), secret: secret.to_owned(), via })
        }
        "vault" | "infisical" | "doppler" | "1password" => Err(format!("`{kind}:` secrets need a {kind} integration, which this server doesn't have yet")),
        other => Err(format!("`{other}:` isn't a source kiln brokers (integration:, aws-sm:, gcp-sm:, azure-kv:)")),
    }
}

/// The integration a source reads through: the one it names, else the first of `kind` the
/// repository may use.
async fn integration_for(hub: &Hub, repo: &Repo, kind: &str, named: Option<&str>) -> anyhow::Result<Integration> {
    let all = integrations::list(hub).await?;
    let usable = |i: &&Integration| integrations::usable_by(i, repo);
    let found = match named {
        Some(n) => all.iter().filter(usable).find(|i| i.name == n),
        None => all.iter().filter(usable).find(|i| i.kind == kind),
    };
    let i = found.cloned().ok_or_else(|| anyhow::anyhow!("no {kind} integration {} may use{}", repo.name, named.map(|n| format!(" named `{n}`")).unwrap_or_default()))?;
    anyhow::ensure!(i.kind == kind || named.is_none() || kind.is_empty(), "`{}` is a {} integration, not {kind}", i.name, i.kind);
    Ok(i)
}

/// A brokered secret's value, for a leg of `repo` acting as `w`.
pub async fn value(hub: &Hub, repo: &Repo, w: &kiln_ci::oidc::Workload<'_>, src: &Source) -> anyhow::Result<String> {
    use super::integrations::{aws, azure, gcp};
    let http = crate::http::client::Client::public()?;
    match src {
        Source::Integration { name, scope } => {
            let i = integration_for(hub, repo, "", Some(name)).await?;
            match i.kind.as_str() {
                "aws" => {
                    let c = aws::credentials(hub, &i, w).await?;
                    let expires = c.expires_at.and_then(|e| time::OffsetDateTime::from_unix_timestamp(e).ok()).and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok());
                    Ok(json!({ "Version": 1, "AccessKeyId": c.access_key_id, "SecretAccessKey": c.secret_access_key, "SessionToken": c.session_token, "Expiration": expires }).to_string())
                }
                "gcp" => Ok(gcp::token(hub, &i, w).await?.access_token),
                "azure" => Ok(azure::token(hub, &i, w, scope.as_deref()).await?.access_token),
                _ => integrations::token(hub, &i).await?.ok_or_else(|| anyhow::anyhow!("`{name}` has no token stored")),
            }
        }
        Source::AwsSm { arn, via } => {
            let i = integration_for(hub, repo, "aws", via.as_deref()).await?;
            let region = arn.split(':').nth(3).filter(|r| !r.is_empty()).unwrap_or("us-east-1").to_owned();
            let creds = aws::credentials(hub, &i, w).await?;
            let a = aws::Aws { creds, region };
            let v = a.json("secretsmanager", "secretsmanager.GetSecretValue", &json!({ "SecretId": arn })).await?;
            if let Some(s) = v.get("SecretString").and_then(Value::as_str) {
                return Ok(s.to_owned());
            }
            let b = v.get("SecretBinary").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{arn} has no value"))?;
            Ok(String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(b)?).into_owned())
        }
        Source::GcpSm { name, via } => {
            let i = integration_for(hub, repo, "gcp", via.as_deref()).await?;
            let t = gcp::token(hub, &i, w).await?;
            let base = integrations::endpoint("gcp:secretmanager", || "https://secretmanager.googleapis.com".into());
            let r = http.get(&format!("{base}/v1/{name}:access")).bearer(&t.access_token).send().await?;
            let status = r.status();
            let v: Value = r.json(1 << 20).await.unwrap_or_default();
            anyhow::ensure!((200..300).contains(&status), "Secret Manager: {}", v.pointer("/error/message").and_then(Value::as_str).unwrap_or("refused"));
            let data = v.pointer("/payload/data").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{name} has no payload"))?;
            Ok(String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(data)?).into_owned())
        }
        Source::AzureKv { vault, secret, via } => {
            let i = integration_for(hub, repo, "azure", via.as_deref()).await?;
            let t = azure::token(hub, &i, w, Some("https://vault.azure.net/.default")).await?;
            let base = integrations::endpoint("azure:keyvault", || format!("https://{vault}.vault.azure.net"));
            let r = http.get(&format!("{base}/secrets/{secret}?api-version=7.4")).bearer(&t.access_token).send().await?;
            let status = r.status();
            let v: Value = r.json(1 << 20).await.unwrap_or_default();
            anyhow::ensure!((200..300).contains(&status), "Key Vault: {}", v.pointer("/error/message").and_then(Value::as_str).unwrap_or("refused"));
            v.get("value").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{vault}/{secret} has no value"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w09_r08_sources() {
        assert_eq!(parse("integration:aws-prod"), Ok(Source::Integration { name: "aws-prod".into(), scope: None }));
        assert_eq!(parse("integration:az?scope=https://vault.azure.net/.default"), Ok(Source::Integration { name: "az".into(), scope: Some("https://vault.azure.net/.default".into()) }));
        assert_eq!(parse("aws-sm:arn:aws:secretsmanager:eu-west-1:123:secret:npm-AbCd@aws-prod"), Ok(Source::AwsSm { arn: "arn:aws:secretsmanager:eu-west-1:123:secret:npm-AbCd".into(), via: Some("aws-prod".into()) }));
        assert!(parse("aws-sm:arn:aws:s3:::bucket").is_err());
        assert_eq!(parse("gcp-sm:projects/p/secrets/npm"), Ok(Source::GcpSm { name: "projects/p/secrets/npm/versions/latest".into(), via: None }));
        assert_eq!(parse("gcp-sm:projects/p/secrets/npm/versions/3"), Ok(Source::GcpSm { name: "projects/p/secrets/npm/versions/3".into(), via: None }));
        assert!(parse("gcp-sm:secrets/npm").is_err());
        assert_eq!(parse("azure-kv:kiln-vault/npm-token@az"), Ok(Source::AzureKv { vault: "kiln-vault".into(), secret: "npm-token".into(), via: Some("az".into()) }));
        assert!(parse("azure-kv:vault").is_err());
        assert!(parse("vault:kv/data/ci#npm").is_err_and(|e| e.contains("integration")));
        assert!(parse("nonsense").is_err() && parse("integration:").is_err());
    }
}
