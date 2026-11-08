// @kiln/sdk (docs/ci.md W29-R27): the types of kiln's code workflows, from the kiln dialect's
// JSON Schema (GET /api/ci/schema/kiln). Served at GET /api/ci/sdk/kiln.d.ts.
declare module "@kiln/sdk" {
  /** kiln workflow (.kiln/*.yaml): GitHub's syntax plus kiln's keys */
  export type Workflow = {
    /** Cache access for every job: `read`, `write`, `write-only` or `none` (low-trust events default to `read`). */
    "cache-mode"?: string;
    /** kiln: `{ on: free }` or `{ every }`: the newest commit runs when its runner frees, or once per interval (W28-R62). */
    cadence?: unknown;
    /** kiln: `false` turns the implicit checkout off. */
    checkout?: boolean;
    /** kiln: `{ quiet, max-wait, when }`: a burst of pushes runs its newest commit while the runners it needs are busy (W28-R61). */
    coalesce?: unknown;
    /** A concurrency group for the runs. */
    concurrency?: string | {
      /** Cancel the run or job in progress when a new one joins the group. */
      "cancel-in-progress"?: boolean | Expr;
      /** The concurrency group: at most one run (or job) of a group is in progress and one is pending. */
      group?: string;
      /** kiln: at most this many in progress (docs/ci.md W05-R07). */
      limit?: number;
      /** `single` (default: one pending, replaced by a newer one) or `max` (up to 100 pending, in order); not with `cancel-in-progress: true`. */
      queue?: string;
    };
    /** Defaults for every job. */
    defaults?: {
      /** Defaults for `run` steps. */
      run?: {
        /** The shell for `run` steps. */
        shell?: string;
        /** The working directory for `run` steps. */
        "working-directory"?: string;
      };
    };
    /** Environment variables for every job. */
    env?: {
      [key: string]: string | number | boolean | null;
    };
    /** The jobs. */
    jobs?: {
      [id: string]: Job;
    };
    /** kiln: `{ honour-skip-ci, chain-depth }` (W03-R06, R13). */
    kiln?: unknown;
    /** The workflow's name. */
    name?: string;
    /** What triggers the workflow. */
    on?: On;
    /** The `GITHUB_TOKEN`'s permissions for every job. */
    permissions?: unknown;
    /** kiln: `{ infer: true }` (W05-R19). */
    routing?: unknown;
    /** kiln: collect `$KILN_RUN_ENV` into `run.env` (W04-R21). */
    "run-env"?: boolean;
    /** Each run's name. */
    "run-name"?: string;
    /** kiln: the secrets this workflow uses (W09-R06). */
    secrets?: unknown;
    /** kiln: cancel or deprioritize older runs (W05-R08). */
    supersede?: unknown;
  };
  /** A job. */
  export type Job = {
    /** kiln: what attached agents may do with this job (W20-R09). */
    agents?: unknown;
    /** kiln v1: files to keep after the job. */
    artifacts?: string | string[];
    /** kiln: provenance and SBOMs (W18-R01). */
    attest?: unknown;
    /** kiln: fail on benchmark regressions (W16-R09). */
    "bench-gate"?: unknown;
    /** kiln: caches (W08-R09), or v1's list of kept directories. */
    cache?: unknown;
    /** Cache access for the job: `read`, `write`, `write-only` or `none` (overrides the workflow's). */
    "cache-mode"?: string;
    /** kiln: `false` skips the implicit checkout. */
    checkout?: boolean;
    /** A concurrency group for the job. */
    concurrency?: string | {
      /** Cancel the run or job in progress when a new one joins the group. */
      "cancel-in-progress"?: boolean | Expr;
      /** The concurrency group: at most one run (or job) of a group is in progress and one is pending. */
      group?: string;
      /** kiln: at most this many in progress (docs/ci.md W05-R07). */
      limit?: number;
      /** `single` (default: one pending, replaced by a newer one) or `max` (up to 100 pending, in order); not with `cancel-in-progress: true`. */
      queue?: string;
    };
    /** Run the steps in this container. */
    container?: string | {
      /** Registry credentials. */
      credentials?: {
        /** Registry password (use a secret). */
        password?: string;
        /** Registry user name. */
        username?: string;
      };
      /** Environment variables in the container. */
      env?: {
        [key: string]: string | number | boolean | null;
      };
      /** The image. */
      image?: string;
      /** Extra `docker create` options. */
      options?: string;
      /** Ports to expose. */
      ports?: unknown;
      /** Volumes to mount. */
      volumes?: unknown;
    };
    /** A failure doesn't fail the run. */
    "continue-on-error"?: boolean | Expr;
    /** kiln: `{ keep-on-failure: 30m }` (W17-R02). */
    debug?: unknown;
    /** Defaults for the job's steps. */
    defaults?: {
      /** Defaults for `run` steps. */
      run?: {
        /** The shell for `run` steps. */
        shell?: string;
        /** The working directory for `run` steps. */
        "working-directory"?: string;
      };
    };
    /** kiln: `{ app, environment, revision, wait, timeout }`: deploy after the job's steps (W15-R05). */
    deploy?: unknown;
    /** Environment variables for every step. */
    env?: {
      [key: string]: string | number | boolean | null;
    };
    /** kiln: import jobs' exported environment (W04-R21). */
    "env-from"?: string | string[];
    /** The environment the job deploys to. */
    environment?: string | {
      /** `false`: use the environment's rules and secrets without recording a deployment. */
      deployment?: boolean | Expr;
      /** The environment's name. */
      name?: string;
      /** The deployment's URL, shown on the run. */
      url?: string;
    };
    /** kiln: the leg has its runner to itself. */
    exclusive?: boolean;
    /** kiln: `{ env: [names] }` published to later jobs (W04-R21). */
    exports?: unknown;
    /** Run the job only when this is true. */
    if?: string;
    /** kiln: the image for container, microVM and Kubernetes legs. */
    image?: string;
    /** kiln: integrations the job authenticates to (W11-R03). */
    integrations?: string | string[];
    /** kiln v1: the job's matrix (`strategy.matrix`). */
    matrix?: unknown;
    /** The job's name. */
    name?: string;
    /** Jobs that must finish first. */
    needs?: string | unknown[];
    /** kiln: `{ egress: none | open | { allow } }` (W18-R08). */
    network?: unknown;
    /** Values later jobs read as `needs.<id>.outputs.<name>`. */
    outputs?: unknown;
    /** The `GITHUB_TOKEN`'s permissions. */
    permissions?: unknown;
    /** kiln: routing preferences (W05-R23). */
    prefer?: unknown;
    /** kiln: `high`, `normal`, `low` or 0–100 (W05-R09). */
    priority?: unknown;
    /** kiln: `enforce` (W16-R05). */
    quarantine?: string;
    /** kiln: registries the job logs in to (W12-R03). */
    registries?: string | string[];
    /** kiln: routing constraints (W05-R22). */
    requires?: string | string[];
    /** kiln: `cpu`, `memory`, `disk`, `gpu` (W05-R03). */
    resources?: unknown;
    /** kiln v1: retries after a lost runner. */
    retries?: number;
    /** kiln: the retry policy (W05-R14). */
    retry?: unknown;
    /** kiln: reuse an earlier result when the inputs are unchanged (W08-R10). */
    reuse?: unknown;
    /** Runner labels, or `{ group, labels }`. */
    "runs-on"?: string | string[] | {
      group?: string;
      labels?: string | string[];
    };
    /** Secrets for the reusable workflow, or `inherit`. */
    secrets?: unknown;
    /** Service containers beside the job. */
    services?: {
      [key: string]: string | {
        /** Registry credentials. */
        credentials?: {
          /** Registry password (use a secret). */
          password?: string;
          /** Registry user name. */
          username?: string;
        };
        /** Environment variables in the container. */
        env?: {
          [key: string]: string | number | boolean | null;
        };
        /** The image. */
        image?: string;
        /** Extra `docker create` options. */
        options?: string;
        /** Ports to expose. */
        ports?: unknown;
        /** Volumes to mount. */
        volumes?: unknown;
      };
    };
    /** GitHub custom images: the image to make from this job. */
    snapshot?: unknown;
    /** What to run, in order. */
    steps?: Step[];
    /** Matrix, fail-fast and max-parallel. */
    strategy?: {
      /** kiln: split a list into legs of N items (W04-R15). */
      chunk?: unknown;
      /** Cancel the other legs when one fails (default true). */
      "fail-fast"?: boolean | Expr;
      /** The matrix: axes, `include`, `exclude`, or an expression giving them. */
      matrix?: Expr | {
        /** kiln: platforms that may run under QEMU emulation (W04-R13). */
        emulate?: unknown;
        /** Combinations to remove. */
        exclude?: unknown;
        /** Extra combinations, or extra values on matching ones. */
        include?: unknown;
        /** kiln: OCI platforms as an axis, each leg on a runner of that platform (W04-R13). */
        platforms?: unknown;
        /** kiln: keep only the combinations for which this is true (W04-R11). */
        where?: unknown;
      } & {
        [key: string]: unknown[] | Expr;
      };
      /** At most this many legs at once. */
      "max-parallel"?: number | Expr;
      /** kiln: split work items into balanced legs (W04-R14). */
      shard?: unknown;
      /** kiln: a condition each leg checks when it is ready (W04-R12). */
      when?: string;
    };
    /** kiln: `discover`: after its steps, the test reports it wrote where tools usually write them (W16-R01). */
    "test-results"?: unknown;
    /** Cancel the job after this long (default 360). */
    "timeout-minutes"?: number | Expr;
    /** kiln: a preset image and caches (`rust`, `go`, `python`, `node`, `bun`, `erlang`). */
    toolchain?: string;
    /** Call a reusable workflow. */
    uses?: string;
    /** Inputs for the reusable workflow. */
    with?: unknown;
    /** kiln: doing the least work (W24): `memo` takes an earlier result when the inputs are unchanged, as `reuse:` (W24-R06); `warm: [folders]` or `warm: { paths, budget }` keeps the job's workspace on its runner for its next leg, those folders kept (W24-R11). */
    work?: unknown;
  };
  /** A step. */
  export type Step = {
    /** Run the step in the background: the job goes on, and a later `wait` or `wait-all` collects it (at most 10 at once). */
    background?: boolean | Expr;
    /** Stop these background steps (SIGTERM, then SIGKILL after a grace period). */
    cancel?: string | string[];
    /** A failure doesn't fail the job. */
    "continue-on-error"?: boolean | Expr;
    /** Environment variables for the step. */
    env?: {
      [key: string]: string | number | boolean | null;
    };
    /** The step's id, for `steps.<id>`. */
    id?: string;
    /** Run the step only when this is true. */
    if?: string;
    /** kiln: integrations this step authenticates to (W11-R03). */
    integrations?: string | string[];
    /** kiln: reuse the step's earlier success while its inputs are unchanged: `check` (nothing to restore), or `{ inputs, outputs, env, max-age, platform, tools }` (W24-R07, W28-R70). */
    memo?: string | {
      /** Variables the step depends on besides those the workflow declares. */
      env?: string | string[];
      /** Paths and globs whose git trees the step depends on (the whole tree when none). */
      inputs?: string | string[];
      /** How long a result stays reusable (`14d` by default). */
      "max-age"?: string;
      /** Paths in the workspace the step leaves for later steps: kept, and put in place when a later step runs. */
      outputs?: string | string[];
      /** The platform facets the key keeps (`[os, arch]`; `any` for none): the step keys by what it declares only, so pipelines share it (W28-R70). */
      platform?: unknown;
      /** `hmac`: a step that receives secrets keyed by their HMAC (W24-R05). */
      secrets?: string;
      /** Toolchains whose identity the key keeps, each at a precision: `{ node: major, rustc: full }` (W28-R71). */
      tools?: unknown;
    };
    /** Its name in the log. */
    name?: string;
    /** Steps run as background steps, with an implicit wait at the end. */
    parallel?: unknown;
    /** kiln: retry this step (W05-R14). */
    retry?: unknown;
    /** Commands to run. */
    run?: string;
    /** The shell for `run`: `bash`, `sh`, `pwsh`, `powershell`, `cmd`, `python` or a template with `{0}`. */
    shell?: string;
    /** kiln: test reports this step writes (W16-R01). */
    "test-results"?: unknown;
    /** kiln: run a build's test binaries, those whose binary or what they read changed, and replay the others' recorded passes: `{ cargo: [args] }` (W24-R10). */
    tests?: unknown;
    /** Fail the step after this long. */
    "timeout-minutes"?: number | Expr;
    /** kiln v1: run under a pseudo-terminal. */
    tty?: boolean;
    /** An action: `owner/repo@ref`, `owner/repo/path@ref`, `./path` or `docker://image`. */
    uses?: string;
    /** Wait for these background steps (by id); their outputs and environment apply from here. */
    wait?: string | string[];
    /** Wait for every earlier background step. */
    "wait-all"?: unknown;
    /** The action's inputs. */
    with?: unknown;
    /** kiln: map a kiln action's declared secrets to this job's (W09-R07). */
    "with-secrets"?: {
      [key: string]: string | number | boolean | null;
    };
    /** Where `run` runs. */
    "working-directory"?: string;
  };
  /** What starts the workflow. */
  export type On = "branch_protection_rule" | "check_run" | "check_suite" | "create" | "delete" | "deployment" | "deployment_status" | "discussion" | "discussion_comment" | "fork" | "gollum" | "image_version" | "issue_comment" | "issues" | "label" | "merge_group" | "milestone" | "page_build" | "project" | "project_card" | "project_column" | "public" | "pull_request" | "pull_request_review" | "pull_request_review_comment" | "pull_request_target" | "push" | "registry_package" | "release" | "repository_dispatch" | "schedule" | "status" | "watch" | "workflow_call" | "workflow_dispatch" | "workflow_run" | "diff" | "land" | "review" | "comment" | "pipeline" | "agent" | "deploy" | "webhook" | "manual" | "tag" | ("branch_protection_rule" | "check_run" | "check_suite" | "create" | "delete" | "deployment" | "deployment_status" | "discussion" | "discussion_comment" | "fork" | "gollum" | "image_version" | "issue_comment" | "issues" | "label" | "merge_group" | "milestone" | "page_build" | "project" | "project_card" | "project_column" | "public" | "pull_request" | "pull_request_review" | "pull_request_review_comment" | "pull_request_target" | "push" | "registry_package" | "release" | "repository_dispatch" | "schedule" | "status" | "watch" | "workflow_call" | "workflow_dispatch" | "workflow_run" | "diff" | "land" | "review" | "comment" | "pipeline" | "agent" | "deploy" | "webhook" | "manual" | "tag")[] | {
    agent?: null | {
      /** kiln `agent`: agents that may trigger (or `*`). */
      agents?: string | string[];
      /** Inputs (`workflow_dispatch`, `workflow_call`). */
      inputs?: unknown;
    };
    branch_protection_rule?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    check_run?: null | {
      /** Activity types that trigger the workflow. */
      types?: "created" | "rerequested" | "completed" | "requested_action" | ("created" | "rerequested" | "completed" | "requested_action")[];
    };
    check_suite?: null | {
      /** Activity types that trigger the workflow. */
      types?: "completed" | "completed"[];
    };
    comment?: null | {
      /** kiln `comment`: slash commands. */
      commands?: string | string[];
      /** kiln `comment`: names mentioned. */
      mentions?: string | string[];
    };
    create?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    delete?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    deploy?: null | {
      /** kiln `deploy`: the app. */
      app?: string;
      /** kiln `deploy`: the environment. */
      environment?: string;
      /** kiln `review` and `deploy`: the states that trigger. */
      states?: string | string[];
    };
    deployment?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    deployment_status?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    diff?: null | {
      /** kiln `diff`: `human`, `agent`. */
      authors?: string | string[];
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** kiln `diff`: also draft diffs. */
      drafts?: boolean;
      /** Path patterns at least one changed file must match. */
      paths?: string | string[];
      /** Path patterns changed files may all match without triggering. */
      "paths-ignore"?: string | string[];
    };
    discussion?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    discussion_comment?: null | {
      /** Activity types that trigger the workflow. */
      types?: "created" | "edited" | "deleted" | ("created" | "edited" | "deleted")[];
    };
    fork?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    gollum?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    image_version?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    issue_comment?: null | {
      /** Activity types that trigger the workflow. */
      types?: "created" | "edited" | "deleted" | ("created" | "edited" | "deleted")[];
    };
    issues?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    label?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    land?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
    };
    manual?: null | {
      /** Inputs (`workflow_dispatch`, `workflow_call`). */
      inputs?: unknown;
    };
    merge_group?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** Branch patterns that don't. */
      "branches-ignore"?: string | string[];
      /** Activity types that trigger the workflow. */
      types?: "checks_requested" | "checks_requested"[];
    };
    milestone?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    page_build?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    pipeline?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** kiln `pipeline`: conclusions that trigger. */
      conclusion?: string | string[];
      /** kiln `pipeline`: the workflow's name or path. */
      name?: string;
    };
    project?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    project_card?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    project_column?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    public?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    pull_request?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** Branch patterns that don't. */
      "branches-ignore"?: string | string[];
      /** Path patterns at least one changed file must match. */
      paths?: string | string[];
      /** Path patterns changed files may all match without triggering. */
      "paths-ignore"?: string | string[];
      /** Activity types that trigger the workflow. */
      types?: "assigned" | "unassigned" | "labeled" | "unlabeled" | "opened" | "edited" | "closed" | "reopened" | "synchronize" | "converted_to_draft" | "locked" | "unlocked" | "enqueued" | "dequeued" | "milestoned" | "demilestoned" | "ready_for_review" | "review_requested" | "review_request_removed" | "auto_merge_enabled" | "auto_merge_disabled" | ("assigned" | "unassigned" | "labeled" | "unlabeled" | "opened" | "edited" | "closed" | "reopened" | "synchronize" | "converted_to_draft" | "locked" | "unlocked" | "enqueued" | "dequeued" | "milestoned" | "demilestoned" | "ready_for_review" | "review_requested" | "review_request_removed" | "auto_merge_enabled" | "auto_merge_disabled")[];
    };
    pull_request_review?: null | {
      /** Activity types that trigger the workflow. */
      types?: "submitted" | "edited" | "dismissed" | ("submitted" | "edited" | "dismissed")[];
    };
    pull_request_review_comment?: null | {
      /** Activity types that trigger the workflow. */
      types?: "created" | "edited" | "deleted" | ("created" | "edited" | "deleted")[];
    };
    pull_request_target?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** Branch patterns that don't. */
      "branches-ignore"?: string | string[];
      /** Path patterns at least one changed file must match. */
      paths?: string | string[];
      /** Path patterns changed files may all match without triggering. */
      "paths-ignore"?: string | string[];
      /** Activity types that trigger the workflow. */
      types?: "assigned" | "unassigned" | "labeled" | "unlabeled" | "opened" | "edited" | "closed" | "reopened" | "synchronize" | "converted_to_draft" | "locked" | "unlocked" | "enqueued" | "dequeued" | "milestoned" | "demilestoned" | "ready_for_review" | "review_requested" | "review_request_removed" | "auto_merge_enabled" | "auto_merge_disabled" | ("assigned" | "unassigned" | "labeled" | "unlabeled" | "opened" | "edited" | "closed" | "reopened" | "synchronize" | "converted_to_draft" | "locked" | "unlocked" | "enqueued" | "dequeued" | "milestoned" | "demilestoned" | "ready_for_review" | "review_requested" | "review_request_removed" | "auto_merge_enabled" | "auto_merge_disabled")[];
    };
    push?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** Branch patterns that don't. */
      "branches-ignore"?: string | string[];
      /** Path patterns at least one changed file must match. */
      paths?: string | string[];
      /** Path patterns changed files may all match without triggering. */
      "paths-ignore"?: string | string[];
      /** Tag patterns that trigger it. */
      tags?: string | string[];
      /** Tag patterns that don't. */
      "tags-ignore"?: string | string[];
    };
    registry_package?: null | {
      /** Activity types that trigger the workflow. */
      types?: "published" | "updated" | ("published" | "updated")[];
    };
    release?: null | {
      /** Activity types that trigger the workflow. */
      types?: "published" | "unpublished" | "created" | "edited" | "deleted" | "prereleased" | "released" | ("published" | "unpublished" | "created" | "edited" | "deleted" | "prereleased" | "released")[];
    };
    repository_dispatch?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    review?: null | {
      /** kiln `review` and `deploy`: the states that trigger. */
      states?: string | string[];
    };
    schedule?: {
      cron: string;
      [key: string]: unknown;
    }[];
    status?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    tag?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
    };
    watch?: null | {
      /** Activity types that trigger the workflow. */
      types?: string | string[];
    };
    webhook?: null | {
      /** kiln `webhook`: a condition over the payload. */
      filter?: string;
      /** kiln `webhook`: the inbound hook's id. */
      hook?: string;
    };
    workflow_call?: null | {
      /** Inputs (`workflow_dispatch`, `workflow_call`). */
      inputs?: unknown;
      /** Outputs (`workflow_call`). */
      outputs?: unknown;
      /** Secrets (`workflow_call`). */
      secrets?: unknown;
    };
    workflow_dispatch?: null | {
      /** Inputs (`workflow_dispatch`, `workflow_call`). */
      inputs?: unknown;
    };
    workflow_run?: null | {
      /** Branch patterns that trigger it. */
      branches?: string | string[];
      /** Branch patterns that don't. */
      "branches-ignore"?: string | string[];
      /** Activity types that trigger the workflow. */
      types?: "completed" | "requested" | "in_progress" | ("completed" | "requested" | "in_progress")[];
      /** Workflows whose runs trigger this one (`workflow_run`). */
      workflows?: string | string[];
    };
  };
  /** A W01 expression: text holding `${{ … }}`, evaluated when the job runs. */
  export type Expr = `${string}\${{${string}`;
  /** An event's payload, as GitHub's webhooks send it. */
  export interface Event {
    readonly [key: string]: any;
  }
  /** The run's event, as `github.event`: the payload of what started it. */
  export const event: Event;
  /** The commit's tree, read-only. */
  export const tree: {
    /** A file's text at the commit, or null when it has none at `path`. */
    read(path: string): string | null;
    /** The entries of `dir`, from the tree's root, a directory's ending in `/`. */
    list(dir: string): string[];
  };
  /** The workflow, its line kept for diagnostics. */
  export function workflow(w: Workflow): Workflow;
  /** A job, its line kept for diagnostics. */
  export function job(j: Job): Job;
  /** A step, its line kept for diagnostics. */
  export function step(s: Step): Step;
  /** A code workflow's default export: a workflow, or a function of the event that returns one. */
  export type Program = Workflow | ((event: Event) => Workflow);
}
