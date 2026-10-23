---
name: kiln-ci-secrets
description: Give kiln CI jobs what they need to reach other systems - secrets and variables at the right scope, integrations, and OpenID Connect trust with clouds and services instead of stored keys. Use when a job needs a credential, a secret is missing or unused, a cloud refuses kiln's token, or setting up AWS, Google Cloud, Azure, Vault or another provider for CI.
---

# Secrets, variables, integrations and OIDC

Prefer, in this order: **federation** (the job's kiln ID token exchanged for a short-lived
credential, nothing stored), a short-lived token minted by an integration, and only then a stored
secret. Never put a credential in a workflow, a log, a diff or a comment.

## Scopes

A secret or variable lives at one scope; a leg sees the narrowest of each name:

| Scope | Written |
| --- | --- |
| an environment | `env:acme/app:production` (only after its reviewers approve the deployment) |
| a workflow | `workflow:acme/app:.github/workflows/release.yml` |
| a repository | `repo:acme/app` |
| an organization | `org:acme` |
| the server | `server` |

```js
// Names and last use only: a value is never shown again once set.
const s = await kiln.ci.secrets.list({ scope: "repo:" + input.repo });
return s.secrets.map(x => ({ name: x.name, set: x.updated_at, last_used: x.last_used_at, from: x.from }));
```

Setting one needs `secrets.manage` there (`kiln.ci.secrets.set({ scope, name, value })`); a
variable (not secret, shown in logs) is `kiln.ci.variables.set`. Ask a person for the value: don't
invent, copy or move secrets between scopes yourself.

## How a leg gets them

- Only the secrets its expressions read (`${{ secrets.NAME }}`, a reusable workflow's
  `secrets: inherit` passes on what the caller has). Everything given is masked in logs.
- A workflow declares what it needs (`secrets: { DEPLOY_KEY: { required: true } }`, kiln dialect);
  a run without a required one fails at start with `KCI4040`, naming the scopes it looked in.
- Runs from forks (untrusted) get none, nor integrations.
- A person's grants decide too: `secrets.use:<NAME>` (developers use any by default; a deny takes
  one away). A run is held to whoever started it, and a leg says when a secret was withheld from
  them, and why.

A secret that's missing at run time is a scope or a name, not a code problem: list the scopes,
compare names (they're kept upper-case), and say which is missing where.

## Integrations

An integration is a configured service (`kiln.ci.integrations.list()`): AWS, Google Cloud,
Azure, container and package registries, Cloudflare, Vercel, chat and paging channels, an
OpenTelemetry receiver. A job names the ones it uses:

```yaml
jobs:
  deploy:
    permissions: { id-token: write, contents: read }   # federating kinds need the ID token
    integrations: [aws-prod]                           # its variables and files for every step
    steps:
      - run: aws sts get-caller-identity
```

or a step uses `kiln/auth@v1` with `integration:`, or runs `kiln auth <name> --print-env`. Check
one works as a repository's job would use it:

```js
return await kiln.ci.integrations.verify({ integration: "aws-prod", repository: input.repo, environment: "production" });
```

## OIDC: trusting kiln's tokens

Jobs with `permissions: { id-token: write }` get ID tokens from kiln's issuer
(`<kiln's public URL>/oidc`, discovery at `/.well-known/openid-configuration`), with GitHub's
claims (`repository`, `ref`, `sha`, `environment`, `job_workflow_ref`, …) and kiln's
(`kiln_trust`, `kiln_runner`, `kiln_diff`, …). The subject is GitHub's form:

- `repo:acme/app:environment:production` for a job with an environment;
- `repo:acme/app:pull_request` for pull requests;
- `repo:acme/app:ref:refs/heads/main` otherwise.

An organization or repository can choose its subject's claims (`kiln oidc subject --scope
repo:acme/app --keys repo,context,kiln_trust`). What each provider needs:

- **AWS**: an IAM OIDC identity provider for the issuer URL with audience `sts.amazonaws.com`, and
  a role whose trust policy allows `sts:AssumeRoleWithWebIdentity` from it with conditions on
  `aud` and `sub` (`StringEquals` for one branch or environment, `StringLike` with `repo:acme/app:*`
  for a repository).
- **Google Cloud**: a workload identity pool and an OIDC provider with the issuer, mapping
  `google.subject = assertion.sub` and `attribute.repository = assertion.repository`, a condition
  such as `assertion.repository == "acme/app"`, and the service account's
  `roles/iam.workloadIdentityUser` for that principal set.
- **Azure**: a federated credential on the app registration (or managed identity) with the issuer,
  the exact subject and audience `api://AzureADTokenExchange`.
- **HashiCorp Vault**: a JWT auth method with the issuer's discovery URL, and a role with
  `bound_audiences`, `bound_claims` (`repository`, `ref` or `environment`) and `user_claim: sub`.

Bind to the narrowest subject that works: a production role trusts the production environment's
subject, not the whole repository. When a cloud refuses the token, compare what it trusts with
what that job's token says (the issuer URL, the audience it asked for, and the subject the rules
above give for its repository, ref and environment): a different subject, audience or issuer URL
is almost always the cause. `kiln.ci.integrations.verify` with the job's repository and
environment makes the same exchange and says what the cloud answered.

## Unused and leaked

`secret.unused` notifications name secrets no leg read in 90 days: propose removing them rather
than leaving them. A secret that appeared somewhere it shouldn't is rotated by a person at its
source first; then the new value is set here.
