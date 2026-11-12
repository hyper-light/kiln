//! The Agentfiles kiln ships, one per harness it runs in a Shards microVM (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, "Every kiln harness in Shards microVMs"): each embedded in kiln's
//! one binary, listed read-only beside a person's own, and copied to make one theirs. Each harness's
//! own step adds its row here, by `include_str!` from `.kiln/images/harness-<id>/`, kiln's home for
//! image definitions (docs/ci.md W13-R11): its Agentfile, its pin.json (the publisher's builds, as its
//! pin.sh checked and wrote them) and its harness's own files. The Dockerfile's build stage copies
//! `.kiln/images/` for them.

use serde::Serialize;

/// An Agentfile kiln ships.
pub struct Builtin {
    /// Its name, an image's (as a person's Agentfile's is: [`super::name_problem`]).
    pub name: &'static str,
    /// The harness it runs (`crate::harness::kinds::Kind`'s id: `claude`, `codex` ...).
    pub harness: &'static str,
    pub text: &'static str,
    /// Its pin.json: the publisher's builds, each file with its URL, SHA-256 and size, and where it goes
    /// in its harness's directory (`path`, and `unpack` for an archive).
    pub pin: &'static str,
    /// Its harness's own files (harness.json ...), by name, written beside what the pin fetches.
    pub files: &'static [(&'static str, &'static str)],
}

/// Every one, in the order they're listed.
pub const ALL: &[Builtin] = &[
    // Devin for Terminal, as `devin acp` (kiln plan 01a1ce705e4d7008993f5c7291ef4476).
    Builtin {
        name: "harness-devin",
        harness: "devin",
        text: include_str!("../../../../.kiln/images/harness-devin/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-devin/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-devin/harness.json"))],
    },
];

/// One as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileBuiltin"))]
pub struct View {
    pub name: String,
    pub harness: String,
    pub text: String,
}

/// Every one, as the list shows them.
pub fn views() -> Vec<View> {
    ALL.iter().map(|b| View { name: b.name.to_owned(), harness: b.harness.to_owned(), text: b.text.to_owned() }).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::Builtin;
    use crate::harness::kinds::{Kind, Protocol};

    /// Each is what a person's Agentfile must be (a name an image may have, a text Shards may read), and
    /// no two share a name.
    #[test]
    fn every_builtin_is_a_valid_agentfile_once() {
        let mut names = std::collections::BTreeSet::new();
        for b in super::ALL {
            assert_eq!(super::super::name_problem(b.name), None, "{}", b.name);
            assert_eq!(super::super::text_problem(b.text), None, "{}", b.name);
            assert!(crate::harness::kinds::Kind::get(b.harness).is_some(), "{}: no harness {}", b.name, b.harness);
            assert!(names.insert(b.name), "{} twice", b.name);
        }
    }

    fn devin() -> anyhow::Result<&'static Builtin> {
        super::ALL.iter().find(|b| b.name == "harness-devin").ok_or_else(|| anyhow::anyhow!("no harness-devin"))
    }

    /// One of a built-in's own files, by name.
    fn file(b: &Builtin, name: &str) -> anyhow::Result<&'static str> {
        b.files.iter().find(|(n, _)| *n == name).map(|(_, text)| *text).ok_or_else(|| anyhow::anyhow!("{}: no {name}", b.name))
    }

    /// An Agentfile's instructions: its lines that are neither blank nor comments.
    fn instructions(text: &str) -> Vec<&str> {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
    }

    fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
        v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
    }

    fn strings(v: Option<&Value>) -> Vec<&str> {
        v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
    }

    fn is_sha256(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }

    /// What's wrong with Devin's pin (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 1): its version
    /// X.Y.Z, read from Cognition's manifest of that version (its SHA-256 kept); the base by digest; and
    /// exactly one build for each of linux/amd64 and linux/arm64, each that version's bundle for its
    /// architecture, with its size and SHA-256, unpacked as a tar.gz at its harness's directory (research
    /// f4, f6).
    fn devin_pin_problems(pin: &Value) -> Vec<String> {
        let mut problems = Vec::new();
        let version = text(pin, "/version");
        if version.split('.').count() != 3 || !version.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) {
            problems.push(format!("version {version:?} is no X.Y.Z"));
        }
        let at = format!("https://static.devin.ai/cli/{version}/");
        if text(pin, "/manifest/url") != format!("{at}manifest.json") || !is_sha256(text(pin, "/manifest/sha256")) {
            problems.push("not read from the version's manifest, its SHA-256 kept".into());
        }
        if text(pin, "/base/ref").is_empty() || !text(pin, "/base/digest").strip_prefix("sha256:").is_some_and(is_sha256) {
            problems.push("the base is not by digest".into());
        }
        let builds = pin.get("builds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let platforms: Vec<&str> = builds.iter().map(|b| text(b, "/platform")).collect();
        if platforms != ["linux/amd64", "linux/arm64"] {
            problems.push(format!("builds for {platforms:?}, not linux/amd64 and linux/arm64"));
        }
        for build in builds {
            let arch = match text(build, "/platform") {
                "linux/amd64" => "x86_64",
                "linux/arm64" => "aarch64",
                _ => continue,
            };
            let [file] = build.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() else {
                problems.push(format!("{build}: not one file"));
                continue;
            };
            if text(file, "/url") != format!("{at}devin-{version}-{arch}-unknown-linux.tar.gz") {
                problems.push(format!("{file}: not the version's bundle for {arch}"));
            }
            if !is_sha256(text(file, "/sha256")) {
                problems.push(format!("{:?} is no SHA-256", text(file, "/sha256")));
            }
            if file.get("size").and_then(Value::as_u64).unwrap_or(0) == 0 {
                problems.push(format!("{file}: no size"));
            }
            if text(file, "/path") != "." || text(file, "/unpack/format") != "tar.gz" {
                problems.push(format!("{file}: not unpacked as a tar.gz at the harness's directory"));
            }
        }
        problems
    }

    /// Devin's pin names Cognition's two Linux bundles of one version, each unpacked; a pin with a short
    /// digest, a platform missing or a bundle not unpacked is refused (plan step 3's test).
    #[test]
    fn devins_pin_is_cognitions_two_linux_bundles() -> anyhow::Result<()> {
        let pin: Value = serde_json::from_str(devin()?.pin)?;
        assert_eq!(devin_pin_problems(&pin), Vec::<String>::new());
        let mutants: [(&str, fn(&mut Value)); 3] = [
            ("a 63-character digest", |p| {
                if let Some(s) = p.pointer_mut("/builds/0/files/0/sha256") {
                    *s = Value::from("0".repeat(63));
                }
            }),
            ("linux/arm64 missing", |p| {
                if let Some(builds) = p.get_mut("builds").and_then(Value::as_array_mut) {
                    builds.retain(|b| b.get("platform").and_then(Value::as_str) != Some("linux/arm64"));
                }
            }),
            ("a bundle not unpacked", |p| {
                if let Some(file) = p.pointer_mut("/builds/1/files/0").and_then(Value::as_object_mut) {
                    file.remove("unpack");
                }
            }),
        ];
        for (what, mutate) in mutants {
            let mut p = pin.clone();
            mutate(&mut p);
            assert!(!devin_pin_problems(&p).is_empty(), "a pin with {what} passed");
        }
        Ok(())
    }

    /// The fields of `h` Shards' OSI config reader doesn't know, at its top and in `run`: it refuses any
    /// (Shards crates/image/src/osi.rs at main ac87190d3: `Config` and `Run`, `deny_unknown_fields`).
    fn unknown_osi_fields(h: &Value) -> Vec<String> {
        let unknown = |v: Option<&Value>, known: &[&str]| -> Vec<String> {
            v.and_then(Value::as_object).into_iter().flat_map(|o| o.keys()).filter(|k| !known.contains(&k.as_str())).cloned().collect()
        };
        let mut fields = unknown(Some(h), &["schemaVersion", "name", "version", "description", "platform", "run", "skills", "mcp", "asks"]);
        fields.extend(unknown(h.get("run"), &["command", "env", "workdir"]));
        fields
    }

    /// The command kiln starts a harness that speaks ACP with: its program, then its ACP arguments.
    fn acp_command(kind: &Kind) -> anyhow::Result<Vec<&'static str>> {
        let Protocol::Acp(args) = kind.protocol else {
            anyhow::bail!("{} doesn't speak ACP", kind.id);
        };
        Ok(std::iter::once(kind.program).chain(args.iter().copied()).collect())
    }

    /// harness.json is Shards' full OSI form, every field one its reader knows: schema version 1, Devin's
    /// name, the pin's version; the command kiln starts Devin with (its Kind's program and ACP arguments,
    /// f9), which Shards' init finds on the harness's PATH; and its env exactly the two XDG roots its
    /// volumes hold (f10, f12), set after init's PATH, HOME and SHARDS_DOMAIN. No update setting: Devin's
    /// updater stays off without the installer's marker (f26).
    #[test]
    fn devins_harness_json_starts_it_as_kiln_does() -> anyhow::Result<()> {
        let b = devin()?;
        let pin: Value = serde_json::from_str(b.pin)?;
        let h: Value = serde_json::from_str(file(b, "harness.json")?)?;
        assert_eq!(unknown_osi_fields(&h), Vec::<String>::new());
        assert_eq!(h.get("schemaVersion").and_then(Value::as_u64), Some(1));
        assert_eq!(text(&h, "/name"), b.harness);
        assert_eq!(h.get("version"), pin.get("version"));
        let kind = Kind::get(b.harness).ok_or_else(|| anyhow::anyhow!("no harness {}", b.harness))?;
        let command = strings(h.pointer("/run/command"));
        assert_eq!(command, acp_command(kind)?);
        // Changing the Kind's ACP arguments alone tells them apart.
        assert_ne!(command, acp_command(&Kind { protocol: Protocol::Acp(&["acp", "--stdio"]), ..*kind })?);
        assert_eq!(strings(h.pointer("/run/env")), ["XDG_CONFIG_HOME=/home/devin/.config", "XDG_DATA_HOME=/home/devin/.local/share"]);
        Ok(())
    }

    /// Devin's Agentfile is the layout's: the pin's base by digest and no RUN, as both builds are static
    /// (f5); TARGETARCH naming the directory kiln writes; exactly the bring-up's three grants, until step
    /// 9's (f14); exactly the project's and Devin's two XDG volumes, the harness's alone (f10, f12); no ENV,
    /// and no ARG but those two.
    #[test]
    fn devins_agentfile_is_the_layouts() -> anyhow::Result<()> {
        let b = devin()?;
        let pin: Value = serde_json::from_str(b.pin)?;
        assert!(text(&pin, "/base/digest").strip_prefix("sha256:").is_some_and(is_sha256), "the pin's base is not by digest");
        let base = format!("ARG BASE={}@{}", text(&pin, "/base/ref"), text(&pin, "/base/digest"));
        assert_eq!(
            instructions(b.text),
            [
                base.as_str(),
                "FROM ${BASE}",
                "ARG TARGETARCH",
                "HARNESS devin FROM ./devin-${TARGETARCH}",
                "NETWORK --dns --egress=443/tcp provider FOR devin",
                "EXPOSE --harnesses=devin 443/tcp AS egress FOR provider",
                "CONNECT devin WITH devin ON provider",
                "VOLUME --target-kind=harness /workspace FOR devin",
                "VOLUME --target-kind=harness /home/devin/.config/devin FOR devin",
                "VOLUME --target-kind=harness /home/devin/.local/share/devin FOR devin",
            ]
        );
        Ok(())
    }

    /// What in a built-in looks like a credential (research f13): an ENV or ARG named like one (key,
    /// token, secret, password, credential), or a harness's env entry so named; Devin's own
    /// (WINDSURF_API_KEY, DEVIN_API_KEY, its sign-in's credentials.toml) in an instruction, the pin or a
    /// file; or a Cognition key's prefix, `cog_` (docs/ci.md's Devin row), anywhere.
    fn credential_problems(text: &str, pin: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
        let named = regex::Regex::new(r"(?i)key|token|secret|passw|credential")?;
        let devins = ["WINDSURF_API_KEY", "DEVIN_API_KEY", "credentials.toml"];
        let mut problems = Vec::new();
        for line in instructions(text) {
            let mut words = line.split_whitespace();
            let op = words.next().unwrap_or_default();
            if op.eq_ignore_ascii_case("ENV") || op.eq_ignore_ascii_case("ARG") {
                for name in words.map(|w| w.split('=').next().unwrap_or_default()).filter(|n| named.is_match(n)) {
                    problems.push(format!("{op} {name} is named like a credential"));
                }
            }
            problems.extend(devins.iter().filter(|d| line.contains(**d)).map(|d| format!("{d} in the Agentfile: {line}")));
        }
        for (name, body) in files {
            if let Ok(h) = serde_json::from_str::<Value>(body) {
                for e in strings(h.pointer("/run/env")).into_iter().filter(|e| e.split('=').next().is_some_and(|n| named.is_match(n))) {
                    problems.push(format!("{name}: env {e} is named like a credential"));
                }
            }
            problems.extend(devins.iter().filter(|d| name.contains(**d) || body.contains(**d)).map(|d| format!("{d} in {name}")));
        }
        problems.extend(devins.iter().filter(|d| pin.contains(**d)).map(|d| format!("{d} in pin.json")));
        for t in [text, pin].into_iter().chain(files.iter().map(|(_, body)| *body)) {
            if t.contains("cog_") {
                problems.push("a Cognition key's prefix, cog_".into());
            }
        }
        Ok(problems)
    }

    /// No built-in carries a credential, and the scan finds one put in.
    #[test]
    fn no_builtin_carries_a_credential() -> anyhow::Result<()> {
        for b in super::ALL {
            assert_eq!(credential_problems(b.text, b.pin, b.files)?, Vec::<String>::new(), "{}", b.name);
        }
        let b = devin()?;
        let leaky = format!("{}ENV WINDSURF_API_KEY=x\n", b.text);
        assert!(!credential_problems(&leaky, b.pin, b.files)?.is_empty(), "ENV WINDSURF_API_KEY passed");
        let leaky_env = [("harness.json", r#"{"run":{"command":["devin","acp"],"env":["DEVIN_API_KEY=x"]}}"#)];
        assert!(!credential_problems(b.text, b.pin, &leaky_env)?.is_empty(), "env DEVIN_API_KEY passed");
        let signed_in = [("credentials.toml", "")];
        assert!(!credential_problems(b.text, b.pin, &signed_in)?.is_empty(), "a credentials.toml passed");
        let key = format!("{}# cog_0123456789abcdef\n", b.text);
        assert!(!credential_problems(&key, b.pin, b.files)?.is_empty(), "a cog_ key passed");
        Ok(())
    }

    /// The repository's paths this table's rows include (`include_str!` before its tests), from its root.
    fn included() -> Vec<String> {
        let rows = include_str!("builtin.rs").split("#[cfg(test)]").next().unwrap_or_default();
        rows.split("include_str!(\"../../../../").skip(1).filter_map(|s| s.split('"').next()).map(str::to_owned).collect()
    }

    /// What keeps kiln's server image from building with `included`: a path the Dockerfile's build stage
    /// copies to no place of its own (`COPY <dir>/ <dir>/`) before cargo builds kiln, or one that
    /// .dockerignore drops from the build context.
    fn image_problems(dockerfile: &str, dockerignore: &str, included: &[String]) -> Vec<String> {
        let copied: Vec<&str> = dockerfile
            .lines()
            .map(str::trim)
            .skip_while(|l| !l.ends_with(" AS build"))
            .take_while(|l| !l.contains("cargo build"))
            .filter_map(|l| l.strip_prefix("COPY "))
            .filter_map(|args| match args.split_whitespace().collect::<Vec<_>>().as_slice() {
                [from, to] if from == to && from.ends_with('/') => Some(*from),
                _ => None,
            })
            .collect();
        let ignores: Vec<&str> = dockerignore.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!')).collect();
        let mut problems = Vec::new();
        for path in included {
            if !copied.iter().any(|dir| path.starts_with(dir)) {
                problems.push(format!("the Dockerfile's build stage copies no directory holding {path}"));
            }
            problems.extend(ignores.iter().filter(|p| drops(p, path)).map(|p| format!(".dockerignore's {p} drops {path}")));
        }
        problems
    }

    /// Whether a .dockerignore pattern drops `path` or a directory holding it, matching as
    /// moby/patternmatcher does the forms kiln's takes: a path, `dir/`, `**` for any directories, and `*`
    /// within a name.
    fn drops(pattern: &str, path: &str) -> bool {
        let pattern: Vec<&str> = pattern.trim_end_matches('/').split('/').collect();
        let parts: Vec<&str> = path.split('/').collect();
        (1..=parts.len()).any(|n| parts.get(..n).is_some_and(|held| glob(&pattern, held)))
    }

    fn glob(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.split_first(), path.split_first()) {
            (None, None) => true,
            (Some((&"**", rest)), _) => glob(rest, path) || path.split_first().is_some_and(|(_, tail)| glob(pattern, tail)),
            (Some((p, rest)), Some((name, tail))) => name_glob(p, name) && glob(rest, tail),
            _ => false,
        }
    }

    fn name_glob(pattern: &str, name: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == name,
            Some((head, tail)) => name.strip_prefix(head).is_some_and(|rest| (0..=rest.len()).any(|i| rest.get(i..).is_some_and(|r| name_glob(tail, r)))),
        }
    }

    /// kiln's server image builds with what this table embeds: the Dockerfile's build stage copies every
    /// path a row includes to its own place before cargo builds kiln, and .dockerignore keeps them in the
    /// build context (research f25). Without the stage's COPY of .kiln/images/, or with .kiln/ ignored, it
    /// fails.
    #[test]
    fn the_server_image_copies_every_path_a_row_includes() {
        let (dockerfile, ignore) = (include_str!("../../../../Dockerfile"), include_str!("../../../../.dockerignore"));
        let included = included();
        assert!(included.iter().any(|p| p == ".kiln/images/harness-devin/Agentfile"), "{included:?}");
        assert_eq!(image_problems(dockerfile, ignore, &included), Vec::<String>::new());
        let uncopied = dockerfile.replace("COPY .kiln/images/ .kiln/images/\n", "");
        assert!(!image_problems(&uncopied, ignore, &included).is_empty(), "the Dockerfile without its COPY of .kiln/images/ passed");
        let ignored = format!("{ignore}.kiln/\n");
        assert!(!image_problems(dockerfile, &ignored, &included).is_empty(), ".kiln/ ignored passed");
        assert!(drops("**/*.log", "a/b/c.log") && drops("docs/", "docs/x.md") && !drops("**/target", ".kiln/images/x"));
    }
}
