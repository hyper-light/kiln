//! Where an agent works, and how its work leaves it (docs/agentic.md, W and C).
//!
//! The machine an agent works on answers questions about its folder ([`Ask`]): the server asks
//! ([`crate::proto::ServerMsg::Workspace`], or in process for the server's own agents), the
//! machine looks at its session's folder itself, so a question can never be about another one.
//!
//! - [`git`]: the folder's repository, and a commit of the agent's work made beside it.
//! - [`change`]: an agent's change request, from its work to review (C1, C6).
//! - [`follow`]: what happens to it, back to the agent (C2, C3).
//! - [`slates`]: a workspace of its own, a Slates volume over its folder (W1–W4).
//! - [`slates_fleet`]: a runner's identity as a node of kiln's Slates fleet, its key and kiln's certificate for it.
//! - [`slates_move`]: an agent moved to another runner with its workspace volume, by Slates' own move (K4).
//! - [`place`]: where a workspace's volume is mounted on its machine (a folder, or a drive letter on Windows).
//! - [`ignored`]: what git ignores in one with no mount, from its volume's own `.gitignore` files.
//! - [`review`]: an agent's work in one with no mount, from its volume's landing entries (A-287).
//! - [`measure`]: an experiment's variants measured beside it, each in a worktree of its own
//!   (docs/ci.md W20-R11).

pub mod change;
pub mod follow;
pub mod git;
pub mod ignored;
pub mod measure;
pub mod place;
pub mod review;
pub mod slates;
pub mod slates_fleet;
pub mod slates_move;

#[cfg(test)]
mod bare_metal_tests;
#[cfg(test)]
mod captured_tests;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where an agent works (`Spec::workspace`): a Slates volume laid over its folder, or the folder itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Workspace {
    /// `volume`: a Slates volume laid over its folder (its own, or another agent's it works in: `owner`); `folder`:
    /// the folder itself, as a person asked.
    pub kind: String,
    /// The folder it's laid over: its repository's top (else the folder itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub over: Option<String>,
    /// Its folder within that, relative to it (`crates/kiln`); empty for the top itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub: String,
    /// The volume, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub made: Option<slates::Made>,
    /// The agent whose volume it works in, when it isn't its own: it was started in another agent's workspace on its
    /// machine (a child in its parent's folder), so it works there, as two agents in one folder do ([`sharing`]). The
    /// volume goes when the last agent working in it ends, and only its owner's change request takes everything in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub owner: Option<String>,
}

/// The kinds of workspace an agent may ask for: its folder itself, or a volume of its own.
pub const KINDS: &[&str] = &["folder", "volume"];

/// The workspace an agent works in when its folder `cwd` lies in one that an agent of its machine works in now
/// (`live`: each such agent's id and workspace): that one, the agent's folder its own place in it. A volume is never
/// laid over a volume (Slates refuses a base on one of its own mounts, its A-227), so an agent started in another's
/// folder, a child in its parent's above all, works in that agent's volume, as two agents in one folder do. The
/// volume's owner stays the agent that made it, whoever works there in turn.
pub fn sharing(cwd: &str, live: &[(String, Workspace)]) -> Option<Workspace> {
    live.iter().find_map(|(id, w)| {
        let made = w.made.as_ref()?;
        within(cwd, &made.mount).then(|| Workspace {
            kind: "volume".to_owned(),
            over: w.over.clone(),
            sub: beneath(cwd, &made.mount),
            made: Some(made.clone()),
            owner: Some(w.owner.clone().unwrap_or_else(|| id.clone())),
        })
    })
}

/// The workspaces agents on one machine work in now (`infos`, its sessions): each made one with its agent's id, what
/// [`sharing`] judges a folder against.
pub fn live(infos: Vec<crate::harness::session::Info>) -> Vec<(String, Workspace)> {
    infos.into_iter().filter_map(|i| i.workspace.filter(|w| w.made.is_some()).map(|w| (i.id, w))).collect()
}

/// What a start on a person's shell takes from it once its machine has read the shell (`harness::host::shell_context`:
/// its folder `cwd` and its environment `env`): the folder and the environment where the start named none; and, where
/// the folder lies in a workspace an agent of this machine works in now (`live`), that agent's volume ([`sharing`]),
/// at the shell's own place in it. The server chose the start's workspace before this machine read the shell's folder
/// (`Registry::open_as`), so a start that asked for a volume of its own, still to be made, takes the one it is in: a
/// volume is never laid over a volume (Slates refuses a base on one of its own mounts, its A-227), as one started in
/// another agent's folder works in that agent's volume (W2). A person's choice of the folder itself, and a workspace
/// decided already, are kept.
pub fn from_shell(spec: &mut crate::harness::session::Spec, (cwd, env): (Option<String>, Vec<(String, String)>), live: &[(String, Workspace)]) {
    if spec.cwd.is_none() {
        spec.cwd = cwd;
    }
    if spec.env.is_empty() {
        spec.env = env;
    }
    let own_to_make = spec.workspace.as_ref().is_some_and(|w| w.kind == "volume" && w.made.is_none() && w.owner.is_none());
    if own_to_make && let Some(shared) = spec.cwd.as_deref().and_then(|cwd| sharing(cwd, live)) {
        spec.workspace = Some(shared);
    }
}

/// Whether `a` and `b` are workspaces in one volume.
pub fn same_volume(a: Option<&Workspace>, b: Option<&Workspace>) -> bool {
    match (a.and_then(|w| w.made.as_ref()), b.and_then(|w| w.made.as_ref())) {
        (Some(x), Some(y)) => x.volume == y.volume,
        _ => false,
    }
}

/// The write mode of a permission mode at `level` (the person's rule, 2026-11-18: writing to disk follows the
/// permission granted to the harness or agent asking to write; kiln goal 01a1ec34599974448aed2672bafaf9a2;
/// docs/agentic.md W6), as Slates' consumer contract names it. A landing writes what the agent changed into the folder
/// its volume is laid over, so a mode that edits files unasked (accept edits, auto, bypass) lands at once (`Bypass`);
/// one that asks before it edits waits on an approval (`Ask`); one that only plans lands nothing (`Forbid`). A mode
/// kiln can't place asks, as `lineage::ceiling` takes it.
pub fn write_mode(level: Option<crate::harness::lineage::Level>) -> slates_api::WriteMode {
    use crate::harness::lineage::Level;
    match level.unwrap_or(Level::Ask) {
        Level::Plan => slates_api::WriteMode::Forbid,
        Level::Ask => slates_api::WriteMode::Ask,
        Level::AcceptEdits | Level::Auto | Level::Bypass => slates_api::WriteMode::Bypass,
    }
}

/// The write mode agent `info` was granted: the stricter of how much its launcher let it do as it started
/// (`Info::launched`; a session an older kiln began, its harness's own default) and its mode now (`lineage::granted`).
/// Its harness reports its mode now, and it may move itself (an ACP agent switching its own mode, a harness that says
/// what it likes), so a mode it works in that is higher than its launcher gave it writes nothing more: an agent never
/// raises its own permission (the person's rule, 2026-11-18). A mode lowered since, by anyone, is honoured.
pub fn granted_mode(info: &crate::harness::session::Info) -> slates_api::WriteMode {
    write_mode(Some(crate::harness::lineage::granted(info)))
}

/// Has workspace `w`'s write policy follow its agent's grant `level` (`write_mode`; the person's rule, 2026-11-18; W6),
/// set by kiln as the launcher, where its agent works in a volume of its own: a person's later change of its mode
/// reaches Slates' own policy, as its start did (`prepare`). A volume another agent owns (`Workspace::owner`) keeps its
/// owner's; a folder has none. Slates' answer is logged, not waited on: kiln's own `agents.land` judges each landing by
/// the agents' grants whatever the volume's policy says.
pub fn follow_grant(w: Option<&Workspace>, level: crate::harness::lineage::Level) {
    let Some(made) = w.filter(|w| w.owner.is_none()).and_then(|w| w.made.clone()) else { return };
    let mode = write_mode(Some(level));
    drop(crate::rt::spawn(async move {
        if let Err(e) = slates::set_policy(&made, slates_api::WritePolicy { merge: mode, land: mode, require_signed_merges: false }).await {
            tracing::info!(volume = %made.volume, error = %format!("{e:#}"), "a workspace's write policy didn't follow its agent's grant: kiln's own landing judges by the grant");
        }
    }));
}

/// The write mode of agent `asker`'s landing of agent `target`'s workspace (`target` itself, or an agent below the one
/// asking): the stricter of their two granted modes ([`granted_mode`]), so a landing never writes past the grant of the
/// agent asking or of the agent whose work it is.
pub fn landing_policy(asker: &crate::harness::session::Info, target: &crate::harness::session::Info) -> slates_api::WriteMode {
    granted_mode(asker).stricter(granted_mode(target))
}

/// A drive letter's path (`K:`), the mount of a Windows workspace.
fn drive(path: &str) -> Option<char> {
    let mut chars = path.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => Some(letter.to_ascii_uppercase()),
        _ => None,
    }
}

/// How long `path`'s leading text that is `mount` is, when it is: exactly, or a drive letter whatever its case
/// (Windows names `k:` and `K:` alike).
fn mount_prefix(path: &str, mount: &str) -> Option<usize> {
    if path.starts_with(mount) {
        return Some(mount.len());
    }
    let letter = drive(mount)?;
    let head = path.get(..mount.len())?;
    (drive(head) == Some(letter)).then_some(mount.len())
}

/// Whether `path` is `mount` or lies beneath it, as either platform writes its paths: the server judges its runners'
/// paths whatever it runs on itself (a Windows runner's `K:\src` lies on its drive `K:`), so this compares their text,
/// never by the server's own `Path`.
pub fn within(path: &str, mount: &str) -> bool {
    let mount = mount.trim_end_matches(['/', '\\']);
    if mount.is_empty() {
        return false;
    }
    match mount_prefix(path, mount).and_then(|n| path.get(n..)) {
        Some(rest) => rest.is_empty() || rest.starts_with(['/', '\\']),
        None => false,
    }
}

/// `path`'s place beneath `mount` (it lies [`within`] it): a workspace's `sub`, empty for the mount itself.
fn beneath(path: &str, mount: &str) -> String {
    let mount = mount.trim_end_matches(['/', '\\']);
    mount_prefix(path, mount).and_then(|n| path.get(n..)).unwrap_or_default().trim_start_matches(['/', '\\']).trim_end_matches(['/', '\\']).to_owned()
}

/// The variables of an agent's environment naming its harness's own homes, where kiln reads what the harness keeps for
/// a session (its transcript, its memories: `harness::homes`) after any volume has gone: never moved onto a mount, even
/// where a person's home lies in the folder a volume is laid over (`XDG_*` stay too).
const HOMES: &[&str] = &["HOME", "USERPROFILE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "GEMINI_CLI_HOME"];

/// The separator of a list of paths in one variable (`PATH`, `PYTHONPATH`) on this system, as `std::env::split_paths`
/// reads them: `;` on Windows, `:` elsewhere.
const PATH_LIST: char = if cfg!(windows) { ';' } else { ':' };

/// `env` with every path in it that lies in folder `over` named at the same place under `top`, where a workspace's
/// volume shows `over`: a variable's whole value, or each path of a list of them ([`PATH_LIST`]); the rest as it was,
/// byte for byte. An agent started in a person's shell is handed the shell's environment, which names the person's
/// folder (`OLDPWD`, an activated virtual environment's `VIRTUAL_ENV` and its `PATH` entry, a `CARGO_TARGET_DIR`);
/// moved onto the mount, a command that trusts its environment works in the volume, never past it (I11; `PWD` is the
/// folder the harness starts in, `harness::kinds`). A path spelled through a link is judged by its real path, as git
/// names `over`. The harness's own homes stay ([`HOMES`], `XDG_*`), and so does a path on the mount already.
pub fn rehomed(env: &[(String, String)], over: &str, top: &str) -> Vec<(String, String)> {
    env.iter()
        .map(|(key, value)| {
            let home = HOMES.contains(&key.as_str()) || key.starts_with("XDG_");
            (key.clone(), if home { value.clone() } else { value.split(PATH_LIST).map(|path| rehomed_path(path, over, top).unwrap_or_else(|| path.to_owned())).collect::<Vec<_>>().join(&PATH_LIST.to_string()) })
        })
        .collect()
}

/// `path` at its place under `top` where it lies in `over`, by its spelling or by its real path (`place::real`: through
/// its nearest folder there is, where it isn't made yet); None for anything else (a relative path or no path at all, one
/// outside `over`, one under `top` already).
fn rehomed_path(path: &str, over: &str, top: &str) -> Option<String> {
    if !Path::new(path).is_absolute() || within(path, top) {
        return None;
    }
    let place = if within(path, over) {
        beneath(path, over)
    } else {
        let real = place::real(Path::new(path)).to_string_lossy().into_owned();
        if !within(&real, over) || within(&real, top) {
            return None;
        }
        beneath(&real, over)
    };
    let top = Path::new(top);
    Some(if place.is_empty() { top.to_string_lossy().into_owned() } else { top.join(place).to_string_lossy().into_owned() })
}

/// Makes the workspace `spec` asks for (on the machine its agent `id` works on), before its
/// harness starts: a volume laid over its folder's repository, mounted, and the agent's folder in
/// it; for an isolated session, the volume with no mount, which Slates serves its microVM itself
/// (its folder stays the folder's own path); or, for one made already (the runner started again),
/// mounted again if its mount went away. Nothing for an agent that works in its folder itself.
pub async fn prepare(spec: &mut crate::harness::session::Spec, id: &str) -> anyhow::Result<()> {
    let Some(w) = spec.workspace.clone().filter(|w| w.kind == "volume") else { return Ok(()) };
    if let Some(m) = &w.made {
        slates::mounted(m).await?;
        // Its environment on that mount too, as its folder is (one started in a person's shell inside another agent's
        // volume, or one started again): nothing of it left naming the folder the volume is laid over.
        if let Some(over) = w.over.as_deref().filter(|_| !m.mount.is_empty()) {
            spec.env = rehomed(&spec.env, over, &place::top(&m.mount).to_string_lossy());
        }
        return Ok(());
    }
    let cwd = PathBuf::from(crate::harness::session::resolve_cwd(spec.cwd.as_deref()));
    anyhow::ensure!(cwd.is_dir(), "{} isn't a folder on this machine", cwd.display());
    // The whole repository, so git works in it as it does in the folder; the agent where it was asked to be.
    let (over, sub) = repo_place(&cwd).await;
    // The whole id: kiln's ids start with the time they were made, so two agents started together
    // share their beginnings (Slates' names are unique on the machine).
    let whole: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    let name = format!("kiln-{whole}");
    // An isolated session's volume is served to its microVM by Slates itself, never mounted here (kiln goal
    // 01a1ec34599974448f6b7663ddb07001; `sandbox::isolated::served_project`): its harness works in the folder's own place
    // in its guest, and nothing of the volume is in this machine's mount table.
    let made = if spec.isolation.is_some() { slates::make_unmounted(&name, &over).await? } else { slates::make(&name, &over).await? };
    // Its landings follow how much its launcher let its agent do (the person's rule, 2026-11-18; W6), as kiln, the
    // launcher, sets the volume's write policy; no agent can raise it. Where kiln can't set one, Slates' unset policy
    // holds: its landings ask, and kiln's own `agents.land` judges each by the agent's granted mode.
    let mode = write_mode(Some(spec.launched_level()));
    if let Err(e) = slates::set_policy(&made, slates_api::WritePolicy { merge: mode, land: mode, require_signed_merges: false }).await {
        tracing::info!(volume = %made.volume, error = %format!("{e:#}"), "a workspace's write policy wasn't set: its landings ask, as Slates' unset policy does");
    }
    if !made.mount.is_empty() {
        let top = place::top(&made.mount);
        // Its environment moved onto the mount with its folder (an agent started in a person's shell is handed the
        // shell's, which names the person's folder: I11).
        spec.env = rehomed(&spec.env, &over.to_string_lossy(), &top.to_string_lossy());
        let here = if sub.is_empty() { top } else { top.join(&sub) };
        spec.cwd = Some(here.to_string_lossy().into_owned());
        // A conversation taken up goes on from the mount's folder: its harness's record of it moved where the harness
        // looks from there, or copied for a branch of it, whose original goes on where it was (I12).
        if let Some(session) = spec.resume.clone() {
            let (kind, dirs, cwd, branch) = (spec.kind.clone(), crate::harness::homes::Dirs::here(&spec.env), here.to_string_lossy().into_owned(), spec.fork);
            let moved = crate::rt::blocking(move || if branch { crate::harness::homes::rehome_branch(&kind, &dirs, &session, &cwd) } else { crate::harness::homes::rehome(&kind, &dirs, &session, &cwd) }).await?;
            if let Err(why) = moved {
                if let Err(gone) = slates::remove(&made, std::time::Duration::ZERO).await {
                    tracing::warn!(volume = %made.volume, error = %format!("{gone:#}"), "the workspace of a conversation that couldn't be taken up stayed");
                }
                anyhow::bail!("its conversation's record couldn't be moved onto its volume: {why}");
            }
        }
    }
    spec.workspace = Some(Workspace { kind: w.kind, over: Some(over.to_string_lossy().into_owned()), sub, made: Some(made), ..Workspace::default() });
    Ok(())
}

/// A session that arrived with a workspace volume (it moved here with its agent, K4, `harness::host`'s take-over): its
/// mount re-placed under this machine's own place for workspaces where it named another machine's, and its folder and
/// environment there ([`follow_to`]: the mount's top, with the agent's place beneath it). Its work is written back here as
/// on the runner it came from (Slates judges a moved captured clone against its capture, its A-503). Whether it has a
/// workspace volume to mount here as it starts ([`mounted_for`]), so its folder is never taken for one this machine lacks.
pub fn arrived_here(spec: &mut crate::harness::session::Spec) -> bool {
    let Some(m) = spec.workspace.as_ref().filter(|w| w.kind == "volume").and_then(|w| w.made.clone()).filter(|m| !m.mount.is_empty()) else { return false };
    match slates::placed_here(&m) {
        Ok(arrived) => follow_to(spec, arrived),
        Err(e) => {
            tracing::warn!(volume = %m.volume, error = %format!("{e:#}"), "a workspace that arrived couldn't be placed here");
            false
        }
    }
}

/// Agent `spec`'s workspace volume as it was placed here, `arrived`: where its mount moved, its folder and its
/// environment follow it (the mount's top, with the agent's place beneath it; every path of its environment on the
/// mount it left, which I11 moved there as it started and a move carries, [`rehomed`] to its place on this one). Whether
/// it has a workspace volume to mount here.
fn follow_to(spec: &mut crate::harness::session::Spec, arrived: slates::Made) -> bool {
    let Some(w) = spec.workspace.as_mut().filter(|w| w.kind == "volume") else { return false };
    let Some(was) = w.made.as_ref().filter(|m| !m.mount.is_empty()).map(|m| m.mount.clone()) else { return false };
    let sub = w.sub.clone();
    let now = arrived.mount.clone();
    w.made = Some(arrived);
    if now != was {
        let (left, top) = (place::top(&was), place::top(&now));
        spec.env = rehomed(&spec.env, &left.to_string_lossy(), &top.to_string_lossy());
        let here = if sub.is_empty() { top } else { top.join(&sub) };
        spec.cwd = Some(here.to_string_lossy().into_owned());
    }
    true
}

/// A session's workspace volume mounted where it says before its harness starts in it (`harness::session`): one that
/// arrived with a move (its new owner may answer a moment after the move, so Slates' pauses are waited out), or one a
/// refused move let go of and took back. Nothing for any other workspace.
pub async fn mounted_for(spec: &crate::harness::session::Spec) -> anyhow::Result<()> {
    match spec.workspace.as_ref().filter(|w| w.kind == "volume").and_then(|w| w.made.as_ref()) {
        Some(m) => slates::mounted_patiently(m).await,
        None => Ok(()),
    }
}

/// The folder a workspace for `cwd` is laid over, and `cwd`'s place beneath it (a workspace's `sub`): its repository's
/// top and git's own place for it ([`git::prefix`], in this system's separators), or `cwd` itself where no repository
/// holds it. The place is git's, never `cwd` with the top taken off its front: git names the top by its real path, which
/// a path spelled with a symbolic link, or on Windows with a short 8.3 name (a runner's `%TEMP%`) or a name in another
/// case, does not share, and the agent then started at its volume's top instead of in its folder.
async fn repo_place(cwd: &Path) -> (PathBuf, String) {
    match git::facts(cwd).await {
        Ok(f) => {
            let sub = git::prefix(cwd).await.map(|prefix| beneath_top(&prefix)).unwrap_or_default();
            (PathBuf::from(&f.top), sub)
        }
        Err(_) => (cwd.to_path_buf(), String::new()),
    }
}

/// git's place for a folder (`crates/kiln/`, [`git::prefix`]) as a path in this system's separators (`crates\kiln` on
/// Windows), empty at the top.
fn beneath_top(prefix: &str) -> String {
    prefix.split('/').filter(|name| !name.is_empty()).collect::<PathBuf>().to_string_lossy().into_owned()
}

/// The volume a start made for its agent alone, which goes with it when it can't start after all: none when its
/// workspace is another agent's (`owner`), which goes on.
pub fn made_alone(spec: &crate::harness::session::Spec) -> Option<slates::Made> {
    spec.workspace.as_ref().filter(|w| w.owner.is_none()).and_then(|w| w.made.clone())
}

/// Whether `spec` needs its workspace made (or mounted again) before its harness starts.
pub fn needs_preparing(spec: &crate::harness::session::Spec) -> bool {
    spec.workspace.as_ref().is_some_and(|w| w.kind == "volume")
}

/// An ended agent's conversation sent back from the project its harness kept for the agent's folder on its mount to the
/// one it keeps for its person's folder, where they resume it from where they work (the reverse of a conversation
/// taken up onto a volume, I12): Claude Code's transcript and its session's own files, Gemini CLI's chat and its
/// subagents' folder (`harness::homes::return_home`). Its harness's conversation is `conversation` (its own id for it).
/// Asked as its agent ends, before its volume's mount goes. One that can't go back stays where it is, both records
/// kept, and kiln says why, typed (`homes::NotMoved`) and logged; a branch's copy of another conversation stays.
pub async fn send_conversation_home(spec: &crate::harness::session::Spec, conversation: Option<String>) {
    let (Some(session), Some((mount, person))) = (conversation, record_places(spec)) else { return };
    let (kind, dirs, asked) = (spec.kind.clone(), crate::harness::homes::Dirs::here(&spec.env), session.clone());
    match crate::rt::blocking(move || crate::harness::homes::return_home(&kind, &dirs, &asked, &mount, &person)).await {
        Ok(Ok(true)) => tracing::info!(%session, "an ended agent's conversation went back to its person's folder"),
        Ok(Ok(false)) => {}
        Ok(Err(not)) => tracing::warn!(%session, %not, "an ended agent's conversation stays where its harness kept it on its volume"),
        Err(e) => tracing::warn!(%session, error = %format!("{e:#}"), "an ended agent's conversation stays where its harness kept it on its volume"),
    }
}

/// Where agent `spec`'s harness kept its conversation, its folder on its volume's mount, and its person's folder that the
/// volume shows there (the folder it's laid over, `over`, and its place beneath, `sub`); none for an agent with no
/// mount.
fn record_places(spec: &crate::harness::session::Spec) -> Option<(String, String)> {
    let w = spec.workspace.as_ref()?;
    w.made.as_ref().filter(|m| !m.mount.is_empty())?;
    let (over, on_mount) = (w.over.as_deref()?, spec.cwd.clone()?);
    let person = if w.sub.is_empty() { PathBuf::from(over) } else { Path::new(over).join(&w.sub) };
    Some((on_mount, person.to_string_lossy().into_owned()))
}

/// Ends agent session `spec`'s workspace for good, when it has a volume: the volume and what it held go
/// with the session. Its caller asks this of the last agent working in it alone ([`same_volume`]), once its processes
/// are gone, giving how long their end took as how long what they left behind may still hold the mount
/// (`slates::remove`).
pub async fn release(spec: &crate::harness::session::Spec, patience: std::time::Duration) {
    let Some(m) = spec.workspace.as_ref().and_then(|w| w.made.clone()) else { return };
    if let Err(e) = slates::remove(&m, patience).await {
        tracing::warn!(volume = %m.volume, error = %format!("{e:#}"), "an ended agent's workspace stayed");
    }
}

/// What the server asks the machine about an agent's folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Ask {
    /// Its repository, as git sees it ([`git::Facts`]).
    Git,
    /// Commit the agent's work and push it ([`git::Made`] and what the push said).
    Change(Change),
    /// Its workspace of its own: what changed on disk under it since the agent changed it.
    Look,
    /// What writing its workspace back to its folder would do: Slates' plan, for a person to read.
    Plan,
    /// Write it back as planned, a person having said yes to that plan (its manifest's hash).
    Land { manifest: String },
    /// One step of a plan run's stack: the agent's work committed on the step before's, and
    /// pushed after the earlier steps' that weren't yet (docs/agentic.md P4).
    Stack(Stacked),
    /// A new version of one step of a plan run's stack, and the steps above it made again on it.
    Restack(Restack),
    /// Look for quotes a finding cites (docs/research.md, "Checks"): each in a file, read in the
    /// agent's folder, or a page, fetched from here as the agent would; only whether each is
    /// there comes back.
    Cite { looks: Vec<crate::research::check::Look> },
    /// Start measuring an experiment's variant: a command run in the variant's own worktree of the
    /// agent's repository ([`measure::start`], docs/ci.md W20-R11).
    Measure(measure::Measure),
    /// How measurement `key` goes, until it ends; `stop` stops it ([`measure::measured`]).
    Measured {
        key: String,
        #[serde(default)]
        stop: bool,
    },
    /// Experiment `experiment` is over: its worktrees go ([`measure::unmeasure`]).
    Unmeasure { experiment: String },
    /// Its repository's branches, tags and latest commits, for Compare's pickers ([`git::refs`]).
    /// Only a runner with [`crate::proto::CAP_WORKSPACE_COMPARE`] is asked.
    Refs,
    /// What changes from one point to another ([`git::compare`]): the Changes tab's Compare. An
    /// agent's point is its files as it left them, which only its machine knows (`versions`).
    Compare {
        from: git::Point,
        to: git::Point,
        /// From where they split (git's three-dot).
        #[serde(default)]
        split: bool,
        /// One file alone, its lines whatever the compare's size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The commit its session began at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_head: Option<String>,
    },
    /// Its branch's commits since its trunk, its session's own marked ([`git::log`]), and, with
    /// `picks`, what those commits change together ([`git::picked`]): the Changes tab's Commits.
    /// Only a runner with [`crate::proto::CAP_WORKSPACE_COMMITS`] is asked.
    Commits {
        /// Another branch than the one checked out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trunk: Option<String>,
        /// The commit its session began at, and when it began (unix ms): its own are after it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_head: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        picks: Vec<String>,
    },
}

impl Ask {
    /// What a runner must have said it can do past `Build`'s 32 bits to be asked this
    /// ([`crate::proto::AgentMsg::Caps`]); 0: [`crate::proto::CAP_WORKSPACE`] is enough.
    pub fn wide_cap(&self) -> u64 {
        match self {
            Self::Commits { .. } => crate::proto::CAP_WORKSPACE_COMMITS,
            Self::Refs | Self::Compare { .. } => crate::proto::CAP_WORKSPACE_COMPARE,
            _ => 0,
        }
    }
}

/// A push of one commit to one ref, as one principal.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Push {
    /// The commit (empty: the one just made).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha: String,
    pub to_ref: String,
    /// An `Authorization: Bearer` for a kiln server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

/// One step of a plan run's stack, committed and pushed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stacked {
    /// The commit it goes on (the step before's); `None`: the folder's last commit.
    pub on: Option<String>,
    /// The files it takes (as the agent's changes name them).
    pub paths: Vec<String>,
    pub message: String,
    /// Who commits when the machine's git doesn't say.
    pub fallback: git::Ident,
    /// Where the pushes go (a remote's name or a URL); none: the commit stays in the folder's
    /// repository.
    pub to: Option<String>,
    /// In order: the earlier steps' commits not pushed yet, then this one's.
    pub pushes: Vec<Push>,
}

/// A step's new version, the steps above it restacked.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Restack {
    /// The stack's commits from the step's up, bottom first.
    pub commits: Vec<String>,
    /// The agent's files: as the folder has them now, they go into the first commit.
    pub paths: Vec<String>,
    /// The first commit's new message, when it changes.
    pub message: Option<String>,
    pub fallback: git::Ident,
    pub to: Option<String>,
    /// The ref each new commit goes to, by its place in `commits` (none: not pushed by itself).
    pub refs: Vec<Option<String>>,
    pub bearer: Option<String>,
}

/// A commit of the agent's work, pushed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    /// The files it takes (as the agent's changes name them); `None`: every change git sees.
    pub paths: Option<Vec<String>>,
    pub message: String,
    /// Who commits when the machine's git doesn't say.
    pub fallback: git::Ident,
    /// A remote's name or a URL.
    pub to: String,
    /// The ref it goes to (`refs/heads/agents/x`, `refs/for/main/branch/agents/x`), rewritten.
    pub to_ref: String,
    /// An `Authorization: Bearer` for a kiln server.
    pub bearer: Option<String>,
}

/// How long the server waits for a machine's answer about its folder (a push can be slow).
pub const WAIT: Duration = Duration::from_secs(240);

/// Answers `ask` (JSON) about an agent's work on the machine it is on: git's questions in the
/// working tree its work is in (`work`: its folder's, or a worktree it went into,
/// [`crate::harness::session::Info::work_dir`]), the files a finding cites under its folder
/// (`cwd`), and its workspace `ws` when it has one of its own: `{"facts"}`, `{"made", "said"}`,
/// `{"drift"}`, `{"plan"}`, `{"landed"}`, or `{"error"}`. `versions`: how those who changed its
/// files left them, for a compare of an agent's point (its session's machine knows them). The
/// commits it makes of the agent's work are signed as the agent with the key this machine holds in
/// slot `sign` (W27-R03; `None`: none here, so unsigned), never with the person's.
pub async fn serve(cwd: &Path, work: &Path, ws: Option<&Workspace>, ask: Ask, versions: &git::Versions, sign: Option<&str>) -> Value {
    let r = match ask {
        Ask::Cite { looks } => Ok(json!({"looks": crate::research::check::local(cwd, looks).await})),
        Ask::Compare { from, to, split, path, since_head } => git::compare(work, &from, &to, split, path.as_deref(), since_head.as_deref(), versions).await.map(|c| json!({"compared": c})),
        ask => answer(work, ws, ask, sign).await,
    };
    match r {
        Ok(v) => v,
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// The workspace of its own an ask about one needs: its volume and the folder it's laid over.
fn own(ws: Option<&Workspace>) -> anyhow::Result<(&slates::Made, &str)> {
    let w = ws.filter(|w| w.kind == "volume").ok_or_else(|| anyhow::anyhow!("it works in its folder itself, not a workspace of its own"))?;
    match (&w.made, &w.over) {
        (Some(m), Some(over)) => Ok((m, over.as_str())),
        _ => anyhow::bail!("its workspace isn't made yet"),
    }
}

/// The workspace of its own an ask is about where it has no mount (an isolated session's Slates volume, served to its
/// microVM: `sandbox::isolated::served_project`), and the folder it lies over; none for any other.
fn unmounted(ws: Option<&Workspace>) -> Option<(&slates::Made, &str)> {
    let w = ws?;
    let m = w.made.as_ref().filter(|m| m.mount.is_empty())?;
    Some((m, w.over.as_deref()?))
}

async fn answer(cwd: &Path, ws: Option<&Workspace>, ask: Ask, sign: Option<&str>) -> anyhow::Result<Value> {
    match ask {
        Ask::Look => {
            let (m, over) = own(ws)?;
            Ok(json!({"drift": slates::drift(m).await?, "over": over, "volume": m.volume, "mount": m.mount}))
        }
        Ask::Plan => {
            let (m, over) = own(ws)?;
            let (plan, excludes) = slates::plan_landing(m, over).await?;
            // Why it can't be written from kiln: the landing would write what git ignores.
            let why_not = slates::writable(m, over, &plan).await.err();
            Ok(json!({"plan": plan, "excludes": excludes, "can_write": why_not.is_none(), "why_not": why_not}))
        }
        Ask::Land { manifest } => {
            let (m, over) = own(ws)?;
            Ok(match slates::land(m, over, &manifest).await? {
                slates::Landed::Written(outcome) => json!({"landed": outcome}),
                slates::Landed::Changed(plan, excludes) => {
                    let why_not = slates::writable(m, over, &plan).await.err();
                    json!({"changed": true, "plan": plan, "excludes": excludes, "can_write": why_not.is_none(), "why_not": why_not})
                }
            })
        }
        Ask::Git => Ok(json!({"facts": git::facts(cwd).await?})),
        Ask::Measure(m) => measure::start(cwd, m).await,
        Ask::Measured { key, stop } => Ok(measure::measured(&key, stop)),
        Ask::Unmeasure { experiment } => measure::unmeasure(cwd, &experiment).await,
        Ask::Cite { looks } => Ok(json!({"looks": crate::research::check::local(cwd, looks).await})),
        Ask::Refs => Ok(json!({"refs": git::refs(cwd).await?})),
        Ask::Compare { .. } => anyhow::bail!("a compare is answered with its versions (`serve`)"),
        Ask::Commits { branch, trunk, since_head, since_at, picks } => {
            let log = git::log(cwd, branch.as_deref(), trunk.as_deref(), since_head.as_deref(), since_at).await?;
            let picked = if picks.is_empty() { None } else { Some(git::picked(cwd, branch.as_deref(), &picks).await?) };
            Ok(json!({"log": log, "picked": picked}))
        }
        Ask::Change(c) => {
            let facts = git::facts(cwd).await?;
            let top = Path::new(&facts.top);
            let who = git::identity(top, &c.fallback).await;
            let made = match unmounted(ws) {
                // No working tree here (an isolated session's volume, served to its microVM): its changes come from the
                // volume itself (kiln goal 01a1ec34599974448f6b7663ddb07001; Slates' A-287), every one, as a workspace of
                // its own's change takes them.
                Some((m, over)) => {
                    anyhow::ensure!(c.paths.is_none(), "a workspace with no mount commits every change it holds, never chosen files");
                    let changes = review::changes(m, over).await?;
                    git::commit_changes(top, facts.head.as_deref(), &changes, &c.message, &who, sign).await?
                }
                None => git::commit(top, facts.head.as_deref(), c.paths.as_deref(), &c.message, &who, sign).await?,
            };
            let Some(made) = made else {
                anyhow::bail!("there's nothing to commit: the agent's files are as the folder's last commit has them");
            };
            let said = git::push(top, &c.to, &format!("+{}:{}", made.sha, c.to_ref), c.bearer.as_deref()).await?;
            Ok(json!({"made": made, "said": said}))
        }
        Ask::Stack(s) => {
            let Some(made) = git::commit_on(cwd, s.on.as_deref(), &s.paths, &s.message, &s.fallback, sign).await? else {
                return Ok(json!({"nothing": true}));
            };
            let facts = git::facts(cwd).await?;
            let pushes: Vec<Push> = s.pushes.into_iter().map(|p| Push { sha: if p.sha.is_empty() { made.sha.clone() } else { p.sha }, ..p }).collect();
            let (pushed, said, error) = push_all(Path::new(&facts.top), s.to.as_deref(), &pushes).await;
            Ok(json!({"made": made, "pushed": pushed, "said": said, "push_error": error}))
        }
        Ask::Restack(r) => {
            let facts = git::facts(cwd).await?;
            let top = Path::new(&facts.top);
            let who = git::identity(top, &r.fallback).await;
            let made = git::restack(top, &r.commits, &r.paths, r.message.as_deref(), &who, sign).await?;
            let pushes: Vec<Push> = made.iter().zip(&r.refs).filter_map(|(m, to)| to.clone().map(|to_ref| Push { sha: m.sha.clone(), to_ref, bearer: r.bearer.clone() })).collect();
            let (pushed, said, error) = push_all(top, r.to.as_deref(), &pushes).await;
            Ok(json!({"made": made, "pushed": pushed, "said": said, "push_error": error}))
        }
    }
}

/// Pushes `pushes` to `to` in order, up to the first that fails: how many went, what was said,
/// and why the one that didn't go didn't.
async fn push_all(top: &Path, to: Option<&str>, pushes: &[Push]) -> (usize, String, Option<String>) {
    let Some(to) = to else { return (0, String::new(), None) };
    let mut said = String::new();
    for (i, p) in pushes.iter().enumerate() {
        match git::push(top, to, &format!("+{}:{}", p.sha, p.to_ref), p.bearer.as_deref()).await {
            Ok(s) => said.push_str(&s),
            Err(e) => return (i, said, Some(format!("{e:#}"))),
        }
    }
    (pushes.len(), said, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn made(volume: &str, mount: &str) -> slates::Made {
        slates::Made { volume: volume.into(), name: format!("kiln-{volume}"), mount: mount.into(), ..Default::default() }
    }

    fn volume_at(volume: &str, mount: &str, over: &str) -> Workspace {
        Workspace { kind: "volume".into(), over: Some(over.into()), made: Some(made(volume, mount)), ..Workspace::default() }
    }

    /// W7 (kiln goal 01a1ec34599974448aed2672bafaf9a2, I14): an agent whose volume moved here with it works at the mount
    /// its volume is placed at here, its folder and its environment with it: every path of its environment on the mount
    /// it left (moved there as it started: an activated virtual environment, a `CARGO_TARGET_DIR`) names its place on the
    /// new one, and the rest stays as it was.
    #[test]
    fn w7_a_moved_agents_folder_and_environment_follow_its_volume_to_its_new_mount() {
        let (was, now) = ("/Users/a/.kiln/state/workspaces/kiln-x", "/home/b/.kiln/state/workspaces/kiln-x");
        let mut spec = crate::harness::session::Spec {
            cwd: Some(format!("{was}/src")),
            env: vec![("VIRTUAL_ENV".to_owned(), format!("{was}/.venv")), ("CARGO_TARGET_DIR".to_owned(), format!("{was}/target")), ("EDITOR".to_owned(), "vi".to_owned())],
            workspace: Some(Workspace { sub: "src".into(), ..volume_at("v1", was, "/Users/a/project") }),
            ..Default::default()
        };
        assert!(follow_to(&mut spec, made("v1", now)));
        assert_eq!(spec.cwd.as_deref(), Some("/home/b/.kiln/state/workspaces/kiln-x/src"));
        let env: std::collections::HashMap<&str, &str> = spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(env.get("VIRTUAL_ENV"), Some(&"/home/b/.kiln/state/workspaces/kiln-x/.venv"), "{env:?}");
        assert_eq!(env.get("CARGO_TARGET_DIR"), Some(&"/home/b/.kiln/state/workspaces/kiln-x/target"), "{env:?}");
        assert_eq!(env.get("EDITOR"), Some(&"vi"));
        assert_eq!(spec.workspace.and_then(|w| w.made).map(|m| m.mount), Some(now.to_owned()));
    }

    /// W2: an ended agent's conversation goes back from its folder on its volume's mount to its person's folder the
    /// volume shows there, its place beneath the folder laid over kept; an agent with no mount has none to send back.
    #[test]
    fn w2_an_ended_agents_conversation_goes_back_to_the_folder_its_volume_showed() {
        let spec = |sub: &str, mount: &str| crate::harness::session::Spec {
            cwd: Some(format!("/state/workspaces/kiln-a/{sub}").trim_end_matches('/').to_owned()),
            workspace: Some(Workspace { sub: sub.into(), ..volume_at("v1", mount, "/Users/person/project") }),
            ..Default::default()
        };
        assert_eq!(record_places(&spec("src", "/state/workspaces/kiln-a")), Some(("/state/workspaces/kiln-a/src".to_owned(), "/Users/person/project/src".to_owned())));
        assert_eq!(record_places(&spec("", "/state/workspaces/kiln-a")), Some(("/state/workspaces/kiln-a".to_owned(), "/Users/person/project".to_owned())));
        assert_eq!(record_places(&spec("src", "")), None, "no mount: an isolated agent's, kept in its microVM's volume");
        assert_eq!(record_places(&crate::harness::session::Spec::default()), None);
    }

    /// I11 (kiln goal 01a1ec34599974448aed2672bafaf9a2, docs/agentic.md W2): an agent started in a person's shell works
    /// on its volume with no second path: every path of the shell's environment in the folder its volume is laid over,
    /// whole or in a list, spelled through a link or not, is moved to its place on the mount; its harness's homes, paths
    /// elsewhere, a path on the mount already, a look-alike folder and what isn't a path stay byte for byte.
    #[test]
    #[cfg(unix)]
    fn i11_a_shells_environment_is_moved_onto_its_agents_mount() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let over = std::fs::canonicalize(dir.path())?.join("project");
        std::fs::create_dir_all(over.join(".venv").join("bin"))?;
        std::fs::create_dir_all(over.join("src"))?;
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&over, &linked)?;
        let (o, l, top) = (over.to_string_lossy().into_owned(), linked.to_string_lossy().into_owned(), "/state/workspaces/kiln-a");
        let shells: Vec<(String, String)> = [
            ("PWD", format!("{o}/src")),
            ("OLDPWD", "/Users/person".to_owned()),
            ("VIRTUAL_ENV", format!("{o}/.venv")),
            ("PATH", format!("{o}/.venv/bin:/usr/bin:{l}/src")),
            ("CARGO_TARGET_DIR", format!("{l}/target")),
            ("HOME", o.clone()),
            ("XDG_CONFIG_HOME", format!("{o}/.config")),
            ("DISPLAY", ":0".to_owned()),
            ("ON_IT", format!("{top}/src")),
            ("LOOKALIKE", format!("{o}-other/x")),
            ("WORDS", "src".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let moved: std::collections::HashMap<String, String> = rehomed(&shells, &o, top).into_iter().collect();
        let at = |key: &str| moved.get(key).map(String::as_str);
        assert_eq!(at("PWD"), Some("/state/workspaces/kiln-a/src"));
        assert_eq!(at("VIRTUAL_ENV"), Some("/state/workspaces/kiln-a/.venv"));
        assert_eq!(at("PATH"), Some("/state/workspaces/kiln-a/.venv/bin:/usr/bin:/state/workspaces/kiln-a/src"), "each path of a list, one spelled through a link");
        assert_eq!(at("CARGO_TARGET_DIR"), Some("/state/workspaces/kiln-a/target"), "through a link, though it isn't made yet");
        for (key, value) in &shells {
            if ["OLDPWD", "HOME", "XDG_CONFIG_HOME", "DISPLAY", "ON_IT", "LOOKALIKE", "WORDS"].contains(&key.as_str()) {
                assert_eq!(at(key), Some(value.as_str()), "{key} stays");
            }
        }
        assert_eq!(moved.len(), shells.len());
        Ok(())
    }

    /// W3 (kiln goal 01a1ec34599974448aed2672bafaf9a2; Windows): an agent's folder keeps its place in its repository on its
    /// volume however its path is spelled. git names the repository's top by its real path, so a folder spelled through
    /// a symbolic link (Unix) or with a name in another case (Windows; a runner's `%TEMP%` in its short 8.3 form is the
    /// same case) had no place beneath it, and the agent started at its volume's top instead of in its folder. Do: a
    /// repository with `src/deep`, reached by another spelling of its top. Expect `src/deep` beneath git's top.
    #[test]
    fn w3_an_agents_folder_keeps_its_place_in_its_repository_however_its_path_is_spelled() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let real = dir.path().join("repo");
        std::fs::create_dir_all(real.join("src").join("deep"))?;
        let no_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let made = std::process::Command::new("git").args(["init", "-q"]).current_dir(&real).env("GIT_CONFIG_GLOBAL", no_config).env("GIT_CONFIG_NOSYSTEM", "1").status()?;
        anyhow::ensure!(made.success(), "git init: {made}");
        // The same folder spelled otherwise: through a link on Unix, its name in capitals on Windows.
        #[cfg(unix)]
        let spelled = {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link)?;
            link
        };
        #[cfg(not(unix))]
        let spelled = dir.path().join("REPO");
        let deep = spelled.join("src").join("deep");
        let (over, sub) = crate::rt::block_on(async move { Ok(repo_place(&deep).await) })?;
        anyhow::ensure!(sub == Path::new("src").join("deep").to_string_lossy(), "its place beneath {}: {sub:?}", over.display());
        let same = std::fs::canonicalize(over.join(&sub))? == std::fs::canonicalize(real.join("src").join("deep"))?;
        anyhow::ensure!(same, "{} beneath {} is the folder asked for", sub, over.display());
        assert_eq!(beneath_top(""), "", "at the top");
        assert_eq!(beneath_top("crates/kiln/"), Path::new("crates").join("kiln").to_string_lossy());
        Ok(())
    }

    /// W2: an agent whose folder lies in another agent's volume works in that volume, at its place there, the volume's
    /// owner the agent that made it (also for an agent started in turn in a sharer's folder); a folder outside every
    /// mount, or beside one (`kiln-a2` is not beneath `kiln-a`), shares nothing.
    #[test]
    fn w2_an_agent_started_in_another_agents_folder_works_in_its_volume() {
        let parent = volume_at("v1", "/state/workspaces/kiln-a", "/home/ada/src/kiln");
        let live = vec![("a".to_owned(), parent.clone())];
        let child = sharing("/state/workspaces/kiln-a/crates/kiln", &live).unwrap_or_default();
        assert_eq!(child.kind, "volume");
        assert_eq!(child.made, parent.made, "its parent's volume, not one of its own");
        assert_eq!(child.over, parent.over, "laid over the same folder");
        assert_eq!(child.sub, "crates/kiln");
        assert_eq!(child.owner.as_deref(), Some("a"));
        assert_eq!(sharing("/state/workspaces/kiln-a", &live).map(|w| w.sub), Some(String::new()), "at the top");
        assert_eq!(sharing("/state/workspaces/kiln-a/", &live).map(|w| w.sub), Some(String::new()));
        assert_eq!(sharing("/state/workspaces/kiln-a2/src", &live), None, "beside it, not in it");
        assert_eq!(sharing("/home/ada/src/kiln", &live), None, "the folder itself: a volume of its own");
        // A grandchild in the child's folder: still the parent's.
        let both = vec![("b".to_owned(), child.clone())];
        assert_eq!(sharing("/state/workspaces/kiln-a/crates", &both).and_then(|w| w.owner), Some("a".to_owned()));
        // One not yet made shares nothing.
        let unmade = vec![("c".to_owned(), Workspace { kind: "volume".into(), ..Workspace::default() })];
        assert_eq!(sharing("/state/workspaces/kiln-c", &unmade), None);
    }

    /// W2 (ledger row W's owed line, 2026-11-18): an agent started in a person's shell whose folder lies in another
    /// agent's mount works in that agent's volume, at the same path, as one started in that folder does. The server
    /// chooses a start's workspace before its machine reads the shell's folder, so it had chosen a volume of its own,
    /// and laying that over the other agent's mount was refused by Slates (its A-227). Do: a start the server left
    /// asking for a volume of its own takes a shell whose folder lies in agent `a`'s mount. Expect `a`'s volume, `a` its
    /// owner, the shell's place beneath the mount, and the shell's own folder and environment. A shell's folder outside
    /// every mount keeps the volume of its own, a person's choice of the folder itself is kept, and a start that named
    /// its folder keeps it.
    #[test]
    fn w2_an_agent_started_in_a_shell_inside_another_agents_mount_works_in_its_volume() {
        let parent = volume_at("v1", "/state/workspaces/kiln-a", "/home/ada/src/kiln");
        let live = vec![("a".to_owned(), parent.clone())];
        let own = || Some(Workspace { kind: "volume".into(), ..Workspace::default() });
        let env = vec![("OLDPWD".to_owned(), "/home/ada".to_owned())];
        let shell = |cwd: &str| (Some(cwd.to_owned()), env.clone());
        let started = |workspace: Option<Workspace>, cwd: Option<&str>, at: &str| {
            let mut spec = crate::harness::session::Spec { workspace, cwd: cwd.map(str::to_owned), ..Default::default() };
            from_shell(&mut spec, shell(at), &live);
            spec
        };
        let inside = started(own(), None, "/state/workspaces/kiln-a/crates/kiln");
        let w = inside.workspace.clone().unwrap_or_default();
        assert_eq!(w.made, parent.made, "the other agent's volume, not one of its own laid over its mount");
        assert_eq!(w.owner.as_deref(), Some("a"));
        assert_eq!(w.over, parent.over, "laid over the folder that agent's is");
        assert_eq!(w.sub, "crates/kiln");
        assert_eq!(inside.cwd.as_deref(), Some("/state/workspaces/kiln-a/crates/kiln"), "at the shell's own folder");
        assert_eq!(inside.env, env, "with the shell's environment");
        assert_eq!(started(own(), None, "/home/ada/src/kiln").workspace, own(), "outside every mount: its own volume");
        let folder = Some(Workspace { kind: "folder".into(), ..Workspace::default() });
        assert_eq!(started(folder.clone(), None, "/state/workspaces/kiln-a/src").workspace, folder, "a person's folder choice");
        let named = started(own(), Some("/home/ada/elsewhere"), "/state/workspaces/kiln-a");
        assert_eq!((named.cwd.as_deref(), named.workspace), (Some("/home/ada/elsewhere"), own()), "a folder the start named");
    }

    /// W2 on Windows (judged on any server): a volume is mounted at a drive letter, and a folder on that drive, written
    /// either way and with the letter in either case, lies in it.
    #[test]
    fn w2_a_folder_on_a_windows_workspaces_drive_lies_in_it() {
        assert!(within(r"K:\src\kiln", "K:"));
        assert!(within(r"k:\src", "K:"));
        assert!(within("K:/src", "K:"));
        assert!(within("K:", "K:"));
        assert!(within(r"K:\", "K:"));
        assert!(!within(r"L:\src", "K:"));
        assert!(!within(r"C:\K:\src", "K:"));
        let live = vec![("w".to_owned(), volume_at("v9", "K:", r"C:\Users\ada\src\kiln"))];
        assert_eq!(sharing(r"k:\crates\kiln", &live).map(|w| w.sub), Some(r"crates\kiln".to_owned()));
        assert!(!within("/anything", ""), "no mount holds everything");
    }

    /// W3: a volume goes with the last agent working in it, and a start that fails takes only a volume made for it
    /// alone with it.
    #[test]
    fn w3_a_shared_volume_is_one_volume_and_only_its_owners_start_takes_it() {
        let parent = volume_at("v1", "/w/kiln-a", "/src");
        let child = sharing("/w/kiln-a/sub", &[("a".to_owned(), parent.clone())]);
        assert!(same_volume(Some(&parent), child.as_ref()));
        assert!(!same_volume(Some(&parent), Some(&volume_at("v2", "/w/kiln-b", "/src"))));
        assert!(!same_volume(Some(&parent), None));
        let unmade = || Workspace { kind: "volume".into(), ..Workspace::default() };
        assert!(!same_volume(Some(&unmade()), Some(&unmade())), "volumes not made yet are no volume");
        let spec = |w: Option<Workspace>| crate::harness::session::Spec { workspace: w, ..Default::default() };
        assert_eq!(made_alone(&spec(Some(parent.clone()))), parent.made);
        assert_eq!(made_alone(&spec(child)), None, "its parent's volume goes on");
        assert_eq!(made_alone(&spec(None)), None);
    }

    /// W6 (the person's rule, 2026-11-18): an agent's landing writes at once where its mode edits unasked, waits on its
    /// person where it asks, and is refused where it only plans; a mode kiln can't place asks at most; and a landing
    /// asked by one agent of another's work writes only as far as the lower of the two modes lets it.
    #[test]
    fn w6_a_landing_writes_as_far_as_the_lower_of_the_two_modes_lets_it() {
        use crate::harness::lineage::Level;
        use slates_api::WriteMode;
        assert_eq!(write_mode(Some(Level::Plan)), WriteMode::Forbid);
        assert_eq!(write_mode(Some(Level::Ask)), WriteMode::Ask);
        for edits in [Level::AcceptEdits, Level::Auto, Level::Bypass] {
            assert_eq!(write_mode(Some(edits)), WriteMode::Bypass, "{edits:?}");
        }
        assert_eq!(write_mode(None), WriteMode::Ask, "a mode kiln can't place asks");
        // Each launched in the mode it works in now.
        let agent = |kind: &str, mode: &str| crate::harness::session::Info { kind: kind.into(), mode: mode.into(), launched: crate::harness::lineage::level(kind, mode), ..Default::default() };
        let (bypass, asks, plans) = (agent("claude", "bypassPermissions"), agent("claude", "default"), agent("claude", "plan"));
        assert_eq!(landing_policy(&bypass, &bypass), WriteMode::Bypass);
        assert_eq!(landing_policy(&agent("claude", "acceptEdits"), &agent("codex", "full-access")), WriteMode::Bypass);
        assert_eq!(landing_policy(&agent("codex", ""), &agent("codex", "")), WriteMode::Bypass, "Codex's own default works unasked in its folder");
        assert_eq!(landing_policy(&agent("claude", ""), &agent("claude", "")), WriteMode::Ask, "Claude Code's own default asks");
        assert_eq!(landing_policy(&bypass, &asks), WriteMode::Ask, "a parent in bypass can't write past its child's mode");
        assert_eq!(landing_policy(&asks, &bypass), WriteMode::Ask, "nor a child past its own");
        assert_eq!(landing_policy(&plans, &bypass), WriteMode::Forbid);
        assert_eq!(landing_policy(&bypass, &plans), WriteMode::Forbid);
        let unplaced = agent("nonesuch", "anything");
        assert_eq!(landing_policy(&unplaced, &bypass), WriteMode::Ask, "unplaced: it asks at most");
        assert_eq!(landing_policy(&bypass, &unplaced), WriteMode::Ask);
        assert_eq!(landing_policy(&unplaced, &plans), WriteMode::Forbid);
        assert_eq!(landing_policy(&unplaced, &unplaced), WriteMode::Ask);
    }

    /// W6, the person's rule of 2026-11-18 (an agent never raises its own permission): an agent writes no more than its
    /// launcher let it, whatever mode it works in now, as its harness reports it or it set it itself; a mode lowered
    /// since is honoured; the launch level means the same in whichever harness it goes on in; a session an older kiln
    /// began, with none recorded, is held to its harness's own default.
    #[test]
    fn w6_an_agent_that_raised_its_own_mode_lands_as_its_launcher_let_it() {
        use crate::harness::lineage::Level;
        use slates_api::WriteMode;
        let agent = |kind: &str, launched: Option<Level>, now: &str| crate::harness::session::Info { kind: kind.into(), mode: now.into(), launched, ..Default::default() };
        let raised = agent("claude", Some(Level::Ask), "bypassPermissions");
        assert_eq!(granted_mode(&raised), WriteMode::Ask, "raised past its launcher's mode: still asks");
        assert_eq!(landing_policy(&raised, &raised), WriteMode::Ask);
        assert_eq!(landing_policy(&agent("claude", Some(Level::Bypass), "bypassPermissions"), &raised), WriteMode::Ask, "nor does a parent's bypass write the raised one's work");
        assert_eq!(granted_mode(&agent("claude", Some(Level::Plan), "acceptEdits")), WriteMode::Forbid, "launched to plan: plans still");
        assert_eq!(granted_mode(&agent("claude", Some(Level::Bypass), "bypassPermissions")), WriteMode::Bypass);
        assert_eq!(granted_mode(&agent("claude", Some(Level::Bypass), "plan")), WriteMode::Forbid, "lowered since: honoured");
        assert_eq!(granted_mode(&agent("claude", Some(Level::Bypass), "default")), WriteMode::Ask);
        assert_eq!(granted_mode(&agent("codex", Some(Level::Bypass), "full-access")), WriteMode::Bypass, "switched to Codex, its launch level holds there");
        assert_eq!(granted_mode(&agent("gemini", Some(Level::Plan), crate::harness::acp::KILN_ASK)), WriteMode::Forbid, "switched to a harness with no plan mode: still plans");
        assert_eq!(granted_mode(&agent("claude", None, "bypassPermissions")), WriteMode::Ask, "none recorded: its harness's own default");
        assert_eq!(granted_mode(&agent("codex", None, "full-access")), WriteMode::Bypass, "Codex's own default edits unasked");
    }

    /// W2: an agent in its folder, whatever the reason, has no workspace of its own to ask about.
    #[test]
    fn w2_an_agent_in_its_folder_has_no_volume_to_ask_about() {
        let folder = Workspace { kind: "folder".into(), ..Workspace::default() };
        let refused = own_of(Some(&folder));
        assert!(refused.contains("its folder itself"), "{refused}");
        assert!(own_of(None).contains("its folder itself"));
        let unmade = Workspace { kind: "volume".into(), ..Workspace::default() };
        assert!(own_of(Some(&unmade)).contains("isn't made yet"));
        assert!(super::own(Some(&volume_at("v1", "/w/kiln-a", "/src"))).is_ok());
    }

    fn own_of(w: Option<&Workspace>) -> String {
        super::own(w).err().map(|e| format!("{e:#}")).unwrap_or_default()
    }
}
