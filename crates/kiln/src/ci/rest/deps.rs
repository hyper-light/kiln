//! The dependency graph's compare (docs/ci.md W14-R11), as `actions/dependency-review-action`
//! asks it: `GET /repos/{o}/{r}/dependency-graph/compare/{base}...{head}[?name=<manifest>]`.
//!
//! What the lockfiles kiln reads (W18-R03's) pin at the two commits (`base` taken at its merge
//! base with `head`, as GitHub takes it), package by package: each one removed and each one added,
//! a version changed as both. Their vulnerabilities come from `[ci.advisories]`'s database (OSV's
//! API, which carries GitHub's advisories), for the packages added. Without one the answer lists
//! none and says so in the header the action shows (`x-github-dependency-graph-snapshot-warnings`),
//! as an Enterprise Server without GitHub Connect has no advisories to give. One that doesn't
//! answer fails the request, so a review never passes for want of it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::Engine as _;
use http::StatusCode;
use kiln_ci::sbom::{Eco, Package};
use serde_json::{Value, json};

use super::repos::Site;
use crate::ci::Fail;
use crate::http::server::{self as srv, Response};

/// The header whose (base64) text the action shows as a warning.
const WARNINGS: &str = "x-github-dependency-graph-snapshot-warnings";

/// GitHub's name for an ecosystem, as its dependency graph says it.
fn ecosystem(e: Eco) -> &'static str {
    match e {
        Eco::Cargo => "rust",
        Eco::Npm => "npm",
        Eco::Golang => "go",
        Eco::Pypi => "pip",
        Eco::Gem => "rubygems",
        Eco::Composer => "composer",
        Eco::Maven => "maven",
        Eco::Nuget => "nuget",
        Eco::Hex => "erlang",
        Eco::Pub => "pub",
        Eco::Swift => "swift",
        Eco::Github => "actions",
        other => other.purl_type(),
    }
}

/// One package's change.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub added: bool,
    pub manifest: String,
    pub package: Package,
}

/// What changed from `before` to `after`, lockfile by lockfile (in path order), each removed
/// package before each added one, by name: a version changed is both.
pub fn changes(before: &[(String, Vec<Package>)], after: &[(String, Vec<Package>)]) -> Vec<Change> {
    let paths: BTreeSet<&String> = before.iter().chain(after).map(|(p, _)| p).collect();
    let key = |p: &Package| (p.eco.purl_type().to_owned(), p.name.clone(), p.version.clone());
    let mut out = Vec::new();
    for path in paths {
        let of = |list: &[(String, Vec<Package>)]| -> BTreeMap<(String, String, String), Package> {
            list.iter().filter(|(p, _)| p == path).flat_map(|(_, pk)| pk.iter().map(|p| (key(p), p.clone()))).collect()
        };
        let (b, a) = (of(before), of(after));
        out.extend(b.iter().filter(|(k, _)| !a.contains_key(*k)).map(|(_, p)| Change { added: false, manifest: path.clone(), package: p.clone() }));
        out.extend(a.iter().filter(|(k, _)| !b.contains_key(*k)).map(|(_, p)| Change { added: true, manifest: path.clone(), package: p.clone() }));
    }
    out
}

/// A vulnerability as GitHub's compare gives it.
fn vulnerability(severity: &str, ghsa: &str, summary: &str, url: &str) -> Value {
    json!({ "severity": severity, "advisory_ghsa_id": ghsa, "advisory_summary": summary, "advisory_url": url })
}

/// A change in GitHub's shape. A scope isn't said (none of the lockfiles kiln reads says one
/// reliably), so the action takes it as `runtime`, as it does an Enterprise Server's.
fn change_json(c: &Change, vulns: &[Value]) -> Value {
    let p = &c.package;
    json!({
        "change_type": if c.added { "added" } else { "removed" },
        "manifest": c.manifest, "ecosystem": ecosystem(p.eco), "name": p.name, "version": p.version, "package_url": p.purl(),
        "license": Value::Null, "source_repository_url": Value::Null, "vulnerabilities": vulns,
    })
}

/// GitHub's severity of a CVSS v3 vector's base score: `critical` from 9, `high` from 7,
/// `moderate` from 4, else `low`; `None` if it isn't one.
pub fn cvss3_severity(vector: &str) -> Option<&'static str> {
    let mut m: BTreeMap<&str, &str> = BTreeMap::new();
    let mut parts = vector.split('/');
    if !parts.next()?.starts_with("CVSS:3") {
        return None;
    }
    for p in parts {
        let (k, v) = p.split_once(':')?;
        m.insert(k, v);
    }
    let pick = |k: &str, table: &[(&str, f64)]| -> Option<f64> { table.iter().find(|(n, _)| Some(*n) == m.get(k).copied()).map(|(_, v)| *v) };
    let changed = *m.get("S")? == "C";
    let av = pick("AV", &[("N", 0.85), ("A", 0.62), ("L", 0.55), ("P", 0.2)])?;
    let ac = pick("AC", &[("L", 0.77), ("H", 0.44)])?;
    let pr = pick("PR", &[("N", 0.85), ("L", if changed { 0.68 } else { 0.62 }), ("H", if changed { 0.5 } else { 0.27 })])?;
    let ui = pick("UI", &[("N", 0.85), ("R", 0.62)])?;
    let cia = [("H", 0.56), ("L", 0.22), ("N", 0.0)];
    let (c, i, a) = (pick("C", &cia)?, pick("I", &cia)?, pick("A", &cia)?);
    let iss = 1.0 - (1.0 - c) * (1.0 - i) * (1.0 - a);
    let impact = if changed { 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15) } else { 6.42 * iss };
    let exploitability = 8.22 * av * ac * pr * ui;
    // CVSS's round up to one decimal.
    let up = |x: f64| (x * 10.0 - 1e-9).ceil() / 10.0;
    let score = if impact <= 0.0 {
        0.0
    } else if changed {
        up((1.08 * (impact + exploitability)).min(10.0))
    } else {
        up((impact + exploitability).min(10.0))
    };
    Some(match score {
        s if s >= 9.0 => "critical",
        s if s >= 7.0 => "high",
        s if s >= 4.0 => "moderate",
        _ => "low",
    })
}

/// An OSV record's vulnerability in GitHub's shape: its GHSA (its own id, or its alias), the
/// severity GitHub gives it (the GHSA record's), else its CVSS v3 vector's.
fn from_osv(record: &Value, ghsa_record: Option<&Value>) -> Value {
    let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
    let ghsa = if id.starts_with("GHSA-") {
        id.to_owned()
    } else {
        record.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).find(|a| a.starts_with("GHSA-")).unwrap_or(id).to_owned()
    };
    let severity = [ghsa_record, Some(record)]
        .into_iter()
        .flatten()
        .find_map(|r| r.pointer("/database_specific/severity").and_then(Value::as_str).map(str::to_ascii_lowercase))
        .filter(|s| matches!(s.as_str(), "critical" | "high" | "moderate" | "low"))
        .or_else(|| record.get("severity").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.get("score").and_then(Value::as_str)).find_map(cvss3_severity).map(str::to_owned))
        .unwrap_or_else(|| "moderate".to_owned());
    let summary = record.get("summary").and_then(Value::as_str).or_else(|| record.get("details").and_then(Value::as_str).and_then(|d| d.lines().next())).unwrap_or(id);
    let url = if ghsa.starts_with("GHSA-") { format!("https://github.com/advisories/{ghsa}") } else { format!("https://osv.dev/vulnerability/{id}") };
    vulnerability(&severity, &ghsa, summary, &url)
}

/// The vulnerabilities OSV at `base` knows of in each package, in order.
async fn osv(base: &str, packages: &[&Package]) -> anyhow::Result<Vec<Vec<Value>>> {
    let http = crate::http::client::Client::public()?;
    let base = base.trim_end_matches('/');
    let mut out = Vec::with_capacity(packages.len());
    let mut records: BTreeMap<String, Value> = BTreeMap::new();
    for chunk in packages.chunks(500) {
        let queries: Vec<Value> = chunk.iter().map(|p| json!({ "package": { "purl": p.purl() } })).collect();
        let resp = http.post(&format!("{base}/v1/querybatch")).json(&json!({ "queries": queries })).timeout(Duration::from_secs(60)).send().await?;
        anyhow::ensure!(resp.is_success(), "{base}/v1/querybatch: HTTP {}", resp.status());
        let got: Value = resp.json(32 << 20).await?;
        let results = got.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
        for i in 0..chunk.len() {
            let ids: Vec<String> =
                results.get(i).and_then(|r| r.get("vulns")).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
            let mut vulns: Vec<Value> = Vec::new();
            for id in ids {
                let record = match records.get(&id) {
                    Some(r) => r.clone(),
                    None => {
                        let r = get_record(&http, base, &id).await?;
                        records.insert(id.clone(), r.clone());
                        r
                    }
                };
                // The GHSA record's severity is the one GitHub gives.
                let alias = record.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).find(|a| a.starts_with("GHSA-")).map(str::to_owned);
                let ghsa_record = match alias {
                    Some(a) if !id.starts_with("GHSA-") => match records.get(&a) {
                        Some(r) => Some(r.clone()),
                        None => {
                            let r = get_record(&http, base, &a).await.ok();
                            if let Some(r) = &r {
                                records.insert(a, r.clone());
                            }
                            r
                        }
                    },
                    _ => None,
                };
                let v = from_osv(&record, ghsa_record.as_ref());
                if !vulns.iter().any(|x| x.get("advisory_ghsa_id") == v.get("advisory_ghsa_id")) {
                    vulns.push(v);
                }
            }
            out.push(vulns);
        }
    }
    Ok(out)
}

async fn get_record(http: &crate::http::client::Client, base: &str, id: &str) -> anyhow::Result<Value> {
    let resp = http.get(&format!("{base}/v1/vulns/{id}")).timeout(Duration::from_secs(60)).send().await?;
    anyhow::ensure!(resp.is_success(), "{base}/v1/vulns/{id}: HTTP {}", resp.status());
    resp.json(8 << 20).await
}

/// `GET /repos/{o}/{r}/dependency-graph/compare/{basehead}[?name=]`.
pub async fn compare(s: &Site<'_>, basehead: &str, name: Option<&str>) -> Result<Response, Fail> {
    let Some((base, head)) = basehead.split_once("...") else {
        return Ok(super::message(StatusCode::UNPROCESSABLE_ENTITY, "basehead is `{base}...{head}`"));
    };
    let (Some(b), Some(h)) = (s.resolve(base).await, s.resolve(head).await) else { return Ok(super::message(StatusCode::NOT_FOUND, "Not Found")) };
    let mb = s.merge_base(&b, &h).await.unwrap_or(b);
    let keep = |list: Vec<(String, Vec<Package>)>| -> Vec<(String, Vec<Package>)> { list.into_iter().filter(|(p, _)| name.is_none_or(|n| n == p)).collect() };
    let before = keep(crate::ci::diffs::lockfiles_at(s.hub, &s.mirror, &mb).await?);
    let after = keep(crate::ci::diffs::lockfiles_at(s.hub, &s.mirror, &h).await?);
    let list = changes(&before, &after);
    let added: Vec<&Package> = list.iter().filter(|c| c.added).map(|c| &c.package).collect();
    let (vulns, warning) = match s.hub.cfg.ci.advisories.osv.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(url) => match osv(url, &added).await {
            Ok(v) => (v, None),
            Err(e) => return Ok(super::message(StatusCode::SERVICE_UNAVAILABLE, &format!("the advisory database ({url}) didn't answer, so no review: {e:#}"))),
        },
        None => (Vec::new(), Some("kiln checked these changes against no advisory database: the server's [ci.advisories] names none, so no vulnerabilities are listed.")),
    };
    // The vulnerabilities are the added packages', in order.
    let mut found = vulns.into_iter();
    let body: Vec<Value> = list.iter().map(|c| change_json(c, &if c.added { found.next().unwrap_or_default() } else { Vec::new() })).collect();
    let resp = srv::json(StatusCode::OK, &Value::Array(body));
    Ok(match warning {
        Some(w) => srv::with_header(resp, http::HeaderName::from_static(WARNINGS), &base64::engine::general_purpose::STANDARD.encode(w)),
        None => resp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(eco: Eco, name: &str, version: &str) -> Package {
        Package { eco, name: name.into(), version: version.into(), hashes: Vec::new(), source: None }
    }

    #[test]
    fn w14_r11_dependency_changes_as_github_lists_them() {
        let before =
            vec![("Cargo.lock".to_owned(), vec![pk(Eco::Cargo, "serde", "1.0.1"), pk(Eco::Cargo, "log", "0.4.0")]), ("web/package-lock.json".to_owned(), vec![pk(Eco::Npm, "left-pad", "1.0.0")])];
        let after = vec![("Cargo.lock".to_owned(), vec![pk(Eco::Cargo, "serde", "1.0.2"), pk(Eco::Cargo, "log", "0.4.0"), pk(Eco::Cargo, "anyhow", "1.0.0")])];
        let got: Vec<Value> = changes(&before, &after).iter().map(|c| change_json(c, &[])).collect();
        let short: Vec<(String, String, String, String)> = got
            .iter()
            .map(|v| {
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                (s("change_type"), s("manifest"), s("name"), s("version"))
            })
            .collect();
        let want = [
            ("removed", "Cargo.lock", "serde", "1.0.1"),
            ("added", "Cargo.lock", "anyhow", "1.0.0"),
            ("added", "Cargo.lock", "serde", "1.0.2"),
            ("removed", "web/package-lock.json", "left-pad", "1.0.0"),
        ]
        .map(|(a, b, c, d)| (a.to_owned(), b.to_owned(), c.to_owned(), d.to_owned()));
        assert_eq!(short, want);
        assert_eq!(got.first().and_then(|v| v.get("ecosystem")), Some(&json!("rust")));
        assert_eq!(got.first().and_then(|v| v.get("package_url")), Some(&json!("pkg:cargo/serde@1.0.1")));
        assert_eq!(got.get(3).and_then(|v| v.get("ecosystem")), Some(&json!("npm")));
    }

    #[test]
    fn w14_r11_severities_as_github_gives_them() {
        // CVSS v3.1 base scores: 9.8, 7.5, 5.3, 3.1, and a scope change (10.0).
        assert_eq!(cvss3_severity("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), Some("critical"));
        assert_eq!(cvss3_severity("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"), Some("high"));
        assert_eq!(cvss3_severity("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"), Some("moderate"));
        assert_eq!(cvss3_severity("CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:U/C:L/I:N/A:N"), Some("low"));
        assert_eq!(cvss3_severity("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H"), Some("critical"));
        assert_eq!(cvss3_severity("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N"), None);
        // A RUSTSEC record with a GHSA alias: the GHSA's id and severity; one with neither, its CVSS.
        let rustsec = json!({ "id": "RUSTSEC-2024-0001", "aliases": ["CVE-2024-1", "GHSA-aaaa-bbbb-cccc"], "summary": "bad", "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:U/C:L/I:N/A:N" }] });
        let ghsa = json!({ "id": "GHSA-aaaa-bbbb-cccc", "database_specific": { "severity": "HIGH" } });
        assert_eq!(from_osv(&rustsec, Some(&ghsa)), vulnerability("high", "GHSA-aaaa-bbbb-cccc", "bad", "https://github.com/advisories/GHSA-aaaa-bbbb-cccc"));
        let alone = json!({ "id": "PYSEC-2024-9", "details": "first line\nmore", "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }] });
        assert_eq!(from_osv(&alone, None), vulnerability("critical", "PYSEC-2024-9", "first line", "https://osv.dev/vulnerability/PYSEC-2024-9"));
    }
}
