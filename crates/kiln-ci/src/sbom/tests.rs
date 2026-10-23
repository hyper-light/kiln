use super::*;

fn names(kind: Lock, text: &str) -> Vec<String> {
    parse(kind, text).unwrap_or_default().iter().map(Package::purl).collect()
}

#[test]
fn w18_r03_lockfiles() {
    let cargo = "version = 3\n\n[[package]]\nname = \"kiln\"\nversion = \"0.2.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.200\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"ddc6f9cc94d67c0e21aaf7eda3a010fd3af78ebf6e096aa6e2e13c79749cce4f\"\n";
    assert_eq!(names(Lock::Cargo, cargo), ["pkg:cargo/kiln@0.2.0", "pkg:cargo/serde@1.0.200"]);
    let p = parse(Lock::Cargo, cargo).unwrap_or_default();
    assert_eq!(p.get(1).map(|p| (p.hashes.clone(), p.source.clone())), Some((vec![("SHA256", "ddc6f9cc94d67c0e21aaf7eda3a010fd3af78ebf6e096aa6e2e13c79749cce4f".to_owned())], Some("https://github.com/rust-lang/crates.io-index".to_owned()))));

    let npm3 = r#"{"lockfileVersion": 3, "packages": {"": {"name": "ui"}, "node_modules/@types/node": {"version": "20.1.0", "integrity": "sha512-AAAA"}, "node_modules/a/node_modules/b": {"version": "1.0.0"}, "packages/x": {"link": true}}}"#;
    assert_eq!(names(Lock::NpmLock, npm3), ["pkg:npm/%40types/node@20.1.0", "pkg:npm/b@1.0.0"]);
    let npm1 = r#"{"lockfileVersion": 1, "dependencies": {"left-pad": {"version": "1.3.0", "dependencies": {"inner": {"version": "2.0.0"}}}}}"#;
    let mut n1 = names(Lock::NpmLock, npm1);
    n1.sort();
    assert_eq!(n1, ["pkg:npm/inner@2.0.0", "pkg:npm/left-pad@1.3.0"]);

    let pnpm9 = "lockfileVersion: '9.0'\npackages:\n  '@babel/core@7.24.0':\n    resolution: {integrity: sha512-AAAA}\n  react-dom@18.2.0(react@18.2.0):\n    resolution: {integrity: sha512-AAAA}\n";
    assert_eq!(names(Lock::Pnpm, pnpm9), ["pkg:npm/%40babel/core@7.24.0", "pkg:npm/react-dom@18.2.0"]);
    let pnpm5 = "lockfileVersion: 5.4\npackages:\n  /@babel/core/7.24.0:\n    resolution: {integrity: sha512-AAAA}\n  /react-dom/18.2.0_react@18.2.0:\n    dev: false\n";
    assert_eq!(names(Lock::Pnpm, pnpm5), ["pkg:npm/%40babel/core@7.24.0", "pkg:npm/react-dom@18.2.0"]);

    let yarn1 = "# yarn lockfile v1\n\n\"@babel/code-frame@^7.0.0\", \"@babel/code-frame@^7.10.4\":\n  version \"7.12.13\"\n  resolved \"https://registry.yarnpkg.com/@babel/code-frame/-/code-frame-7.12.13.tgz#abc\"\n  integrity sha512-AAAA\n\nlodash@^4.17.0:\n  version \"4.17.21\"\n";
    assert_eq!(names(Lock::Yarn, yarn1), ["pkg:npm/%40babel/code-frame@7.12.13", "pkg:npm/lodash@4.17.21"]);
    let yarn2 = "__metadata:\n  version: 6\n\n\"lodash@npm:^4.17.0\":\n  version: 4.17.21\n  resolution: \"lodash@npm:4.17.21\"\n\n\"app@workspace:.\":\n  version: 0.0.0-use.local\n  resolution: \"app@workspace:.\"\n";
    assert_eq!(names(Lock::Yarn, yarn2), ["pkg:npm/lodash@4.17.21"]);

    let gosum = "golang.org/x/text v0.14.0 h1:abc=\ngolang.org/x/text v0.14.0/go.mod h1:def=\ngithub.com/pkg/errors v0.9.1 h1:ghi=\n";
    assert_eq!(names(Lock::GoSum, gosum), ["pkg:golang/golang.org/x/text@v0.14.0", "pkg:golang/github.com/pkg/errors@v0.9.1"]);
    let gomod = "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\n\nrequire golang.org/x/text v0.14.0\n";
    assert_eq!(names(Lock::GoMod, gomod), ["pkg:golang/github.com/pkg/errors@v0.9.1", "pkg:golang/golang.org/x/text@v0.14.0"]);

    let uv = "version = 1\n\n[[package]]\nname = \"Typing_Extensions\"\nversion = \"4.12.2\"\nsdist = { url = \"https://x\", hash = \"sha256:ABCD\" }\n";
    let u = parse(Lock::Uv, uv).unwrap_or_default();
    assert_eq!(u.first().map(|p| (p.purl(), p.hashes.clone())), Some(("pkg:pypi/typing-extensions@4.12.2".into(), vec![("SHA256", "abcd".to_owned())])));
    let poetry = "[[package]]\nname = \"requests\"\nversion = \"2.32.3\"\nfiles = [{file = \"requests-2.32.3.tar.gz\", hash = \"sha256:ffff\"}]\n";
    assert_eq!(names(Lock::Poetry, poetry), ["pkg:pypi/requests@2.32.3"]);
    let reqs = "# pinned\nrequests[socks]==2.32.3 ; python_version >= \"3.8\" --hash=sha256:aaaa\nflask>=2\n-r other.txt\nDjango==5.0.1\n";
    assert_eq!(names(Lock::Requirements, reqs), ["pkg:pypi/requests@2.32.3", "pkg:pypi/django@5.0.1"]);

    let gemfile = "GEM\n  remote: https://rubygems.org/\n  specs:\n    actioncable (7.0.4)\n      actionpack (= 7.0.4)\n    nokogiri (1.15.4-x86_64-linux)\n\nPLATFORMS\n  x86_64-linux\n";
    assert_eq!(names(Lock::Gemfile, gemfile), ["pkg:gem/actioncable@7.0.4", "pkg:gem/nokogiri@1.15.4-x86_64-linux"]);
    let composer = r#"{"packages": [{"name": "monolog/monolog", "version": "3.5.0", "dist": {"url": "https://x/monolog.zip", "shasum": ""}}], "packages-dev": [{"name": "phpunit/phpunit", "version": "10.5.0"}]}"#;
    assert_eq!(names(Lock::Composer, composer), ["pkg:composer/monolog/monolog@3.5.0", "pkg:composer/phpunit/phpunit@10.5.0"]);

    assert_eq!(kind_of("requirements-dev.txt"), Some(Lock::Requirements));
    assert_eq!(kind_of("README.md"), None);
    assert!(parse(Lock::Cargo, "not = toml = at all").is_err());
}

/// Another tool's document (syft's SPDX): its packages by purl, for the ecosystems kiln doesn't read.
#[test]
fn w18_r03_others() {
    let doc = serde_json::json!({ "packages": [
        { "name": "commons-io", "externalRefs": [{ "referenceType": "cpe23Type", "referenceLocator": "cpe:2.3:a:x" }, { "referenceType": "purl", "referenceLocator": "pkg:maven/org.apache.commons/commons-io@2.15.0?type=jar" }], "checksums": [{ "algorithm": "SHA1", "checksumValue": "ABCD" }, { "algorithm": "MD2", "checksumValue": "x" }], "downloadLocation": "NOASSERTION" },
        { "name": "Newtonsoft.Json", "externalRefs": [{ "referenceType": "purl", "referenceLocator": "pkg:nuget/Newtonsoft.Json@13.0.3" }], "downloadLocation": "https://nuget.org/x" },
        { "name": "no purl" },
        { "name": "unknown", "externalRefs": [{ "referenceType": "purl", "referenceLocator": "pkg:bitbucket/a/b@1" }] },
        { "name": "at", "externalRefs": [{ "referenceType": "purl", "referenceLocator": "pkg:npm/%40angular/core@17.0.0" }] },
    ]});
    let p = from_spdx(&doc);
    assert_eq!(p.iter().map(Package::purl).collect::<Vec<_>>(), ["pkg:maven/org.apache.commons/commons-io@2.15.0", "pkg:nuget/Newtonsoft.Json@13.0.3", "pkg:npm/%40angular/core@17.0.0"]);
    assert_eq!(p.first().map(|p| p.hashes.clone()), Some(vec![("SHA1", "abcd".to_owned())]));
    assert_eq!(p.get(1).and_then(|p| p.source.clone()).as_deref(), Some("https://nuget.org/x"));
    assert!(p.first().is_some_and(|p| !p.eco.built_in()) && p.get(2).is_some_and(|p| p.eco.built_in()));
    assert_eq!(from_purl("pkg:cargo/serde@1.0.200").map(|p| (p.name, p.version)), Some(("serde".into(), "1.0.200".into())));
    assert!(from_purl("pkg:cargo/serde").is_none() && from_purl("npm/x@1").is_none());
}

#[test]
fn w18_r03_documents() {
    let pkgs = parse(Lock::NpmLock, r#"{"lockfileVersion": 3, "packages": {"node_modules/b": {"version": "1.0.0", "integrity": "sha512-AAAA", "resolved": "https://r/b.tgz"}, "node_modules/a": {"version": "2.0.0"}}}"#).unwrap_or_default();
    let doc = Doc { name: "acme/app".into(), version: Some("abc123".into()), created: "2026-10-18T00:00:00Z".into(), tool: "kiln-0.2.0".into(), packages: pkgs };
    let s = spdx(&doc, "https://kiln.example/spdx/1");
    assert_eq!(s.get("spdxVersion").and_then(Value::as_str), Some("SPDX-2.3"));
    let refs: Vec<&str> = s.get("packages").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.pointer("/externalRefs/0/referenceLocator").and_then(Value::as_str)).collect();
    assert_eq!(refs, ["pkg:npm/a@2.0.0", "pkg:npm/b@1.0.0"], "by purl, the root first");
    assert_eq!(s.pointer("/packages/2/checksums/0/algorithm").and_then(Value::as_str), Some("SHA512"));
    assert_eq!(s.get("relationships").and_then(Value::as_array).map(Vec::len), Some(3));
    let c = cyclonedx(&doc, "urn:uuid:00000000-0000-4000-8000-000000000000");
    assert_eq!((c.get("specVersion").and_then(Value::as_str), c.pointer("/metadata/tools/components/0/version").and_then(Value::as_str)), (Some("1.6"), Some("0.2.0")));
    assert_eq!(c.pointer("/components/1/hashes/0/alg").and_then(Value::as_str), Some("SHA-512"));
    assert_eq!(c.pointer("/dependencies/0/dependsOn").and_then(Value::as_array).map(Vec::len), Some(2));
}

/// W18-AC02: the SBOM of kiln's own repository lists every crate of `Cargo.lock` and every npm
/// package of `ui/package-lock.json`.
#[test]
fn w18_ac02_kilns_own() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (Ok(cargo), Ok(npm)) = (std::fs::read_to_string(root.join("Cargo.lock")), std::fs::read_to_string(root.join("ui/package-lock.json"))) else {
        return;
    };
    let mut packages = parse(Lock::Cargo, &cargo).unwrap_or_default();
    packages.extend(parse(Lock::NpmLock, &npm).unwrap_or_default());
    let doc = Doc { name: "hyper-light/kiln".into(), version: None, created: "2026-10-18T00:00:00Z".into(), tool: "kiln-test".into(), packages };
    let (s, c) = (spdx(&doc, "https://kiln.test/spdx/self"), cyclonedx(&doc, "urn:uuid:00000000-0000-4000-8000-000000000001"));
    let in_spdx: std::collections::BTreeSet<String> = s.get("packages").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.pointer("/externalRefs/0/referenceLocator").and_then(Value::as_str)).map(str::to_owned).collect();
    let in_cdx: std::collections::BTreeSet<String> = c.get("components").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("purl").and_then(Value::as_str)).map(str::to_owned).collect();
    // Every crate, straight from the lockfile's TOML.
    let lock: toml::Table = cargo.parse().unwrap_or_default();
    let crates: Vec<String> = lock.get("package").and_then(toml::Value::as_array).into_iter().flatten().filter_map(|p| Some(Package { eco: Eco::Cargo, name: p.get("name")?.as_str()?.into(), version: p.get("version")?.as_str()?.into(), hashes: vec![], source: None }.purl())).collect();
    assert!(crates.len() > 200, "{} crates", crates.len());
    for purl in &crates {
        assert!(in_spdx.contains(purl) && in_cdx.contains(purl), "{purl} is missing");
    }
    // Every npm package, straight from the lockfile's JSON.
    let v: Value = serde_json::from_str(&npm).unwrap_or_default();
    let npms: Vec<String> = v
        .get("packages")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, p)| !k.is_empty() && p.get("link").and_then(Value::as_bool) != Some(true))
        .filter_map(|(k, p)| Some(Package { eco: Eco::Npm, name: k.rsplit_once("node_modules/")?.1.into(), version: p.get("version")?.as_str()?.into(), hashes: vec![], source: None }.purl()))
        .collect();
    assert!(npms.len() > 100, "{} npm packages", npms.len());
    for purl in &npms {
        assert!(in_spdx.contains(purl) && in_cdx.contains(purl), "{purl} is missing");
    }
}

/// W18-R03's review: what a change does to the dependencies, package by package.
#[test]
fn w18_r03_dependency_changes() {
    let lock = |pkgs: &[(&str, &str)]| -> String { pkgs.iter().map(|(n, v)| format!("[[package]]\nname = \"{n}\"\nversion = \"{v}\"\n\n")).collect() };
    let old = parse(Lock::Cargo, &lock(&[("serde", "1.0.200"), ("rand", "0.8.5"), ("rand", "0.9.0"), ("left", "1.0.0")])).unwrap_or_default();
    let new = parse(Lock::Cargo, &lock(&[("serde", "1.0.210"), ("rand", "0.9.0"), ("tokio", "1.40.0")])).unwrap_or_default();
    let c = changes(&old, &new);
    let said: Vec<(String, Vec<String>, Vec<String>)> = c.iter().map(|x| (x.name.clone(), x.from.clone(), x.to.clone())).collect();
    assert_eq!(
        said,
        [
            ("left".to_owned(), vec!["1.0.0".to_owned()], vec![]),
            ("rand".to_owned(), vec!["0.8.5".to_owned()], vec![]),
            ("serde".to_owned(), vec!["1.0.200".to_owned()], vec!["1.0.210".to_owned()]),
            ("tokio".to_owned(), vec![], vec!["1.40.0".to_owned()]),
        ],
        "removed, a version dropped, upgraded, added"
    );
    assert!(c.iter().all(|x| x.ecosystem == "cargo"));
    assert!(changes(&new, &new).is_empty(), "nothing changed");
}
