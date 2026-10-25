//! Every key either dialect allows, where, and what it holds: the one source for unknown-key
//! detection, "did you mean", the JSON Schemas (W02-R09) and the editor's completion and hover.

/// What a key holds (for the schemas).
#[derive(Debug, Clone, Copy)]
pub enum Ty {
    /// A plain string.
    Str,
    /// A string that may hold `${{ }}`.
    Tpl,
    Bool,
    /// A boolean or an expression.
    BoolOrExpr,
    /// A number or an expression.
    NumOrExpr,
    Num,
    /// A string or a list of strings.
    StrList,
    /// A mapping with these keys.
    Map(&'static [Key]),
    /// A mapping of names to strings.
    StrMap,
    /// A string, or a mapping with these keys.
    StrOrMap(&'static [Key]),
    /// Anything (validated where it is used).
    Any,
}

#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub name: &'static str,
    /// Only in the kiln dialect (and overlays, when [`Key::overlay`]).
    pub kiln: bool,
    pub overlay: bool,
    pub ty: Ty,
    pub doc: &'static str,
}

const fn gha(name: &'static str, ty: Ty, doc: &'static str) -> Key {
    Key { name, kiln: false, overlay: false, ty, doc }
}

/// A kiln key that overlays may also set on GHA workflows.
const fn kiln(name: &'static str, ty: Ty, doc: &'static str) -> Key {
    Key { name, kiln: true, overlay: true, ty, doc }
}

/// A kiln key overlays may not set (it would change a GHA workflow's meaning, or is a v1 form).
const fn kiln_only(name: &'static str, ty: Ty, doc: &'static str) -> Key {
    Key { name, kiln: true, overlay: false, ty, doc }
}

pub fn find(table: &[Key], name: &str) -> Option<Key> {
    table.iter().find(|k| k.name == name).copied()
}

pub const DEFAULTS_RUN: &[Key] = &[gha("shell", Ty::Tpl, "The shell for `run` steps."), gha("working-directory", Ty::Tpl, "The working directory for `run` steps.")];
pub const DEFAULTS: &[Key] = &[gha("run", Ty::Map(DEFAULTS_RUN), "Defaults for `run` steps.")];

pub const CONCURRENCY: &[Key] = &[
    gha("group", Ty::Tpl, "The concurrency group: at most one run (or job) of a group is in progress and one is pending."),
    gha("cancel-in-progress", Ty::BoolOrExpr, "Cancel the run or job in progress when a new one joins the group."),
    kiln("limit", Ty::Num, "kiln: at most this many in progress (docs/ci.md W05-R07)."),
    gha("queue", Ty::Str, "`single` (default: one pending, replaced by a newer one) or `max` (up to 100 pending, in order); not with `cancel-in-progress: true`."),
];

pub const ENVIRONMENT: &[Key] = &[
    gha("name", Ty::Tpl, "The environment's name."),
    gha("url", Ty::Tpl, "The deployment's URL, shown on the run."),
    gha("deployment", Ty::BoolOrExpr, "`false`: use the environment's rules and secrets without recording a deployment."),
];

pub const CREDENTIALS: &[Key] = &[gha("username", Ty::Tpl, "Registry user name."), gha("password", Ty::Tpl, "Registry password (use a secret).")];

pub const CONTAINER: &[Key] = &[
    gha("image", Ty::Tpl, "The image."),
    gha("credentials", Ty::Map(CREDENTIALS), "Registry credentials."),
    gha("env", Ty::StrMap, "Environment variables in the container."),
    gha("ports", Ty::Any, "Ports to expose."),
    gha("volumes", Ty::Any, "Volumes to mount."),
    gha("options", Ty::Tpl, "Extra `docker create` options."),
];

pub const DISPATCH_INPUT: &[Key] = &[
    gha("description", Ty::Str, "Shown where the run is started."),
    gha("required", Ty::Bool, "The input must be given."),
    gha("default", Ty::Any, "The value when none is given."),
    gha("type", Ty::Str, "`string`, `boolean`, `choice`, `number` or `environment`."),
    gha("options", Ty::Any, "The choices of a `choice` input."),
];

pub const CALL_INPUT: &[Key] = &[
    gha("description", Ty::Str, "What the input is for."),
    gha("required", Ty::Bool, "The caller must pass it."),
    gha("default", Ty::Any, "The value when the caller passes none."),
    gha("type", Ty::Str, "`boolean`, `number` or `string`."),
];

pub const CALL_OUTPUT: &[Key] = &[gha("description", Ty::Str, "What the output is."), gha("value", Ty::Tpl, "Its value: usually `${{ jobs.<id>.outputs.<name> }}`.")];

pub const SECRET_DECL: &[Key] = &[gha("description", Ty::Str, "What the secret is for."), gha("required", Ty::Bool, "It must be passed (or exist, for kiln's declarations).")];

pub const FILTERS: &[Key] = &[
    gha("types", Ty::StrList, "Activity types that trigger the workflow."),
    gha("branches", Ty::StrList, "Branch patterns that trigger it."),
    gha("branches-ignore", Ty::StrList, "Branch patterns that don't."),
    gha("tags", Ty::StrList, "Tag patterns that trigger it."),
    gha("tags-ignore", Ty::StrList, "Tag patterns that don't."),
    gha("paths", Ty::StrList, "Path patterns at least one changed file must match."),
    gha("paths-ignore", Ty::StrList, "Path patterns changed files may all match without triggering."),
    gha("inputs", Ty::Any, "Inputs (`workflow_dispatch`, `workflow_call`)."),
    gha("outputs", Ty::Any, "Outputs (`workflow_call`)."),
    gha("secrets", Ty::Any, "Secrets (`workflow_call`)."),
    gha("workflows", Ty::StrList, "Workflows whose runs trigger this one (`workflow_run`)."),
    kiln("drafts", Ty::Bool, "kiln `diff`: also draft diffs."),
    kiln("authors", Ty::StrList, "kiln `diff`: `human`, `agent`."),
    kiln("states", Ty::StrList, "kiln `review` and `deploy`: the states that trigger."),
    kiln("mentions", Ty::StrList, "kiln `comment`: names mentioned."),
    kiln("commands", Ty::StrList, "kiln `comment`: slash commands."),
    kiln("name", Ty::Str, "kiln `pipeline`: the workflow's name or path."),
    kiln("conclusion", Ty::StrList, "kiln `pipeline`: conclusions that trigger."),
    kiln("agents", Ty::StrList, "kiln `agent`: agents that may trigger (or `*`)."),
    kiln("app", Ty::Str, "kiln `deploy`: the app."),
    kiln("environment", Ty::Str, "kiln `deploy`: the environment."),
    kiln("hook", Ty::Str, "kiln `webhook`: the inbound hook's id."),
    kiln("filter", Ty::Tpl, "kiln `webhook`: a condition over the payload."),
];

/// The keys each event takes.
pub fn event_keys(event: &str) -> &'static [&'static str] {
    match event {
        "push" => &["branches", "branches-ignore", "tags", "tags-ignore", "paths", "paths-ignore"],
        "pull_request" | "pull_request_target" => &["types", "branches", "branches-ignore", "paths", "paths-ignore"],
        "workflow_dispatch" => &["inputs"],
        "workflow_call" => &["inputs", "outputs", "secrets"],
        "workflow_run" => &["workflows", "types", "branches", "branches-ignore"],
        "merge_group" => &["types", "branches", "branches-ignore"],
        "diff" => &["branches", "paths", "paths-ignore", "drafts", "authors"],
        "land" => &["branches"],
        "review" => &["states"],
        "comment" => &["mentions", "commands"],
        "pipeline" => &["name", "conclusion", "branches"],
        "agent" => &["agents", "inputs"],
        "deploy" => &["app", "environment", "states"],
        "webhook" => &["hook", "filter"],
        "manual" => &["inputs"],
        "tag" => &["branches"],
        _ => &["types"],
    }
}

/// Every event GitHub defines.
pub const GHA_EVENTS: &[&str] = &[
    "branch_protection_rule",
    "check_run",
    "check_suite",
    "create",
    "delete",
    "deployment",
    "deployment_status",
    "discussion",
    "discussion_comment",
    "fork",
    "gollum",
    "image_version",
    "issue_comment",
    "issues",
    "label",
    "merge_group",
    "milestone",
    "page_build",
    "project",
    "project_card",
    "project_column",
    "public",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "pull_request_target",
    "push",
    "registry_package",
    "release",
    "repository_dispatch",
    "schedule",
    "status",
    "watch",
    "workflow_call",
    "workflow_dispatch",
    "workflow_run",
];

/// kiln's events (W03-R09), and v1's forms.
pub const KILN_EVENTS: &[&str] = &["diff", "land", "review", "comment", "pipeline", "agent", "deploy", "webhook", "manual", "tag"];

/// The activity types events accept, where GitHub restricts them.
pub fn event_types(event: &str) -> Option<&'static [&'static str]> {
    Some(match event {
        "pull_request" | "pull_request_target" => &[
            "assigned",
            "unassigned",
            "labeled",
            "unlabeled",
            "opened",
            "edited",
            "closed",
            "reopened",
            "synchronize",
            "converted_to_draft",
            "locked",
            "unlocked",
            "enqueued",
            "dequeued",
            "milestoned",
            "demilestoned",
            "ready_for_review",
            "review_requested",
            "review_request_removed",
            "auto_merge_enabled",
            "auto_merge_disabled",
        ],
        "workflow_run" => &["completed", "requested", "in_progress"],
        "release" => &["published", "unpublished", "created", "edited", "deleted", "prereleased", "released"],
        "issue_comment" | "pull_request_review_comment" | "discussion_comment" => &["created", "edited", "deleted"],
        "pull_request_review" => &["submitted", "edited", "dismissed"],
        "merge_group" => &["checks_requested"],
        "registry_package" => &["published", "updated"],
        "check_run" => &["created", "rerequested", "completed", "requested_action"],
        "check_suite" => &["completed"],
        _ => return None,
    })
}

pub const PERMISSION_SCOPES: &[&str] = &[
    "actions",
    "attestations",
    "checks",
    "contents",
    "copilot-requests",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "repository-projects",
    "security-events",
    "statuses",
];

/// kiln's extra permissions (W10-R04).
pub const KILN_PERMISSION_SCOPES: &[&str] = &["binaries", "environments", "registry"];

pub const STRATEGY: &[Key] = &[
    gha("matrix", Ty::Any, "The matrix: axes, `include`, `exclude`, or an expression giving them."),
    gha("fail-fast", Ty::BoolOrExpr, "Cancel the other legs when one fails (default true)."),
    gha("max-parallel", Ty::NumOrExpr, "At most this many legs at once."),
    kiln("when", Ty::Tpl, "kiln: a condition each leg checks when it is ready (W04-R12)."),
    kiln("shard", Ty::Any, "kiln: split work items into balanced legs (W04-R14)."),
    kiln("chunk", Ty::Any, "kiln: split a list into legs of N items (W04-R15)."),
];

/// Matrix keys that are not axes.
pub const MATRIX_RESERVED: &[Key] = &[
    gha("include", Ty::Any, "Extra combinations, or extra values on matching ones."),
    gha("exclude", Ty::Any, "Combinations to remove."),
    kiln("where", Ty::Tpl, "kiln: keep only the combinations for which this is true (W04-R11)."),
    kiln("platforms", Ty::Any, "kiln: OCI platforms as an axis, each leg on a runner of that platform (W04-R13)."),
    kiln("emulate", Ty::StrList, "kiln: platforms that may run under QEMU emulation (W04-R13)."),
];

pub const STEP: &[Key] = &[
    gha("id", Ty::Str, "The step's id, for `steps.<id>`."),
    gha("if", Ty::Tpl, "Run the step only when this is true."),
    gha("name", Ty::Tpl, "Its name in the log."),
    gha("uses", Ty::Str, "An action: `owner/repo@ref`, `owner/repo/path@ref`, `./path` or `docker://image`."),
    gha("run", Ty::Tpl, "Commands to run."),
    gha("shell", Ty::Tpl, "The shell for `run`: `bash`, `sh`, `pwsh`, `powershell`, `cmd`, `python` or a template with `{0}`."),
    gha("with", Ty::Any, "The action's inputs."),
    gha("env", Ty::StrMap, "Environment variables for the step."),
    gha("continue-on-error", Ty::BoolOrExpr, "A failure doesn't fail the job."),
    gha("timeout-minutes", Ty::NumOrExpr, "Fail the step after this long."),
    gha("working-directory", Ty::Tpl, "Where `run` runs."),
    gha("background", Ty::BoolOrExpr, "Run the step in the background: the job goes on, and a later `wait` or `wait-all` collects it (at most 10 at once)."),
    gha("wait", Ty::StrList, "Wait for these background steps (by id); their outputs and environment apply from here."),
    gha("wait-all", Ty::Any, "Wait for every earlier background step."),
    gha("cancel", Ty::StrList, "Stop these background steps (SIGTERM, then SIGKILL after a grace period)."),
    gha("parallel", Ty::Any, "Steps run as background steps, with an implicit wait at the end."),
    kiln("retry", Ty::Any, "kiln: retry this step (W05-R14)."),
    kiln_only("with-secrets", Ty::StrMap, "kiln: map a kiln action's declared secrets to this job's (W09-R07)."),
    kiln("test-results", Ty::Any, "kiln: test reports this step writes (W16-R01)."),
    kiln("integrations", Ty::StrList, "kiln: integrations this step authenticates to (W11-R03)."),
    kiln_only("tty", Ty::Bool, "kiln v1: run under a pseudo-terminal."),
    kiln("memo", Ty::StrOrMap(MEMO), "kiln: reuse the step's earlier success while its inputs are unchanged: `check` (nothing to restore), or `{ inputs, outputs, env, max-age }` (W24-R07)."),
    kiln("tests", Ty::Any, "kiln: run a build's test binaries, those whose binary or what they read changed, and replay the others' recorded passes: `{ cargo: [args] }` (W24-R10)."),
];

/// A step's `memo:` (W24-R07).
pub const MEMO: &[Key] = &[
    kiln("inputs", Ty::StrList, "Paths and globs whose git trees the step depends on (the whole tree when none)."),
    kiln("outputs", Ty::StrList, "Paths in the workspace the step leaves for later steps: kept, and put in place when a later step runs."),
    kiln("env", Ty::StrList, "Variables the step depends on besides those the workflow declares."),
    kiln("max-age", Ty::Str, "How long a result stays reusable (`14d` by default)."),
    kiln("secrets", Ty::Str, "`hmac`: a step that receives secrets keyed by their HMAC (W24-R05)."),
];

pub const JOB: &[Key] = &[
    gha("name", Ty::Tpl, "The job's name."),
    gha("permissions", Ty::Any, "The `GITHUB_TOKEN`'s permissions."),
    gha("needs", Ty::Any, "Jobs that must finish first."),
    gha("if", Ty::Tpl, "Run the job only when this is true."),
    gha("runs-on", Ty::Any, "Runner labels, or `{ group, labels }`."),
    gha("environment", Ty::StrOrMap(ENVIRONMENT), "The environment the job deploys to."),
    gha("concurrency", Ty::StrOrMap(CONCURRENCY), "A concurrency group for the job."),
    gha("outputs", Ty::Any, "Values later jobs read as `needs.<id>.outputs.<name>`."),
    gha("env", Ty::StrMap, "Environment variables for every step."),
    gha("defaults", Ty::Map(DEFAULTS), "Defaults for the job's steps."),
    gha("steps", Ty::Any, "What to run, in order."),
    gha("timeout-minutes", Ty::NumOrExpr, "Cancel the job after this long (default 360)."),
    gha("strategy", Ty::Map(STRATEGY), "Matrix, fail-fast and max-parallel."),
    gha("continue-on-error", Ty::BoolOrExpr, "A failure doesn't fail the run."),
    gha("container", Ty::StrOrMap(CONTAINER), "Run the steps in this container."),
    gha("services", Ty::Any, "Service containers beside the job."),
    gha("uses", Ty::Str, "Call a reusable workflow."),
    gha("with", Ty::Any, "Inputs for the reusable workflow."),
    gha("secrets", Ty::Any, "Secrets for the reusable workflow, or `inherit`."),
    gha("snapshot", Ty::Any, "GitHub custom images: the image to make from this job."),
    gha("cache-mode", Ty::Str, "Cache access for the job: `read`, `write`, `write-only` or `none` (overrides the workflow's)."),
    kiln_only("matrix", Ty::Any, "kiln v1: the job's matrix (`strategy.matrix`)."),
    kiln_only("toolchain", Ty::Str, "kiln: a preset image and caches (`rust`, `go`, `python`, `node`, `bun`, `erlang`)."),
    kiln_only("image", Ty::Tpl, "kiln: the image for container, microVM and Kubernetes legs."),
    kiln("exclusive", Ty::Bool, "kiln: the leg has its runner to itself."),
    kiln("resources", Ty::Any, "kiln: `cpu`, `memory`, `disk`, `gpu` (W05-R03)."),
    kiln("requires", Ty::StrList, "kiln: routing constraints (W05-R22)."),
    kiln("prefer", Ty::Any, "kiln: routing preferences (W05-R23)."),
    kiln("priority", Ty::Any, "kiln: `high`, `normal`, `low` or 0–100 (W05-R09)."),
    kiln("retry", Ty::Any, "kiln: the retry policy (W05-R14)."),
    kiln_only("retries", Ty::Num, "kiln v1: retries after a lost runner."),
    kiln("reuse", Ty::Any, "kiln: reuse an earlier result when the inputs are unchanged (W08-R10)."),
    kiln("work", Ty::Any, "kiln: doing the least work (W24): `memo` takes an earlier result when the inputs are unchanged, as `reuse:` (W24-R06); `warm: [folders]` or `warm: { paths, budget }` keeps the job's workspace on its runner for its next leg, those folders kept (W24-R11)."),
    kiln("cache", Ty::Any, "kiln: caches (W08-R09), or v1's list of kept directories."),
    kiln("exports", Ty::Any, "kiln: `{ env: [names] }` published to later jobs (W04-R21)."),
    kiln("env-from", Ty::StrList, "kiln: import jobs' exported environment (W04-R21)."),
    kiln("integrations", Ty::StrList, "kiln: integrations the job authenticates to (W11-R03)."),
    kiln("registries", Ty::StrList, "kiln: registries the job logs in to (W12-R03)."),
    kiln("network", Ty::Any, "kiln: `{ egress: none | open | { allow } }` (W18-R08)."),
    kiln("debug", Ty::Any, "kiln: `{ keep-on-failure: 30m }` (W17-R02)."),
    kiln("attest", Ty::Any, "kiln: provenance and SBOMs (W18-R01)."),
    kiln("quarantine", Ty::Str, "kiln: `enforce` (W16-R05)."),
    kiln("bench-gate", Ty::Any, "kiln: fail on benchmark regressions (W16-R09)."),
    kiln_only("deploy", Ty::Any, "kiln: `{ app, environment, revision, wait, timeout }`: deploy after the job's steps (W15-R05)."),
    kiln("test-results", Ty::Any, "kiln: `discover`: after its steps, the test reports it wrote where tools usually write them (W16-R01)."),
    kiln("agents", Ty::Any, "kiln: what attached agents may do with this job (W20-R09)."),
    kiln_only("artifacts", Ty::StrList, "kiln v1: files to keep after the job."),
    kiln_only("checkout", Ty::Bool, "kiln: `false` skips the implicit checkout."),
];

pub const WORKFLOW: &[Key] = &[
    gha("name", Ty::Str, "The workflow's name."),
    gha("run-name", Ty::Tpl, "Each run's name."),
    gha("on", Ty::Any, "What triggers the workflow."),
    gha("permissions", Ty::Any, "The `GITHUB_TOKEN`'s permissions for every job."),
    gha("env", Ty::StrMap, "Environment variables for every job."),
    gha("defaults", Ty::Map(DEFAULTS), "Defaults for every job."),
    gha("concurrency", Ty::StrOrMap(CONCURRENCY), "A concurrency group for the runs."),
    gha("jobs", Ty::Any, "The jobs."),
    gha("cache-mode", Ty::Str, "Cache access for every job: `read`, `write`, `write-only` or `none` (low-trust events default to `read`)."),
    kiln_only("secrets", Ty::Any, "kiln: the secrets this workflow uses (W09-R06)."),
    kiln("supersede", Ty::Any, "kiln: cancel or deprioritize older runs (W05-R08)."),
    kiln("routing", Ty::Any, "kiln: `{ infer: true }` (W05-R19)."),
    kiln_only("run-env", Ty::Bool, "kiln: collect `$KILN_RUN_ENV` into `run.env` (W04-R21)."),
    kiln("kiln", Ty::Any, "kiln: `{ honour-skip-ci, chain-depth }` (W03-R06, R13)."),
    kiln_only("checkout", Ty::Bool, "kiln: `false` turns the implicit checkout off."),
];

pub const ACTION: &[Key] = &[
    gha("name", Ty::Str, "The action's name."),
    gha("description", Ty::Str, "What it does."),
    gha("author", Ty::Str, "Who made it."),
    gha("inputs", Ty::Any, "Its inputs."),
    gha("outputs", Ty::Any, "Its outputs."),
    gha("runs", Ty::Any, "How it runs."),
    gha("branding", Ty::Any, "Marketplace icon and color."),
    kiln("secrets", Ty::Any, "kiln actions: the secrets it needs (W09-R07)."),
    kiln("requires", Ty::StrList, "kiln actions: routing constraints its steps need (W07-R11)."),
    kiln("permissions", Ty::Any, "kiln actions: the job permissions it needs (W07-R11)."),
];

pub const ACTION_INPUT: &[Key] = &[
    gha("description", Ty::Str, "What the input is for."),
    gha("required", Ty::Bool, "It must be given."),
    gha("default", Ty::Tpl, "The value when none is given."),
    gha("deprecationMessage", Ty::Str, "Shown when the input is used."),
];

pub const ACTION_OUTPUT: &[Key] = &[gha("description", Ty::Str, "What the output is."), gha("value", Ty::Tpl, "Composite actions: its value.")];

pub const ACTION_RUNS: &[Key] = &[
    gha("using", Ty::Str, "`node20`, `node24`, `composite` or `docker` (`node12`, `node16` run on node20)."),
    gha("main", Ty::Str, "JavaScript actions: the file to run."),
    gha("pre", Ty::Str, "JavaScript actions: run before the main step."),
    gha("post", Ty::Str, "JavaScript actions: run after the job's steps."),
    gha("pre-if", Ty::Tpl, "When to run `pre`."),
    gha("post-if", Ty::Tpl, "When to run `post`."),
    gha("image", Ty::Str, "Docker actions: `Dockerfile` or `docker://image`."),
    gha("entrypoint", Ty::Str, "Docker actions: the entrypoint."),
    gha("pre-entrypoint", Ty::Str, "Docker actions: run before the main step."),
    gha("post-entrypoint", Ty::Str, "Docker actions: run after the job's steps."),
    gha("args", Ty::Any, "Docker actions: arguments."),
    gha("env", Ty::StrMap, "Docker actions: environment variables."),
    gha("steps", Ty::Any, "Composite actions: the steps."),
];
