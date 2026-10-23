//! S3 credentials, for what kiln's CI puts in S3-compatible stores itself: the OIDC issuer's
//! documents (W10-R12, `[ci.oidc] publish_credentials`, and `kiln ci oidc publish`). The
//! content-addressed store's bucket is kiln's store of bytes (`[blobs] store = "s3"`,
//! docs/blobs.md). For the tests, a fake S3 that checks each request's signature as S3 does.

use anyhow::{Context as _, Result, bail};

use crate::ci::integrations::aws::{self, Credentials};
use crate::config::ci::S3Credentials;
use crate::hub::Hub;

/// S3 credentials as `c` says (`what` names the setting, for errors): an `aws` integration's,
/// minted for kiln itself as the server's own workload (W10-R11); a key pair; or the server's
/// environment.
pub async fn credentials(hub: &Hub, c: &S3Credentials, what: &str) -> Result<Credentials> {
    if let Some(name) = &c.integration {
        let i = crate::ci::integrations::get(hub, name).await?.with_context(|| format!("{what}: no integration `{name}`"))?;
        if i.kind != "aws" {
            bail!("{what}: `{name}` is a {} integration; S3 is signed in to through an aws one", i.kind);
        }
        let w = kiln_ci::oidc::Workload { repository: "kiln", environment: None, git_ref: None, sha: None, actor: "kiln", source: "service", app: None, deployment: None, trigger: Some("store") };
        return aws::credentials(hub, &i, &w).await;
    }
    if let Some(id) = &c.access_key_id {
        let secret = match (&c.secret_access_key_file, &c.secret_access_key_env) {
            (Some(p), _) => std::fs::read_to_string(p).with_context(|| format!("{what}: reading {}", p.display()))?.trim().to_owned(),
            (None, Some(v)) => std::env::var(v).with_context(|| format!("{what}: {v} isn't set"))?,
            (None, None) => bail!("{what}: access_key_id needs secret_access_key_file or secret_access_key_env"),
        };
        return Ok(Credentials { access_key_id: id.clone(), secret_access_key: secret, session_token: None, expires_at: None });
    }
    from_env().with_context(|| format!("{what}: no credentials: an integration, access_key_id, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY in the server's environment"))
}

/// Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
pub fn from_env() -> Result<Credentials> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
        (Some(id), Some(secret)) => Ok(Credentials { access_key_id: id, secret_access_key: secret, session_token: var("AWS_SESSION_TOKEN"), expires_at: None }),
        _ => bail!("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY aren't set"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use bytes::Bytes;
    use http::StatusCode;
    use parking_lot::Mutex;

    use super::*;
    use crate::ci::integrations::aws::Signable;
    use crate::http::server::{self as srv, Request};

    /// A fake S3: objects by path, and each request's signature checked as S3 checks it (the
    /// signed headers as they came, the body against its hash), with the one key pair it knows
    /// (`AKIDTEST`, `s3cr3t`, in `us-east-1`).
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) objects: Mutex<HashMap<String, Vec<u8>>>,
        pub(crate) puts: Mutex<usize>,
        pub(crate) gets: Mutex<usize>,
        pub(crate) refused: Mutex<Vec<String>>,
    }

    impl srv::Handler for Fake {
        async fn handle(self: Arc<Self>, req: Request) -> srv::Response {
            let (method, path) = (req.method().to_string(), req.uri().path().to_owned());
            let headers: Vec<(String, String)> = req.headers().iter().map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())).collect();
            let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let auth = header("authorization");
            let body = srv::body_bytes(req.into_body(), 16 << 20).await.unwrap_or_default();
            let names = auth.split("SignedHeaders=").nth(1).and_then(|r| r.split(',').next()).unwrap_or("");
            let signed: Vec<(String, String)> = names.split(';').map(|k| (k.to_owned(), header(k))).collect();
            let (date, payload) = (header("x-amz-date"), header("x-amz-content-sha256"));
            let creds = Credentials { access_key_id: "AKIDTEST".into(), secret_access_key: "s3cr3t".into(), session_token: None, expires_at: None };
            let want = aws::authorization(&creds, "us-east-1", "s3", &date, &Signable { method: &method, path: &path, query: &[], headers: &signed, payload_sha256: &payload }, false);
            if auth != want || crate::util::sha256_hex(&body) != payload {
                self.refused.lock().push(format!("{method} {path}"));
                return srv::response(
                    StatusCode::FORBIDDEN,
                    "application/xml",
                    srv::full(Bytes::from_static(
                        b"<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided.</Message></Error>",
                    )),
                );
            }
            match method.as_str() {
                "PUT" => {
                    self.objects.lock().insert(path, body.to_vec());
                    *self.puts.lock() += 1;
                    srv::status(StatusCode::OK)
                }
                "GET" => {
                    *self.gets.lock() += 1;
                    match self.objects.lock().get(&path) {
                        Some(b) => srv::response(StatusCode::OK, "application/octet-stream", srv::full(Bytes::from(b.clone()))),
                        None => srv::response(StatusCode::NOT_FOUND, "application/xml", srv::full(Bytes::from_static(b"<Error><Code>NoSuchKey</Code></Error>"))),
                    }
                }
                "DELETE" => {
                    self.objects.lock().remove(&path);
                    srv::status(StatusCode::NO_CONTENT)
                }
                _ => srv::status(StatusCode::METHOD_NOT_ALLOWED),
            }
        }
    }

    pub(crate) fn fake_s3() -> Result<(String, Arc<Fake>)> {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        let fake = Arc::new(Fake::default());
        let f = Arc::clone(&fake);
        crate::rt::spawn(async move {
            while let Ok(s) = tcp.accept().await {
                let (h, r) = (Arc::clone(&f), s.peer_addr().ok());
                crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
            }
        });
        Ok((format!("http://{addr}"), fake))
    }
}
