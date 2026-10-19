//! `[sso]` (docs/ci.md W23-R08): signing in with GitHub, GitLab and Bitbucket.
//!
//! ```toml
//! [sso.github]
//! client_id = "Iv1.0123456789abcdef"       # or none: the GitHub App's, when its manifest flow made it
//! client_secret_file = "/etc/kiln/github-client-secret"
//! allowed = ["acme"]                       # organizations; empty: anyone GitHub signs in
//! create_users = true                      # a kiln user on a first sign-in
//! grants = { "acme" = ["viewer@org:acme"], "acme/platform" = ["developer@org:acme"] }
//!
//! [sso.gitlab]
//! url = "https://gitlab.com"
//! client_id = "…"
//! client_secret_file = "…"
//! allowed = ["acme"]                       # groups (`acme/platform` counts as `acme` too)
//!
//! [sso.bitbucket]
//! client_id = "…"
//! client_secret_file = "…"
//! allowed = ["acme"]                       # workspaces
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sso {
    pub github: Option<Provider>,
    pub gitlab: Option<Provider>,
    pub bitbucket: Option<Provider>,
}

/// One host's sign-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub client_id: Option<String>,
    pub client_secret_file: Option<PathBuf>,
    /// GitLab's (or a self-managed one's) origin; GitHub's and Bitbucket's come from their own
    /// settings (`web_url`, `api_url`) or their defaults.
    pub url: Option<String>,
    pub web_url: Option<String>,
    pub api_url: Option<String>,
    /// Organizations (GitHub), groups (GitLab) or workspaces (Bitbucket) a person must belong to.
    pub allowed: Vec<String>,
    /// Make a kiln user on a first sign-in (else only people kiln has, by email or a linked
    /// account, sign in).
    pub create_users: bool,
    /// Grants by membership: an organization, group or workspace (or `org/team`, `group/subgroup`)
    /// → grants (`developer@org:acme`), given at each sign-in and taken back when it's gone.
    pub grants: BTreeMap<String, Vec<String>>,
}
