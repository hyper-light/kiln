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
    pub debug: Debug,
    pub limits: Limits,
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
        /// the names of secret outputs that were set (their values go to W09, not here).
        #[serde(default)]
        json_outputs: BTreeMap<String, serde_json::Value>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        run_env: Vec<(String, String)>,
        #[serde(default)]
        secret_outputs: Vec<String>,
    },
    TestReport { idx: u32, format: String, object: String },
    Metrics { idx: u32, points: Vec<serde_json::Value> },
    /// Why the attempt failed (W05-R15): `runner-lost`, `infra`, `image-pull`, `oom`, `disk-full`,
    /// `timeout`, `failure`.
    FailureClass { class: String },
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
