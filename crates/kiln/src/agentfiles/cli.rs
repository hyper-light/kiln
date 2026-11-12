//! `kiln agentfiles`: the Agentfiles kiln ships, from the command line (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, steps 19 to 28; plan 01a1cf48e8157494ac9a51f7a564f526, step 3).
//! `kiln agentfiles context` writes one's build context and prints its recipe, for kiln's CI and for a
//! build by hand: `shards build --platform … DIR`, tagged with the recipe.

use std::path::PathBuf;

use super::{builtin, context};

#[derive(clap::Args)]
pub struct AgentfilesArgs {
    #[command(subcommand)]
    cmd: AgentfilesCmd,
}

#[derive(clap::Subcommand)]
enum AgentfilesCmd {
    /// Write a built-in Agentfile's build context into DIR: for each platform its harness's directory,
    /// every pinned file downloaded and held to its pin's size and SHA-256, then the Agentfile. Prints
    /// the recipe, the tag its image is built under.
    Context {
        /// The harness it runs: claude, goose ...
        harness: String,
        /// Where to write it (made where it's missing; nothing there is replaced).
        dir: PathBuf,
        /// The platforms, comma-separated: by default both that every built-in is pinned for.
        #[arg(long, value_delimiter = ',', default_value = "linux/amd64,linux/arm64")]
        platform: Vec<String>,
        /// The build's variant, a libc (gnu, musl), where its pin has more than one per platform.
        #[arg(long)]
        variant: Option<String>,
        /// The base image to build on, where not the Agentfile's own (its `ARG BASE`).
        #[arg(long)]
        base: Option<String>,
    },
}

pub async fn run(args: AgentfilesArgs) -> anyhow::Result<()> {
    match args.cmd {
        AgentfilesCmd::Context { harness, dir, platform, variant, base } => {
            let shipped = || builtin::ALL.iter().map(|b| b.harness).collect::<Vec<_>>().join(", ");
            let b = builtin::ALL.iter().find(|b| b.harness == harness).ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for {harness}; it ships {}", shipped()))?;
            let base = match base {
                Some(base) => base,
                None => context::default_base(b.text).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{}'s Agentfile has no ARG BASE: give --base", b.name))?,
            };
            context::materialize(b, &platform, variant.as_deref(), &dir).await.map_err(|e| anyhow::anyhow!("{}: {e}", b.name))?;
            println!("{}", context::recipe(b, &base, variant.as_deref()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use sha2::{Digest as _, Sha256};

    use super::*;

    /// The command line as `kiln agentfiles` reads it.
    #[derive(clap::Parser)]
    struct Line {
        #[command(flatten)]
        args: AgentfilesArgs,
    }

    /// `kiln agentfiles context` reads a harness and a folder, both platforms unless told, and a variant
    /// and a base where given.
    #[test]
    fn step19_context_reads_its_words() -> anyhow::Result<()> {
        let AgentfilesCmd::Context { harness, dir, platform, variant, base } = Line::try_parse_from(["agentfiles", "context", "claude", "/x"])?.args.cmd;
        anyhow::ensure!(harness == "claude" && dir == std::path::Path::new("/x") && platform == ["linux/amd64", "linux/arm64"] && variant.is_none() && base.is_none());
        let line = ["agentfiles", "context", "claude", "/x", "--platform", "linux/arm64", "--variant", "musl", "--base", "alpine@sha256:00"];
        let AgentfilesCmd::Context { platform, variant, base, .. } = Line::try_parse_from(line)?.args.cmd;
        anyhow::ensure!(platform == ["linux/arm64"] && variant.as_deref() == Some("musl") && base.as_deref() == Some("alpine@sha256:00"));
        anyhow::ensure!(Line::try_parse_from(["agentfiles", "context", "claude"]).is_err(), "a context without its folder was read");
        Ok(())
    }

    /// Live, where a test names a folder for it (`KILN_TEST_LIVE_CONTEXT`, under `~/.cache` on kiln's Linux
    /// machines): `kiln agentfiles context claude` makes Claude Code's context for both platforms, for
    /// each libc, from Anthropic's servers, and each binary hashes to its pin (plan
    /// 01a1cf48e8157494ac9a51f7a564f526, step 3's live test: 4 of 4).
    #[test]
    fn step19_a_live_claude_context_holds_the_pinned_builds() -> anyhow::Result<()> {
        let Some(root) = std::env::var_os("KILN_TEST_LIVE_CONTEXT") else {
            eprintln!("skipped: KILN_TEST_LIVE_CONTEXT names no folder");
            return Ok(());
        };
        let claude = builtin::ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no claude built-in"))?;
        let pin: serde_json::Value = serde_json::from_str(claude.pin)?;
        crate::rt::block_on(async move {
            let mut matched = 0;
            for variant in ["gnu", "musl"] {
                let dir = tempfile::tempdir_in(&root)?;
                let cmd = AgentfilesCmd::Context {
                    harness: "claude".into(),
                    dir: dir.path().to_owned(),
                    platform: vec!["linux/amd64".into(), "linux/arm64".into()],
                    variant: Some(variant.into()),
                    base: None,
                };
                run(AgentfilesArgs { cmd }).await?;
                for (platform, arch) in [("linux/amd64", "amd64"), ("linux/arm64", "arm64")] {
                    let want = pin
                        .get("builds")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|b| b.iter().find(|b| b.get("platform").and_then(serde_json::Value::as_str) == Some(platform) && b.get("variant").and_then(serde_json::Value::as_str) == Some(variant)))
                        .and_then(|b| b.pointer("/files/0/sha256"))
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("no {platform} {variant} build pinned"))?;
                    let binary = dir.path().join(format!("claude-{arch}-{variant}/bin/claude"));
                    let mut hash = Sha256::new();
                    let mut file = std::fs::File::open(&binary)?;
                    let mut buf = vec![0u8; 1 << 20];
                    loop {
                        let n = std::io::Read::read(&mut file, &mut buf)?;
                        if n == 0 {
                            break;
                        }
                        hash.update(buf.get(..n).unwrap_or_default());
                    }
                    let got = hex::encode(hash.finalize());
                    anyhow::ensure!(got == want, "{}: SHA-256 {got}, pinned {want}", binary.display());
                    println!("MATCH {platform} {variant} {got}");
                    matched += 1;
                }
            }
            anyhow::ensure!(matched == 4, "{matched} of 4 binaries match their pins");
            Ok(())
        })
    }

    /// Live, where a test names a folder for it (`KILN_TEST_LIVE_CONTEXT`, under `~/.cache` on kiln's Linux
    /// machines): `kiln agentfiles context antigravity` makes agy's context for linux/amd64 and
    /// linux/arm64 from Google's storage. It holds the Agentfile and the two harness directories alone,
    /// each with the embedded harness.json and bin/agy, unpacked from its archive's one entry, hashing to
    /// the pin's member SHA-256 at 0755; and this machine's own, run, prints the pin's version as its
    /// first line (kiln plan 01a1ce705e55753182217afdfe607de9, step 3's live test: 2 of 2).
    #[test]
    fn step26_a_live_antigravity_context_holds_the_pinned_builds() -> anyhow::Result<()> {
        use serde_json::Value;
        let Some(root) = std::env::var_os("KILN_TEST_LIVE_CONTEXT") else {
            eprintln!("skipped: KILN_TEST_LIVE_CONTEXT names no folder");
            return Ok(());
        };
        let agy = builtin::ALL.iter().find(|b| b.harness == "antigravity").ok_or_else(|| anyhow::anyhow!("no antigravity built-in"))?;
        let pin: Value = serde_json::from_str(agy.pin)?;
        let version = pin.get("version").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no version pinned"))?.to_owned();
        let harness_json = agy.files.iter().find(|(name, _)| *name == "harness.json").map(|(_, text)| *text).ok_or_else(|| anyhow::anyhow!("no harness.json"))?;
        let names = |dir: &std::path::Path| -> anyhow::Result<Vec<String>> {
            let mut names = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect::<Result<Vec<_>, _>>()?;
            names.sort();
            Ok(names)
        };
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir_in(&root)?;
            let cmd = AgentfilesCmd::Context { harness: "antigravity".into(), dir: dir.path().to_owned(), platform: vec!["linux/amd64".into(), "linux/arm64".into()], variant: None, base: None };
            run(AgentfilesArgs { cmd }).await?;
            anyhow::ensure!(names(dir.path())? == ["Agentfile", "antigravity-amd64", "antigravity-arm64"], "the context holds {:?}", names(dir.path())?);
            anyhow::ensure!(std::fs::read_to_string(dir.path().join("Agentfile"))? == agy.text, "the Agentfile isn't the embedded one");
            let mut matched = 0;
            for (platform, arch) in [("linux/amd64", "amd64"), ("linux/arm64", "arm64")] {
                let want = pin
                    .get("builds")
                    .and_then(Value::as_array)
                    .and_then(|b| b.iter().find(|b| b.get("platform").and_then(Value::as_str) == Some(platform)))
                    .and_then(|b| b.pointer("/files/0/unpack/sha256"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("no {platform} member pinned"))?;
                let harness = dir.path().join(format!("antigravity-{arch}"));
                anyhow::ensure!(names(&harness)? == ["bin", "harness.json"] && names(&harness.join("bin"))? == ["agy"], "{platform}: {:?}", names(&harness)?);
                anyhow::ensure!(std::fs::read_to_string(harness.join("harness.json"))? == harness_json, "{platform}: harness.json isn't the embedded one");
                let binary = harness.join("bin/agy");
                let mut hash = Sha256::new();
                let mut file = std::fs::File::open(&binary)?;
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = std::io::Read::read(&mut file, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(buf.get(..n).unwrap_or_default());
                }
                let got = hex::encode(hash.finalize());
                anyhow::ensure!(got == want, "{}: SHA-256 {got}, the pin's member {want}", binary.display());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&binary)?.permissions().mode() & 0o7777;
                    anyhow::ensure!(mode == 0o755, "{}: mode {mode:o}, not 755", binary.display());
                }
                println!("MATCH {platform} {got}");
                matched += 1;
            }
            anyhow::ensure!(matched == 2, "{matched} of 2 binaries match their pins");
            let own = match (std::env::consts::OS, std::env::consts::ARCH) {
                ("linux", "x86_64") => Some("amd64"),
                ("linux", "aarch64") => Some("arm64"),
                _ => None,
            };
            if let Some(arch) = own {
                // Its own HOME, and no self-update (harness.json's AGY_CLI_DISABLE_AUTO_UPDATE).
                let home = tempfile::tempdir_in(&root)?;
                let out = std::process::Command::new(dir.path().join(format!("antigravity-{arch}/bin/agy")))
                    .arg("--version")
                    .env("HOME", home.path())
                    .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
                    .output()?;
                let stdout = String::from_utf8_lossy(&out.stdout);
                let first = stdout.lines().next().unwrap_or_default();
                anyhow::ensure!(out.status.success() && first == version, "antigravity-{arch}/bin/agy --version: {} {stdout:?}", out.status);
                println!("VERSION linux/{arch} {first}");
            }
            Ok(())
        })
    }
}
