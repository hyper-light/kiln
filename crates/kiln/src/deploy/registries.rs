//! An app's registries (docs/deploy-api.md §3.1, step 5): the repositories its images and chart are
//! pushed to, made with it on the registry its template names, `name` from its template
//! (`{project}/{app}`):
//! - **release images** at `<name>`, their tags immutable: a release's image is the image it was;
//! - **snapshot images** at `<name>/snapshots` (prereleases), tags that move, the newest
//!   `keep_snapshots` kept;
//! - **its chart** at `charts/<name>`, versions immutable.
//!
//! On kiln's own registry they're its repositories' policies; on ECR (an `aws` integration) its
//! repositories' tag mutability and lifecycle policies (untagged images go after a week, too); on
//! Artifact Registry (a `gcp` integration) its repositories' immutable tags and cleanup policies,
//! one repository each (Artifact Registry's repositories hold images). GHCR, ACR and Docker Hub
//! make a repository at its first push, with none of these rules: kiln makes nothing there.
//!
//! Made again, what's there already is kept (and its rules set again).

use http::StatusCode;
use serde_json::{Value, json};

use super::Deploy;
use super::flow::Fail;
use super::model::{App, KILN_HOST, Registries, RegistryTemplate};
use crate::ci::integrations::aws::{self, Aws, AwsError};

/// The repositories' name a template gives an app.
pub fn name_for(t: &RegistryTemplate, app: &App) -> String {
    t.name.replace("{app}", &app.name).replace("{project}", &app.project)
}

/// What a template says of the registries it makes, checked when it is put.
pub async fn check(d: &Deploy, t: &RegistryTemplate) -> Result<(), String> {
    let example = t.name.replace("{app}", "app").replace("{project}", "project");
    if !crate::ci::registry::valid_name(&example) || !crate::ci::registry::valid_name(&format!("charts/{example}/snapshots")) {
        return Err(format!("registry.name {}: a repository's name (lowercase letters, digits, `.`, `_`, `-` and `/`), with {{app}} and {{project}} filled in", t.name));
    }
    if t.keep_snapshots == 0 {
        return Err("registry.keep_snapshots: at least 1".into());
    }
    if t.at == KILN_HOST {
        return Ok(());
    }
    let i = crate::ci::integrations::get(&d.hub, &t.at).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("registry.at {}: no such integration (kiln, or an aws or gcp one)", t.at))?;
    match i.kind.as_str() {
        "aws" if i.config.get("region").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) => Ok(()),
        "aws" => Err(format!("registry.at {}: ECR needs the integration's region", t.at)),
        "gcp" if t.location.as_deref().is_some_and(|l| !l.is_empty()) => Ok(()),
        "gcp" => Err("registry.location: Artifact Registry's location (europe-west1)".into()),
        k => Err(format!("registry.at {}: a {k} integration makes no registries (kiln, or an aws or gcp one)", t.at)),
    }
}

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}

/// Makes an app's registries from its template: their references.
pub async fn make(d: &Deploy, t: &RegistryTemplate, app: &App, by: &str) -> Result<Registries, Fail> {
    let name = name_for(t, app);
    if !crate::ci::registry::valid_name(&format!("charts/{name}/snapshots")) {
        return Err((StatusCode::BAD_REQUEST, format!("{name}: a repository's name is lowercase letters, digits, `.`, `_`, `-` and `/`")));
    }
    let made = if t.at == KILN_HOST {
        kiln_s(d, &name, t.keep_snapshots).await.map_err(fail)?
    } else {
        let i = crate::ci::integrations::get(&d.hub, &t.at).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.ok_or_else(|| (StatusCode::BAD_REQUEST, format!("no integration {}", t.at)))?;
        let w = kiln_ci::oidc::Workload { repository: app.source.as_deref().unwrap_or(&name), app: Some(&app.name), actor: by, source: "cd", trigger: Some("registry"), ..Default::default() };
        match i.kind.as_str() {
            "aws" => {
                let region = i.config.get("region").and_then(Value::as_str).unwrap_or_default().to_owned();
                let creds = aws::credentials(&d.hub, &i, &w).await.map_err(fail)?;
                ecr(&Aws { creds, region }, &name, t.keep_snapshots, &t.at).await.map_err(fail)?
            }
            "gcp" => {
                let project = t.project.clone().or_else(|| i.config.get("project").and_then(Value::as_str).map(str::to_owned)).filter(|p| !p.is_empty()).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("registry.project: {} names no project", t.at)))?;
                let token = crate::ci::integrations::gcp::token(&d.hub, &i, &w).await.map_err(fail)?;
                artifact_registry(&token.access_token, &project, t.location.as_deref().unwrap_or_default(), &name, t.keep_snapshots, &t.at).await.map_err(fail)?
            }
            k => return Err((StatusCode::BAD_REQUEST, format!("a {k} integration makes no registries"))),
        }
    };
    d.hub.store.audit(by.to_owned(), "registry.make".into(), format!("{}, {} and {}: app {}'s, on {}", made.images, made.snapshots, made.chart, app.name, t.at)).await;
    Ok(made)
}

/// On kiln's own registry: its repositories with their policies.
async fn kiln_s(d: &Deploy, name: &str, keep: u32) -> anyhow::Result<Registries> {
    let host = crate::githost::host_of(&d.hub.cfg.server.public_url);
    let (images, snapshots, chart) = (name.to_owned(), format!("{name}/snapshots"), format!("charts/{name}"));
    crate::ci::registry::set_policy(&d.hub, &images, true, None).await?;
    crate::ci::registry::set_policy(&d.hub, &snapshots, false, Some(keep)).await?;
    crate::ci::registry::set_policy(&d.hub, &chart, true, None).await?;
    Ok(Registries { at: KILN_HOST.into(), images: format!("{host}/{images}"), snapshots: format!("{host}/{snapshots}"), chart: format!("oci://{host}/{chart}") })
}

const ECR: &str = "AmazonEC2ContainerRegistry_V20150921";

/// ECR's lifecycle rules: untagged images go after a week, and (`keep`) only the newest tags stay.
fn lifecycle(keep: Option<u32>) -> String {
    let mut rules = vec![json!({ "rulePriority": 1, "description": "untagged images go after a week (kiln)", "selection": { "tagStatus": "untagged", "countType": "sinceImagePushed", "countUnit": "days", "countNumber": 7 }, "action": { "type": "expire" } })];
    if let Some(n) = keep {
        rules.push(json!({ "rulePriority": 2, "description": format!("the newest {n} snapshots stay (kiln)"), "selection": { "tagStatus": "any", "countType": "imageCountMoreThan", "countNumber": n }, "action": { "type": "expire" } }));
    }
    json!({ "rules": rules }).to_string()
}

/// One ECR repository, made if it isn't there, its tags (im)mutable and its lifecycle rules set:
/// its URI.
async fn ecr_repository(aws: &Aws, name: &str, immutable: bool, keep: Option<u32>) -> anyhow::Result<String> {
    let mutability = if immutable { "IMMUTABLE" } else { "MUTABLE" };
    let made = aws.json("ecr", &format!("{ECR}.CreateRepository"), &json!({ "repositoryName": name, "imageTagMutability": mutability, "imageScanningConfiguration": { "scanOnPush": true } })).await;
    let uri = match made {
        Ok(v) => v.pointer("/repository/repositoryUri").and_then(Value::as_str).map(str::to_owned),
        Err(e) if e.downcast_ref::<AwsError>().is_some_and(|a| a.code.ends_with("RepositoryAlreadyExistsException")) => {
            // There already: it keeps what it holds, and takes the rules.
            aws.json("ecr", &format!("{ECR}.PutImageTagMutability"), &json!({ "repositoryName": name, "imageTagMutability": mutability })).await?;
            let v = aws.json("ecr", &format!("{ECR}.DescribeRepositories"), &json!({ "repositoryNames": [name] })).await?;
            v.pointer("/repositories/0/repositoryUri").and_then(Value::as_str).map(str::to_owned)
        }
        Err(e) => return Err(e),
    };
    aws.json("ecr", &format!("{ECR}.PutLifecyclePolicy"), &json!({ "repositoryName": name, "lifecyclePolicyText": lifecycle(keep) })).await?;
    uri.ok_or_else(|| anyhow::anyhow!("ECR said nothing of {name}'s URI"))
}

/// On ECR: its three repositories.
pub async fn ecr(aws: &Aws, name: &str, keep: u32, at: &str) -> anyhow::Result<Registries> {
    let images = ecr_repository(aws, name, true, None).await?;
    let snapshots = ecr_repository(aws, &format!("{name}/snapshots"), false, Some(keep)).await?;
    let chart = ecr_repository(aws, &format!("charts/{name}"), true, None).await?;
    Ok(Registries { at: at.to_owned(), images, snapshots, chart: format!("oci://{chart}") })
}

/// An Artifact Registry repository's id for one of an app's: its name's parts joined by `-`.
fn ar_id(name: &str) -> String {
    name.split('/').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-").replace(['.', '_'], "-")
}

/// One Artifact Registry repository (a Docker one), made if it isn't there, its tags immutable or
/// (`keep`) its newest versions kept and the rest cleaned up.
async fn ar_repository(base: &str, token: &str, project: &str, location: &str, id: &str, immutable: bool, keep: Option<u32>) -> anyhow::Result<()> {
    let client = crate::http::client::github()?;
    let mut ask = json!({ "format": "DOCKER", "description": "made by kiln (docs/deploy-api.md §3.1)", "dockerConfig": { "immutableTags": immutable } });
    if let (Some(n), Some(o)) = (keep, ask.as_object_mut()) {
        o.insert(
            "cleanupPolicies".into(),
            json!({
                "keep-newest": { "id": "keep-newest", "action": "KEEP", "mostRecentVersions": { "keepCount": n } },
                "delete-older": { "id": "delete-older", "action": "DELETE", "condition": { "tagState": "ANY", "olderThan": "86400s" } },
            }),
        );
        o.insert("cleanupPolicyDryRun".into(), json!(false));
    }
    let url = format!("{base}/v1/projects/{project}/locations/{location}/repositories?repositoryId={id}");
    let resp = client.request(crate::http::client::Method::Post, &url).bearer(token).json(&ask).timeout(std::time::Duration::from_secs(60)).send().await?;
    let status = resp.status();
    let body = resp.bytes(1 << 20).await?;
    match status {
        200..=299 => Ok(()),
        // There already: it keeps what it holds.
        409 => Ok(()),
        s => Err(crate::ci::integrations::gcp::error_of(s, &body).into()),
    }
}

/// On Artifact Registry: its three repositories, and the references images and the chart take.
pub async fn artifact_registry(token: &str, project: &str, location: &str, name: &str, keep: u32, at: &str) -> anyhow::Result<Registries> {
    anyhow::ensure!(!location.is_empty() && location.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "`{location}` isn't a location");
    let base = crate::ci::integrations::endpoint("gcp:artifactregistry", || "https://artifactregistry.googleapis.com".into());
    let id = ar_id(name);
    let app = name.rsplit('/').next().unwrap_or(name);
    for (repo, immutable, keep) in [(id.clone(), true, None), (format!("{id}-snapshots"), false, Some(keep)), (format!("{id}-charts"), true, None)] {
        ar_repository(&base, token, project, location, &repo, immutable, keep).await?;
    }
    let host = format!("{location}-docker.pkg.dev/{project}");
    Ok(Registries { at: at.to_owned(), images: format!("{host}/{id}/{app}"), snapshots: format!("{host}/{id}-snapshots/{app}"), chart: format!("oci://{host}/{id}-charts") })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A fake answering each request with `answer(method, path, headers, body)`: its port, and
    /// what it was asked (method, path, the AWS target, the body).
    #[allow(clippy::type_complexity)]
    fn fake(answer: fn(&str, &str, &str, &Value) -> (u16, Value)) -> anyhow::Result<(u16, Arc<Mutex<Vec<(String, String, String, Value)>>>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let asked: Arc<Mutex<Vec<(String, String, String, Value)>>> = Arc::default();
        let log = Arc::clone(&asked);
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
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
                let (mut len, mut target) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap_or((&h, ""));
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap_or(0),
                        "x-amz-target" => target = v.trim().to_owned(),
                        _ => {}
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                if let Ok(mut l) = log.lock() {
                    l.push((method.clone(), path.clone(), target.clone(), body.clone()));
                }
                let (status, reply) = answer(&method, &path, &target, &body);
                let text = reply.to_string();
                let mut s = reader.into_inner();
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
            }
        });
        Ok((port, asked))
    }

    #[test]
    fn ecr_repositories_for_releases_snapshots_and_the_chart() -> anyhow::Result<()> {
        // `shop/web` is there already (another app's apply), the rest are made.
        let (port, asked) = fake(|_, _, target, body| {
            let name = body.get("repositoryName").and_then(Value::as_str).unwrap_or_default().to_owned();
            let uri = |n: &str| format!("123456789012.dkr.ecr.eu-west-1.amazonaws.com/{n}");
            match target.rsplit('.').next().unwrap_or_default() {
                "CreateRepository" if name == "shop/web" => (400, json!({ "__type": "RepositoryAlreadyExistsException", "message": "The repository with name 'shop/web' already exists" })),
                "CreateRepository" => (200, json!({ "repository": { "repositoryName": name, "repositoryUri": uri(&name) } })),
                "DescribeRepositories" => (200, json!({ "repositories": [{ "repositoryUri": uri(body.pointer("/repositoryNames/0").and_then(Value::as_str).unwrap_or_default()) }] })),
                "PutImageTagMutability" | "PutLifecyclePolicy" => (200, json!({})),
                _ => (400, json!({ "__type": "UnknownOperationException" })),
            }
        })?;
        aws::override_endpoint("ecr", &format!("http://127.0.0.1:{port}"));
        let creds = aws::Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, expires_at: None };
        let aws = Aws { creds, region: "eu-west-1".into() };
        let made = crate::rt::block_on(async move { ecr(&aws, "shop/web", 5, "aws-prod").await })?;
        assert_eq!(made.images, "123456789012.dkr.ecr.eu-west-1.amazonaws.com/shop/web");
        assert_eq!(made.snapshots, "123456789012.dkr.ecr.eu-west-1.amazonaws.com/shop/web/snapshots");
        assert_eq!(made.chart, "oci://123456789012.dkr.ecr.eu-west-1.amazonaws.com/charts/shop/web");
        let asked = asked.lock().map(|a| a.clone()).unwrap_or_default();
        let op = |a: &(String, String, String, Value)| a.2.rsplit('.').next().unwrap_or_default().to_owned();
        let of = |o: &str, n: &str| asked.iter().find(|a| op(a) == o && (a.3.get("repositoryName") == Some(&json!(n)) || a.3.pointer("/repositoryNames/0") == Some(&json!(n)))).map(|a| a.3.clone());
        assert_eq!(of("CreateRepository", "shop/web/snapshots").and_then(|b| b.get("imageTagMutability").cloned()), Some(json!("MUTABLE")));
        assert_eq!(of("CreateRepository", "charts/shop/web").and_then(|b| b.get("imageTagMutability").cloned()), Some(json!("IMMUTABLE")));
        // The one there already takes the rules: immutable tags, and its lifecycle.
        assert_eq!(of("PutImageTagMutability", "shop/web").and_then(|b| b.get("imageTagMutability").cloned()), Some(json!("IMMUTABLE")));
        let rules = |n: &str| -> Value { of("PutLifecyclePolicy", n).and_then(|b| b.get("lifecyclePolicyText").and_then(Value::as_str).and_then(|t| serde_json::from_str(t).ok())).unwrap_or_default() };
        let snapshots = rules("shop/web/snapshots");
        assert_eq!(snapshots.pointer("/rules/1/selection/countNumber"), Some(&json!(5)), "the newest five snapshots: {snapshots}");
        assert_eq!(rules("shop/web").pointer("/rules").and_then(Value::as_array).map(Vec::len), Some(1), "a release path keeps every tag; untagged images go");
        assert!(asked.iter().all(|a| a.1 == "/" && a.0 == "POST"), "ECR's JSON protocol: {asked:?}");
        Ok(())
    }

    #[test]
    fn artifact_registry_repositories_for_releases_snapshots_and_the_chart() -> anyhow::Result<()> {
        let (port, asked) = fake(|method, path, _, _| match (method, path.contains("repositoryId=shop-web-charts")) {
            ("POST", true) => (409, json!({ "error": { "code": 409, "message": "the repository already exists", "status": "ALREADY_EXISTS" } })),
            ("POST", false) => (200, json!({ "name": "projects/acme/locations/europe-west1/operations/1" })),
            _ => (404, json!({})),
        })?;
        crate::ci::integrations::override_endpoint("gcp:artifactregistry", &format!("http://127.0.0.1:{port}"));
        let made = crate::rt::block_on(artifact_registry("ya29.token", "acme", "europe-west1", "shop/web", 7, "gcp-prod"))?;
        assert_eq!(made.images, "europe-west1-docker.pkg.dev/acme/shop-web/web");
        assert_eq!(made.snapshots, "europe-west1-docker.pkg.dev/acme/shop-web-snapshots/web");
        assert_eq!(made.chart, "oci://europe-west1-docker.pkg.dev/acme/shop-web-charts");
        let asked = asked.lock().map(|a| a.clone()).unwrap_or_default();
        let of = |id: &str| asked.iter().find(|a| a.1.ends_with(&format!("repositoryId={id}"))).map(|a| a.3.clone()).unwrap_or_default();
        assert_eq!(of("shop-web").pointer("/dockerConfig/immutableTags"), Some(&json!(true)));
        assert_eq!(of("shop-web-snapshots").pointer("/dockerConfig/immutableTags"), Some(&json!(false)));
        assert_eq!(of("shop-web-snapshots").pointer("/cleanupPolicies/keep-newest/mostRecentVersions/keepCount"), Some(&json!(7)));
        assert!(of("shop-web").get("cleanupPolicies").is_none(), "a release path keeps every version");
        assert!(asked.iter().all(|a| a.1.starts_with("/v1/projects/acme/locations/europe-west1/repositories?")), "{asked:?}");
        Ok(())
    }

    #[test]
    fn repository_names_for_artifact_registry() {
        assert_eq!(ar_id("shop/web"), "shop-web");
        assert_eq!(ar_id("acme/shop.web_x"), "acme-shop-web-x");
    }
}
