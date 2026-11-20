//! Workspaces this machine holds for sessions that run elsewhere: a Kubernetes pod's (docs/agentic.md W10). A pod's agent
//! works in the Slates volume its *home* gives it, the runner whose machine holds its folder, which kubelet mounts into
//! the pod from the home's network export; the volume stays at home, so its review, drift and landing are the bare-metal
//! engine's, asked of the home as an isolated session's are of its machine.
//!
//! **What holding is.** The server asks the home to hold a workspace for session `id` ([`super::Ask::Hold`]): the home
//! makes its volume over the folder's repository with no mount (an isolated session's volume is made so too), sets its
//! write policy from the grant its launcher gave the agent (W6, [`super::write_mode`]), and has its daemon export it to
//! one client acting as the folder's owner (Slates' `Instance::export`, A-115: that uid's calls are served, every other
//! caller's, root's too, refused). It answers the export's path and attachment, the owner's uid and gid, and the folder
//! ([`Holding`]), and keeps the holding under the session's id until it's let go ([`super::Ask::Release`]: the export
//! detached, so its path reaches nothing whatever kubelet still holds, then the volume ended by W3's rules).
//!
//! **Asked only of what it holds.** A question about a folder is answered here only for a session working here, or one
//! whose workspace is held here ([`find`]): as a session's workspace, never a folder the server names. The server asks a
//! hold only as it starts a session for a person who may start one there.
//!
//! What a holding keeps is this process's: a runner restarted forgets its holdings, and their volumes and exports end
//! with its instance's own restart rules, as an isolated session's do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Workspace;
use super::slates::Made;

/// What the server asks a home to hold ([`super::Ask::Hold`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hold {
    /// The session's folder on the home's machine.
    pub cwd: String,
    /// How much its launcher let its agent do (W6): its volume's write policy. None: it asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<crate::harness::lineage::Level>,
    /// Its pod reads its workspace alone: the export refuses its changes.
    #[serde(default)]
    pub read_only: bool,
}

/// A workspace held here for a session elsewhere: the workspace as a session's names it, and its export.
#[derive(Clone)]
struct Holding {
    workspace: Workspace,
    cwd: PathBuf,
    attachment: u64,
    path: String,
    uid: u32,
    gid: u32,
}

/// What this process holds, by session id.
static HELD: LazyLock<Mutex<HashMap<String, Holding>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn held() -> std::sync::MutexGuard<'static, HashMap<String, Holding>> {
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answer to a hold: the volume made, the export's path and attachment, the uid and gid it serves, and the folder.
fn answered(h: &Holding) -> Value {
    json!({"made": h.workspace.made, "path": h.path, "attachment": h.attachment, "uid": h.uid, "gid": h.gid, "folder": h.cwd.to_string_lossy()})
}

/// Session `id`'s workspace if this machine holds it, with the session's folder: what a question about its folder is
/// answered from (`agent::loops::workspace`).
pub fn find(id: &str) -> Option<(PathBuf, Workspace)> {
    held().get(id).map(|h| (h.cwd.clone(), h.workspace.clone()))
}

/// The uid and gid that own `folder`: who a pod's export serves, and whom the pod runs as.
#[cfg(unix)]
fn owner(folder: &Path) -> anyhow::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::metadata(folder)?;
    Ok((m.uid(), m.gid()))
}

/// No Windows folder is held for a pod: Slates' network export serves Unix hosts alone.
#[cfg(not(unix))]
fn owner(_folder: &Path) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("a pod's workspace is held on a Unix machine alone: Slates serves no network export on Windows")
}

/// The volume's name for session `id`: `kiln-` and its id's letters and digits, as a session's own workspace is named.
fn name_for(id: &str) -> String {
    format!("kiln-{}", id.chars().filter(char::is_ascii_alphanumeric).collect::<String>())
}

/// Holds a workspace here for session `id`, as `ask` says (see the module's notes): what it answers the server. Asked
/// again for a session it holds, it answers the same holding.
pub async fn hold(id: &str, ask: Hold) -> anyhow::Result<Value> {
    if let Some(h) = held().get(id) {
        return Ok(answered(h));
    }
    let cwd = PathBuf::from(&ask.cwd);
    anyhow::ensure!(cwd.is_dir(), "{} isn't a folder on this machine", cwd.display());
    let (over, sub) = super::repo_place(&cwd).await;
    let (uid, gid) = owner(&over)?;
    let made = super::slates::make_unmounted(&name_for(id), &over).await?;
    let held_up: anyhow::Result<slates_api::Exported> = async {
        let mode = super::write_mode(ask.level);
        super::slates::set_policy(&made, slates_api::WritePolicy { merge: mode, land: mode, require_signed_merges: false }).await?;
        super::slates::export(&made, uid, ask.read_only).await
    }
    .await;
    let exported = match held_up {
        Ok(exported) => exported,
        Err(e) => {
            // A hold that failed holds nothing: its volume goes with it.
            if let Err(gone) = super::slates::remove(&made).await {
                tracing::warn!(volume = %made.volume, error = %format!("{gone:#}"), "a failed hold's volume stayed");
            }
            return Err(e);
        }
    };
    let workspace = Workspace { kind: "volume".to_owned(), over: Some(over.to_string_lossy().into_owned()), sub, made: Some(made), owner: None };
    let holding = Holding { workspace, cwd, attachment: exported.attachment, path: exported.path, uid, gid };
    let answer = answered(&holding);
    held().insert(id.to_owned(), holding);
    Ok(answer)
}

/// Lets session `id`'s held workspace go (see the module's notes): its export detached, then its volume ended by W3's
/// rules. One it doesn't hold is let go already.
pub async fn release(id: &str) -> anyhow::Result<Value> {
    let Some(h) = held().remove(id) else { return Ok(json!({"released": false})) };
    if let Err(e) = super::slates::detach(h.attachment).await {
        tracing::warn!(session = id, error = %format!("{e:#}"), "a held workspace's export wasn't detached: it ends with its volume");
    }
    if let Some(made) = &h.workspace.made {
        super::slates::remove(made).await?;
    }
    Ok(json!({"released": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10. Do: ask a hold as the server sends it, then keep one and look it up and let it go from the map, with no
    /// daemon. Expect the ask in the workspace asks' own words (`op: hold`, a level, read-only off by default); the volume
    /// named as a session's own is; a held session's folder and workspace found by its id alone, the answer carrying the
    /// export's path, attachment and owner; and a folder's owner read as the machine records it.
    #[test]
    fn w10_a_home_holds_a_pods_workspace_by_its_session_and_answers_its_export() -> anyhow::Result<()> {
        let ask = super::super::Ask::Hold(Hold { cwd: "/src/kiln".to_owned(), level: Some(crate::harness::lineage::Level::AcceptEdits), read_only: false });
        let said = serde_json::to_value(&ask)?;
        assert_eq!(said.get("op"), Some(&json!("hold")));
        assert_eq!(serde_json::from_value::<super::super::Ask>(json!({"op": "hold", "cwd": "/src/kiln"}))?, super::super::Ask::Hold(Hold { cwd: "/src/kiln".to_owned(), level: None, read_only: false }));
        assert_eq!(serde_json::from_value::<super::super::Ask>(json!({"op": "release"}))?, super::super::Ask::Release);
        assert_eq!(name_for("01a1f7-pod.x"), "kiln-01a1f7podx");

        let made = Made { volume: "0".repeat(32), name: "kiln-p1".to_owned(), ..Made::default() };
        let workspace = Workspace { kind: "volume".to_owned(), over: Some("/src/kiln".to_owned()), sub: "crates".to_owned(), made: Some(made), owner: None };
        let holding = Holding { workspace: workspace.clone(), cwd: PathBuf::from("/src/kiln/crates"), attachment: 7, path: "/kiln-p1@7.4f1c".to_owned(), uid: 501, gid: 20 };
        held().insert("w10-test-p1".to_owned(), holding.clone());
        assert_eq!(find("w10-test-p1"), Some((PathBuf::from("/src/kiln/crates"), workspace)));
        assert_eq!(find("w10-test-none"), None);
        let a = answered(&holding);
        assert_eq!((a.get("path"), a.get("attachment"), a.get("uid"), a.get("gid")), (Some(&json!("/kiln-p1@7.4f1c")), Some(&json!(7)), Some(&json!(501)), Some(&json!(20))));
        held().remove("w10-test-p1");
        assert_eq!(find("w10-test-p1"), None);

        #[cfg(unix)]
        {
            let dir = tempfile::tempdir()?;
            // Its group is its parent folder's on a BSD (a Mac), so the user alone is held to this process's.
            let me = std::process::Command::new("id").arg("-u").output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok());
            assert_eq!(Some(owner(dir.path())?.0), me, "a folder this process made is its user's");
        }
        Ok(())
    }
}
