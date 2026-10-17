# Repo sync

> **Design, being built.** Phase 1 (the engine) is in progress; nothing here ships yet.

A repository, or any folder, kept on several machines stays one. A change on any machine reaches
the others within seconds. Two machines changing the same file never lose either change. No ports
beyond the ones kiln already uses. Git is one option, not a requirement.

## Model

- A **synced repo** has a name, its **copies** (a machine and a path on it), a **mode** and its
  **exclusions**.
- Each synced repo is one **Raft group** (hyper-raft's core in hyper-durable's shell, its log on
  hyper-log). The log is the order of the repo's changes; every copy applies the same changes in
  the same order, so every copy converges on the same files.
- **Members:** the server is a voter; each machine with a copy joins as a **learner**, which
  receives the log and proposes through the leader. When machines become fleet members
  ([fleet.md](fleet.md)), they are promoted to voters (a configuration change), and the group no
  longer needs the server.
- **No new ports:** the group's messages travel on the links kiln already has (a runner's QUIC
  link to the server, or its HTTP tunnel when UDP is blocked), relayed by the server.

## A change

A log entry is one **step**: the changes a machine saw together, debounced so that a save of many
files is one step:

| field | |
| --- | --- |
| `by` | the machine |
| `at` | unix milliseconds |
| `changes[]` | `{path, kind: "write" | "delete" | "link", hash?, size?, exec?, target?, base?}` |

- `path` is relative to the repo, `/`-separated, never `..` or absolute, never inside `.git/`
  (except in the *everything* mode).
- `hash` is the SHA-256 of the new contents; `base` is the hash the machine had before its change
  (none: the file was new to it).
- **Contents are not in the log.** They are blobs, addressed by hash, fetched from any member that
  holds them. The server keeps a blob until every copy has applied the step that names it.

## Applying, and conflicts

Every copy applies committed steps in log order. For each change:

- If the copy's current file is the change's `base` (or absent when `base` is none), the change
  is applied: written to a temporary file in the same folder and renamed over the old one (or the
  file is deleted), its exec bit set.
- Otherwise the file changed here in the meantime: a **conflict**. The rule is the same on every
  copy, so every copy converges on the same files:
  - **The first step to commit keeps the name.**
  - A later step whose `base` no longer matches is written beside it as
    `<name>.conflict-<machine>-<YYYYMMDD-HHMMSS><.ext>`, and listed in the UI.
  - On the machine that made the later change, its own version is the one that moves to the
    conflict copy, once its step commits. If it was edited again in the meantime, that edit is
    proposed anew, based on what won.
- Conflicts are resolved in the UI: keep this one, keep that one, or keep both. A resolution is
  itself a step, so every copy follows it.

## Modes

| mode | what travels | for |
| --- | --- | --- |
| **working tree** (default) | every file but `.git/`, what `.gitignore` ignores and kiln's list (`node_modules/`, `target/`, `.venv/`, ...); uncommitted work included | repos and plain folders |
| **+ git refs** (default on for git repos) | commits and branches too: fetched by the other copies as git bundles over kiln's links; a branch moves on a copy whose working tree already matches it, so `git status` agrees everywhere | git repos |
| **git only** | commits and branches; working trees stay each machine's own | copies that shouldn't see work in progress |
| **everything** (expert) | `.git/` included; one writer at a time | mirrors |

## Catching up

A copy that was away catches up from the log, or from a snapshot of the repo's index when it is
far behind, then fetches the blobs it lacks. Changes it made while away are proposed when it is
back, with the `base` it had, and go through the conflict rule.

## Limits

- Files over a size limit (256 MiB by default) are listed as skipped, not synced.
- Symbolic links travel as links; permissions as the exec bit only.
- A copy's path must exist and be writable by the runner's user.

## The group (phase 2)

- **Roles.** Each synced repo is one hyper-raft group. The server's member is its voter (and so its
  leader); each copy's machine runs a member as a **learner**. Members are hyper-durable `Replica`s:
  the server's log on hyper-log (`GroupStore`), a runner's in its state folder.
- **The state machine** is the repo's index as the log makes it: path → version (the `log` of a
  copy's state). Applying an entry updates it, a pure function of the entry, the same on every
  member; images (snapshots) are that index. Writing files is not the state machine's job: a copy
  applies committed steps to its folder after the machine has taken them, once their blobs are
  there, so a missing blob delays a copy's files, never the group.
- **Proposals.** A runner sends its step to the server, which proposes it (a learner doesn't
  propose). A step is proposed only once its blobs are on the server.
- **Messages.** One kiln message each way, appended last to the wire enums (`ServerMsg::Sync`,
  `AgentMsg::Sync`), sent only to runners announcing `CAP_SYNC` (64). Its payload is
  `{repo, kind, ...}`:
  - `raft`: a hyper-raft message, between the server's member and a runner's;
  - `copies` (server → runner): the copies this machine holds: repo, path, mode, limits;
  - `step` (runner → server): a step to propose;
  - `blob`, `want` (both ways): contents, in chunks on the bulk lane, and asking for them;
  - `status` (runner → server): applied index, pending steps, conflicts, skipped files.
- **Blobs.** The server keeps every blob a step names until each copy has applied it, then drops
  it. A copy asks the server for what it lacks before applying a step.

## API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/sync` | synced repos: name, mode, copies (machine, path, state, applied, conflicts) |
| POST | `/api/sync` | `{name, mode?, copies: [{machine, path}]}`: start syncing |
| POST | `/api/sync/:id/copies` | `{machine, path}`: add a copy (it catches up from the log) |
| DELETE | `/api/sync/:id/copies/:machine` | stop syncing there (its files stay) |
| GET | `/api/sync/:id/conflicts` | conflict copies, with both versions |
| POST | `/api/sync/:id/conflicts/resolve` | `{path, keep: "name" | "copy" | "both"}` |
| DELETE | `/api/sync/:id` | stop syncing (every copy's files stay) |

## Phases

1. **Engine:** scanning a folder into an index with exclusions, detecting changes (polling size,
   mtime and hash; inotify and FSEvents later), the blob store, and applying steps with the conflict
   rule. Tested on its own.
2. **Group:** a hyper-durable replica per synced repo; the server as voter, copies as learners;
   relay over kiln's links; API and UI.
3. **Git refs.**
4. **Voters on machines**, with fleet membership: no server needed.
