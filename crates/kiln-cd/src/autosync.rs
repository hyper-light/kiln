//! Auto-sync as a person sets it for one app (docs/ci.md W26-R27): whether kiln syncs the app on its
//! own whenever git changes what it renders. The app's deploy file (or its definition) says so
//! (`sync.auto`); a person's word for the app beats it until the file changes what it says, and then
//! the file's stands again ([`decide`]). Prune keeps its own knob, and self-heal its own.
//!
//! Pure: the person's setting and the file's word are handed in.

use serde::{Deserialize, Serialize};

use crate::apps::AppDef;
use crate::model::AppConfig;

/// The store setting (`Data::settings`) that holds one app's auto-sync, by the app's id.
pub const APP_SETTING: &str = "auto-sync/app/";

/// The setting key of an app's auto-sync.
pub fn app_setting(id: &str) -> String {
    format!("{APP_SETTING}{id}")
}

/// A person's word on an app's auto-sync, as the store keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    pub on: bool,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub at_ms: u64,
    /// What the deploy file said when it was set: once the file says otherwise, the file stands.
    pub file: bool,
}

/// Whose word an app's auto-sync is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whose {
    /// A person's, for the app.
    App,
    /// Its deploy file's (or its definition's) `sync.auto`.
    DeployFile,
}

impl Whose {
    pub fn as_str(self) -> &'static str {
        match self {
            Whose::App => "app",
            Whose::DeployFile => "deploy-file",
        }
    }
}

/// Whether an app syncs on its own, and whose word that is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub on: bool,
    pub from: Whose,
}

/// An app's auto-sync: `file` is its deploy file's `sync.auto`, `setting` a person's for the app.
/// The person's stands while the file says what it said when they set it.
pub fn decide(file: bool, setting: Option<&Setting>) -> Decision {
    match setting {
        Some(s) if s.file == file => Decision { on: s.on, from: Whose::App },
        _ => Decision { on: file, from: Whose::DeployFile },
    }
}

/// What the engine needs of an app (`AppDef::config`), its auto-sync as [`decide`]s.
pub fn config(def: &AppDef, setting: Option<&Setting>) -> AppConfig {
    let mut c = def.config();
    c.sync.auto = decide(def.sync.auto, setting).on;
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W26-R27: a person's word beats the deploy file's while the file says what it said; once the
    /// file changes its word, the file's stands; with no word, the file's.
    #[test]
    fn w26_r27_a_persons_auto_sync_beats_the_file_until_the_file_changes() {
        let set = |on, file| Setting { on, by: Some("ada".into()), at_ms: 1, file };
        assert_eq!(decide(false, None), Decision { on: false, from: Whose::DeployFile });
        assert_eq!(decide(false, Some(&set(true, false))), Decision { on: true, from: Whose::App });
        assert_eq!(decide(true, Some(&set(false, true))), Decision { on: false, from: Whose::App });
        // The file changed its word since: the file's stands.
        assert_eq!(decide(true, Some(&set(false, false))), Decision { on: true, from: Whose::DeployFile });
        let mut def = crate::state::tests::app("web");
        def.sync.auto = false;
        assert!(config(&def, Some(&set(true, false))).sync.auto);
        assert!(!config(&def, None).sync.auto);
    }
}
