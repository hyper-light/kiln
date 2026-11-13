//! What an isolated agent's microVM is made from (docs/isolation.md; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 17): its Agentfile's image, built for this machine by the
//! `shards` kiln carries (`crate::shards`, step 16) into the Shards home its microVMs boot from, and
//! named as Hex boots one, pinned by its digest (`NAME@sha256:…`, Hex's `ShardsImage::image`).
//!
//! - **What it's built from** is its Agentfile and, where kiln ships one for its harness, that one's
//!   harness directory for this machine's platform beside it (`agentfiles::context`). A person's own
//!   Agentfile is built beside kiln's harness directory, so one copied from kiln's finds it where
//!   kiln's does. Of a pin with several builds for the platform (one per libc), the build is the one
//!   the Agentfile's own defaults name (`ARG CLAUDE_LIBC=gnu`), as a build given no argument takes it.
//! - **Its tag is its content's digest** (`NAME:<recipe>`, `context::recipe_with`). An image the
//!   home's store holds under it is what a build would make, so it boots without one; a content the
//!   store holds none of is built and tagged by its build. Builds in this process take turns, so the
//!   starts that wait for one find what it built.
//! - **A build Shards refuses** is refused in Shards' own words. What kiln made for a build goes
//!   either way. A build runs as long as its start waits for it, and is ended, with all it started,
//!   when its start gives up first.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agentfiles::builtin::Builtin;
use crate::agentfiles::context;

/// How a person's own Agentfile's image is named in the home's store, before its id.
const KEPT: &str = "agentfile-";

/// The Agentfile a microVM is made from.
#[derive(Clone, Copy)]
pub enum Source<'a> {
    /// One kiln ships, for its own harness.
    Builtin(&'static Builtin),
    /// One of a person's own: its id, its text as its start read it, and kiln's for its harness where
    /// kiln ships one, whose harness directory its build is given.
    Kept { id: &'a str, text: &'a str, beside: Option<&'static Builtin> },
}

impl Source<'_> {
    /// Its image's name in the home's store: kiln's by its own (`harness-claude`), a person's by its
    /// id (`agentfile-<id>`).
    pub fn name(&self) -> String {
        match self {
            Source::Builtin(b) => b.name.to_owned(),
            Source::Kept { id, .. } => format!("{KEPT}{id}"),
        }
    }

    fn text(&self) -> &str {
        match self {
            Source::Builtin(b) => b.text,
            Source::Kept { text, .. } => text,
        }
    }

    /// Kiln's Agentfile whose harness directory and files its build is given.
    fn harness(&self) -> Option<&'static Builtin> {
        match self {
            Source::Builtin(b) => Some(*b),
            Source::Kept { beside, .. } => *beside,
        }
    }
}

/// Why a microVM's image couldn't be had.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// This machine's architecture isn't one Shards' images are built for.
    #[error("this machine's architecture, {0}, isn't one Shards builds images for (amd64, arm64)")]
    Platform(String),
    /// Its harness's pin has several builds for the platform, and its Agentfile's argument defaults
    /// name none of them, or more than one.
    #[error("its Agentfile's argument defaults name none of its harness's builds for {platform} ({}), or more than one", builds.join(", "))]
    Which { platform: String, builds: Vec<String> },
    /// Its build context couldn't be made: its pin, a download, the disk.
    #[error("its build context: {0}")]
    Context(String),
    /// The home's store couldn't be read.
    #[error("the Shards home's store: {0}")]
    Store(String),
    /// Shards didn't build it: its own words, or why it couldn't be asked.
    #[error("Shards didn't build it: {0}")]
    Build(String),
}

/// The images this machine's microVMs boot, built by the carried `shards` into the Shards home they
/// boot from.
pub struct Images {
    /// The carried `shards` (`crate::shards::Held::shards`), kept written out by whoever holds this.
    shards: PathBuf,
    /// The Shards home (`SHARDS_HOME`), whose store, `images`, holds what's built.
    home: PathBuf,
    /// Where builds make their contexts, each under a name of its own, removed once it's done: big files
    /// (a harness's directory), so never under `/tmp` (AGENTS.md).
    scratch: PathBuf,
    /// Builds in this process take turns.
    building: futures::lock::Mutex<()>,
}

impl Images {
    pub fn new(shards: PathBuf, home: PathBuf, scratch: PathBuf) -> Self {
        Self { shards, home, scratch, building: futures::lock::Mutex::new(()) }
    }

    /// `source`'s image for this machine, pinned by its digest: the one the home's store holds of its
    /// content, else built.
    pub async fn pinned(&self, source: Source<'_>) -> Result<String, Refusal> {
        let plan = Plan::of(source)?;
        if let Some(digest) = self.held(&plan).await? {
            return Ok(plan.pinned(&digest));
        }
        let _turn = self.building.lock().await;
        if let Some(digest) = self.held(&plan).await? {
            return Ok(plan.pinned(&digest));
        }
        let digest = self.build(source, &plan).await?;
        Ok(plan.pinned(&digest))
    }

    async fn held(&self, plan: &Plan) -> Result<Option<String>, Refusal> {
        let (root, tag) = (self.home.join("images"), plan.tag.clone());
        crate::rt::blocking(move || held_in(&root, &tag)).await.map_err(|e| Refusal::Store(format!("{e:#}")))?
    }

    /// Builds `plan` in a folder of its own under `scratch`, removed after: its image's digest.
    async fn build(&self, source: Source<'_>, plan: &Plan) -> Result<String, Refusal> {
        let dir = self.scratch.join(format!("{}.{}", plan.recipe, crate::util::new_id()));
        let built = self.build_in(&dir, source, plan).await;
        // A harness's tree may hold a directory its owner may not search, which `discard` makes searchable.
        let _ = crate::rt::blocking(move || crate::agentfiles::unpack::discard(&dir)).await;
        built
    }

    async fn build_in(&self, dir: &Path, source: Source<'_>, plan: &Plan) -> Result<String, Refusal> {
        let (context_dir, config, iid) = (dir.join("context"), dir.join("docker-config"), dir.join("iid"));
        let (d, c) = (dir.to_path_buf(), config.clone());
        let made = crate::rt::blocking(move || crate::shards::private_dir(&d).and_then(|()| crate::shards::private_dir(&c))).await;
        made.and_then(|r| r).map_err(|e| Refusal::Context(format!("{e:#}")))?;
        match source.harness() {
            Some(b) => {
                context::materialize_with(b, source.text(), std::slice::from_ref(&plan.platform), plan.variant.as_deref(), &context_dir).await.map_err(|e| Refusal::Context(e.to_string()))?;
            }
            None => {
                let (into, at, text) = (context_dir.clone(), context_dir.join("Agentfile"), source.text().to_owned());
                let written = crate::rt::blocking(move || crate::shards::private_dir(&into).and_then(|()| Ok(std::fs::write(&at, text)?))).await;
                written.and_then(|r| r).map_err(|e| Refusal::Context(format!("{e:#}")))?;
            }
        }
        let mut cmd = Command::new(&self.shards);
        cmd.arg("build").arg("--progress=quiet").arg("--platform").arg(&plan.platform);
        cmd.arg("--file").arg(context_dir.join("Agentfile")).arg("--tag").arg(&plan.tag).arg("--iidfile").arg(&iid).arg(&context_dir);
        // Its own home, and a Docker configuration of its own: none of the machine's credential helpers.
        cmd.env("SHARDS_HOME", &self.home).env("DOCKER_CONFIG", &config);
        let (code, said) = run(cmd).await.map_err(|e| Refusal::Build(format!("{}: {e:#}", self.shards.display())))?;
        if code != Some(0) {
            let words = String::from_utf8_lossy(&said).trim().to_owned();
            return Err(Refusal::Build(if words.is_empty() { code.map_or_else(|| "its build was ended by a signal".to_owned(), |c| format!("its build ended with exit code {c}")) } else { words }));
        }
        let named = crate::rt::blocking(move || std::fs::read_to_string(&iid)).await;
        let id = named.and_then(|r| Ok(r?)).map_err(|e| Refusal::Build(format!("it named no image (its --iidfile: {e:#})")))?;
        let digest = shards_image::reference::Digest::parse(id.trim()).map_err(|e| Refusal::Build(format!("it named its image {:?}: {e}", id.trim())))?;
        Ok(digest.to_string())
    }
}

/// What a build of a source for this machine is: its image's name, the platform, its harness's build,
/// its content's digest, and the tag it's built and found under.
struct Plan {
    name: String,
    platform: String,
    variant: Option<String>,
    recipe: String,
    tag: String,
}

impl Plan {
    fn of(source: Source<'_>) -> Result<Plan, Refusal> {
        let platform = platform()?;
        let text = source.text();
        let variant = match source.harness() {
            Some(b) => build_of(b, text, &platform)?,
            None => None,
        };
        let recipe = context::recipe_with(text, source.harness(), context::default_base(text).unwrap_or_default(), variant.as_deref());
        let name = source.name();
        let tag = format!("{name}:{recipe}");
        Ok(Plan { name, platform, variant, recipe, tag })
    }

    /// The image of `digest`, named as Hex boots it.
    fn pinned(&self, digest: &str) -> String {
        format!("{}@{digest}", self.name)
    }
}

/// This machine's platform as a microVM's image is built for it: Linux on this machine's architecture,
/// named as OCI names it (image-spec's `platform.architecture`, Go's GOARCH), a guest's being its host's.
fn platform() -> Result<String, Refusal> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("linux/amd64".to_owned()),
        "aarch64" => Ok("linux/arm64".to_owned()),
        other => Err(Refusal::Platform(other.to_owned())),
    }
}

/// Which of `b`'s builds for `platform` a build of `text` takes: the platform's only one, else the one
/// its argument defaults name (`ARG CLAUDE_LIBC=gnu`), as a build given no argument takes it.
fn build_of(b: &Builtin, text: &str, platform: &str) -> Result<Option<String>, Refusal> {
    let pin = context::Pin::read(b.pin).map_err(|e| Refusal::Context(e.to_string()))?;
    let builds: Vec<Option<&str>> = pin.builds.iter().filter(|x| x.platform == platform).map(|x| x.variant.as_deref()).collect();
    match builds.as_slice() {
        [] => return Err(Refusal::Context(pin.build(platform, None).err().map(|e| e.to_string()).unwrap_or_default())),
        [only] => return Ok(only.map(str::to_owned)),
        _ => {}
    }
    let mut named: Vec<&str> = text.lines().filter_map(|l| l.trim().strip_prefix("ARG ")).filter_map(|a| a.split_once('=')).map(|(_, v)| v.trim()).filter(|v| builds.contains(&Some(*v))).collect();
    named.sort_unstable();
    named.dedup();
    match named.as_slice() {
        [one] => Ok(Some((*one).to_owned())),
        _ => Err(Refusal::Which { platform: platform.to_owned(), builds: builds.iter().map(|v| v.unwrap_or_default().to_owned()).collect() }),
    }
}

/// The digest the store at `root` names `tag` by, the tag read as Shards' build reads the ones it
/// names (`Reference::parse`): none where it holds no image under it, or there's no store yet.
fn held_in(root: &Path, tag: &str) -> Result<Option<String>, Refusal> {
    if !root.is_dir() {
        return Ok(None);
    }
    let reference = shards_image::reference::Reference::parse(tag).map_err(|e| Refusal::Store(format!("{tag}: {e}")))?;
    let store = shards_image::store::Store::open(root).map_err(|e| Refusal::Store(format!("{}: {e}", root.display())))?;
    let digest = store.resolved(&reference.to_string()).map_err(|e| Refusal::Store(format!("{tag}: {e}")))?;
    Ok(digest.map(|d| d.to_string()))
}

/// `cmd` run to its end: its exit code and what it said. One whose waiter goes first (its start
/// given up) is ended, with all it started, so no build outlives the start that asked for it.
async fn run(cmd: Command) -> anyhow::Result<(Option<i32>, Vec<u8>)> {
    let mut ending = Ending(Some(crate::proc::spawn_group(cmd)?));
    let mut said = Vec::new();
    let code = match ending.0.as_mut() {
        Some(running) => {
            while let Ok(chunk) = running.output.recv().await {
                said.extend_from_slice(&chunk);
            }
            running.wait().await
        }
        None => None,
    };
    // It ended by itself: there's nothing of it to end.
    ending.0 = None;
    Ok((code, said))
}

/// A run its waiter holds: its group is killed where the waiter goes before it ends.
struct Ending(Option<crate::proc::Running>);

impl Drop for Ending {
    fn drop(&mut self) {
        if let Some(running) = &self.0 {
            running.kill_group();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records, in the store at `root`, that `tag` names the image `digest`, as Shards' build records
    /// what it builds: its tag read by `Reference::parse`.
    fn tag_in(root: &Path, tag: &str, digest: &str) -> anyhow::Result<()> {
        use shards_image::reference::{Digest, Reference};
        std::fs::create_dir_all(root)?;
        let store = shards_image::store::Store::open(root).map_err(|e| anyhow::anyhow!("{e}"))?;
        let manifest = shards_image::oci::Descriptor { media_type: "application/vnd.oci.image.manifest.v1+json".into(), digest: digest.into(), size: 1, platform: None, annotations: Default::default() };
        let reference = Reference::parse(tag).map_err(|e| anyhow::anyhow!("{e}"))?.to_string();
        let resolved = Digest::parse(digest).map_err(|e| anyhow::anyhow!("{e}"))?;
        store.tag(&reference, &manifest, &resolved, &[]).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// A digest of its own for each word.
    fn digest_of(word: &str) -> String {
        use sha2::Digest as _;
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(word.as_bytes())))
    }

    /// Images whose home's store already holds `source`'s image, as its build would have left it, and
    /// that image's pinned name. It builds nothing: its `shards` is none.
    pub(crate) fn holding(dir: &Path, source: Source<'_>) -> anyhow::Result<(Images, String)> {
        let plan = Plan::of(source)?;
        let home = dir.join("shards-home");
        let digest = digest_of(&plan.tag);
        tag_in(&home.join("images"), &plan.tag, &digest)?;
        Ok((Images::new(dir.join("no-shards"), home, dir.join("scratch")), plan.pinned(&digest)))
    }

    /// A built-in of the test's own: `text`, for Claude Code, with a pin of no file for each platform
    /// (one build each, or one per variant), so its context is made with nothing fetched.
    fn builtin(text: &str, variants: &[&str]) -> &'static Builtin {
        let builds: Vec<serde_json::Value> = ["linux/amd64", "linux/arm64"]
            .iter()
            .flat_map(|p| match variants {
                [] => vec![serde_json::json!({"platform": p, "files": []})],
                vs => vs.iter().map(|v| serde_json::json!({"platform": p, "variant": v, "files": []})).collect(),
            })
            .collect();
        let pin = serde_json::json!({ "builds": builds }).to_string();
        Box::leak(Box::new(Builtin { name: "harness-t", harness: "claude", text: Box::leak(text.to_owned().into_boxed_str()), pin: Box::leak(pin.into_boxed_str()), files: &[("harness.json", "{\"name\": \"claude\"}")] }))
    }

    /// Step 17: each image's build for this machine is the platform's, its harness's build is the one
    /// its Agentfile's defaults name, and its tag changes exactly with its content; a person's own is
    /// named by its id, built beside kiln's harness directory.
    #[test]
    fn s17_an_images_build_and_tag_are_its_contents() -> anyhow::Result<()> {
        let one = builtin("ARG BASE=scratch\nFROM ${BASE}\n", &[]);
        let plan = Plan::of(Source::Builtin(one))?;
        let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };
        assert_eq!((plan.name.as_str(), plan.platform.as_str(), plan.variant.as_deref()), ("harness-t", format!("linux/{arch}").as_str(), None));
        assert_eq!(plan.tag, format!("harness-t:{}", context::recipe_with(one.text, Some(one), "scratch", None)));
        assert_eq!(plan.pinned("sha256:00"), "harness-t@sha256:00");
        // A libc each: the one the Agentfile's default names, else refused, naming the builds.
        let libcs = builtin("ARG BASE=scratch\nARG T_LIBC=musl\nFROM ${BASE}\n", &["gnu", "musl"]);
        assert_eq!(Plan::of(Source::Builtin(libcs))?.variant.as_deref(), Some("musl"));
        let unnamed = builtin("FROM scratch\n", &["gnu", "musl"]);
        let refused = Plan::of(Source::Builtin(unnamed)).err();
        assert!(matches!(&refused, Some(Refusal::Which { builds, .. }) if *builds == ["gnu", "musl"]), "{refused:?}");
        // Kiln's own: each built-in shipped is planned for this machine, Claude Code's on glibc, its default.
        let claude = crate::agentfiles::builtin::ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no Claude Code Agentfile"))?;
        assert_eq!(Plan::of(Source::Builtin(claude))?.variant.as_deref(), Some("gnu"));
        for b in crate::agentfiles::builtin::ALL {
            Plan::of(Source::Builtin(b))?;
        }
        // A person's own, named by its id; its tag moves with its text, and with the harness beside it.
        let id = crate::util::new_id();
        let kept = |text: &'static str, beside: Option<&'static Builtin>| Plan::of(Source::Kept { id: &id, text, beside });
        let a = kept("FROM scratch\n", Some(one))?;
        assert_eq!(a.name, format!("agentfile-{id}"));
        assert_ne!(a.tag, kept("FROM scratch\nLABEL x=1\n", Some(one))?.tag);
        assert_ne!(a.tag, kept("FROM scratch\n", None)?.tag);
        assert_eq!(a.tag, kept("FROM scratch\n", Some(one))?.tag);
        Ok(())
    }

    /// A stand-in for the carried `shards`: each build asked of it is a line of `builds` (the home it's
    /// given and its arguments), and does what Shards' does as far as kiln reads it: its image named in
    /// its iidfile, and its tag in the home's store (a record Shards' own store code made of it
    /// beforehand: [`StandIn::ready`]). A tag none was made for fails, in Shards' words.
    #[cfg(unix)]
    struct StandIn {
        dir: tempfile::TempDir,
        program: PathBuf,
        builds: PathBuf,
        made: PathBuf,
    }

    /// What the stand-in says of a build it fails, as Shards' build says one (buildx's words).
    #[cfg(unix)]
    const FAILED: &str = "ERROR: failed to build: failed to solve: process \"/bin/sh -c exit 3\" did not complete successfully: exit code: 3";

    #[cfg(unix)]
    impl StandIn {
        fn new() -> anyhow::Result<Self> {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempfile::tempdir()?;
            let (program, builds, made) = (dir.path().join("shards"), dir.path().join("builds"), dir.path().join("made"));
            std::fs::create_dir_all(&made)?;
            let script = format!(
                "#!/bin/sh\necho \"$SHARDS_HOME $*\" >> '{builds}'\nwhile [ $# -gt 1 ]; do case \"$1\" in --tag) tag=\"$2\";; --iidfile) iid=\"$2\";; esac; shift; done\n[ -f \"$1/Agentfile\" ] || {{ echo \"no Agentfile in $1\" >&2; exit 2; }}\nmade='{made}'/$(printf %s \"$tag\" | tr ':/' '__')\n[ -d \"$made\" ] || {{ echo '{FAILED}' >&2; exit 1; }}\nmkdir -p \"$SHARDS_HOME/images\" && cp -R \"$made/images/.\" \"$SHARDS_HOME/images/\" && cp \"$made/iid\" \"$iid\"\n",
                builds = builds.display(),
                made = made.display(),
            );
            std::fs::write(&program, script)?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
            Ok(Self { dir, program, builds, made })
        }

        /// What a build of `source` makes: the image `digest`, its tag recorded as Shards' build records it.
        fn ready(&self, source: Source<'_>, digest: &str) -> anyhow::Result<()> {
            let tag = Plan::of(source)?.tag;
            let made = self.made.join(tag.replace([':', '/'], "_"));
            tag_in(&made.join("images"), &tag, digest)?;
            std::fs::write(made.join("iid"), digest)?;
            Ok(())
        }

        /// The builds asked of it so far.
        fn builds(&self) -> Vec<String> {
            std::fs::read_to_string(&self.builds).unwrap_or_default().lines().map(str::to_owned).collect()
        }

        fn images(&self) -> Images {
            Images::new(self.program.clone(), self.dir.path().join("home"), self.dir.path().join("scratch"))
        }
    }

    /// Step 17: a built-in's image is built once for its content, into the home's store, by the carried
    /// `shards` given that home, for this machine's platform, tagged by its content; its answer is its
    /// image pinned by digest; asked again, the store's is found without a build; and nothing of its
    /// build is left behind.
    #[cfg(unix)]
    #[test]
    fn s17_a_builtin_is_built_once_for_its_content_and_pinned_by_digest() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let shards = StandIn::new()?;
            let source = Source::Builtin(builtin("ARG BASE=scratch\nFROM ${BASE}\nHARNESS claude FROM ./claude-${TARGETARCH}\n", &[]));
            shards.ready(source, &digest_of("one"))?;
            let images = shards.images();
            let pinned = images.pinned(source).await?;
            assert_eq!(pinned, format!("harness-t@{}", digest_of("one")));
            let builds = shards.builds();
            let plan = Plan::of(source)?;
            assert_eq!(builds.len(), 1, "{builds:?}");
            let build = builds.first().cloned().unwrap_or_default();
            let home = shards.dir.path().join("home");
            assert!(build.starts_with(&format!("{} build --progress=quiet --platform {} ", home.display(), plan.platform)), "{build}");
            assert!(build.contains(&format!(" --tag {} ", plan.tag)), "{build}");
            assert_eq!(images.pinned(source).await?, pinned, "the same image, found in the store");
            assert_eq!(shards.builds().len(), 1, "found without a build");
            let scratch = shards.dir.path().join("scratch");
            assert_eq!(std::fs::read_dir(&scratch)?.count(), 0, "nothing of its build is left");
            Ok(())
        })
    }

    /// Step 17: a person's own Agentfile builds again once its text changes, and not while it's the
    /// same; starts that ask for one image at once wait for one build of it.
    #[cfg(unix)]
    #[test]
    fn s17_a_changed_agentfile_builds_again_and_one_content_builds_once() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let shards = StandIn::new()?;
            let images = shards.images();
            let id = crate::util::new_id();
            let beside = Some(builtin("ARG BASE=scratch\nFROM ${BASE}\n", &[]));
            let (a, b) = (Source::Kept { id: &id, text: "FROM scratch\n", beside }, Source::Kept { id: &id, text: "FROM scratch\nLABEL changed=1\n", beside });
            shards.ready(a, &digest_of("a"))?;
            shards.ready(b, &digest_of("b"))?;
            let (first, second) = futures::join!(images.pinned(a), images.pinned(a));
            assert_eq!((first?, second?), (format!("agentfile-{id}@{}", digest_of("a")), format!("agentfile-{id}@{}", digest_of("a"))));
            assert_eq!(shards.builds().len(), 1, "two starts at once, one build: {:?}", shards.builds());
            assert_eq!(images.pinned(b).await?, format!("agentfile-{id}@{}", digest_of("b")), "its changed text, built");
            assert_eq!(images.pinned(a).await?, format!("agentfile-{id}@{}", digest_of("a")), "the first text's, still held");
            assert_eq!(shards.builds().len(), 2, "{:?}", shards.builds());
            Ok(())
        })
    }

    /// Step 17: a build Shards refuses is refused in its words, nothing of it left behind, and nothing
    /// of it held: asked again, it's built again.
    #[cfg(unix)]
    #[test]
    fn s17_a_refused_build_says_shards_words() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let shards = StandIn::new()?;
            let images = shards.images();
            let source = Source::Builtin(builtin("FROM scratch\nRUN exit 3\n", &[]));
            let refused = images.pinned(source).await.err();
            assert_eq!(refused, Some(Refusal::Build(FAILED.to_owned())));
            assert!(refused.map(|r| r.to_string()).unwrap_or_default().contains("did not complete successfully: exit code: 3"));
            assert_eq!(std::fs::read_dir(shards.dir.path().join("scratch"))?.count(), 0, "nothing of its build is left");
            assert!(images.pinned(source).await.is_err());
            assert_eq!(shards.builds().len(), 2, "refused, it's no image of the store's");
            // A `shards` that isn't there is refused, saying where it was looked for.
            let none = Images::new(shards.dir.path().join("gone"), shards.dir.path().join("home"), shards.dir.path().join("scratch"));
            let said = none.pinned(source).await.err().map(|r| r.to_string()).unwrap_or_default();
            assert!(said.starts_with("Shards didn't build it: ") && said.contains("gone"), "{said}");
            Ok(())
        })
    }
}
