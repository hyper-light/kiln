//! Caching proxies for package registries (docs/ci.md W12-R04, R06), against one fake upstream
//! standing for each ecosystem's: what's asked through kiln comes from the upstream once (an index
//! again after its TTL), its links point back at kiln, and with the upstream down kiln answers from
//! its copy and says so. And who may read: anyone signed in, or anyone with `anonymous`.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Options, start};

type Asked = Arc<Mutex<Vec<String>>>;

/// The fake upstream: each ecosystem under its own prefix; 503 for everything while `down`.
fn upstream(down: Arc<AtomicBool>) -> anyhow::Result<(u16, Asked)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let asked: Asked = Arc::default();
    let seen = asked.clone();
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
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            if let Ok(mut a) = seen.lock() {
                a.push(path.clone());
            }
            let mut s = reader.into_inner();
            if down.load(Ordering::SeqCst) {
                let _ = write!(s, "HTTP/1.1 503 Down\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let u = format!("http://127.0.0.1:{port}");
            let (status, ctype, body): (u16, &str, Vec<u8>) = match path.as_str() {
                "/npm/left-pad" => (200, "application/json", format!(r#"{{"name":"left-pad","versions":{{"1.3.0":{{"dist":{{"tarball":"{u}/npm/left-pad/-/left-pad-1.3.0.tgz"}}}}}}}}"#).into_bytes()),
                "/npm/left-pad/-/left-pad-1.3.0.tgz" => (200, "application/octet-stream", b"npm tarball".to_vec()),
                "/pypi/simple/six/" => (200, "text/html", format!(r#"<a href="{u}/pypi-files/packages/six-1.16.0-py2.py3-none-any.whl#sha256=00">six</a>"#).into_bytes()),
                "/pypi-files/packages/six-1.16.0-py2.py3-none-any.whl" => (200, "application/octet-stream", b"a wheel".to_vec()),
                "/crates/it/oa/itoa" => (200, "text/plain", br#"{"name":"itoa","vers":"1.0.11","deps":[],"cksum":"00","features":{},"yanked":false}"#.to_vec()),
                "/crates-files/itoa/itoa-1.0.11.crate" => (200, "application/octet-stream", b"a crate".to_vec()),
                "/go/github.com/pkg/errors/@v/list" => (200, "text/plain", b"v0.9.1\n".to_vec()),
                "/go/github.com/pkg/errors/@v/v0.9.1.zip" => (200, "application/zip", b"a module".to_vec()),
                "/maven/org/x/y/1.0/y-1.0.jar" => (200, "application/java-archive", b"a jar".to_vec()),
                "/maven/org/x/y/maven-metadata.xml" => (200, "text/xml", b"<metadata/>".to_vec()),
                "/nuget/v3/index.json" => (200, "application/json", format!(r#"{{"resources":[{{"@id":"{u}/nuget/v3-flatcontainer/","@type":"PackageBaseAddress/3.0.0"}}]}}"#).into_bytes()),
                "/nuget/v3-flatcontainer/x/1.0.0/x.1.0.0.nupkg" => (200, "application/octet-stream", b"a nupkg".to_vec()),
                "/rubygems/info/rake" => (200, "text/plain", b"---\n13.0.6 |checksum:00\n".to_vec()),
                "/rubygems/gems/rake-13.0.6.gem" => (200, "application/octet-stream", b"a gem".to_vec()),
                "/files/tool.tar.gz" => (200, "application/gzip", b"a tool".to_vec()),
                _ => (404, "text/plain", b"not here".to_vec()),
            };
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    Ok((port, asked))
}

/// A GET: its status, headers (lowercase) and body.
fn get(port: u16, path: &str, auth: Option<&str>) -> anyhow::Result<(u16, HashMap<String, String>, Vec<u8>)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let a = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{a}Connection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no head"))?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let mut body = raw.get(split + 4..).unwrap_or_default().to_vec();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut at = 0usize;
        while let Some(end) = body.get(at..).and_then(|b| b.windows(2).position(|w| w == b"\r\n")) {
            let size = usize::from_str_radix(String::from_utf8_lossy(body.get(at..at + end).unwrap_or_default()).trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            out.extend_from_slice(body.get(at + end + 2..at + end + 2 + size).unwrap_or_default());
            at += end + 2 + size + 2;
        }
        body = out;
    }
    Ok((status, headers, body))
}

#[test]
fn w12_r04_proxies() -> anyhow::Result<()> {
    let down = Arc::new(AtomicBool::new(false));
    let (up, asked) = upstream(down.clone())?;
    let u = format!("http://127.0.0.1:{up}");
    let dir = tempfile::tempdir()?;
    let extra = format!(
        r#"[[ci.registry.proxies]]
ecosystem = "npm"
upstream = "{u}/npm"
ttl = 0

[[ci.registry.proxies]]
ecosystem = "pypi"
upstream = "{u}/pypi"
files = "{u}/pypi-files"

[[ci.registry.proxies]]
ecosystem = "crates"
upstream = "{u}/crates"
files = "{u}/crates-files"
anonymous = true

[[ci.registry.proxies]]
ecosystem = "go"
upstream = "{u}/go"

[[ci.registry.proxies]]
ecosystem = "maven"
upstream = "{u}/maven"

[[ci.registry.proxies]]
ecosystem = "nuget"
upstream = "{u}/nuget"

[[ci.registry.proxies]]
ecosystem = "rubygems"
upstream = "{u}/rubygems"

[[ci.registry.proxies]]
ecosystem = "http"
name = "tools"
hosts = ["http://127.0.0.1:{up}"]
"#
    );
    let s = start(dir.path(), &Options { extra: &extra, ..Options::default() })?;
    let port = s.port;
    let auth = format!("Bearer {}", s.admin);
    let a = Some(auth.as_str());
    let times = |p: &str| asked.lock().map(|l| l.iter().filter(|x| x.as_str() == p).count()).unwrap_or(0);

    // Who: no token, no read (but crates' proxy is open); a kiln token, a read.
    assert_eq!(get(port, "/_kiln/proxy/npm/left-pad", None)?.0, 401);
    let (st, _, body) = get(port, "/_kiln/proxy/crates/config.json", None)?;
    assert_eq!(st, 200, "an anonymous proxy");
    let config = String::from_utf8_lossy(&body).into_owned();
    assert!(config.contains(&format!("http://127.0.0.1:{port}/_kiln/proxy/crates/dl/{{crate}}/{{version}}")) && config.contains("\"auth-required\":false"), "{config}");

    // An index's links point back at kiln; what they lead to comes through, once.
    let (st, _, body) = get(port, "/_kiln/proxy/npm/left-pad", a)?;
    let doc = String::from_utf8_lossy(&body).into_owned();
    assert!(st == 200 && doc.contains(&format!("\"http://127.0.0.1:{port}/_kiln/proxy/npm/left-pad/-/left-pad-1.3.0.tgz\"")), "{st} {doc}");
    let (st, _, body) = get(port, "/_kiln/proxy/pypi/simple/six/", a)?;
    let page = String::from_utf8_lossy(&body).into_owned();
    assert!(st == 200 && page.contains(&format!("http://127.0.0.1:{port}/_kiln/proxy/pypi/files/packages/six-1.16.0-py2.py3-none-any.whl#sha256=00")), "{page}");
    let (st, _, body) = get(port, "/_kiln/proxy/nuget/v3/index.json", a)?;
    assert!(st == 200 && String::from_utf8_lossy(&body).contains(&format!("http://127.0.0.1:{port}/_kiln/proxy/nuget/v3-flatcontainer/")), "{}", String::from_utf8_lossy(&body));
    let packages = [
        ("/_kiln/proxy/npm/left-pad/-/left-pad-1.3.0.tgz", "/npm/left-pad/-/left-pad-1.3.0.tgz", "npm tarball"),
        ("/_kiln/proxy/pypi/files/packages/six-1.16.0-py2.py3-none-any.whl", "/pypi-files/packages/six-1.16.0-py2.py3-none-any.whl", "a wheel"),
        ("/_kiln/proxy/crates/dl/itoa/1.0.11", "/crates-files/itoa/itoa-1.0.11.crate", "a crate"),
        ("/_kiln/proxy/go/github.com/pkg/errors/@v/v0.9.1.zip", "/go/github.com/pkg/errors/@v/v0.9.1.zip", "a module"),
        ("/_kiln/proxy/maven/org/x/y/1.0/y-1.0.jar", "/maven/org/x/y/1.0/y-1.0.jar", "a jar"),
        ("/_kiln/proxy/nuget/v3-flatcontainer/x/1.0.0/x.1.0.0.nupkg", "/nuget/v3-flatcontainer/x/1.0.0/x.1.0.0.nupkg", "a nupkg"),
        ("/_kiln/proxy/rubygems/gems/rake-13.0.6.gem", "/rubygems/gems/rake-13.0.6.gem", "a gem"),
        ("/_kiln/proxy/tools/127.0.0.1:{up}/files/tool.tar.gz", "/files/tool.tar.gz", "a tool"),
    ];
    for round in 0..2 {
        for (path, upstream_path, content) in &packages {
            let path = path.replace("{up}", &up.to_string());
            let auth = if path.contains("/crates/") { None } else { a };
            let (st, h, body) = get(port, &path, auth)?;
            assert!(st == 200 && body == content.as_bytes(), "round {round}, {path}: {st} {}", String::from_utf8_lossy(&body));
            assert!(h.get("etag").is_some_and(|e| e.starts_with('"')), "{path}: {h:?}");
            assert_eq!(times(upstream_path), 1, "round {round}, {path}: the upstream asked once");
        }
    }
    assert_eq!(get(port, "/_kiln/proxy/tools/evil.example/x", a)?.0, 403, "only the hosts named");
    assert_eq!(get(port, "/_kiln/proxy/go/sumdb/sum.golang.org/supported", a)?.0, 200, "Go's checksum database through the proxy");
    for (path, upstream_path) in [("/_kiln/proxy/crates/it/oa/itoa", "/crates/it/oa/itoa"), ("/_kiln/proxy/go/github.com/pkg/errors/@v/list", "/go/github.com/pkg/errors/@v/list"), ("/_kiln/proxy/maven/org/x/y/maven-metadata.xml", "/maven/org/x/y/maven-metadata.xml"), ("/_kiln/proxy/rubygems/info/rake", "/rubygems/info/rake")] {
        let auth = if path.contains("/crates/") { None } else { a };
        assert_eq!(get(port, path, auth)?.0, 200, "{path}");
        assert_eq!(get(port, path, auth)?.0, 200, "{path}");
        assert_eq!(times(upstream_path), 1, "{path}: an index within its TTL comes from kiln");
    }
    // npm's TTL is 0: its packument is asked again each time.
    assert!(times("/npm/left-pad") >= 1);
    let before = times("/npm/left-pad");
    get(port, "/_kiln/proxy/npm/left-pad", a)?;
    assert_eq!(times("/npm/left-pad"), before + 1);

    // The upstream down: packages from kiln without asking; an index from kiln's copy, saying so; what kiln never had, a 502.
    down.store(true, Ordering::SeqCst);
    let (st, _, body) = get(port, "/_kiln/proxy/npm/left-pad/-/left-pad-1.3.0.tgz", a)?;
    assert!(st == 200 && body == b"npm tarball");
    let (st, h, _) = get(port, "/_kiln/proxy/npm/left-pad", a)?;
    assert!(st == 200 && h.get("warning").is_some_and(|w| w.contains("out of reach")), "{st} {h:?}");
    assert_eq!(get(port, "/_kiln/proxy/npm/is-number", a)?.0, 502);
    Ok(())
}

/// A tool run in `dir` with `env`: whether it succeeded, and what it said.
fn tool(dir: &std::path::Path, program: &str, args: &[&str], env: &[(&str, String)]) -> anyhow::Result<(bool, String)> {
    let mut c = std::process::Command::new(program);
    c.args(args).current_dir(dir);
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

/// `script` in an Alpine container with `packages`, `dir` at `/w`, on the host's network (kiln's
/// test server is on its loopback); what it leaves is handed back to this user.
fn in_container(dir: &std::path::Path, packages: &str, script: &str) -> anyhow::Result<(bool, String)> {
    let (uid, gid) = (users_id("-u"), users_id("-g"));
    let whole = format!("apk add -q --no-cache {packages} >/dev/null 2>&1; s=0; ( {script} ) || s=$?; chown -R {uid}:{gid} /w; exit $s");
    let o = std::process::Command::new("docker").args(["run", "--rm", "--network", "host", "-v"]).arg(format!("{}:/w", dir.display())).args(["-w", "/w", "alpine:3.24", "sh", "-c", &whole]).output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

fn users_id(flag: &str) -> String {
    std::process::Command::new("id").arg(flag).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_else(|_| "0".into())
}

/// W12-AC03: real package managers install a fixture through kiln's proxies, from the public
/// registries, then again with every upstream pointed nowhere (the same data; their own caches
/// fresh), which kiln answers from what it kept. npm and uv sign in with a kiln token; cargo and go
/// read an anonymous proxy. With `KILN_E2E_PROXIES=1` (it needs the internet, npm, uv, cargo and go).
#[test]
fn w12_ac03_installs_through_proxies() -> anyhow::Result<()> {
    if std::env::var("KILN_E2E_PROXIES").is_err() {
        eprintln!("KILN_E2E_PROXIES isn't set: no installs from the internet here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let proxies = |online: bool| -> String {
        let (npm, pypi, files, crates, crate_files, go) = if online {
            ("https://registry.npmjs.org", "https://pypi.org", "https://files.pythonhosted.org", "https://index.crates.io", "https://static.crates.io/crates", "https://proxy.golang.org")
        } else {
            let nowhere = "http://127.0.0.1:9";
            (nowhere, nowhere, nowhere, nowhere, nowhere, nowhere)
        };
        let (maven, gems, nuget) = if online { ("https://repo1.maven.org/maven2", "https://rubygems.org", "https://api.nuget.org") } else { ("http://127.0.0.1:9", "http://127.0.0.1:9", "http://127.0.0.1:9") };
        format!(
            "[[ci.registry.proxies]]\necosystem = \"npm\"\nupstream = \"{npm}\"\nttl = 0\n\n[[ci.registry.proxies]]\necosystem = \"pypi\"\nupstream = \"{pypi}\"\nfiles = \"{files}\"\nttl = 0\n\n\
             [[ci.registry.proxies]]\necosystem = \"crates\"\nupstream = \"{crates}\"\nfiles = \"{crate_files}\"\nanonymous = true\nttl = 0\n\n[[ci.registry.proxies]]\necosystem = \"go\"\nupstream = \"{go}\"\nanonymous = true\nttl = 0\n\n\
             [[ci.registry.proxies]]\necosystem = \"maven\"\nupstream = \"{maven}\"\nttl = 0\n\n[[ci.registry.proxies]]\necosystem = \"rubygems\"\nupstream = \"{gems}\"\nttl = 0\n\n[[ci.registry.proxies]]\necosystem = \"nuget\"\nupstream = \"{nuget}\"\nttl = 0\n"
        )
    };
    // The fixtures.
    let npm_dir = root.join("npm");
    std::fs::create_dir_all(&npm_dir)?;
    std::fs::write(npm_dir.join("package.json"), r#"{"name":"t","version":"1.0.0","private":true,"dependencies":{"is-number":"7.0.0"}}"#)?;
    let rs = root.join("rs");
    std::fs::create_dir_all(rs.join("src"))?;
    // A workspace of its own: cargo looks above for one otherwise.
    std::fs::write(rs.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nitoa = \"=1.0.11\"\n\n[workspace]\n")?;
    std::fs::write(rs.join("src/lib.rs"), "pub fn f() -> String { itoa::Buffer::new().format(1).to_owned() }\n")?;
    let go = root.join("go");
    std::fs::create_dir_all(&go)?;
    std::fs::write(go.join("go.mod"), "module t\n\ngo 1.22\n\nrequire github.com/pkg/errors v0.9.1\n")?;
    std::fs::write(go.join("main.go"), "package main\n\nimport \"github.com/pkg/errors\"\n\nfunc main() { _ = errors.New(\"x\") }\n")?;

    // Docker, for the tools that aren't on this machine.
    let docker = std::process::Command::new("docker").args(["image", "inspect", "alpine:3.24"]).output().is_ok_and(|o| o.status.success());
    for (phase, online) in [("online", true), ("upstreams out of reach", false)] {
        let s = start(&root, &Options { extra: &proxies(online), ..Options::default() })?;
        let (port, token) = (s.port, s.admin.clone());
        let base = format!("http://127.0.0.1:{port}/_kiln/proxy");
        let fresh = root.join(format!("caches-{online}"));
        // npm, signed in with the token.
        let _ = std::fs::remove_dir_all(npm_dir.join("node_modules"));
        let _ = std::fs::remove_file(npm_dir.join("package-lock.json"));
        std::fs::write(npm_dir.join(".npmrc"), format!("registry={base}/npm/\n//127.0.0.1:{port}/_kiln/proxy/npm/:_authToken={token}\n"))?;
        let (ok, out) = tool(&npm_dir, "npm", &["install", "--no-audit", "--no-fund", "--cache", &fresh.join("npm").to_string_lossy()], &[])?;
        assert!(ok && npm_dir.join("node_modules/is-number/package.json").exists(), "{phase}: npm: {out}\n{}", s.log());
        // uv, with the token as the index's password.
        let target = fresh.join("py");
        let index = format!("http://kiln:{token}@127.0.0.1:{port}/_kiln/proxy/pypi/simple/");
        let (ok, out) = tool(&root, "uv", &["pip", "install", "--no-cache", "--target", &target.to_string_lossy(), "--index-url", &index, "six==1.16.0"], &[("UV_PYTHON_DOWNLOADS", "never".into())])?;
        assert!(ok && target.join("six.py").exists(), "{phase}: uv: {out}\n{}", s.log());
        // cargo, through the sparse index.
        std::fs::create_dir_all(rs.join(".cargo"))?;
        std::fs::write(rs.join(".cargo/config.toml"), format!("[source.crates-io]\nreplace-with = \"kiln\"\n\n[source.kiln]\nregistry = \"sparse+{base}/crates/\"\n"))?;
        let _ = std::fs::remove_file(rs.join("Cargo.lock"));
        let (ok, out) = tool(&rs, "cargo", &["fetch"], &[("CARGO_HOME", fresh.join("cargo").to_string_lossy().into_owned())])?;
        assert!(ok, "{phase}: cargo: {out}\n{}", s.log());
        // go, its checksum database through the proxy too.
        let go_env = [
            ("GOPROXY", format!("{base}/go")),
            ("GOMODCACHE", fresh.join("gomod").to_string_lossy().into_owned()),
            ("GOPATH", fresh.join("gopath").to_string_lossy().into_owned()),
            ("GOFLAGS", "-mod=mod".into()),
            ("GOTOOLCHAIN", "local".into()),
        ];
        let (ok, out) = tool(&go, "go", &["mod", "download", "github.com/pkg/errors"], &go_env)?;
        // The module cache is read-only, as Go leaves it: emptied by Go itself.
        let _ = tool(&go, "go", &["clean", "-modcache"], &go_env);
        assert!(ok, "{phase}: go: {out}\n{}", s.log());
        // Maven, RubyGems and NuGet in containers (their tools aren't here), on the host's network.
        if docker {
            let jvm = root.join("jvm");
            std::fs::create_dir_all(&jvm)?;
            std::fs::write(
                jvm.join("settings.xml"),
                format!("<settings><mirrors><mirror><id>kiln</id><mirrorOf>*</mirrorOf><url>{base}/maven</url></mirror></mirrors><servers><server><id>kiln</id><username>kiln</username><password>{token}</password></server></servers></settings>"),
            )?;
            let (ok, out) = in_container(&jvm, "maven", &format!("mvn -B -q -s /w/settings.xml -Dmaven.repo.local=/w/repo-{online} dependency:get -Dartifact=org.apache.commons:commons-lang3:3.14.0 && test -f /w/repo-{online}/org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.jar"))?;
            assert!(ok, "{phase}: maven: {out}\n{}", s.log());
            let ruby = root.join("ruby");
            std::fs::create_dir_all(&ruby)?;
            let (ok, out) = in_container(&ruby, "ruby", &format!("gem install rake -v 13.2.1 --clear-sources --source http://kiln:{token}@127.0.0.1:{port}/_kiln/proxy/rubygems/ --install-dir /w/gems-{online} --no-document && test -d /w/gems-{online}/gems/rake-13.2.1"))?;
            assert!(ok, "{phase}: gem: {out}\n{}", s.log());
            let net = root.join("dotnet");
            std::fs::create_dir_all(&net)?;
            std::fs::write(
                net.join("nuget.config"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?><configuration><packageSources><clear /><add key=\"kiln\" value=\"{base}/nuget/v3/index.json\" allowInsecureConnections=\"true\" /></packageSources><packageSourceCredentials><kiln><add key=\"Username\" value=\"kiln\" /><add key=\"ClearTextPassword\" value=\"{token}\" /></kiln></packageSourceCredentials></configuration>"
                ),
            )?;
            std::fs::write(net.join("t.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup><ItemGroup><PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.3\" /></ItemGroup></Project>")?;
            let (ok, out) = in_container(&net, "dotnet8-sdk", &format!("DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 NUGET_PACKAGES=/w/nuget-{online} dotnet restore t.csproj --configfile nuget.config && test -d /w/nuget-{online}/newtonsoft.json/13.0.3"))?;
            assert!(ok, "{phase}: dotnet: {out}\n{}", s.log());
        }
        drop(s);
    }
    Ok(())
}

fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// Jobs through the proxies by themselves (W12-R04's first part): a proxy with `jobs = true` is
/// in every leg's settings, npm's, pip's and Go's, signed in with the job's token.
#[test]
fn w12_r04_jobs_use_proxies() -> anyhow::Result<()> {
    let down = Arc::new(AtomicBool::new(false));
    let (up, _) = upstream(down)?;
    let u = format!("http://127.0.0.1:{up}");
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "proxies\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!(
        "[[ci.registry.proxies]]\necosystem = \"npm\"\nupstream = \"{u}/npm\"\njobs = true\n\n[[ci.registry.proxies]]\necosystem = \"pypi\"\nupstream = \"{u}/pypi\"\nfiles = \"{u}/pypi-files\"\njobs = true\n\n\
         [[ci.registry.proxies]]\necosystem = \"go\"\nupstream = \"{u}/go\"\njobs = true\n\n[[repos]]\nname = \"acme/px\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n",
        repo.display()
    );
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let port = s.port;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/px") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        r#"name: px
on: { push: {} }
jobs:
  px:
    runs-on: [self-hosted, local]
    steps:
      - run: |
          set -e
          grep -q "registry=" "$NPM_CONFIG_USERCONFIG" && grep -q "_authToken=" "$NPM_CONFIG_USERCONFIG"
          echo npm-ok
          curl -sf "${PIP_INDEX_URL}six/" | grep -q six-1.16.0
          echo pip-ok
          curl -sf --netrc-file "$NETRC" "$GOPROXY/github.com/pkg/errors/@v/list" | grep -q v0.9.1
          echo go-ok
"#,
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "px"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/px&limit=5", &s.admin, None)?;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(serde_json::Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(serde_json::Value::as_str).unwrap_or_default();
    let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<serde_json::Value>(&detail)?.pointer("/legs/0/id").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert_eq!(run.get("conclusion").and_then(serde_json::Value::as_str), Some("success"), "{run}\n{log}");
    for said in ["npm-ok", "pip-ok", "go-ok"] {
        assert!(log.contains(said), "{said}: {log}");
    }
    Ok(())
}

/// A fake npm registry that takes a publish (`PUT /<name>`) with its token: what it was sent.
fn npm_registry() -> anyhow::Result<(u16, Arc<Mutex<Vec<String>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let got: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = got.clone();
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
            let (mut len, mut auth) = (0usize, String::new());
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap_or((&h, ""));
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap_or(0),
                    "authorization" => auth = v.trim().to_owned(),
                    _ => {}
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let mut s = reader.into_inner();
            let (status, reply) = if method == "PUT" && path == "/kiln-test-pkg" && auth == "Bearer npm_fake_token" {
                if let Ok(mut g) = seen.lock() {
                    g.push(String::from_utf8_lossy(&body).into_owned());
                }
                (200, r#"{"ok":true}"#)
            } else if method == "PUT" {
                (401, r#"{"error":"bad token"}"#)
            } else {
                (404, r#"{"error":"not found"}"#)
            };
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
        }
    });
    Ok((port, got))
}

/// `kiln/publish@v1` (W12-R04's third part): a package published to its registry with a
/// `package` integration's token, which never shows in the log.
#[test]
fn w12_r04_publish() -> anyhow::Result<()> {
    if !std::process::Command::new("npm").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("no npm here");
        return Ok(());
    }
    let (reg, got) = npm_registry()?;
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "publish\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/pub\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let port = s.port;
    let body = serde_json::json!({ "kind": "package", "name": "npmreg", "config": { "ecosystem": "npm", "registry": format!("http://127.0.0.1:{reg}") }, "secret": "npm_fake_token", "scope": "server" });
    let (st, b) = common::http(port, "POST", "/api/ci/integrations", &s.admin, Some(&body))?;
    anyhow::ensure!((200..300).contains(&st), "the integration: {st} {b}");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/pub") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".kiln/ci.yaml"),
        "name: pub\non: { push: {} }\njobs:\n  pub:\n    runs-on: [self-hosted, local]\n    integrations: [npmreg]\n    steps:\n      - run: mkdir -p pkg && printf '{\"name\":\"kiln-test-pkg\",\"version\":\"1.0.0\"}' > pkg/package.json\n      - uses: kiln/publish@v1\n        with: { ecosystem: npm, integration: npmreg, path: pkg }\n",
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "pub"])?;
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/pub&limit=5", &s.admin, None)?;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = v.get("runs").and_then(serde_json::Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
            break r;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(serde_json::Value::as_str).unwrap_or_default();
    let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<serde_json::Value>(&detail)?.pointer("/legs/0/id").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert_eq!(run.get("conclusion").and_then(serde_json::Value::as_str), Some("success"), "{run}\n{log}");
    let sent = got.lock().map(|g| g.join("\n")).unwrap_or_default();
    assert!(sent.contains("\"name\":\"kiln-test-pkg\"") && sent.contains("_attachments"), "the registry got: {sent}");
    assert!(!log.contains("npm_fake_token"), "the token is masked: {log}");
    Ok(())
}

/// A stand-in for Packagist and ConanCenter.
fn packagist_and_conan() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
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
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let mut s = reader.into_inner();
            let body: Option<&str> = match path.as_str() {
                "/composer/packages.json" => Some(r#"{"packages":[],"metadata-url":"/p2/%package%.json","notify-batch":"https://packagist.org/downloads/"}"#),
                "/composer/p2/psr/log.json" => {
                    Some(r#"{"packages":{"psr/log":[{"version":"3.0.2","dist":{"type":"zip","url":"https://api.github.com/repos/php-fig/log/zipball/f16e1d5863e37f8d8c2a01719f5b34baa2b714d3"}}]}}"#)
                }
                "/conan/v2/conans/zlib/1.3.1/_/_/revisions/latest" => Some(r#"{"revision":"b8bc2603263cf7eccbd6e17e66b0ed76","time":"2024-01-01T00:00:00Z"}"#),
                _ => None,
            };
            let (status, body) = body.map_or((404, "not here"), |b| (200, b));
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    Ok(port)
}

/// A request with a body, and headers: its status, headers (lowercase) and body.
fn send(port: u16, method: &str, path: &str, auth: Option<&str>, body: &[u8]) -> anyhow::Result<(u16, HashMap<String, String>, Vec<u8>)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let a = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{a}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no head"))?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).into_owned();
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    Ok((status, headers, raw.get(split + 4..).unwrap_or_default().to_vec()))
}

/// Appendix D.1's rest (docs/ci.md W12-R04): Composer's root and metadata pointing back at kiln
/// (each version's archive through it); Conan's ping, sign-in and revisions; and vcpkg's binary
/// cache, which kiln keeps itself: a miss, a package put by a person, then served, and a put
/// without credentials refused.
#[test]
fn w12_r04_composer_conan_and_vcpkg() -> anyhow::Result<()> {
    let up = packagist_and_conan()?;
    let u = format!("http://127.0.0.1:{up}");
    let dir = tempfile::tempdir()?;
    let extra = format!(
        "[[ci.registry.proxies]]\necosystem = \"composer\"\nupstream = \"{u}/composer\"\n\n[[ci.registry.proxies]]\necosystem = \"conan\"\nupstream = \"{u}/conan\"\n\n[[ci.registry.proxies]]\necosystem = \"vcpkg\"\n"
    );
    let s = start(dir.path(), &Options { extra: &extra, ..Options::default() })?;
    let port = s.port;
    let auth = format!("Bearer {}", s.admin);
    let a = Some(auth.as_str());
    let here = format!("http://127.0.0.1:{port}/_kiln/proxy");

    let (st, _, body) = get(port, "/_kiln/proxy/composer/packages.json", a)?;
    let root = String::from_utf8_lossy(&body).into_owned();
    assert!(st == 200 && root.contains(&format!("\"metadata-url\":\"{here}/composer/p2/%package%.json\"")) && !root.contains("notify-batch"), "{st} {root}");
    let (st, _, body) = get(port, "/_kiln/proxy/composer/p2/psr/log.json", a)?;
    let meta = String::from_utf8_lossy(&body).into_owned();
    assert!(st == 200 && meta.contains(&format!("\"{here}/composer/dist/api.github.com/repos/php-fig/log/zipball/f16e1d5863e37f8d8c2a01719f5b34baa2b714d3\"")), "{st} {meta}");

    let (st, h, _) = get(port, "/_kiln/proxy/conan/v2/ping", None)?;
    assert!(st == 200 && h.get("x-conan-server-capabilities").is_some_and(|c| c.contains("revisions")), "{st} {h:?}");
    let basic = format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("ci:{}", s.admin)));
    let (st, _, token) = get(port, "/_kiln/proxy/conan/v2/users/authenticate", Some(&basic))?;
    assert!(st == 200 && token == s.admin.as_bytes(), "{st}: Conan's token is the kiln token it signed in with");
    let (st, _, body) = get(port, "/_kiln/proxy/conan/v2/conans/zlib/1.3.1/_/_/revisions/latest", Some(&format!("Bearer {}", String::from_utf8_lossy(&token))))?;
    assert!(st == 200 && String::from_utf8_lossy(&body).contains("b8bc2603263cf7eccbd6e17e66b0ed76"), "{st}");

    let package = "/_kiln/proxy/vcpkg/zlib/1.3.1/0a1b2c3d4e5f";
    assert_eq!(get(port, package, a)?.0, 404, "a miss");
    assert_eq!(send(port, "PUT", package, None, b"a binary")?.0, 403, "not without credentials");
    assert_eq!(send(port, "PUT", package, a, b"a binary")?.0, 200);
    let (st, _, body) = get(port, package, a)?;
    assert!(st == 200 && body == b"a binary", "{st} {}", String::from_utf8_lossy(&body));
    let (st, _, _) = send(port, "PUT", "/_kiln/proxy/vcpkg/../escape", a, b"x")?;
    assert!(matches!(st, 400 | 404), "no path out of the cache: {st}");
    Ok(())
}
