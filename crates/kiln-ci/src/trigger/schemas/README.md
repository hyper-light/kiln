# Vendored webhook schemas

`octokit-webhooks-7.6.1.json` is the part of `@octokit/webhooks-schemas` 7.6.1's `schema.json`
(https://github.com/octokit/webhooks) that kiln's payloads are checked against (docs/ci.md
W03-AC04): the definitions of `push`, `pull_request` (opened, synchronize, closed, reopened),
`workflow_dispatch`, `merge_group` (checks_requested), `workflow_run` (requested, completed) and
`release` (created, published), with every definition they refer to. GitHub has no webhook for
`schedule`, so there is no schema for it.

It is used under the MIT license, whose text and copyright notice are in
`LICENSE-octokit-webhooks` (the upstream repository's `LICENSE`).
