//! `aws-static` (docs/cd-targets.md §3): a built site in an S3 prefix, served through CloudFront.
//!
//! ```yaml
//! www:
//!   aws-static:
//!     bucket: shop-www
//!     prefix: sites/shop                   # by default sites/<app>
//!     site: "${{ artifacts.site }}"        # a zip or a .tar.gz of the built site, CI's
//!     distribution: E2QWRUHAPOMQZL         # its origin path switched to each release's prefix
//!     origin: www-bucket                   # the distribution's origin (by default the bucket's)
//!     keep: 5                              # releases kept under the prefix, for rollbacks
//! ```
//!
//! - **Site** (wave 0): each release's files go under a prefix of their own,
//!   `<prefix>/<the site's sha256's first 16>/`, uploaded unless the release before holds the same
//!   file (by its hash), which is copied over in S3 instead; a manifest last says the prefix is
//!   whole. Then `<prefix>/current.json` names it, and releases beyond `keep` go. A rollback finds
//!   its prefix whole and uploads nothing.
//! - **Distribution** (wave 1): the CloudFront distribution's origin path is switched to the
//!   release's prefix at once, and what it caches invalidated. kiln never deletes a distribution
//!   it didn't make: an app that goes leaves it, its origin path as it was.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const S3: &str = "s3.aws.kiln.dev";
pub const CLOUDFRONT: &str = "cloudfront.aws.kiln.dev";

/// The pointer to a site's live release, under its prefix.
pub const CURRENT: &str = "current.json";
/// What says a release's prefix is whole: its files and their hashes.
pub const MANIFEST: &str = ".kiln-site.json";
/// How many releases a site keeps by default.
pub const KEEP: u64 = 5;

/// The files a render reads: none, the site is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// A release's prefix for the site `sha256:<hex>`.
pub fn release_prefix(prefix: &str, site: &str) -> String {
    let hex = site.trim_start_matches("sha256:");
    format!("{}/{}", prefix.trim_matches('/'), hex.get(..16).unwrap_or(hex))
}

/// A Site's name, for the engine's keys: the bucket and prefix, `/` read as `.`.
pub fn site_name(bucket: &str, prefix: &str) -> String {
    format!("{bucket}.{}", prefix.trim_matches('/').replace('/', "."))
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let bucket = text(input.spec, "bucket").ok_or("aws-static needs bucket: the S3 bucket")?;
    let default = format!("sites/{}", input.app.rsplit('/').next().unwrap_or(input.app));
    let prefix = text(input.spec, "prefix").map_or(default, str::to_owned).trim_matches('/').to_owned();
    if prefix.is_empty() || prefix.split('/').any(|p| p.is_empty() || p == "..") {
        return Err(format!("prefix `{prefix}`: path segments, without `..`"));
    }
    let site = text(input.spec, "site").ok_or("aws-static needs site: the built site CI made (${{ artifacts.<name> }})")?;
    if !site.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(format!("site `{site}`: the built site CI made (${{{{ artifacts.<name> }}}}), a zip or a .tar.gz"));
    }
    let keep = match input.spec.get("keep") {
        None | Some(Value::Null) => KEEP,
        Some(v) => v.as_u64().filter(|k| *k >= 1).ok_or("keep: how many releases the site keeps, at least 1")?,
    };
    let mut out = vec![wave(object(S3, "Site", &site_name(bucket, &prefix), None, json!({ "bucket": bucket, "prefix": prefix, "site": site.to_ascii_lowercase(), "keep": keep })), 0)];
    if let Some(d) = text(input.spec, "distribution") {
        let mut spec = Map::new();
        spec.insert("originPath".into(), Value::String(format!("/{}", release_prefix(&prefix, site))));
        spec.insert("bucket".into(), Value::String(bucket.to_owned()));
        if let Some(o) = text(input.spec, "origin") {
            spec.insert("origin".into(), Value::String(o.to_owned()));
        }
        out.push(wave(object(CLOUDFRONT, "Distribution", d, None, Value::Object(spec)), 1));
    }
    Ok(out)
}

/// A site's file's content type, by its name.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

// ------------------------------------------------------------------ the answers → objects

/// A Site object from its `current.json`.
pub fn live_site(name: &str, bucket: &str, prefix: &str, current: &Value, keep: u64) -> Option<Value> {
    let site = current.get("site")?.as_str()?;
    let mut o = object(S3, "Site", name, None, json!({ "bucket": bucket, "prefix": prefix, "site": site, "keep": keep }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "path": current.get("path"), "files": current.get("files") }));
    }
    Some(o)
}

/// A Distribution object from GetDistribution's answer (as JSON): the origin path of the origin
/// `origin` (by its id), or of the one whose domain is `bucket`'s.
pub fn live_distribution(d: &Value, bucket: &str, origin: Option<&str>) -> Option<Value> {
    let id = d.get("Id")?.as_str()?;
    let origins = d.pointer("/DistributionConfig/Origins/Items/Origin").map(|o| if o.is_array() { o.as_array().cloned().unwrap_or_default() } else { vec![o.clone()] }).unwrap_or_default();
    let chosen = origins.iter().find(|o| match origin {
        Some(want) => o.get("Id").and_then(Value::as_str) == Some(want),
        None => o.get("DomainName").and_then(Value::as_str).is_some_and(|dn| dn.starts_with(&format!("{bucket}.s3"))),
    })?;
    let mut spec = Map::new();
    spec.insert("originPath".into(), Value::String(chosen.get("OriginPath").and_then(Value::as_str).unwrap_or_default().to_owned()));
    spec.insert("bucket".into(), Value::String(bucket.to_owned()));
    if let Some(o) = origin {
        spec.insert("origin".into(), Value::String(o.to_owned()));
    }
    let mut o = object(CLOUDFRONT, "Distribution", id, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "status": d.get("Status"), "domain": d.get("DomainName"), "origin": chosen.get("Id") }));
    }
    Some(o)
}

/// A distribution's config with the origin path of the origin `origin` (by its id; by default
/// the one whose domain is `bucket`'s) set to `path`: the config as CloudFront gave it, that one
/// element changed.
pub fn with_origin_path(config: &str, bucket: &str, origin: Option<&str>, path: &str) -> Result<String, String> {
    let mut out = String::with_capacity(config.len() + path.len());
    let mut rest = config;
    let mut done = false;
    while let Some(at) = rest.find("<Origin>") {
        let Some(end) = rest.get(at..).and_then(|r| r.find("</Origin>")).map(|e| at + e + "</Origin>".len()) else { break };
        out.push_str(rest.get(..at).unwrap_or_default());
        let block = rest.get(at..end).unwrap_or_default();
        let field = |tag: &str| block.split_once(&format!("<{tag}>")).and_then(|(_, r)| r.split_once(&format!("</{tag}>"))).map(|(v, _)| v.to_owned());
        let ours = match origin {
            Some(want) => field("Id").as_deref() == Some(want),
            None => field("DomainName").is_some_and(|d| d.starts_with(&format!("{bucket}.s3"))),
        };
        if ours && !done {
            let replaced = if let Some((before, after)) = block.split_once("<OriginPath>") {
                let after = after.split_once("</OriginPath>").map(|(_, a)| a).unwrap_or(after);
                format!("{before}<OriginPath>{path}</OriginPath>{after}")
            } else if block.contains("<OriginPath/>") {
                block.replacen("<OriginPath/>", &format!("<OriginPath>{path}</OriginPath>"), 1)
            } else {
                block.replacen("</DomainName>", &format!("</DomainName><OriginPath>{path}</OriginPath>"), 1)
            };
            out.push_str(&replaced);
            done = true;
        } else {
            out.push_str(block);
        }
        rest = rest.get(end..).unwrap_or_default();
    }
    out.push_str(rest);
    if done {
        Ok(out)
    } else {
        Err(match origin {
            Some(o) => format!("the distribution has no origin {o}"),
            None => format!("the distribution has no origin in {bucket} (name it: origin)"),
        })
    }
}

// ------------------------------------------------------------------ health

pub fn health(obj: &Value) -> Option<Status> {
    match crate::obj::kind(obj) {
        "Site" => status(Health::Healthy, crate::obj::str_at(obj, &["status", "path"]).unwrap_or_default()),
        "Distribution" => match crate::obj::str_at(obj, &["status", "status"]) {
            Some("Deployed") => status(Health::Healthy, "deployed"),
            Some("InProgress") => status(Health::Progressing, "CloudFront is deploying the change"),
            Some(s) => status(Health::Progressing, s),
            None => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12";

    fn render_www(extra: Value) -> Result<Vec<Value>, String> {
        let mut spec = json!({ "bucket": "shop-www", "site": format!("sha256:{HEX}"), "distribution": "E2TEST" });
        if let (Value::Object(m), Value::Object(e)) = (&mut spec, extra) {
            m.extend(e);
        }
        let none = Map::new();
        render(&RenderInput { spec: &spec, target: &none, app: "acme/shop-staging", namespace: "", revision: "abc", read: &|_| None })
    }

    #[test]
    fn a_definition_renders_its_site_and_the_switch_of_its_distribution() -> Result<(), String> {
        let objs = render_www(json!({}))?;
        let (site, dist) = (objs.first().ok_or("site")?, objs.get(1).ok_or("distribution")?);
        assert_eq!(crate::obj::name(site), "shop-www.sites.shop-staging", "the bucket and prefix, no slashes");
        assert_eq!(site.pointer("/spec/prefix"), Some(&json!("sites/shop-staging")), "by default sites/<app>");
        assert_eq!(site.pointer("/spec/keep"), Some(&json!(KEEP)));
        assert_eq!(dist.pointer("/spec/originPath"), Some(&json!("/sites/shop-staging/ab12cd34ef56ab12")));
        assert_eq!((crate::sync::wave_of(site), crate::sync::wave_of(dist)), (0, 1), "the files first, then the switch");
        assert_eq!(render_www(json!({ "distribution": null }))?.len(), 1, "a site without CloudFront");
        assert!(render_www(json!({ "site": "dist.zip" })).is_err());
        assert!(render_www(json!({ "prefix": "a/../b" })).is_err());
        assert_eq!(content_type("assets/app.JS"), "text/javascript; charset=utf-8");
        Ok(())
    }

    #[test]
    fn a_distributions_origin_path_is_switched_in_its_config() -> Result<(), String> {
        let config = "<DistributionConfig><CallerReference>x</CallerReference><Origins><Quantity>2</Quantity><Items><Origin><Id>api</Id><DomainName>api.example.com</DomainName><OriginPath>/v1</OriginPath></Origin><Origin><Id>www</Id><DomainName>shop-www.s3.us-east-1.amazonaws.com</DomainName><OriginPath></OriginPath><S3OriginConfig><OriginAccessIdentity></OriginAccessIdentity></S3OriginConfig></Origin></Items></Origins><Enabled>true</Enabled></DistributionConfig>";
        let switched = with_origin_path(config, "shop-www", None, "/sites/shop/ab12")?;
        assert!(switched.contains("<Id>www</Id><DomainName>shop-www.s3.us-east-1.amazonaws.com</DomainName><OriginPath>/sites/shop/ab12</OriginPath>"), "{switched}");
        assert!(switched.contains("<OriginPath>/v1</OriginPath>"), "the other origin as it was");
        assert_eq!(switched.len(), config.len() + "/sites/shop/ab12".len());
        assert!(with_origin_path(config, "other", None, "/x").is_err_and(|e| e.contains("no origin in other")));
        assert!(with_origin_path(config, "shop-www", Some("api"), "/v2")?.contains("<OriginPath>/v2</OriginPath>"));
        let d = json!({ "Id": "E2TEST", "Status": "InProgress", "DomainName": "d1.cloudfront.net", "DistributionConfig": { "Origins": { "Items": { "Origin": [{ "Id": "api", "DomainName": "api.example.com", "OriginPath": "/v1" }, { "Id": "www", "DomainName": "shop-www.s3.us-east-1.amazonaws.com", "OriginPath": "/sites/shop/ab12" }] } } } });
        let live = live_distribution(&d, "shop-www", None).ok_or("live")?;
        assert_eq!(live.pointer("/spec/originPath"), Some(&json!("/sites/shop/ab12")));
        assert_eq!(health(&live).map(|h| h.health), Some(Health::Progressing));
        Ok(())
    }
}
