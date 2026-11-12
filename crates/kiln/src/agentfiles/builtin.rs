//! The Agentfiles kiln ships, one per harness it runs in a Shards microVM (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, "Every kiln harness in Shards microVMs"): each embedded in kiln's
//! one binary, listed read-only beside a person's own, and copied to make one theirs. Each harness's own
//! step adds its Agentfile here (`include_str!`) from `.kiln/images/harness-<id>/`, with its pin and the
//! other files of its build context; kiln's Dockerfile copies that folder for the build.

use serde::Serialize;

/// An Agentfile kiln ships.
pub struct Builtin {
    /// Its name, an image's (as a person's Agentfile's is: [`super::name_problem`]).
    pub name: &'static str,
    /// The harness it runs (`crate::harness::kinds::Kind`'s id: `claude`, `codex` ...).
    pub harness: &'static str,
    pub text: &'static str,
    /// What its build installs, pinned: `pin.json`, as its `pin.sh` wrote it (steps 19 to 28's shared
    /// layout: the version, how it was verified, the base, and per platform its files by URL, size and
    /// SHA-256, an archive's with its `unpack`).
    pub pin: &'static str,
    /// The other files of its build context, by name (`harness.json` ...).
    pub files: &'static [(&'static str, &'static str)],
    /// How a smoke run asks the harness its version.
    pub smoke: Smoke,
}

/// A harness asked its version: its program's arguments, and the exact standard output it answers.
pub struct Smoke {
    pub args: &'static [&'static str],
    /// `{version}` stands for the pin's version.
    pub stdout: &'static str,
}

impl Smoke {
    /// What the harness prints when it's `version`.
    pub fn expected(&self, version: &str) -> String {
        self.stdout.replace("{version}", version)
    }
}

/// Every one, in the order they're listed.
pub const ALL: &[Builtin] = &[
    Builtin {
        name: "harness-goose",
        harness: "goose",
        text: include_str!("../../../../.kiln/images/harness-goose/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-goose/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-goose/harness.json"))],
        // clap prints Goose's empty display name, then a space and the version: 1.59.0's musl build wrote
        // the bytes 20 31 2e 35 39 2e 30 0a (step 23's research 01a1ce705e3d72af8563e07fa92a7460, f12).
        smoke: Smoke { args: &["--version"], stdout: " {version}\n" },
    },
    Builtin {
        name: "harness-pi",
        harness: "pi",
        text: include_str!("../../../../.kiln/images/harness-pi/Agentfile"),
        pin: include_str!("../../../../.kiln/images/harness-pi/pin.json"),
        files: &[("harness.json", include_str!("../../../../.kiln/images/harness-pi/harness.json"))],
        // pi prints its bare version: 1.5.2's x64 build wrote the bytes 31 2e 35 2e 32 0a to its standard
        // output, and nothing to its standard error (step 28's record, docs/research/s28-pi-agentfile/step2/).
        smoke: Smoke { args: &["--version"], stdout: "{version}\n" },
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
    use std::collections::BTreeSet;
    use std::path::Path;

    use serde_json::Value;

    use super::*;
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

    /// The instructions of an Agentfile, comments and blank lines aside.
    fn instructions(text: &str) -> Vec<&str> {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
    }

    /// `ARG BASE=`'s default.
    fn base_of(text: &str) -> Option<&str> {
        instructions(text).into_iter().find_map(|l| l.strip_prefix("ARG BASE="))
    }

    /// The paths of the Agentfile's volumes for `harness` alone.
    fn harness_volumes<'a>(text: &'a str, harness: &str) -> Vec<&'a str> {
        let tail = format!(" FOR {harness}");
        instructions(text).into_iter().filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(tail.as_str())).collect()
    }

    /// A built-in's harness file, as JSON.
    fn harness_file(b: &Builtin) -> anyhow::Result<Value> {
        let text = b.files.iter().find(|(n, _)| *n == "harness.json").map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no harness.json", b.name))?;
        Ok(serde_json::from_str(text)?)
    }

    /// The command a harness file starts its harness with.
    fn command_of(h: &Value) -> Vec<String> {
        h.pointer("/run/command").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
    }

    /// What kiln runs for `kind` when it's an ACP harness: its program, then its arguments.
    fn acp_command(kind: &Kind) -> Option<Vec<String>> {
        match kind.protocol {
            Protocol::Acp(args) => Some(std::iter::once(kind.program).chain(args.iter().copied()).map(str::to_owned).collect()),
            _ => None,
        }
    }

    fn is_sha256(d: &str) -> bool {
        d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// A path relative to a folder that stays inside it: no root, and no empty, `.` or `..` part.
    fn is_inside(p: &str) -> bool {
        !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
    }

    /// What in a pin would mislead a build, as the shared layout writes pins (step 19's plan): a
    /// version not x.y.z, a base not pinned by digest, a platform missing or twice, and per file an
    /// https URL, a SHA-256, a size, a path inside its folder and octal permissions, an archive's
    /// `unpack` naming its one member with that member's own SHA-256 and size. An archive unpacked
    /// whole (step 25's unpacker's tree: no `member`, pi's tarball) goes into its folder (`.`) or one
    /// inside it with no mode, as its entries keep their own, and gives the totals the unpacker holds
    /// it to, as a member gives its own digest; an unpack is never both.
    fn pin_problems(text: &str) -> Vec<String> {
        let pin: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return vec![format!("not JSON: {e}")],
        };
        let mut found = Vec::new();
        let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
        if !(version.split('.').count() == 3 && version.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))) {
            found.push(format!("version `{version}` isn't x.y.z"));
        }
        let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
        if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
            found.push(format!("base `{base}` isn't pinned by digest"));
        }
        let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
        for platform in ["linux/amd64", "linux/arm64"] {
            if !builds.iter().any(|b| b.get("platform").and_then(Value::as_str) == Some(platform)) {
                found.push(format!("no build for {platform}"));
            }
        }
        let mut seen = BTreeSet::new();
        for b in &builds {
            let at = format!("{} {}", b.get("platform").and_then(Value::as_str).unwrap_or_default(), b.get("variant").and_then(Value::as_str).unwrap_or_default());
            if !seen.insert(at.clone()) {
                found.push(format!("two builds for {at}"));
            }
            let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
            if files.is_empty() {
                found.push(format!("{at}: no files"));
            }
            for f in &files {
                let s = |k: &str| f.get(k).and_then(Value::as_str);
                let tree = f.get("unpack").is_some_and(|u| u.get("member").is_none());
                if !s("url").is_some_and(|u| u.starts_with("https://")) {
                    found.push(format!("{at}: no https URL"));
                }
                if !s("sha256").is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none() {
                    found.push(format!("{at}: a file without its SHA-256 and size"));
                }
                if !s("path").is_some_and(|p| is_inside(p) || (tree && p == ".")) {
                    found.push(format!("{at}: a path outside its folder"));
                }
                if tree {
                    if f.get("mode").is_some() {
                        found.push(format!("{at}: a mode for a whole tree, whose entries keep their own"));
                    }
                } else if !s("mode").is_some_and(|m| (3..=4).contains(&m.len()) && m.bytes().all(|b| (b'0'..=b'7').contains(&b))) {
                    found.push(format!("{at}: a mode that isn't octal permissions"));
                }
                if let Some(u) = f.get("unpack") {
                    let us = |k: &str| u.get(k).and_then(Value::as_str);
                    if us("format") != Some("tar.gz") {
                        found.push(format!("{at}: an unpack that isn't tar.gz"));
                    }
                    if tree {
                        if ["unpacked_bytes", "entries"].iter().any(|k| u.get(k).and_then(Value::as_u64).is_none()) {
                            found.push(format!("{at}: a tree without its totals"));
                        }
                        if ["sha256", "size"].iter().any(|k| u.get(k).is_some()) {
                            found.push(format!("{at}: an unpack that's both a tree and a member"));
                        }
                    } else {
                        if !us("member").map(|m| m.strip_prefix("./").unwrap_or(m)).is_some_and(is_inside) {
                            found.push(format!("{at}: a member outside the folder"));
                        }
                        if !us("sha256").is_some_and(is_sha256) || u.get("size").and_then(Value::as_u64).is_none() {
                            found.push(format!("{at}: a member without its own SHA-256 and size"));
                        }
                        if ["strip", "unpacked_bytes", "entries"].iter().any(|k| u.get(k).is_some()) {
                            found.push(format!("{at}: an unpack that's both a tree and a member"));
                        }
                    }
                }
            }
        }
        found
    }

    /// What in `text` holds or names a credential: an ENV or ARG named like one, a variable named like
    /// one given a value (`OPENAI_API_KEY=…`, in a harness file's env too), or a provider's key (`sk-…`).
    fn credentials_in(text: &str) -> Vec<String> {
        const WORDS: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
        let named = |name: &str| WORDS.iter().any(|w| name.to_ascii_uppercase().contains(w));
        let mut found = Vec::new();
        for l in instructions(text) {
            let Some(rest) = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG ")) else { continue };
            for name in rest.split_whitespace().map(|w| w.split('=').next().unwrap_or(w)) {
                if named(name) {
                    found.push(format!("{l}: names {name}"));
                }
            }
        }
        let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let mut start = None;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            match (is_word(c), start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    let word = text.get(s..i).unwrap_or_default();
                    if c == '=' && named(word) {
                        found.push(format!("{word}= gives a value"));
                    }
                    let tail = text.get(i..).unwrap_or_default();
                    if word == "sk" && tail.starts_with('-') && tail.chars().skip(1).take_while(|c| is_word(*c) || *c == '-').count() >= 20 {
                        found.push("a provider key (sk-…)".to_owned());
                    }
                    start = None;
                }
                _ => {}
            }
        }
        found
    }

    /// Of `paths` (from the repository's root), those no COPY of the Dockerfile's `build` stage brings
    /// in from the build context.
    fn uncopied<'a>(dockerfile: &str, paths: &[&'a str]) -> Vec<&'a str> {
        let mut sources = Vec::new();
        let stage = dockerfile.lines().map(str::trim).skip_while(|l| !(l.starts_with("FROM ") && l.ends_with(" AS build"))).skip(1).take_while(|l| !l.starts_with("FROM "));
        for rest in stage.filter_map(|l| l.strip_prefix("COPY ")) {
            if rest.contains("--from=") {
                continue;
            }
            let words: Vec<&str> = rest.split_whitespace().filter(|w| !w.starts_with("--")).collect();
            if let Some((_, from)) = words.split_last() {
                sources.extend(from.iter().map(|s| s.to_string()));
            }
        }
        paths.iter().copied().filter(|p| !sources.iter().any(|s| if s.ends_with('/') { p.starts_with(s.as_str()) } else { p == s })).collect()
    }

    /// Each pin holds (both platforms, every digest a SHA-256, an archive's member with its own and a
    /// tree with its totals), each harness file is an OSI config of its harness at the pin's version,
    /// and each Agentfile builds on the base its pin was checked with by default.
    #[test]
    fn step23_every_builtin_pin_and_harness_file_hold() -> anyhow::Result<()> {
        for b in ALL {
            let problems = pin_problems(b.pin);
            anyhow::ensure!(problems.is_empty(), "{}: {problems:?}", b.name);
            let pin: Value = serde_json::from_str(b.pin)?;
            let (version, base) = (pin.get("version").and_then(Value::as_str), pin.get("base").and_then(Value::as_str));
            anyhow::ensure!(base_of(b.text) == base, "{}: the Agentfile's BASE isn't its pin's {base:?}", b.name);
            let h = harness_file(b)?;
            anyhow::ensure!(h.get("schemaVersion") == Some(&Value::from(1)), "{}: {h}", b.name);
            anyhow::ensure!(h.get("name").and_then(Value::as_str) == Some(b.harness), "{}: {h}", b.name);
            anyhow::ensure!(h.get("version").and_then(Value::as_str) == version, "{}: {h}", b.name);
            anyhow::ensure!(!command_of(&h).is_empty(), "{}: no run.command", b.name);
            anyhow::ensure!(b.smoke.stdout.contains("{version}"), "{}: the smoke's line doesn't name the version", b.name);
        }
        Ok(())
    }

    /// `pin` with the value at `path` (a JSON pointer) set to `to`, or taken out without one.
    fn edited(pin: &Value, path: &str, to: Option<Value>) -> anyhow::Result<String> {
        let mut v = pin.clone();
        let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
        let at = v.pointer_mut(parent).and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("{path}"))?;
        match to {
            Some(to) => at.insert(key.to_owned(), to),
            None => at.remove(key),
        };
        Ok(v.to_string())
    }

    /// Whether one of the problems found in a pin's `text` says `what`.
    fn says(text: &str, what: &str) -> bool {
        pin_problems(text).iter().any(|p| p.contains(what))
    }

    /// A pin no build could trust is caught: a digest one digit short, a platform missing, an
    /// archive's member without its own digest, a member path that climbs out.
    #[test]
    fn step23_a_pin_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
        let goose = ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("no goose built-in"))?;
        let real: Value = serde_json::from_str(goose.pin)?;
        let short = real.pointer("/builds/0/files/0/sha256").and_then(Value::as_str).and_then(|d| d.get(1..)).map(Value::from);
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/sha256", short)?, "without its SHA-256"));
        anyhow::ensure!(says(&edited(&real, "/builds/1/platform", Some(Value::from("linux/amd64")))?, "no build for linux/arm64"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/unpack/sha256", None)?, "member without its own SHA-256"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/unpack/member", Some(Value::from("../goose")))?, "member outside"));
        anyhow::ensure!(says(&edited(&real, "/base", Some(Value::from("buildpack-deps:trixie-scm")))?, "pinned by digest"));
        anyhow::ensure!(pin_problems(goose.pin).is_empty());
        Ok(())
    }

    /// A tree no build could trust is caught, as a member is: pi's tarball given a mode, a member's
    /// digest beside its totals, a total left out, or a path that climbs out; and rg's member given a
    /// tree's strip, or the folder itself as its path.
    #[test]
    fn step28_a_tree_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
        let pi = ALL.iter().find(|b| b.harness == "pi").ok_or_else(|| anyhow::anyhow!("no pi built-in"))?;
        let real: Value = serde_json::from_str(pi.pin)?;
        anyhow::ensure!(pin_problems(pi.pin).is_empty(), "{:?}", pin_problems(pi.pin));
        // files/0 is pi's tarball, unpacked whole; files/1 is rg, one member of its own.
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/mode", Some(Value::from("0755")))?, "a mode for a whole tree"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/unpack/sha256", Some(Value::from("0".repeat(64))))?, "both a tree and a member"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/unpack/entries", None)?, "a tree without its totals"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/0/path", Some(Value::from("../pi")))?, "a path outside"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/1/unpack/strip", Some(Value::from(1)))?, "both a tree and a member"));
        anyhow::ensure!(says(&edited(&real, "/builds/0/files/1/path", Some(Value::from(".")))?, "a path outside"));
        Ok(())
    }

    /// Nothing kiln ships holds a credential or names an ENV or ARG like one; adding one is caught.
    #[test]
    fn step23_no_builtin_holds_a_credential() -> anyhow::Result<()> {
        for b in ALL {
            for (name, text) in [("Agentfile", b.text), ("pin.json", b.pin)].into_iter().chain(b.files.iter().copied()) {
                let found = credentials_in(text);
                anyhow::ensure!(found.is_empty(), "{} {name}: {found:?}", b.name);
            }
        }
        let goose = ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("no goose built-in"))?;
        anyhow::ensure!(!credentials_in(&format!("{}ENV OPENAI_API_KEY=x\n", goose.text)).is_empty());
        anyhow::ensure!(!credentials_in("ARG GITHUB_TOKEN\n").is_empty());
        anyhow::ensure!(!credentials_in(r#"{"env":["ANTHROPIC_API_KEY=y"]}"#).is_empty());
        anyhow::ensure!(!credentials_in("# sk-proj-abcdefghijklmnopqrstuvwx\n").is_empty());
        Ok(())
    }

    /// Goose runs as kiln's Goose kind starts it (`goose acp`), with its root on the volume the
    /// Agentfile gives it; a kind or a volume that drifts from the files is caught.
    #[test]
    fn step23_goose_runs_as_kilns_goose_kind_with_its_root_on_its_volume() -> anyhow::Result<()> {
        let goose = ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("no goose built-in"))?;
        let kind = Kind::get("goose").ok_or_else(|| anyhow::anyhow!("no goose kind"))?;
        let h = harness_file(goose)?;
        anyhow::ensure!(Some(command_of(&h)) == acp_command(kind), "{h} isn't {:?}", acp_command(kind));
        let drifted = Kind { protocol: Protocol::Acp(&["--acp"]), ..*kind };
        anyhow::ensure!(Some(command_of(&h)) != acp_command(&drifted));

        let env: Vec<&str> = h.pointer("/run/env").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let root = env.iter().find_map(|e| e.strip_prefix("GOOSE_PATH_ROOT=")).ok_or_else(|| anyhow::anyhow!("no GOOSE_PATH_ROOT in {env:?}"))?;
        anyhow::ensure!(env.len() == 1, "{env:?}");
        let volumes = harness_volumes(goose.text, "goose");
        anyhow::ensure!(volumes == ["/workspace", root], "{volumes:?}");
        let moved = goose.text.replace("/home/goose/.local/share/goose FOR goose", "/home/goose/.goose FOR goose");
        anyhow::ensure!(harness_volumes(&moved, "goose").get(1) != Some(&root));
        Ok(())
    }

    /// Goose's Agentfile is the plan's shape: a base by digest, Goose's own build per platform with no
    /// RUN, exactly the bring-up's three grants, exactly its two volumes, and no ENV; its smoke line is
    /// what Goose prints.
    #[test]
    fn step23_goose_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
        let goose = ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("no goose built-in"))?;
        let lines = instructions(goose.text);
        let want = [
            "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
            "FROM ${BASE}",
            "ARG TARGETARCH",
            "HARNESS goose FROM ./goose-${TARGETARCH}",
            "NETWORK --dns --egress=443/tcp provider FOR goose",
            "EXPOSE --harnesses=goose 443/tcp AS egress FOR provider",
            "CONNECT goose WITH goose ON provider",
            "VOLUME --target-kind=harness /workspace FOR goose",
            "VOLUME --target-kind=harness /home/goose/.local/share/goose FOR goose",
        ];
        anyhow::ensure!(lines == want, "{lines:#?}");
        anyhow::ensure!(!lines.iter().any(|l| l.starts_with("RUN ") || l.starts_with("ENV ")));
        // The bytes 1.59.0's `goose --version` wrote (step 23's research, f12).
        let version = serde_json::from_str::<Value>(goose.pin)?.get("version").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
        anyhow::ensure!(version != "1.59.0" || goose.smoke.expected(&version).as_bytes() == [0x20, 0x31, 0x2e, 0x35, 0x39, 0x2e, 0x30, 0x0a]);
        anyhow::ensure!(goose.smoke.args == ["--version"]);
        Ok(())
    }

    /// pi runs as kiln's Pi kind starts it: harness.json's command is the kind's program and what
    /// `pi::args` gives every session before its own flags (`--mode rpc`, f14). Its env is exactly its
    /// config directory, on the volume the Agentfile gives it, and the version check off (f15, f19),
    /// so it sets nothing shards-init sets first (PATH, HOME, SHARDS_DOMAIN; f16); and the one file
    /// beside the Agentfile is harness.json, with no sign-in, as pi's auth.json lives on that volume
    /// (f27). A kind, an env, a volume or a file that drifts from them is caught.
    #[test]
    fn step28_pi_runs_as_kilns_pi_kind_with_its_config_on_its_volume() -> anyhow::Result<()> {
        let pi = ALL.iter().find(|b| b.harness == "pi").ok_or_else(|| anyhow::anyhow!("no pi built-in"))?;
        let kind = Kind::get("pi").ok_or_else(|| anyhow::anyhow!("no pi kind"))?;
        let started = |kind: &Kind| -> Vec<String> {
            let args = crate::harness::pi::args(&crate::harness::session::Spec { kind: kind.id.to_owned(), ..Default::default() });
            std::iter::once(kind.program.to_owned()).chain(args.into_iter().take_while(|a| a != "--session-id")).collect()
        };
        let h = harness_file(pi)?;
        anyhow::ensure!(command_of(&h) == started(kind), "{h} isn't {:?}", started(kind));
        anyhow::ensure!(command_of(&h) != started(&Kind { program: "pi-rpc", ..*kind }));

        let env: Vec<&str> = h.pointer("/run/env").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let config = env.iter().find_map(|e| e.strip_prefix("PI_CODING_AGENT_DIR=")).ok_or_else(|| anyhow::anyhow!("no PI_CODING_AGENT_DIR in {env:?}"))?;
        let want = [format!("PI_CODING_AGENT_DIR={config}"), "PI_SKIP_VERSION_CHECK=1".to_owned()];
        anyhow::ensure!(env == want, "{env:?}");
        anyhow::ensure!([env.as_slice(), &["HOME=/home/pi"]].concat() != want);
        let volumes = harness_volumes(pi.text, "pi");
        anyhow::ensure!(volumes == ["/workspace", config], "{volumes:?}");
        let moved = pi.text.replace("/home/pi/.pi/agent FOR pi", "/home/pi/.pi FOR pi");
        anyhow::ensure!(harness_volumes(&moved, "pi").get(1) != Some(&config));

        let names = |files: &[(&str, &str)]| files.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>();
        anyhow::ensure!(names(pi.files) == ["harness.json"], "{:?}", names(pi.files));
        anyhow::ensure!(names([pi.files, &[("auth.json", "{}")]].concat().as_slice()) != ["harness.json"]);
        Ok(())
    }

    /// pi's Agentfile is the plan's shape: a base by digest, pi's own build per platform with no RUN,
    /// exactly the bring-up's three grants, exactly its two volumes, and no ENV. Its pin lays each
    /// platform's directory out as pi needs it: its tarball whole at the root less its pi/ (f11), and
    /// ripgrep's and fd's builds at bin/rg and bin/fd, 0755, so that pi downloads neither (f17, f18); a
    /// platform without rg is caught. Its smoke line is what pi prints.
    #[test]
    fn step28_pi_agentfile_and_pin_are_the_plans_shape() -> anyhow::Result<()> {
        let pi = ALL.iter().find(|b| b.harness == "pi").ok_or_else(|| anyhow::anyhow!("no pi built-in"))?;
        let lines = instructions(pi.text);
        let want = [
            "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
            "FROM ${BASE}",
            "ARG TARGETARCH",
            "HARNESS pi FROM ./pi-${TARGETARCH}",
            "NETWORK --dns --egress=443/tcp provider FOR pi",
            "EXPOSE --harnesses=pi 443/tcp AS egress FOR provider",
            "CONNECT pi WITH pi ON provider",
            "VOLUME --target-kind=harness /workspace FOR pi",
            "VOLUME --target-kind=harness /home/pi/.pi/agent FOR pi",
        ];
        anyhow::ensure!(lines == want, "{lines:#?}");

        let pin: Value = serde_json::from_str(pi.pin)?;
        // Each platform's files, each as `path mode what`: a tree's strip, or its member's file name.
        let layout = |pin: &Value| -> Vec<String> {
            let file = |f: &Value| {
                let s = |p: &str| f.pointer(p).and_then(Value::as_str).unwrap_or("-");
                let member = || s("/unpack/member").rsplit('/').next().unwrap_or_default().to_owned();
                let what = f.pointer("/unpack/strip").and_then(Value::as_u64).map_or_else(member, |n| format!("strip {n}"));
                format!("{} {} {what}", s("/path"), s("/mode"))
            };
            let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
            builds.iter().map(|b| b.get("files").and_then(Value::as_array).map(|f| f.iter().map(&file).collect::<Vec<_>>().join(", ")).unwrap_or_default()).collect()
        };
        let each = ". - strip 1, bin/rg 0755 rg, bin/fd 0755 fd";
        anyhow::ensure!(layout(&pin) == [each, each], "{:?}", layout(&pin));
        let mut no_rg = pin.clone();
        no_rg.pointer_mut("/builds/1/files").and_then(Value::as_array_mut).ok_or_else(|| anyhow::anyhow!("no arm64 files"))?.retain(|f| f.get("path") != Some(&Value::from("bin/rg")));
        anyhow::ensure!(layout(&no_rg) != [each, each]);

        // The bytes 1.5.2's x64 `pi --version` wrote (step 28's record).
        let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
        anyhow::ensure!(version != "1.5.2" || pi.smoke.expected(version).as_bytes() == [0x31, 0x2e, 0x35, 0x2e, 0x32, 0x0a]);
        anyhow::ensure!(pi.smoke.args == ["--version"]);
        Ok(())
    }

    /// kiln's own image builds with every file a built-in embeds: the Dockerfile's `build` stage copies
    /// each one, as `.kiln/images/` sits outside `crates/` (step 25's finding: without it kiln's docker
    /// build fails). The Dockerfile without that COPY is caught.
    #[test]
    fn step23_kilns_dockerfile_copies_what_builtins_embed() -> anyhow::Result<()> {
        let dockerfile = include_str!("../../../../Dockerfile");
        let table = include_str!("builtin.rs").split("#[cfg(test)]").next().unwrap_or_default();
        let embedded: Vec<&str> = table.split("include_str!(\"../../../../").skip(1).filter_map(|s| s.split('"').next()).collect();
        anyhow::ensure!(!embedded.is_empty(), "no built-in embeds a file from outside the crate");
        let missing = uncopied(dockerfile, &embedded);
        anyhow::ensure!(missing.is_empty(), "the Dockerfile's build stage copies none of {missing:?}");
        let without: String = dockerfile.lines().filter(|l| l.trim() != "COPY .kiln/images/ .kiln/images/").map(|l| format!("{l}\n")).collect();
        anyhow::ensure!(uncopied(&without, &embedded) == embedded);
        Ok(())
    }

    /// The real Shards, where a test names it (`KILN_TEST_SHARDS`, a `shards` binary, as
    /// `check`'s tests take it): each built-in passes its static check with no network asked for.
    #[test]
    fn step23_a_real_shards_checks_every_builtin() -> anyhow::Result<()> {
        let Some(bin) = std::env::var_os("KILN_TEST_SHARDS") else {
            eprintln!("skipped: KILN_TEST_SHARDS names no shards binary");
            return Ok(());
        };
        crate::rt::block_on(async move {
            let root = tempfile::tempdir()?;
            for b in ALL {
                let c = super::super::check::run(Path::new(&bin), root.path(), b.text).await;
                anyhow::ensure!(c.checked && c.diagnostics.is_empty(), "{}: {c:?}", b.name);
            }
            Ok(())
        })
    }
}
