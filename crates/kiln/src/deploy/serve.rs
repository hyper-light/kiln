//! kiln's own git server, read over HTTP (docs/deploy-api.md §7): its repositories, at
//! `/_kiln/git/_hosted/<repo>.git`, to readers holding a token for that repository (an Argo CD
//! instance, a person cloning it). Fetches only. A GitHub owner can't start with `_`, so these
//! paths never meet the mirrors CI serves beside them.
//!
//! A token is shown once, when it's made; kiln keeps its SHA-256 (`deploy/reader/<hash>`), never
//! the token.

use std::sync::Arc;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

type Fail = (StatusCode, String);

const READER: &str = "deploy/reader/";

/// A reader of one hosted repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reader {
    pub repo: String,
    pub name: String,
    pub created_at: i64,
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Makes a reader of `repo`; its token, shown this once.
pub async fn add_reader(hub: &Hub, repo: &str, name: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!name.is_empty() && name.len() <= 64, "a reader's name: 1 to 64 characters");
    if readers(hub, Some(repo)).await?.iter().any(|r| r.name == name) {
        anyhow::bail!("{repo} has a reader called {name} already: remove it first");
    }
    let token = format!("kr_{}{}", crate::util::new_id(), crate::util::new_id());
    let r = Reader { repo: repo.to_owned(), name: name.to_owned(), created_at: crate::util::now_unix() };
    hub.store.kv_set(format!("{READER}{}", hash(&token)), serde_json::to_string(&r)?).await?;
    Ok(token)
}

/// The readers of `repo`, or of every hosted repository.
pub async fn readers(hub: &Hub, repo: Option<&str>) -> anyhow::Result<Vec<Reader>> {
    let mut out: Vec<Reader> = hub.store.kv_prefix(READER.to_owned()).await?.into_iter().filter_map(|(_, v)| serde_json::from_str::<Reader>(&v).ok()).filter(|r| repo.is_none_or(|x| r.repo == x)).collect();
    out.sort_by(|a, b| (&a.repo, &a.name).cmp(&(&b.repo, &b.name)));
    Ok(out)
}

/// Removes a reader; whether there was one.
pub async fn remove_reader(hub: &Hub, repo: &str, name: &str) -> anyhow::Result<bool> {
    for (k, v) in hub.store.kv_prefix(READER.to_owned()).await? {
        if serde_json::from_str::<Reader>(&v).is_ok_and(|r| r.repo == repo && r.name == name) {
            hub.store.kv_delete(k).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// The token a request carries: a bearer token, or a basic password (git's way).
fn token_of(headers: &http::HeaderMap) -> Option<String> {
    let v = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.trim().to_owned());
    }
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(v.strip_prefix("Basic ")?.trim()).ok()?;
    let text = String::from_utf8(raw).ok()?;
    Some(text.split_once(':').map_or(text.clone(), |(_, p)| p.to_owned()))
}

fn unauthorized() -> Response {
    let r = srv::text(StatusCode::UNAUTHORIZED, "a reader's token for this repository is needed");
    srv::with_header(r, http::header::WWW_AUTHENTICATE, "Basic realm=\"kiln\"")
}

/// `/_kiln/git/_hosted/<repo>.git/info/refs` and `…/git-upload-pack`.
pub async fn git(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let tail_len = match rest {
        [.., "info", "refs"] => 2,
        [.., "git-upload-pack"] => 1,
        _ => return Err((StatusCode::FORBIDDEN, "kiln's git server serves fetches only".into())),
    };
    let segs = rest.get(..rest.len() - tail_len).unwrap_or(&[]);
    let tail = rest.get(rest.len() - tail_len..).unwrap_or(&[]);
    let repo = segs.join("/");
    let repo = repo.trim_end_matches(".git");
    let hosted = super::host::Hosted::new(&hub.cfg.server.data_dir);
    let dir = hosted.path(repo).map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
    let Some(token) = token_of(req.headers()) else { return Ok(unauthorized()) };
    let reader = hub.store.kv_get(format!("{READER}{}", hash(&token))).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let Some(reader) = reader.and_then(|v| serde_json::from_str::<Reader>(&v).ok()) else { return Ok(unauthorized()) };
    if reader.repo != repo {
        return Err((StatusCode::FORBIDDEN, format!("this token reads {} only", reader.repo)));
    }
    if !dir.join("HEAD").exists() {
        return Err((StatusCode::NOT_FOUND, format!("kiln hosts no repository {repo}")));
    }
    let root = hosted.root().to_path_buf();
    let rel = dir.strip_prefix(&root).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "the repository's path".to_owned()))?.to_string_lossy().into_owned();
    crate::ci::facade::backend(req, &root, &rel, tail).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_come_as_bearers_or_basic_passwords() {
        use base64::Engine as _;
        let req = |v: &str| {
            let mut h = http::HeaderMap::new();
            if let Ok(v) = http::HeaderValue::from_str(v) {
                h.insert(http::header::AUTHORIZATION, v);
            }
            h
        };
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("argocd:kr_abc"));
        assert_eq!(token_of(&req(&basic)).as_deref(), Some("kr_abc"));
        assert_eq!(token_of(&req("Bearer kr_def")).as_deref(), Some("kr_def"));
        assert_eq!(token_of(&req("Digest x")), None);
        assert_ne!(hash("a"), hash("b"));
    }
}
