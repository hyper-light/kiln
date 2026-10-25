---
name: kiln-ci-migrate
description: Move a repository's CI to kiln from GitHub Actions, GitLab CI/CD, Bitbucket Pipelines, Jenkins or CircleCI, step by step, running both side by side until kiln's results match. Use when asked to migrate CI, to port a .gitlab-ci.yml, bitbucket-pipelines.yml, Jenkinsfile or .circleci/config.yml, or to make a GitHub workflow run on kiln's runners.
---

# Migrating CI to kiln

Migrate in steps a person can stop at, never all at once:

1. **Inventory** what runs today: every pipeline file, its triggers, the secrets and variables it
   reads, the runners or images it needs, what it publishes or deploys.
2. **Port** one workflow, validate it and preview it (the kiln-ci-author loop).
3. **Run both** side by side on the same commits, kiln's not deploying anything yet.
4. **Compare** results for a while: the same jobs pass and fail, artifacts match, times are
   known. `kiln.ci.runs.compare` explains differences between kiln runs.
5. **Switch** deployments and required checks over, then retire the old pipeline. Steps 3 to 5
   are a person's decisions: propose them, with the evidence.

kiln also runs other systems as providers (a repository's CI can stay GitHub Actions, GitLab
CI/CD or Bitbucket Pipelines, shown and driven through kiln) when a move isn't wanted yet.

## From GitHub Actions

Most workflows run unchanged in `.github/workflows`: kiln reads GitHub's dialect strictly and runs
its actions, contexts and expressions as GitHub does. What to look at:

- **Labels.** `runs-on: ubuntu-latest` waits for a runner with that label unless the server maps
  GitHub's labels by routing rules. `kiln.ci.routing.explain({ repo, runs_on: "ubuntu-latest" })`
  says where it would go; map labels in an overlay (`.kiln/overlays/<stem>.yaml` may replace
  `runs-on`) to keep the file working on GitHub too.
- **Secrets and variables**: set them at kiln's scopes (kiln-ci-secrets); names stay the same.
- **OIDC**: a cloud that trusts GitHub's issuer must trust kiln's for the same roles (a second
  identity provider and trust condition), with the same subject format.
- **`GITHUB_TOKEN`**: kiln's job token, scoped by `permissions` as GitHub's; it acts on kiln, and
  on GitHub only through a configured app.
- **`.kiln` beside `.github`**: once a commit has a kiln-dialect workflow, GitHub's workflows stop
  running on kiln unless the repository's `github_workflows` setting is `on`. Port by overlay
  first; move to `.kiln/` only when GitHub needn't run it.

## From GitLab CI/CD (`.gitlab-ci.yml`)

| GitLab | kiln (`.kiln/ci.yaml`) |
| --- | --- |
| `stages` and job order | `needs:` between jobs (stages are only order) |
| `script`, `before_script`, `after_script` | steps' `run:`; after-scripts as a last step with `if: always()` |
| `image`, `services` | `container:`, `services:` |
| `rules`, `only`/`except` | `on:` (branches, paths, tags) and the job's `if:` |
| `variables` | `env:`; secret ones as secrets |
| `parallel: matrix` | `strategy.matrix` |
| `parallel: N` | `strategy.chunk` or `shard` |
| `cache` | `cache: [{ key, paths }]` |
| `artifacts` | `actions/upload-artifact` and `download-artifact`, or `kiln/upload@v1` |
| `environment`, `when: manual` | `environment:` with reviewers (an approval), or `workflow_dispatch` |
| `include`, `extends` | reusable workflows (`uses: ./.kiln/templates/x.yaml`) and YAML anchors |
| `tags` (runner) | `runs-on:` labels or `requires:` |

## From Bitbucket Pipelines (`bitbucket-pipelines.yml`)

`pipelines.default` and `branches` become `on: push` (with `branches`), `pull-requests` `on: diff`
(and `pull_request`); each step a job (`image:` as `container:`, `script:` as `run:` steps);
`parallel:` steps are jobs without `needs` between them; `caches` and `artifacts` as above;
`deployment:` an `environment:`.

## From Jenkins and CircleCI

Jenkins' `stages` are jobs chained by `needs`, `agent { label }` is `runs-on`, `post { always }`
a step with `if: always()`, credentials are secrets or integrations, `parallel` is jobs or a
matrix. CircleCI's `workflows` are `needs` between jobs, `executors` and `docker:` images are
`container:`, `orbs` are actions (or steps written out), `save_cache`/`restore_cache` are `cache:`.

## Proving it

For each ported workflow, report: the file, its diagnostics (none with errors), the preview's
legs against the old pipeline's jobs, and the first runs' results side by side with the old
system's. Leave the old pipeline in place until a person retires it.
