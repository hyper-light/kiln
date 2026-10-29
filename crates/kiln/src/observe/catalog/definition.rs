//! A service's definition (`docs/observe.md` §10.8): Datadog's schema v2.2 (v2.1 read the same),
//! checked as its JSON schema checks it: what it does not name refused, its version, each contact's
//! address, each link's URL, the on-call integrations' service URLs.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// A definition, as Datadog's v2.2 says it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Definition {
    pub schema_version: String,
    pub dd_service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<Contact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrations: Option<Integrations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci_pipeline_fingerprints: Vec<String>,
}

/// What a service is (the schema's `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Web,
    Db,
    Cache,
    Function,
    Browser,
    Mobile,
    Custom,
}

/// Who to reach: an email address, or a channel's URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub contact: String,
}

fn other() -> String {
    "other".into()
}

/// A link: a runbook, docs, a repository, a dashboard, another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub name: String,
    #[serde(rename = "type", default = "other")]
    pub kind: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Its on-call, in PagerDuty or Opsgenie.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integrations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagerduty: Option<PagerDuty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opsgenie: Option<Opsgenie>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PagerDuty {
    pub service_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Opsgenie {
    pub service_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Region {
    #[serde(rename = "US")]
    Us,
    #[serde(rename = "EU")]
    Eu,
}

impl Definition {
    /// Whether it names an on-call (PagerDuty or Opsgenie).
    pub fn on_call(&self) -> bool {
        self.integrations.as_ref().is_some_and(|i| i.pagerduty.is_some() || i.opsgenie.is_some())
    }

    /// Whether a link of one of `kinds` is among its links.
    pub fn links_to(&self, kinds: &[&str]) -> bool {
        self.links.iter().any(|l| kinds.contains(&l.kind.as_str()))
    }
}

/// The definitions `text` holds: JSON (one, or a list of them), or YAML (one or several, `---`
/// apart; an empty document passed over). Each checked ([`check`]).
pub fn parse(text: &str) -> Result<Vec<Definition>, String> {
    let t = text.trim_start();
    let defs: Vec<Definition> = if t.starts_with('[') {
        serde_json::from_str(t).map_err(|e| format!("the definitions: {e}"))?
    } else if t.starts_with('{') {
        vec![serde_json::from_str(t).map_err(|e| format!("the definition: {e}"))?]
    } else {
        let mut out = Vec::new();
        for (i, doc) in serde_yaml::Deserializer::from_str(text).enumerate() {
            let v = serde_yaml::Value::deserialize(doc).map_err(|e| format!("document {}: {e}", i + 1))?;
            if v.is_null() {
                continue;
            }
            out.push(serde_yaml::from_value(v).map_err(|e| format!("document {}: {e}", i + 1))?);
        }
        out
    };
    if defs.is_empty() {
        return Err("no definition".into());
    }
    for d in &defs {
        check(d)?;
    }
    Ok(defs)
}

/// An email address, as a schema's `format: email` asks: one `@`, something before it, a domain
/// with a dot after it, no space.
fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.contains('@') && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !s.contains(char::is_whitespace),
        None => false,
    }
}

/// A URI, as a schema's `format: uri` asks: a scheme (a letter, then letters, digits, `+`, `-` or
/// `.`), a colon, and more; no space.
fn is_uri(s: &str) -> bool {
    match s.split_once(':') {
        Some((scheme, rest)) => {
            scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !rest.is_empty()
                && !s.contains(char::is_whitespace)
        }
        None => false,
    }
}

fn patterns() -> &'static (Option<regex::Regex>, Option<regex::Regex>) {
    static P: OnceLock<(Option<regex::Regex>, Option<regex::Regex>)> = OnceLock::new();
    P.get_or_init(|| {
        (
            regex::Regex::new(r"^(https?://)?[a-zA-Z\d_\-.]+\.pagerduty\.com/service-directory/(P[a-zA-Z\d_\-]+)/?$").ok(),
            regex::Regex::new(r"^(https?://)?[a-zA-Z\d_\-.]+\.opsgenie\.com/service/([a-zA-Z\d_\-]+)/?$").ok(),
        )
    })
}

/// `d` checked as Datadog's schema checks it.
pub fn check(d: &Definition) -> Result<(), String> {
    let name = d.dd_service.trim();
    let who = if name.is_empty() { "a definition".to_owned() } else { format!("{name:?}") };
    if !matches!(d.schema_version.as_str(), "v2.2" | "v2.1") {
        return Err(format!("{who}: schema-version v2.2 (or v2.1), not {:?}", d.schema_version));
    }
    if name.is_empty() || name.len() > 200 || name.contains(char::is_whitespace) {
        return Err(format!("{who}: dd-service, a name of 1 to 200 characters and no space"));
    }
    for c in &d.contacts {
        if c.name.as_deref().is_some_and(|n| n.chars().count() < 2) {
            return Err(format!("{who}: a contact's name, 2 characters at least"));
        }
        if c.kind.trim().is_empty() {
            return Err(format!("{who}: a contact's type"));
        }
        let ok = if c.kind == "email" { is_email(&c.contact) } else { is_uri(&c.contact) };
        if !ok {
            return Err(format!("{who}: contact {:?}: {}", c.contact, if c.kind == "email" { "an email address" } else { "a URI" }));
        }
    }
    for l in &d.links {
        if l.name.trim().is_empty() {
            return Err(format!("{who}: a link's name"));
        }
        if !is_uri(&l.url) {
            return Err(format!("{who}: link {:?}: its url a URI", l.name));
        }
    }
    if let Some(i) = &d.integrations {
        let (pd, og) = patterns();
        if let Some(p) = &i.pagerduty
            && !pd.as_ref().is_some_and(|r| r.is_match(&p.service_url))
        {
            return Err(format!("{who}: integrations.pagerduty.service-url, a service's URL in PagerDuty's service directory"));
        }
        if let Some(o) = &i.opsgenie
            && !og.as_ref().is_some_and(|r| r.is_match(&o.service_url))
        {
            return Err(format!("{who}: integrations.opsgenie.service-url, a service's URL in Opsgenie"));
        }
    }
    if d.tags.iter().any(|t| t.trim().is_empty()) {
        return Err(format!("{who}: an empty tag"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const SHOP: &str = r#"
schema-version: v2.2
dd-service: shop-api
team: checkout
application: shop
description: Takes orders.
tier: "1"
lifecycle: production
type: web
languages: [rust]
contacts:
  - name: Checkout on-call
    type: email
    contact: checkout@example.com
  - type: slack
    contact: https://acme.slack.com/archives/C123
links:
  - name: Runbook
    type: runbook
    url: https://wiki.example.com/shop/runbook
  - name: Source
    type: repo
    provider: github
    url: https://github.com/acme/shop
integrations:
  pagerduty:
    service-url: https://acme.pagerduty.com/service-directory/PSHOP12
  opsgenie:
    service-url: https://acme.app.opsgenie.com/service/abc-123
    region: EU
tags: ["product:shop", critical]
extensions:
  acme.com/cost-center: "42"
---
schema-version: v2.1
dd-service: shop-worker
---
"#;

    /// Two documents of one file read, every field as written; an empty one passed over.
    #[test]
    fn definitions_read_as_datadog_writes_them() {
        let d = parse(SHOP).unwrap();
        assert_eq!(d.len(), 2);
        let s = &d[0];
        assert_eq!((s.dd_service.as_str(), s.team.as_deref(), s.kind, s.tier.as_deref()), ("shop-api", Some("checkout"), Some(Kind::Web), Some("1")));
        assert_eq!(s.contacts.len(), 2);
        assert_eq!(s.links[1].provider.as_deref(), Some("github"));
        assert!(s.on_call() && s.links_to(&["runbook"]) && !s.links_to(&["dashboard"]));
        assert_eq!(s.integrations.as_ref().and_then(|i| i.opsgenie.as_ref()).and_then(|o| o.region), Some(Region::Eu));
        assert_eq!(d[1].dd_service, "shop-worker");
        // JSON: one, or a list, the same.
        let json = serde_json::to_string(&d[0]).unwrap();
        assert_eq!(parse(&json).unwrap(), vec![d[0].clone()]);
        assert_eq!(parse(&serde_json::to_string(&d).unwrap()).unwrap(), d);
    }

    /// What the schema refuses is refused, saying what.
    #[test]
    fn what_the_schema_refuses_is_refused() {
        for (text, says) in [
            ("schema-version: v2.2\ndd-service: a\nowner: x\n", "owner"),
            ("schema-version: v2\ndd-service: a\n", "schema-version"),
            ("schema-version: v2.2\ndd-service: two words\n", "dd-service"),
            ("schema-version: v2.2\ndd-service: a\ntype: daemon\n", "daemon"),
            ("schema-version: v2.2\ndd-service: a\ncontacts: [{type: email, contact: nobody}]\n", "email address"),
            ("schema-version: v2.2\ndd-service: a\ncontacts: [{type: slack, contact: '#general'}]\n", "a URI"),
            ("schema-version: v2.2\ndd-service: a\ncontacts: [{name: x, type: email, contact: a@b.co}]\n", "2 characters"),
            ("schema-version: v2.2\ndd-service: a\nlinks: [{name: r, type: runbook, url: wiki/page}]\n", "URI"),
            ("schema-version: v2.2\ndd-service: a\nintegrations: {pagerduty: {service-url: https://acme.pagerduty.com/services/X}}\n", "pagerduty"),
            ("schema-version: v2.2\ndd-service: a\nintegrations: {opsgenie: {service-url: https://acme.app.opsgenie.com/service/x, region: APAC}}\n", "APAC"),
            ("---\n", "no definition"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(says), "{text:?}: {e}");
        }
    }
}
