//! Where the cache service's writes go up to, and its misses are looked for (docs/ci.md
//! W24-R08): the server, by scope.
//!
//! A token minted under a grant that names an [`Upstream`] sends every object it writes up as
//! well, in the background (a write is answered once it's kept here), and a read this runner
//! can't answer is asked of the server before it's a miss. [`Leg`] is a leg's: kiln's chunked
//! cache protocol (`ci/services/chunks.rs`, through `runner::cache`), with the leg's own token,
//! so only chunks the repository's caches lack move, and the server applies its own scope rules.

use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::store::Scope;
use crate::http::client::Client;

/// An object written in a scope, to send up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Up {
    pub scope: Scope,
    pub ns: String,
    pub name: String,
    pub digest: String,
    pub size: u64,
}

/// Where a scope's writes go and its misses are looked for.
pub trait Upstream: Send + Sync + 'static {
    /// Sends `up` (its bytes in the file at `path`); whether the server kept it (`false`: it had it).
    fn put<'a>(&'a self, up: &'a Up, path: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>>;

    /// The entry `name` of `ns` in `scope`, written to `into` when the server holds one the
    /// holder may read: whether it did.
    fn get<'a>(&'a self, scope: &'a Scope, ns: &'a str, name: &'a str, into: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>>;
}

/// The server's cache scope for a trust domain's entries (docs/ci.md W08-R09's scopes): trusted
/// runs write the repository's (`repo`), which every ref reads; an untrusted leg its own ref's.
/// Protected units share nothing, and a person's scope has no place on the server yet.
pub fn server_scope(domain: &str) -> Option<&'static str> {
    match domain {
        "trusted" => Some("repo"),
        "untrusted" => Some("ref"),
        _ => None,
    }
}

/// The server's key for an entry: its namespace and its name's hash (any name fits GitHub's key
/// rules: no commas, at most 512 characters, compared without case).
pub fn server_key(ns: &str, name: &str) -> String {
    format!("kiln/cas/{ns}/{}", crate::util::sha256_hex(name.as_bytes()))
}

/// The server's version for every entry the service sends: never an `actions/cache` version, nor
/// `kiln/cache@v1`'s.
pub fn server_version() -> String {
    crate::util::sha256_hex(b"kiln/cas v1")
}

/// A leg's upstream: the server's chunked cache protocol, as the leg.
pub struct Leg {
    pub http: Arc<Client>,
    /// The results service's origin.
    pub base: String,
    /// The leg's `ACTIONS_RUNTIME_TOKEN`.
    pub token: String,
}

impl Leg {
    fn service(&self) -> crate::runner::cache::Service<'_> {
        crate::runner::cache::Service { http: &self.http, base: self.base.clone(), token: &self.token }
    }
}

impl Upstream for Leg {
    fn put<'a>(&'a self, up: &'a Up, path: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let Some(scope) = server_scope(&up.scope.domain) else { return Ok(false) };
            let saved = crate::runner::cache::save(&self.service(), &server_key(&up.ns, &up.name), &server_version(), scope, None, path).await?;
            Ok(saved.is_some())
        })
    }

    fn get<'a>(&'a self, scope: &'a Scope, ns: &'a str, name: &'a str, into: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let Some(server) = server_scope(&scope.domain) else { return Ok(false) };
            let found = crate::runner::cache::restore(&self.service(), &[server_key(ns, name)], &server_version(), server, None, into, false).await?;
            Ok(found.is_some())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w24_r08_writes_go_up_by_scope() {
        assert_eq!(server_scope("trusted"), Some("repo"));
        assert_eq!(server_scope("untrusted"), Some("ref"));
        assert_eq!((server_scope("protected"), server_scope("person:ada")), (None, None));
        let k = server_key("sccache", "a/b/c/abc,def");
        assert!(k.starts_with("kiln/cas/sccache/") && !k.contains(',') && k.len() < 512);
        assert_ne!(k, server_key("bazel-ac", "a/b/c/abc,def"), "namespaces never collide");
        assert_eq!(server_version().len(), 64);
    }
}
