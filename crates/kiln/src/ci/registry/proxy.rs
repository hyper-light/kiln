//! Caching proxies for package registries (docs/ci.md W12-R04's second part, R05, R06):
//! `/_kiln/proxy/<name>/…` speaks an ecosystem's read protocol in front of its upstream
//! (`[[ci.registry.proxies]]`), so a project installs through kiln, and again when the upstream is
//! out of reach.
//! - **What's kept**: every answer, in the content-addressed store. Packages (tarballs, wheels,
//!   crates, modules, jars, nupkgs, gems) never change once published, so a kept one is served
//!   without asking; an index (a packument, a simple page, a sparse index file, a module's
//!   versions, `maven-metadata.xml`) is asked again after `ttl`.
//! - **Where clients go next**: an index's links to the upstream (and its file host) are written
//!   over to point at the proxy, so what the client fetches next comes through kiln too.
//! - **Out of reach** (W12-R06): what kiln has is served, with a `Warning` saying so.
//! - **Who**: anyone signed in to kiln (a PAT, an agent's or a job's token, as Bearer, as Basic's
//!   password, or bare, as Cargo sends it); anyone at all with `anonymous`.
//!
//! The ecosystems: npm, PyPI (the simple API, PEP 503 and 691), crates.io's sparse index, Go's
//! module proxy (and its checksum database), Maven repositories, NuGet v3, RubyGems' compact
//! index, and plain HTTP for the rest (only the hosts named).

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, StatusCode, header};
use rusqlite::{OptionalExtension as _, params};

use crate::config::ci::Proxy;
use crate::http::client::Method as Verb;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The biggest package kept (and the biggest index read whole).
const MAX_PACKAGE: u64 = 4 << 30;
const MAX_INDEX: usize = 64 << 20;

/// An ecosystem's rules: where its upstream is, what never changes, how links are written over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Npm,
    Pypi,
    Crates,
    Go,
    Maven,
    Nuget,
    Rubygems,
    Http,
}

impl Ecosystem {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "npm" => Self::Npm,
            "pypi" | "python" | "pip" => Self::Pypi,
            "crates" | "cargo" | "crates.io" => Self::Crates,
            "go" | "golang" => Self::Go,
            "maven" | "gradle" => Self::Maven,
            "nuget" | "dotnet" => Self::Nuget,
            "rubygems" | "gems" | "ruby" => Self::Rubygems,
            "http" => Self::Http,
            _ => return None,
        })
    }

    /// The public upstream, when the proxy names none.
    fn upstream(self) -> &'static str {
        match self {
            Self::Npm => "https://registry.npmjs.org",
            Self::Pypi => "https://pypi.org",
            Self::Crates => "https://index.crates.io",
            Self::Go => "https://proxy.golang.org",
            Self::Maven => "https://repo1.maven.org/maven2",
            Self::Nuget => "https://api.nuget.org",
            Self::Rubygems => "https://rubygems.org",
            Self::Http => "",
        }
    }

    /// Whether what's at `path` never changes once published.
    fn immutable(self, path: &str) -> bool {
        let file = path.rsplit('/').next().unwrap_or(path);
        match self {
            Self::Npm => path.contains("/-/") && file.ends_with(".tgz"),
            Self::Pypi => path.starts_with("files/"),
            Self::Crates => path.starts_with("dl/"),
            Self::Go => {
                (path.contains("/@v/") && (file.ends_with(".zip") || file.ends_with(".mod") || file.ends_with(".info")))
                    || (path.starts_with("sumdb/") && path.contains("/tile/") && !path.contains(".p/"))
            }
            Self::Maven => !file.starts_with("maven-metadata.xml") && !path.contains("-SNAPSHOT/"),
            Self::Nuget => file.ends_with(".nupkg") || file.ends_with(".nuspec"),
            Self::Rubygems => path.starts_with("gems/") || path.starts_with("quick/"),
            Self::Http => false,
        }
    }
}

/// A proxy `[[ci.registry.proxies]]` names: its rules, upstream and credentials.
struct Upstream<'a> {
    cfg: &'a Proxy,
    eco: Ecosystem,
    base: String,
}

fn proxies(hub: &Hub) -> impl Iterator<Item = Upstream<'_>> {
    hub.cfg.ci.registry.proxies.iter().filter_map(|p| {
        let eco = Ecosystem::parse(&p.ecosystem)?;
        let base = p.upstream.clone().unwrap_or_else(|| eco.upstream().to_owned()).trim_end_matches('/').to_owned();
        Some(Upstream { cfg: p, eco, base })
    })
}

fn name_of(p: &Proxy) -> String {
    p.name.clone().unwrap_or_else(|| p.ecosystem.clone())
}

impl Upstream<'_> {
    /// Where the upstream keeps its packages: PyPI's files, crates.io's crates.
    fn files(&self) -> String {
        if let Some(f) = &self.cfg.files {
            return f.trim_end_matches('/').to_owned();
        }
        match self.eco {
            Ecosystem::Pypi => "https://files.pythonhosted.org".to_owned(),
            Ecosystem::Crates if self.base.contains("index.crates.io") => "https://static.crates.io/crates".to_owned(),
            Ecosystem::Crates => format!("{}/crates", self.base),
            _ => self.base.clone(),
        }
    }

    /// The upstream URL for a path below the proxy (and the hosts an ecosystem keeps files on).
    fn url(&self, path: &str, query: Option<&str>) -> Option<String> {
        let q = query.map(|q| format!("?{q}")).unwrap_or_default();
        Some(match self.eco {
            Ecosystem::Pypi if path.starts_with("files/") => format!("{}/{}{q}", self.files(), path.trim_start_matches("files/")),
            Ecosystem::Crates if path.starts_with("dl/") => {
                // `dl/<crate>/<version>`: the crate's file where crates are kept.
                let mut parts = path.trim_start_matches("dl/").splitn(2, '/');
                let (c, v) = (parts.next()?, parts.next()?);
                format!("{}/{c}/{c}-{v}.crate", self.files())
            }
            Ecosystem::Go if path.starts_with("sumdb/") => {
                // `sumdb/<host>/<rest>`: the checksum database itself, passed through.
                let rest = path.trim_start_matches("sumdb/");
                let (host, rest) = rest.split_once('/').unwrap_or((rest, ""));
                if host != "sum.golang.org" && !self.cfg.hosts.iter().any(|h| h == host) {
                    return None;
                }
                format!("https://{host}/{rest}{q}")
            }
            Ecosystem::Http => {
                // `<host>/<path>`, for the hosts named (`http://host` for one without TLS).
                let (host, rest) = path.split_once('/').unwrap_or((path, ""));
                let named = self.cfg.hosts.iter().find(|h| !host.is_empty() && h.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').eq_ignore_ascii_case(host))?;
                let scheme = if named.starts_with("http://") { "http" } else { "https" };
                format!("{scheme}://{host}/{rest}{q}")
            }
            _ => format!("{}/{path}{q}", self.base),
        })
    }

    /// What the proxy answers instead of the upstream (crates.io's `config.json`, Go's sumdb
    /// support), if anything.
    fn own(&self, path: &str, origin: &str) -> Option<(String, Bytes)> {
        let here = format!("{origin}/_kiln/proxy/{}", name_of(self.cfg));
        match (self.eco, path) {
            (Ecosystem::Crates, "config.json") => Some(("application/json".into(), Bytes::from(serde_json::json!({ "dl": format!("{here}/dl/{{crate}}/{{version}}"), "api": null, "auth-required": !self.cfg.anonymous }).to_string()))),
            (Ecosystem::Go, p) if p.starts_with("sumdb/") && p.ends_with("/supported") => Some(("text/plain".into(), Bytes::new())),
            _ => None,
        }
    }

    /// An index's links to the upstream written over to point at the proxy.
    fn rewrite(&self, path: &str, body: &[u8], origin: &str) -> Option<Vec<u8>> {
        let here = format!("{origin}/_kiln/proxy/{}", name_of(self.cfg));
        let text = std::str::from_utf8(body).ok()?;
        let out = match self.eco {
            Ecosystem::Npm if !self.eco.immutable(path) => text.replace(&format!("{}/", self.base), &format!("{here}/")),
            Ecosystem::Pypi if path.starts_with("simple") => text.replace(&format!("{}/", self.files()), &format!("{here}/files/")).replace(&format!("{}/simple/", self.base), &format!("{here}/simple/")),
            // NuGet's service index: every resource through kiln, but its repository signatures,
            // which NuGet takes only over HTTPS, stay the upstream's when kiln speaks plain HTTP.
            Ecosystem::Nuget if path == "v3/index.json" => {
                let mut v: serde_json::Value = serde_json::from_str(text).ok()?;
                for r in v.get_mut("resources").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
                    let signatures = r.get("@type").and_then(serde_json::Value::as_str).is_some_and(|t| t.starts_with("RepositorySignatures"));
                    if signatures && here.starts_with("http://") {
                        continue;
                    }
                    let id = r.get("@id").and_then(serde_json::Value::as_str).map(|i| i.replace(&format!("{}/", self.base), &format!("{here}/")));
                    if let (Some(id), Some(o)) = (id, r.as_object_mut()) {
                        o.insert("@id".into(), serde_json::Value::String(id));
                    }
                }
                v.to_string()
            }
            Ecosystem::Nuget if !self.eco.immutable(path) => text.replace(&format!("{}/", self.base), &format!("{here}/")),
            _ => return None,
        };
        Some(out.into_bytes())
    }
}

/// A kept answer.
struct Kept {
    digest: String,
    size: u64,
    content_type: String,
    fetched_at: i64,
}

async fn kept(hub: &Hub, proxy: &str, key: &str) -> anyhow::Result<Option<Kept>> {
    let (p, k) = (proxy.to_owned(), key.to_owned());
    hub.store
        .call(move |c| {
            Ok(c.query_row("SELECT digest, size, content_type, fetched_at FROM proxy_cache WHERE proxy = ?1 AND key = ?2", params![p, k], |r| {
                Ok(Kept { digest: r.get(0)?, size: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0), content_type: r.get(2)?, fetched_at: r.get(3)? })
            })
            .optional()?)
        })
        .await
}

async fn keep(hub: &Hub, proxy: &str, key: &str, obj: &crate::ci::services::cas::Object, content_type: &str, immutable: bool) -> anyhow::Result<()> {
    let (p, k, d, s, ct) = (proxy.to_owned(), key.to_owned(), obj.digest.clone(), i64::try_from(obj.size).unwrap_or(i64::MAX), content_type.to_owned());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO proxy_cache (proxy, key, digest, size, content_type, immutable, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (proxy, key) DO UPDATE SET digest = excluded.digest, size = excluded.size, content_type = excluded.content_type, fetched_at = excluded.fetched_at",
                params![p, k, d, s, ct, immutable, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    srv::response(status, "text/plain; charset=utf-8", srv::full(message.into()))
}

/// Who may read through `p`: anyone signed in to kiln, anyone at all when it's `anonymous`.
async fn allowed(hub: &Hub, p: &Proxy, req: &Request) -> bool {
    if p.anonymous {
        return true;
    }
    let raw = srv::header_str(req, &header::AUTHORIZATION).map(str::trim).unwrap_or_default();
    let secret = match super::auth::credentials(hub, Some(raw)) {
        super::auth::Credentials::Secret(s) => s,
        super::auth::Credentials::Registry(_) => return true,
        // Cargo sends its token as it is.
        super::auth::Credentials::Invalid if !raw.is_empty() && !raw.contains(' ') => raw.to_owned(),
        _ => return false,
    };
    super::caller_of(hub, &secret).await.is_some()
}

/// The upstream's answer for `url` (a redirect followed, without credentials when it leaves the
/// upstream's host), streamed into a file; its status and content type.
async fn fetch(up: &Upstream<'_>, url: &str, accept: Option<&str>, to: &std::path::Path) -> anyhow::Result<(u16, String)> {
    let client = crate::http::client::github()?;
    let mut target = url.to_owned();
    for _ in 0..5 {
        let mut req = client.request(Verb::Get, &target).timeout(Duration::from_secs(60));
        if let Some(a) = accept {
            req = req.header("Accept", a);
        }
        // Credentials only for the upstream's own host.
        if crate::githost::host_of(&target) == crate::githost::host_of(&up.base) {
            if let Some(f) = &up.cfg.token_file
                && let Some(t) = std::fs::read_to_string(f).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
            {
                req = req.bearer(&t);
            } else if let (Some(u), Some(f)) = (&up.cfg.username, &up.cfg.password_file)
                && let Some(p) = std::fs::read_to_string(f).ok().map(|t| t.trim().to_owned())
            {
                req = req.header("Authorization", &format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{u}:{p}"))));
            }
        }
        let mut resp = req.send().await?;
        let status = resp.status();
        if (300..400).contains(&status) && status != 304 {
            let next = resp.header("location").ok_or_else(|| anyhow::anyhow!("a redirect with no location"))?.to_owned();
            target = if next.starts_with("http://") || next.starts_with("https://") { next } else { format!("{}{next}", target.split_once("://").map(|(s, r)| format!("{s}://{}", r.split('/').next().unwrap_or(r))).unwrap_or_default()) };
            continue;
        }
        let content_type = resp.header("content-type").unwrap_or("application/octet-stream").to_owned();
        let gzipped = resp.header("content-encoding").is_some_and(|e| e.contains("gzip"));
        if !(200..300).contains(&status) {
            return Ok((status, content_type));
        }
        let (sink, done) = crate::rt::file_sink(to.to_path_buf(), MAX_PACKAGE)?;
        while let Some(chunk) = resp.chunk().await? {
            if sink.send(chunk).await.is_err() {
                break;
            }
        }
        drop(sink);
        done.await?;
        // An index sent gzipped (NuGet's registration pages) is kept plain, to be written over.
        if gzipped {
            let p = to.to_path_buf();
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let raw = std::fs::read(&p)?;
                let mut plain = Vec::new();
                flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut plain)?;
                std::fs::write(&p, plain)?;
                Ok(())
            })
            .await??;
        }
        return Ok((status, content_type));
    }
    anyhow::bail!("too many redirects from {url}")
}

/// `/_kiln/proxy/<name>/<path>`: what the upstream answers there, kept.
pub async fn handle(hub: &Arc<Hub>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let rest = path.strip_prefix("/_kiln/proxy/").unwrap_or_default();
    let (name, sub) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(up) = proxies(hub).find(|u| name_of(u.cfg) == name) else { return fail(StatusCode::NOT_FOUND, format!("no proxy `{name}` here ([[ci.registry.proxies]])")) };
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return fail(StatusCode::METHOD_NOT_ALLOWED, "the proxy reads: publish with kiln/publish@v1");
    }
    if !allowed(hub, up.cfg, &req).await {
        let r = fail(StatusCode::UNAUTHORIZED, "sign in with a kiln token (a PAT, or the job's token)");
        return srv::with_header(r, header::WWW_AUTHENTICATE, "Basic realm=\"kiln\"");
    }
    let origin = crate::ci::services::blob::origin(&req);
    if let Some((ct, body)) = up.own(sub, &origin) {
        return srv::response(StatusCode::OK, &ct, srv::full(body));
    }
    let query = req.uri().query().map(str::to_owned);
    let Some(url) = up.url(sub, query.as_deref()) else { return fail(StatusCode::FORBIDDEN, format!("`{sub}` isn't one this proxy reaches")) };
    // What a client asks for changes the answer (npm's abbreviated metadata, PyPI's JSON pages).
    let accept = srv::header_str(&req, &header::ACCEPT).map(str::to_owned).filter(|a| matches!(up.eco, Ecosystem::Npm | Ecosystem::Pypi) && (a.contains("vnd.npm.install-v1") || a.contains("vnd.pypi.simple")));
    let key = format!("{sub}{}{}", query.as_deref().map(|q| format!("?{q}")).unwrap_or_default(), accept.as_deref().map(|a| format!("#{}", if a.contains("npm") { "npm-install-v1" } else { "pypi-json" })).unwrap_or_default());
    let immutable = up.eco.immutable(sub);
    let have = match kept(hub, name, &key).await {
        Ok(k) => k,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let ttl = i64::try_from(up.cfg.ttl.unwrap_or(300)).unwrap_or(300);
    let fresh = have.as_ref().is_some_and(|k| immutable || crate::util::now_unix() - k.fetched_at < ttl);
    let (k, stale) = if fresh {
        (have, false)
    } else {
        let tmp: PathBuf = crate::ci::services::cas::staging(hub).join(format!("proxy-{}", crate::util::new_id()));
        let got = fetch(&up, &url, accept.as_deref(), &tmp).await;
        let outcome = match got {
            Ok((st, ct)) if (200..300).contains(&st) => match crate::ci::services::cas::put_file(hub, &tmp).await {
                Ok(obj) => match keep(hub, name, &key, &obj, &ct, immutable).await {
                    Ok(()) => Ok(Some(Kept { digest: obj.digest, size: obj.size, content_type: ct, fetched_at: crate::util::now_unix() })),
                    Err(e) => Err(format!("{e:#}")),
                },
                Err(e) => Err(format!("{e:#}")),
            },
            Ok((404 | 410, _)) => Ok(None),
            Ok((st, _)) => Err(format!("{} answered {st}", up.base)),
            Err(e) => Err(format!("{e:#}")),
        };
        let t = tmp.clone();
        let _ = crate::rt::blocking(move || std::fs::remove_file(t)).await;
        match outcome {
            Ok(Some(k)) => (Some(k), false),
            Ok(None) => return fail(StatusCode::NOT_FOUND, format!("{} has no {sub}", up.base)),
            // Out of reach: what kiln has, saying so (W12-R06).
            Err(why) if have.is_some() => {
                tracing::warn!(proxy = name, path = sub, why, "a proxy's upstream is out of reach; serving what kiln has");
                (have, true)
            }
            Err(why) => return fail(StatusCode::BAD_GATEWAY, format!("the upstream is out of reach and kiln has no copy: {why}")),
        }
    };
    let Some(k) = k else { return fail(StatusCode::NOT_FOUND, "nothing here") };
    let etag = format!("\"{}\"", k.digest);
    if srv::header_str(&req, &header::IF_NONE_MATCH).is_some_and(|m| m.split(',').any(|t| t.trim() == etag)) {
        return srv::with_header(srv::status(StatusCode::NOT_MODIFIED), header::ETAG, &etag);
    }
    let Ok(Some(obj)) = crate::ci::services::cas::object(hub, &k.digest).await else { return fail(StatusCode::BAD_GATEWAY, "kiln's copy is gone") };
    let mut r = if matches!(up.eco, Ecosystem::Npm | Ecosystem::Pypi | Ecosystem::Nuget) && !immutable && k.size <= MAX_INDEX as u64 {
        // An index, its links written over for this origin.
        let mut buf = Vec::with_capacity(usize::try_from(k.size).unwrap_or(0));
        let rx = crate::ci::services::cas::read(hub, &obj, 0, obj.size);
        while let Ok(b) = rx.recv().await {
            buf.extend_from_slice(&b);
        }
        let body = up.rewrite(sub, &buf, &origin).unwrap_or(buf);
        let len = body.len();
        let r = srv::response(StatusCode::OK, &k.content_type, if *req.method() == Method::HEAD { srv::empty() } else { srv::full(body) });
        srv::with_header(r, header::CONTENT_LENGTH, &len.to_string())
    } else {
        let body = if *req.method() == Method::HEAD { srv::empty() } else { srv::channel_body(crate::ci::services::cas::read(hub, &obj, 0, obj.size)) };
        let r = srv::response(StatusCode::OK, &k.content_type, body);
        srv::with_header(r, header::CONTENT_LENGTH, &k.size.to_string())
    };
    r = srv::with_header(r, header::ETAG, &etag);
    if stale {
        r = srv::with_header(r, header::WARNING, &format!("110 kiln \"served from kiln's copy: {} is out of reach\"", up.base));
    }
    r
}

/// Whether a path is a proxy's.
pub fn is_proxy(path: &str) -> bool {
    path.starts_with("/_kiln/proxy/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w12_r04_immutables() {
        assert!(Ecosystem::Npm.immutable("left-pad/-/left-pad-1.3.0.tgz") && !Ecosystem::Npm.immutable("left-pad") && !Ecosystem::Npm.immutable("@types%2fnode"));
        assert!(Ecosystem::Pypi.immutable("files/packages/ab/cd/x-1.0-py3-none-any.whl") && !Ecosystem::Pypi.immutable("simple/requests/"));
        assert!(Ecosystem::Crates.immutable("dl/serde/1.0.0") && !Ecosystem::Crates.immutable("se/rd/serde"));
        assert!(Ecosystem::Go.immutable("github.com/pkg/errors/@v/v0.9.1.zip") && !Ecosystem::Go.immutable("github.com/pkg/errors/@v/list") && !Ecosystem::Go.immutable("github.com/pkg/errors/@latest"));
        assert!(Ecosystem::Go.immutable("sumdb/sum.golang.org/tile/8/0/000") && !Ecosystem::Go.immutable("sumdb/sum.golang.org/tile/8/0/000.p/7") && !Ecosystem::Go.immutable("sumdb/sum.golang.org/lookup/x@v1"));
        assert!(Ecosystem::Maven.immutable("org/a/b/1.0/b-1.0.jar") && !Ecosystem::Maven.immutable("org/a/b/maven-metadata.xml") && !Ecosystem::Maven.immutable("org/a/b/1.0-SNAPSHOT/b-1.0-20260101.jar"));
        assert!(Ecosystem::Nuget.immutable("v3-flatcontainer/newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg") && !Ecosystem::Nuget.immutable("v3/index.json"));
        assert!(Ecosystem::Rubygems.immutable("gems/rake-13.0.6.gem") && !Ecosystem::Rubygems.immutable("info/rake") && !Ecosystem::Rubygems.immutable("versions"));
        assert_eq!(Ecosystem::parse("cargo"), Some(Ecosystem::Crates));
    }

    #[test]
    fn w12_r04_upstream_urls_and_rewrites() {
        let cfg = |eco: &str| Proxy { ecosystem: eco.into(), hosts: vec!["nodejs.org".into()], ..Proxy::default() };
        let (pypi, crates, go, http) = (cfg("pypi"), cfg("crates"), cfg("go"), cfg("http"));
        fn up(p: &Proxy) -> Upstream<'_> {
            Upstream { cfg: p, eco: Ecosystem::parse(&p.ecosystem).unwrap_or(Ecosystem::Http), base: Ecosystem::parse(&p.ecosystem).map(Ecosystem::upstream).unwrap_or_default().to_owned() }
        }
        assert_eq!(up(&pypi).url("files/packages/a/b.whl", None).as_deref(), Some("https://files.pythonhosted.org/packages/a/b.whl"));
        assert_eq!(up(&pypi).url("simple/requests/", None).as_deref(), Some("https://pypi.org/simple/requests/"));
        assert_eq!(up(&crates).url("dl/serde/1.0.0", None).as_deref(), Some("https://static.crates.io/crates/serde/serde-1.0.0.crate"));
        assert_eq!(up(&crates).url("se/rd/serde", None).as_deref(), Some("https://index.crates.io/se/rd/serde"));
        assert_eq!(up(&go).url("sumdb/sum.golang.org/lookup/x@v1", None).as_deref(), Some("https://sum.golang.org/lookup/x@v1"));
        assert_eq!(up(&go).url("sumdb/evil.example/lookup/x", None), None, "only the checksum database");
        assert_eq!(up(&http).url("nodejs.org/dist/v22.0.0/node.tar.gz", None).as_deref(), Some("https://nodejs.org/dist/v22.0.0/node.tar.gz"));
        assert_eq!(up(&http).url("evil.example/x", None), None, "only the hosts named");
        let page = br#"<a href="https://files.pythonhosted.org/packages/a/b.whl#sha256=00">b</a>"#;
        let out = up(&pypi).rewrite("simple/b/", page, "http://k:1").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        assert_eq!(out, r#"<a href="http://k:1/_kiln/proxy/pypi/files/packages/a/b.whl#sha256=00">b</a>"#);
        let npm = cfg("npm");
        let doc = br#"{"versions":{"1.0.0":{"dist":{"tarball":"https://registry.npmjs.org/x/-/x-1.0.0.tgz"}}}}"#;
        let out = up(&npm).rewrite("x", doc, "http://k:1").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        assert!(out.contains("\"http://k:1/_kiln/proxy/npm/x/-/x-1.0.0.tgz\""), "{out}");
        let own = up(&crates).own("config.json", "http://k:1").map(|(_, b)| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        assert!(own.contains("http://k:1/_kiln/proxy/crates/dl/{crate}/{version}") && own.contains("\"auth-required\":true"), "{own}");
    }
}
