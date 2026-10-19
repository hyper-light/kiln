//! JSON payloads between the server and CI runners (docs/ci.md §2.5): [`AssignmentV2`] in
//! `ServerMsg::AssignV2` and [`JobEventV2`] in `AgentMsg::JobEvent`. Plain data, so both sides
//! (and every version of either) agree on one definition. Every field defaults, so a runner and a
//! server a version apart still understand each other; new fields are added, never renamed.
//!
//! What GitHub evaluates on the runner travels unevaluated: the job's and the workflow's `env`,
//! `defaults.run`, the steps, the job's outputs. The runner evaluates them with the contexts
//! below and the ones only it knows (`steps`, `runner`, `job`, `env`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wf::ir::{Defaults, Step, Tpl};

/// One leg for a runner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssignmentV2 {
    /// The leg (`jobs.id`).
    pub job_id: String,
    pub run_id: String,
    /// The leg's attempt (`KILN_ATTEMPT`); `github.run_attempt` is the run's.
    pub attempt: u32,
    /// `owner/name`.
    pub repo: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// `gha` or `kiln`.
    pub dialect: String,
    pub leg: LegInfo,
    pub executor: ExecutorSpec,
    /// The job container's image, evaluated; its other keys come with `job_container`.
    pub container: Option<String>,
    pub services: BTreeMap<String, String>,
    /// The job's `container` and `services` as written: their `credentials`, `env`, `ports`,
    /// `volumes` and `options` are evaluated on the runner (W13-R08).
    pub job_container: Option<crate::wf::ir::Container>,
    pub job_services: Vec<(String, crate::wf::ir::Container)>,
    /// The job's steps, unevaluated (W06).
    pub steps: Vec<Step>,
    /// `env` of the workflow, then of the job, unevaluated.
    pub workflow_env: Vec<(String, Tpl)>,
    pub job_env: Vec<(String, Tpl)>,
    /// `defaults.run` of the workflow, then of the job.
    pub workflow_defaults: Defaults,
    pub job_defaults: Defaults,
    /// The job's `outputs`, evaluated after its steps.
    pub outputs: Vec<(String, Tpl)>,
    /// `environment.url`, evaluated after its steps too (W15-R04).
    #[serde(default)]
    pub environment_url: Option<Tpl>,
    pub contexts: Contexts,
    /// Secrets the leg may read (W09 narrows them to what it references).
    pub secrets: Vec<Secret>,
    /// Values to mask besides the secrets.
    pub masks: Vec<String>,
    /// `env-from` and `run.env` (W04-R21), applied before the job's own `env`.
    pub env_from: Vec<(String, String)>,
    /// A `shard` or `chunk` leg's items (`$KILN_SHARD_ITEMS`).
    pub items: Vec<String>,
    pub timeout_minutes: u32,
    pub continue_on_error: bool,
    pub runtime: Runtime,
    /// `none` or `auto` (the kiln dialect's implicit checkout).
    pub checkout: String,
    /// Remote actions the steps use, resolved when the run started (W07-R02), by `uses`.
    pub actions: BTreeMap<String, ActionRef>,
    /// kiln (W04-R21): the job's `exports: { env }`, whose final values it publishes.
    #[serde(default)]
    pub exports_env: Vec<String>,
    /// kiln (W04-R22): the job's outputs declared `secret: true`.
    #[serde(default)]
    pub secret_outputs: Vec<String>,
    /// kiln (W04-R21): the workflow's `run-env: true`; `$KILN_RUN_ENV` lines go into `run.env`.
    #[serde(default)]
    pub run_env: bool,
    /// kiln (W16-R05): the job's `quarantine: enforce`: a step whose failed tests are all
    /// quarantined passes, with a warning.
    #[serde(default)]
    pub quarantine: bool,
    /// kiln (W11-R03): the integrations its job, its steps and its `kiln/auth@v1` steps name.
    #[serde(default)]
    pub integrations: Vec<Integration>,
    pub debug: Debug,
    pub limits: Limits,
    /// W18-R08: where the leg's containers may connect, through the runner's egress proxy;
    /// `None` when they connect freely.
    #[serde(default)]
    pub network: Option<Network>,
}

/// A leg's egress (W18-R08): the hosts it may reach (`*.example.com`, `host:port`) besides kiln's
/// own server, which it always reaches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    pub allow: Vec<String>,
    /// The leg is untrusted: if its executor can't enforce the policy it must not run.
    pub untrusted: bool,
}

/// An integration a leg authenticates to (W11-R03): what the runner needs to give its steps the
/// provider's own federation files and variables, or its stored token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Integration {
    pub name: String,
    pub kind: String,
    /// `oidc` (exchanged with the leg's ID token) or `token` (stored).
    pub mode: String,
    /// Its configuration, without secrets: a role, a provider, a tenant, a registry…
    pub config: serde_json::Value,
    /// A stored token or key (masked).
    pub token: Option<String>,
    /// The job's own `integrations:` named it: every step gets it.
    pub job: bool,
}

/// The leg itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegInfo {
    pub job_id: String,
    pub name: String,
    pub matrix: serde_json::Value,
    pub index: u32,
    pub total: u32,
}

/// Where the leg runs (W13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutorSpec {
    /// `host`, `docker`, `firecracker`, `shards`, `k8s`.
    pub kind: String,
    pub image: Option<String>,
    pub options: serde_json::Value,
    /// The pool's executor, ready on the runner, the leg runs in (W13-R09).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// The contexts the server knows (W06-R03).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contexts {
    pub github: serde_json::Value,
    pub vars: serde_json::Value,
    pub needs: serde_json::Value,
    pub strategy: serde_json::Value,
    pub matrix: serde_json::Value,
    pub inputs: serde_json::Value,
    /// kiln: `run.env` from the jobs it needs (W04-R21).
    #[serde(default)]
    pub run: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Secret {
    pub name: String,
    pub value: String,
}

/// The runtime services a leg talks to (W08, W10, W14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Runtime {
    pub results_url: String,
    pub runtime_token: String,
    pub cache_url: String,
    pub id_token_url: Option<String>,
    pub id_token_request_token: Option<String>,
    /// `GITHUB_SERVER_URL`: the code host (kiln's facade, or GitHub).
    pub facade_url: String,
    pub api_url: String,
    pub graphql_url: String,
    pub github_token: String,
    /// `ACTIONS_CACHE_MODE`: `read`, `write`, `write-only` or `none` (W08-R14).
    pub cache_mode: Option<String>,
    /// kiln's own registry (W12-R01): the server's origin, as the runner reaches it.
    pub registry_url: String,
    /// kiln's package proxies the leg's tools are pointed at (W12-R04): `(ecosystem, url)`.
    pub proxies: Vec<(String, String)>,
    /// The registries kiln mirrors (W12-R02): `(upstream, prefix)`; the runner pulls their images
    /// through kiln.
    pub mirrors: Vec<(String, String)>,
}

/// A remote action, resolved (W07-R02).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionRef {
    /// `owner/repo`.
    pub repo: String,
    /// The commit the ref named when the run started.
    pub sha: String,
    /// The directory of `action.yml` inside the repository (`""` for the root).
    pub path: String,
    /// Where the runner downloads it (a tarball the server serves from its mirror).
    pub url: String,
    /// A kiln action's declarations (W07-R11): the secrets it reads (name, required), the
    /// constraints its steps need, and the permissions it needs (scope, level; `*` for all).
    pub secrets: Vec<(String, bool)>,
    pub requires: Vec<String>,
    pub permissions: Vec<(String, String)>,
    /// Why it couldn't be resolved when the run started (W07-R03): its source unreachable and
    /// never mirrored, no such ref. A job using it fails as it starts, as GitHub's fails at "Set
    /// up job", and the run's other jobs go on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ActionRef {
    /// Its declarations, from its metadata.
    pub fn declares(&mut self, a: &crate::wf::ir::Action) {
        self.secrets = a.secrets.iter().map(|d| (d.name.to_ascii_uppercase(), d.required)).collect();
        self.requires = a.requires.clone();
        self.permissions = match &a.permissions {
            Some(crate::wf::ir::Permissions::ReadAll) => vec![("*".into(), "read".into())],
            Some(crate::wf::ir::Permissions::WriteAll) => vec![("*".into(), "write".into())],
            Some(crate::wf::ir::Permissions::Scopes(s)) => s.clone(),
            None => Vec::new(),
        };
    }

    /// The permissions it declares, as a job's would resolve.
    pub fn needs(&self) -> Option<crate::plan::perms::Resolved> {
        if self.permissions.is_empty() {
            return None;
        }
        let p = match self.permissions.as_slice() {
            [(all, level)] if all == "*" => {
                if level == "write" {
                    crate::wf::ir::Permissions::WriteAll
                } else {
                    crate::wf::ir::Permissions::ReadAll
                }
            }
            s => crate::wf::ir::Permissions::Scopes(s.to_vec()),
        };
        Some(crate::plan::perms::resolve(Some(&p), None, crate::plan::perms::Default::Read, true))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Debug {
    pub runner_debug: bool,
    pub step_debug: bool,
    pub keep_on_failure_minutes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub output_bytes: u64,
    pub summary_bytes: u64,
    pub log_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { output_bytes: 1 << 20, summary_bytes: 1 << 20, log_bytes: 1 << 30 }
    }
}

/// How a step or a leg ended, as GitHub says it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    #[default]
    Success,
    Failure,
    Cancelled,
    Skipped,
}

/// What a runner reports about a leg while it runs (`AgentMsg::JobEvent`). The leg's end is
/// `AgentMsg::JobFinished`, as for v1 jobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum JobEventV2 {
    StepStarted {
        idx: u32,
        name: String,
        /// `run`, `uses`, `pre`, `post`, `internal`.
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    StepFinished {
        idx: u32,
        outcome: Conclusion,
        conclusion: Conclusion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default)]
        outputs: BTreeMap<String, String>,
    },
    Annotation {
        idx: u32,
        /// `error`, `warning`, `notice`, `debug`.
        level: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        col: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_col: Option<u32>,
        #[serde(default)]
        log_offset: u64,
    },
    Group {
        idx: u32,
        /// `start` or `end`.
        op: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default)]
        log_offset: u64,
    },
    /// A value was masked (its digest, never the value).
    Mask { value_sha256: String },
    Summary { idx: u32, markdown: String },
    JobOutputs {
        outputs: BTreeMap<String, String>,
        /// kiln: typed outputs (`$KILN_OUTPUT_JSON`), exported environment, `run.env` lines, and
        /// the names of secret outputs that were set. Their values come in `secret_values`, which
        /// the server seals the moment it reads them (W09-R10) and never keeps in the clear.
        #[serde(default)]
        json_outputs: BTreeMap<String, serde_json::Value>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        run_env: Vec<(String, String)>,
        #[serde(default)]
        secret_outputs: Vec<String>,
        #[serde(default)]
        secret_values: BTreeMap<String, String>,
        /// `environment.url`, evaluated (W15-R04).
        #[serde(default)]
        environment_url: Option<String>,
    },
    TestReport { idx: u32, format: String, object: String },
    /// W16-R07: what the step wrote to `$KILN_METRICS`, and the runner's fingerprint (its OS and
    /// architecture, CPU model, kernel and executor), which baselines are kept by.
    Metrics {
        idx: u32,
        points: Vec<serde_json::Value>,
        #[serde(default)]
        fingerprint: String,
    },
    /// Why the attempt failed (W05-R15): `runner-lost`, `infra`, `image-pull`, `oom`, `disk-full`,
    /// `timeout`, `failure`.
    FailureClass { class: String },
    /// The failed leg is kept to look into until `until` (Unix seconds, W17-R02).
    Kept { until: i64 },
    /// W18-R01: what an attest step (or the job's `attest:`) names, by digest. The server builds
    /// and signs their provenance from what it knows of the run, and their SBOM's statement when
    /// `sbom` (the document the runner uploaded under `nonce`).
    Attest {
        idx: u32,
        /// The runner's random name for this report: its SBOM's upload and its bundles are
        /// found by it (`/_kiln/attestations`).
        nonce: String,
        subjects: Vec<crate::attest::Subject>,
        #[serde(default)]
        provenance: bool,
        #[serde(default)]
        sbom: bool,
    },
    /// W13-R11: an image a `kiln/build-image@v1` step built and pushed to kiln's registry.
    ImageBuilt {
        idx: u32,
        name: String,
        /// Where it is (`<registry>/<repository>:<tag>`), and its digest.
        reference: String,
        digest: String,
        #[serde(default)]
        platforms: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_as_the_spec_writes_them() -> Result<(), serde_json::Error> {
        let e: JobEventV2 = serde_json::from_str(r#"{"t":"step_finished","idx":2,"outcome":"failure","conclusion":"success","outputs":{"x":"1"}}"#)?;
        assert_eq!(e, JobEventV2::StepFinished { idx: 2, outcome: Conclusion::Failure, conclusion: Conclusion::Success, exit_code: None, outputs: [("x".to_owned(), "1".to_owned())].into_iter().collect() });
        let e: JobEventV2 = serde_json::from_str(r#"{"t":"failure_class","class":"oom"}"#)?;
        assert_eq!(e, JobEventV2::FailureClass { class: "oom".into() });
        // An assignment from an older or newer server: missing fields default, unknown ones are
        // ignored.
        let a: AssignmentV2 = serde_json::from_str(r#"{"job_id":"j","ref":"refs/heads/main","future_field":1}"#)?;
        assert_eq!((a.job_id.as_str(), a.git_ref.as_str(), a.limits.output_bytes), ("j", "refs/heads/main", 1 << 20));
        Ok(())
    }
}
