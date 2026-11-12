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
}
