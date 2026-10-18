//! SBOMs from lockfiles (docs/ci.md W18-R03): the packages a lockfile pins, as SPDX 2.3 and
//! CycloneDX 1.6 documents. Pure: the caller passes the files' text, the time and the document's
//! unique names.
//!
//! Lockfiles read: `Cargo.lock`, `package-lock.json` (v1 to v3), `pnpm-lock.yaml` (v5 to v9),
//! `yarn.lock` (classic and berry), `go.sum` (else `go.mod`), `uv.lock`, `poetry.lock`,
//! `requirements*.txt` (pinned lines), `Gemfile.lock` and `composer.lock`. Other ecosystems come
//! from another tool's SPDX document ([`from_spdx`], `syft`'s on a runner that has it).

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::{Value, json};

/// A package ecosystem, as purls name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eco {
    Cargo,
    Npm,
    Golang,
    Pypi,
    Gem,
    Composer,
    // Those only another tool's document names.
    Maven,
    Nuget,
    Hex,
    Pub,
    Swift,
    Cocoapods,
    Conan,
    Github,
    Deb,
    Rpm,
    Apk,
    Generic,
}

/// Every ecosystem, with its purl type.
const ECOS: &[(Eco, &str)] = &[
    (Eco::Cargo, "cargo"),
    (Eco::Npm, "npm"),
    (Eco::Golang, "golang"),
    (Eco::Pypi, "pypi"),
    (Eco::Gem, "gem"),
    (Eco::Composer, "composer"),
    (Eco::Maven, "maven"),
    (Eco::Nuget, "nuget"),
    (Eco::Hex, "hex"),
    (Eco::Pub, "pub"),
    (Eco::Swift, "swift"),
    (Eco::Cocoapods, "cocoapods"),
    (Eco::Conan, "conan"),
    (Eco::Github, "github"),
    (Eco::Deb, "deb"),
    (Eco::Rpm, "rpm"),
    (Eco::Apk, "apk"),
    (Eco::Generic, "generic"),
];

impl Eco {
    fn purl_type(self) -> &'static str {
        ECOS.iter().find(|(e, _)| *e == self).map_or("generic", |(_, t)| t)
    }

    /// Whether kiln reads this ecosystem's lockfiles itself.
    pub fn built_in(self) -> bool {
        matches!(self, Eco::Cargo | Eco::Npm | Eco::Golang | Eco::Pypi | Eco::Gem | Eco::Composer)
    }

    /// Whether a name keeps its slashes (a scope, a namespace, a module path).
    fn namespaced(self) -> bool {
        !matches!(self, Eco::Cargo | Eco::Pypi | Eco::Gem | Eco::Nuget | Eco::Hex | Eco::Pub | Eco::Cocoapods | Eco::Conan)
    }
}

/// One package a lockfile pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub eco: Eco,
    pub name: String,
    pub version: String,
    /// (`SHA256`, hex), …, as the lockfile has them.
    pub hashes: Vec<(&'static str, String)>,
    /// Where it was downloaded from, when the lockfile says.
    pub source: Option<String>,
}

/// Percent-encodes what a purl's name part may not hold as is.
fn purl_part(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

impl Package {
    /// Its package URL (`pkg:cargo/serde@1.0.200`, `pkg:npm/%40types/node@20.1.0`).
    pub fn purl(&self) -> String {
        // A scope or a module path keeps its slashes; each part is encoded.
        let name = if self.eco.namespaced() { self.name.split('/').map(purl_part).collect::<Vec<_>>().join("/") } else { purl_part(&self.name) };
        format!("pkg:{}/{name}@{}", self.eco.purl_type(), purl_part(&self.version))
    }
}

fn unpercent(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while let Some(&c) = b.get(i) {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (c, hex) {
            (b'%', Some(x)) => {
                out.push(x);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The package a purl names (`pkg:maven/org.apache/commons-io@2.15.0`), when it has a version.
pub fn from_purl(purl: &str) -> Option<Package> {
    let rest = purl.strip_prefix("pkg:")?;
    let rest = rest.split(['?', '#']).next()?;
    let (ty, rest) = rest.split_once('/')?;
    let (name, version) = rest.rsplit_once('@')?;
    let eco = ECOS.iter().find(|(_, t)| t.eq_ignore_ascii_case(ty)).map(|(e, _)| *e)?;
    let name = name.split('/').map(unpercent).collect::<Vec<_>>().join("/");
    let version = unpercent(version);
    (!name.is_empty() && !version.is_empty()).then_some(Package { eco, name, version, hashes: Vec::new(), source: None })
}

/// The packages of an SPDX document (JSON) that name a purl, with their checksums and where they
/// came from.
pub fn from_spdx(doc: &Value) -> Vec<Package> {
    let mut out = Vec::new();
    for p in doc.get("packages").and_then(Value::as_array).into_iter().flatten() {
        let purl = p.get("externalRefs").and_then(Value::as_array).into_iter().flatten().find(|r| r.get("referenceType").and_then(Value::as_str) == Some("purl")).and_then(|r| r.get("referenceLocator")).and_then(Value::as_str);
        let Some(mut pkg) = purl.and_then(from_purl) else { continue };
        for c in p.get("checksums").and_then(Value::as_array).into_iter().flatten() {
            let alg = match c.get("algorithm").and_then(Value::as_str) {
                Some("SHA1") => "SHA1",
                Some("SHA256") => "SHA256",
                Some("SHA384") => "SHA384",
                Some("SHA512") => "SHA512",
                _ => continue,
            };
            if let Some(v) = c.get("checksumValue").and_then(Value::as_str) {
                pkg.hashes.push((alg, v.to_ascii_lowercase()));
            }
        }
        pkg.source = p.get("downloadLocation").and_then(Value::as_str).filter(|l| !matches!(*l, "NOASSERTION" | "NONE" | "")).map(str::to_owned);
        out.push(pkg);
    }
    out
}

/// A lockfile kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Cargo,
    NpmLock,
    Pnpm,
    Yarn,
    GoSum,
    GoMod,
    Uv,
    Poetry,
    Requirements,
    Gemfile,
    Composer,
}

/// Which lockfile `file_name` is.
pub fn kind_of(file_name: &str) -> Option<Lock> {
    Some(match file_name {
        "Cargo.lock" => Lock::Cargo,
        "package-lock.json" | "npm-shrinkwrap.json" => Lock::NpmLock,
        "pnpm-lock.yaml" => Lock::Pnpm,
        "yarn.lock" => Lock::Yarn,
        "go.sum" => Lock::GoSum,
        "go.mod" => Lock::GoMod,
        "uv.lock" => Lock::Uv,
        "poetry.lock" => Lock::Poetry,
        "Gemfile.lock" => Lock::Gemfile,
        "composer.lock" => Lock::Composer,
        n if n.starts_with("requirements") && n.ends_with(".txt") => Lock::Requirements,
        _ => return None,
    })
}

/// `sha512-<base64>` (npm's integrity) as (`SHA512`, hex).
fn integrity(s: &str) -> Option<(&'static str, String)> {
    let (alg, b64) = s.split_whitespace().next()?.split_once('-')?;
    let alg = match alg {
        "sha512" => "SHA512",
        "sha256" => "SHA256",
        "sha1" => "SHA1",
        _ => return None,
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some((alg, bytes.iter().map(|b| format!("{b:02x}")).collect()))
}

/// `sha256:<hex>` as (`SHA256`, hex).
fn prefixed(s: &str) -> Option<(&'static str, String)> {
    let (alg, hex) = s.split_once(':')?;
    let alg = match alg {
        "sha256" => "SHA256",
        "sha512" => "SHA512",
        "sha1" => "SHA1",
        _ => return None,
    };
    Some((alg, hex.to_ascii_lowercase()))
}

fn toml_packages(text: &str) -> Result<Vec<toml::Table>, String> {
    let doc: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    Ok(doc.get("package").and_then(toml::Value::as_array).map(|a| a.iter().filter_map(|p| p.as_table().cloned()).collect()).unwrap_or_default())
}

fn tstr<'a>(t: &'a toml::Table, k: &str) -> Option<&'a str> {
    t.get(k).and_then(toml::Value::as_str)
}

/// A Python package's normalized name (PEP 503).
fn pep503(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if matches!(c, '_' | '.' | '-') { '-' } else { c.to_ascii_lowercase() };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out
}

/// npm's name for an entry of `packages` (`node_modules/@scope/name` → `@scope/name`).
fn npm_name(path: &str) -> Option<String> {
    let rest = path.rsplit_once("node_modules/").map(|(_, r)| r)?;
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// `name@version` with a scope's `@` kept: (`@scope/name`, `version`).
fn split_at_version(s: &str) -> Option<(String, String)> {
    let at = s.get(1..)?.find('@')? + 1;
    let (n, v) = (s.get(..at)?, s.get(at + 1..)?);
    (!n.is_empty() && !v.is_empty()).then(|| (n.to_owned(), v.to_owned()))
}

/// The packages `text` (a lockfile of `kind`) pins, in the order it lists them, each once.
pub fn parse(kind: Lock, text: &str) -> Result<Vec<Package>, String> {
    let mut out: Vec<Package> = Vec::new();
    let mut push = |p: Package| {
        if !out.iter().any(|q| q.eco == p.eco && q.name == p.name && q.version == p.version) {
            out.push(p);
        }
    };
    match kind {
        Lock::Cargo => {
            for p in toml_packages(text)? {
                let (Some(name), Some(version)) = (tstr(&p, "name"), tstr(&p, "version")) else { continue };
                let source = tstr(&p, "source").map(|s| s.trim_start_matches("registry+").trim_start_matches("git+").to_owned());
                let hashes = tstr(&p, "checksum").map(|c| vec![("SHA256", c.to_owned())]).unwrap_or_default();
                push(Package { eco: Eco::Cargo, name: name.into(), version: version.into(), hashes, source });
            }
        }
        Lock::NpmLock => {
            let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            if let Some(pkgs) = v.get("packages").and_then(Value::as_object) {
                for (path, p) in pkgs {
                    if path.is_empty() || p.get("link").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    let Some(name) = p.get("name").and_then(Value::as_str).map(str::to_owned).or_else(|| npm_name(path)) else { continue };
                    let Some(version) = p.get("version").and_then(Value::as_str) else { continue };
                    let hashes = p.get("integrity").and_then(Value::as_str).and_then(integrity).into_iter().collect();
                    push(Package { eco: Eco::Npm, name, version: version.into(), hashes, source: p.get("resolved").and_then(Value::as_str).map(str::to_owned) });
                }
            } else if let Some(deps) = v.get("dependencies").and_then(Value::as_object) {
                // lockfileVersion 1: nested `dependencies`.
                let mut todo: Vec<(&String, &Value)> = deps.iter().collect();
                while let Some((name, p)) = todo.pop() {
                    if let Some(version) = p.get("version").and_then(Value::as_str) {
                        let hashes = p.get("integrity").and_then(Value::as_str).and_then(integrity).into_iter().collect();
                        push(Package { eco: Eco::Npm, name: name.clone(), version: version.into(), hashes, source: p.get("resolved").and_then(Value::as_str).map(str::to_owned) });
                    }
                    if let Some(inner) = p.get("dependencies").and_then(Value::as_object) {
                        todo.extend(inner.iter());
                    }
                }
            }
        }
        Lock::Pnpm => {
            let v: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
            let v5 = v.get("lockfileVersion").and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).is_some_and(|n| n < 6.0);
            let Some(pkgs) = v.get("packages").and_then(serde_yaml::Value::as_mapping) else { return Ok(Vec::new()) };
            for (k, p) in pkgs {
                let Some(key) = k.as_str() else { continue };
                // `(peer@1.0.0)` suffixes, and v5's `_peer@1.0.0`.
                let key = key.split('(').next().unwrap_or(key).trim_start_matches('/');
                let parsed = if v5 {
                    key.rsplit_once('/').map(|(n, ver)| (n.to_owned(), ver.split('_').next().unwrap_or(ver).to_owned()))
                } else {
                    split_at_version(key)
                };
                let Some((name, version)) = parsed else { continue };
                let hashes = p.get("resolution").and_then(|r| r.get("integrity")).and_then(serde_yaml::Value::as_str).and_then(integrity).into_iter().collect();
                push(Package { eco: Eco::Npm, name, version, hashes, source: p.get("resolution").and_then(|r| r.get("tarball")).and_then(serde_yaml::Value::as_str).map(str::to_owned) });
            }
        }
        Lock::Yarn if text.contains("__metadata:") => {
            // Berry: YAML, each entry with its `resolution` (`name@npm:1.2.3`).
            let v: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
            for (k, p) in v.as_mapping().into_iter().flatten() {
                if k.as_str() == Some("__metadata") {
                    continue;
                }
                let Some(res) = p.get("resolution").and_then(serde_yaml::Value::as_str) else { continue };
                let Some((name, spec)) = split_at_version(res) else { continue };
                let Some(version) = spec.strip_prefix("npm:") else { continue };
                push(Package { eco: Eco::Npm, name, version: version.into(), hashes: Vec::new(), source: None });
            }
        }
        Lock::Yarn => {
            // Classic: a block per descriptor list, `version "x"` inside.
            let mut name: Option<String> = None;
            let mut source: Option<String> = None;
            let mut version: Option<String> = None;
            let mut hash: Option<(&'static str, String)> = None;
            let mut flush = |name: &mut Option<String>, version: &mut Option<String>, source: &mut Option<String>, hash: &mut Option<(&'static str, String)>| {
                if let (Some(n), Some(v)) = (name.take(), version.take()) {
                    push(Package { eco: Eco::Npm, name: n, version: v, hashes: hash.take().into_iter().collect(), source: source.take() });
                }
                *source = None;
                *hash = None;
            };
            for line in text.lines() {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                if !line.starts_with(' ') {
                    flush(&mut name, &mut version, &mut source, &mut hash);
                    let first = line.trim_end_matches(':').split(',').next().unwrap_or_default().trim().trim_matches('"');
                    name = split_at_version(first).map(|(n, _)| n);
                } else if let Some(v) = line.trim().strip_prefix("version ") {
                    version = Some(v.trim_matches('"').to_owned());
                } else if let Some(r) = line.trim().strip_prefix("resolved ") {
                    source = Some(r.trim_matches('"').split('#').next().unwrap_or_default().to_owned());
                } else if let Some(i) = line.trim().strip_prefix("integrity ") {
                    hash = integrity(i);
                }
            }
            flush(&mut name, &mut version, &mut source, &mut hash);
        }
        Lock::GoSum => {
            for line in text.lines() {
                let mut f = line.split_whitespace();
                let (Some(module), Some(version)) = (f.next(), f.next()) else { continue };
                if version.ends_with("/go.mod") {
                    continue;
                }
                push(Package { eco: Eco::Golang, name: module.into(), version: version.into(), hashes: Vec::new(), source: None });
            }
        }
        Lock::GoMod => {
            let mut block = false;
            for line in text.lines().map(|l| l.split("//").next().unwrap_or_default().trim()) {
                if line == "require (" {
                    block = true;
                    continue;
                }
                if block && line == ")" {
                    block = false;
                    continue;
                }
                let spec = if block { Some(line) } else { line.strip_prefix("require ").filter(|r| !r.starts_with('(')) };
                if let Some(spec) = spec {
                    let mut f = spec.split_whitespace();
                    if let (Some(m), Some(v)) = (f.next(), f.next()) {
                        push(Package { eco: Eco::Golang, name: m.into(), version: v.into(), hashes: Vec::new(), source: None });
                    }
                }
            }
        }
        Lock::Uv | Lock::Poetry => {
            for p in toml_packages(text)? {
                let (Some(name), Some(version)) = (tstr(&p, "name"), tstr(&p, "version")) else { continue };
                let hash = match kind {
                    Lock::Uv => p.get("sdist").and_then(|s| s.get("hash")).and_then(toml::Value::as_str).and_then(prefixed),
                    _ => p.get("files").and_then(toml::Value::as_array).and_then(|a| a.first()).and_then(|f| f.get("hash")).and_then(toml::Value::as_str).and_then(prefixed),
                };
                push(Package { eco: Eco::Pypi, name: pep503(name), version: version.into(), hashes: hash.into_iter().collect(), source: None });
            }
        }
        Lock::Requirements => {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or_default().trim();
                if line.is_empty() || line.starts_with('-') {
                    continue;
                }
                let spec = line.split(';').next().unwrap_or_default().trim();
                let Some((name, version)) = spec.split_once("==") else { continue };
                let name = name.split('[').next().unwrap_or(name).trim();
                let version = version.split_whitespace().next().unwrap_or_default();
                let hash = line.split_whitespace().find_map(|w| w.strip_prefix("--hash=")).and_then(prefixed);
                if !name.is_empty() && !version.is_empty() {
                    push(Package { eco: Eco::Pypi, name: pep503(name), version: version.into(), hashes: hash.into_iter().collect(), source: None });
                }
            }
        }
        Lock::Gemfile => {
            let mut specs = false;
            for line in text.lines() {
                if line.trim() == "specs:" {
                    specs = true;
                    continue;
                }
                if !line.starts_with(' ') {
                    specs = false;
                    continue;
                }
                // Four spaces: a gem; six: what it depends on.
                if specs && line.starts_with("    ") && !line.starts_with("     ") {
                    let l = line.trim();
                    if let Some((name, rest)) = l.split_once(" (") {
                        let version = rest.trim_end_matches(')');
                        push(Package { eco: Eco::Gem, name: name.into(), version: version.into(), hashes: Vec::new(), source: None });
                    }
                }
            }
        }
        Lock::Composer => {
            let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            for p in ["packages", "packages-dev"].iter().filter_map(|k| v.get(*k).and_then(Value::as_array)).flatten() {
                let (Some(name), Some(version)) = (p.get("name").and_then(Value::as_str), p.get("version").and_then(Value::as_str)) else { continue };
                let hash = p.pointer("/dist/shasum").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| ("SHA1", s.to_owned()));
                push(Package { eco: Eco::Composer, name: name.into(), version: version.into(), hashes: hash.into_iter().collect(), source: p.pointer("/dist/url").and_then(Value::as_str).map(str::to_owned) });
            }
        }
    }
    Ok(out)
}

/// What a document is about, and who made it when.
#[derive(Debug, Clone)]
pub struct Doc {
    /// What it describes (`acme/app`).
    pub name: String,
    /// Its version (a commit), when known.
    pub version: Option<String>,
    /// RFC 3339.
    pub created: String,
    /// `kiln-0.2.0`.
    pub tool: String,
    pub packages: Vec<Package>,
}

/// The packages once each, by purl.
fn unique(packages: &[Package]) -> Vec<&Package> {
    let mut by: BTreeMap<String, &Package> = BTreeMap::new();
    for p in packages {
        by.entry(p.purl()).or_insert(p);
    }
    by.into_values().collect()
}

/// An SPDX 2.3 document (JSON). `namespace` makes it unique (`https://kiln.example/spdx/<id>`).
pub fn spdx(doc: &Doc, namespace: &str) -> Value {
    let pkgs = unique(&doc.packages);
    let mut root = json!({ "name": doc.name, "SPDXID": "SPDXRef-Root", "downloadLocation": "NOASSERTION", "filesAnalyzed": false });
    if let (Some(v), Some(o)) = (&doc.version, root.as_object_mut()) {
        o.insert("versionInfo".into(), json!(v));
    }
    let mut packages = vec![root];
    let mut relationships = vec![json!({ "spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES", "relatedSpdxElement": "SPDXRef-Root" })];
    for (i, p) in pkgs.iter().enumerate() {
        let id = format!("SPDXRef-Package-{}", i + 1);
        let checksums: Vec<Value> = p.hashes.iter().map(|(alg, hex)| json!({ "algorithm": alg, "checksumValue": hex })).collect();
        let mut v = json!({
            "name": p.name, "SPDXID": id, "versionInfo": p.version,
            "downloadLocation": p.source.clone().unwrap_or_else(|| "NOASSERTION".into()), "filesAnalyzed": false,
            "externalRefs": [{ "referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl", "referenceLocator": p.purl() }],
        });
        if let (false, Some(o)) = (checksums.is_empty(), v.as_object_mut()) {
            o.insert("checksums".into(), Value::Array(checksums));
        }
        packages.push(v);
        relationships.push(json!({ "spdxElementId": "SPDXRef-Root", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": id }));
    }
    json!({
        "spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0", "SPDXID": "SPDXRef-DOCUMENT", "name": doc.name, "documentNamespace": namespace,
        "creationInfo": { "created": doc.created, "creators": [format!("Tool: {}", doc.tool)] },
        "packages": packages, "relationships": relationships,
    })
}

/// A CycloneDX 1.6 document (JSON). `serial` makes it unique (`urn:uuid:…`).
pub fn cyclonedx(doc: &Doc, serial: &str) -> Value {
    let pkgs = unique(&doc.packages);
    let components: Vec<Value> = pkgs
        .iter()
        .map(|p| {
            let mut c = json!({ "type": "library", "bom-ref": p.purl(), "name": p.name, "version": p.version, "purl": p.purl() });
            if let Some(o) = c.as_object_mut() {
                if !p.hashes.is_empty() {
                    let alg = |a: &str| match a {
                        "SHA512" => "SHA-512",
                        "SHA1" => "SHA-1",
                        _ => "SHA-256",
                    };
                    o.insert("hashes".into(), Value::Array(p.hashes.iter().map(|(a, h)| json!({ "alg": alg(a), "content": h })).collect()));
                }
                if let Some(s) = &p.source {
                    o.insert("externalReferences".into(), json!([{ "type": "distribution", "url": s }]));
                }
            }
            c
        })
        .collect();
    let (tool, tool_version) = doc.tool.split_once('-').unwrap_or((&doc.tool, ""));
    let mut component = json!({ "type": "application", "bom-ref": "root", "name": doc.name });
    if let (Some(v), Some(o)) = (&doc.version, component.as_object_mut()) {
        o.insert("version".into(), json!(v));
    }
    json!({
        "bomFormat": "CycloneDX", "specVersion": "1.6", "serialNumber": serial, "version": 1,
        "metadata": {
            "timestamp": doc.created,
            "tools": { "components": [{ "type": "application", "name": tool, "version": tool_version }] },
            "component": component,
        },
        "components": components,
        "dependencies": [{ "ref": "root", "dependsOn": pkgs.iter().map(|p| p.purl()).collect::<Vec<_>>() }],
    })
}

#[cfg(test)]
mod tests;
