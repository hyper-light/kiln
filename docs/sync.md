# Repo sync

> **Phases 1 and 2 are built:** the engine, each repo's Raft group, the server and runner sides,
> the API and the synced folders on the Repositories page. So are [repositories on machines](#repositories-on-machines):
> what each machine holds, and cloning, syncing and moving a repository between machines. Git refs
> (phase 3) and voters on machines (phase 4) are not yet.

## Using it

**Repositories** in the console's menu: *Sync a folder*, name it, and pick a machine and a folder for each
copy. A folder that doesn't exist yet is made; one that holds files already keeps them, and they
meet the others' by the rule below (a file the same on both is no conflict). Each copy shows how it
is doing (*in step*, *catching up*, *fetching*, *sending*, *offline*, *error*) and what it left out;
conflicts are listed with what to keep. Removing a copy, or the whole repo, leaves the files where
they are.

A machine syncs through its runner (`kiln agent`), version with sync (`CAP_SYNC`); an older runner
shows its copies *offline* until it is upgraded. One runner process of a machine syncs at a time
(it holds `<state>/sync/lock`); during an in-place upgrade the new process takes over.

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

## Repositories on machines

The Repositories page lists every repository kiln knows of once: the ones it builds, the synced
ones, and every git repository a machine holds, each with the machines it is on. It filters by kind,
machine, OS, architecture, machine labels and state (each option counting what it would show) and
sorts by name, machines, activity, latest run, last poll or age, as the agents list does. A
repository opens under its row: where it is (each folder: a synced copy, kiln's clone, or one that
was there; its branch, commit, what isn't committed or pushed, how it syncs), the tasks that put it
there, and forms to put it on more machines, move it, or give a folder up.

**Where repositories are.** Each runner looks at its machine's git repositories when it connects,
every five minutes and when asked (*Look again*): home one level deep, two levels deep under its
usual folders (`~/Projects`, `~/src`, `~/code`, `~/dev`, `~/Developer`, `~/work`, `~/repos`, `~/git`,
`~/workspace`, `~/go/src`), the clones kiln made there (`<state>/sync/clones.json`) and its synced
copies, wherever they are. Hidden folders, symlinks, dependencies and build output are not looked
in, nor is a repository's inside; a look reports 400 repositories at the most. Git reports each one
without taking a lock a person's git would trip over (`--no-optional-locks`): its remote (never its
credentials: `https://token@host/…` is sent as `https://host/…`), branch, commit, upstream with how
far ahead and behind, and how many tracked files changed. The server keeps what each machine last
said (`<data>/sync/places.json`), so an offline machine's repositories still show.

A repository is known by its **key**, its remote without scheme, user, credentials or `.git`
(`github.com/hyper-light/kiln`): a clone over ssh and one over https are one repository, and so are
the repository kiln builds and the clones of it. A synced repo joins the repository kiln builds of
its name, else the one its copies' remote names. One with no remote is a place of its own.

**Putting a repository on machines.** Any machines at once, picked one by one or by OS,
architecture or label, each with a folder of its own: a whole path, or `~/…`, which is where that
machine says its home is (`C:\Users\…` on Windows). It defaults to where the source is under its
home (`~/Projects/kiln` stays `~/Projects/kiln`). A folder must be absent or empty: kiln never clones
over files.

- **Clone with git**: each machine gets a clone of its own, its commits and branches, from kiln's
  mirror of a repository it builds (a clean checkout of a branch), from a machine that has it, or
  from its remote (each machine fetching it itself). Work not committed stays where it is.
- **Keep in sync**: each new folder is cloned, then joins the repo's sync group as a folder that
  **adopts** the group's files: it proposes nothing until the log has brought it through the index
  it joined at, the log's version of each file replaces its own, and what it holds that the group
  doesn't (a file the source deleted and didn't commit) goes, unless it changed there meanwhile. A
  clone of the source's commit becomes the source's working tree, uncommitted work and all, with no
  conflict copies; from then on it is a copy like any other. A repository not synced yet starts its
  group from the source folder, once that copy has looked at its folder and is in step (cloned from
  kiln's mirror, the first new folder starts it). Only a folder the task itself cloned adopts: one
  kiln made before joins as it is, so nothing changed in it is lost.
- **Move** (a folder to another machine, or another folder): as a sync, to one place. Once the new
  folder is in step, and the old one too (nothing of it still on its way), the old copy leaves the
  group; a folder that wasn't synced gets a group for the move alone, which stops once it is done.
  Every branch of the source becomes a branch of the new clone. With *delete the old folder*, the
  old machine deletes it last.
- **Remove** a place: a copy stops syncing there, kiln forgets its clone, and, if asked, the folder
  goes.

Nothing travels but over kiln's links, the ports it already uses. Commits go as a git bundle (the
branches, tags and `HEAD`; not remote-tracking branches, not a mirror's pull requests): kiln makes
it from its mirror, or the source machine makes it and sends it to the server (`PUT
/api/sync/bundles/:batch`, hashed as it comes); each machine fetches it from the server (`GET`,
checked against its SHA-256) and clones it, then points `origin` at the repository's real remote.
Working trees go through the group, as any sync does. The bundle is deleted when its batch is done.

**Deleting a folder** is the runner's call: only a real folder (no symlink) at least two levels
below the root, never home, never one that holds kiln's state or its binary, never a folder still
synced there, and, when the repository has a remote, only if its `origin` is still that remote.

**Tasks.** Each machine's part is a task, the tasks started together a batch, driven by the server
(`crate::sync::tasks`, kept in `<data>/sync/tasks.json`): *queued*, *bundling*, *fetching*,
*cloning*, *joining*, *catching up*, *leaving*, *deleting*, then *done* or *failed*. The runner says
how its part goes as it goes (progress while bytes move, and again every few seconds while it works,
and after a reconnect); a machine silent for three minutes, or away for ten, fails its task. A task
the server's restart cut short fails too. *Start it again* finds what is done already (kiln's clone
in the folder, the copy in the group) and carries on from there.

**Messages**, inside `Sync` on the sync lane, to runners announcing `CAP_REPOS` (`1 << 18`); a side
that doesn't know a kind skips it: `repos` (runner → server: what it holds), `look`, `bundle`
(`{task, path}`), `clone` (`{task, key, path, bundle, hash, size, url, branch, origin,
all_branches}`), `delete` (`{task, path, origin}`), `forget` (`{path}`) and `task` (runner → server:
`{task, state, detail, error, hash, size, branch, head}`). A copy's spec carries `adopt` (the index
it adopts the group's files through) and its status `looked` and `adopting`.

**Not carried**: what is staged arrives unstaged; stashes, hooks and a repository's own config stay;
commits made after a sync joined move between copies with `git` (fetch, pull) until git refs
(phase 3) carry them. Line endings a Windows checkout converts are a difference like any other.

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
| GET | `/api/places` | every repository with its places, the machines (home, what their runners can do), the tasks |
| POST | `/api/places/clone` | `{key, from, branch?, to: [{machine, path}]}`: clone it to those machines (`from`: `{kind: "kiln", repo}`, `{kind: "machine", machine, path}` or `{kind: "url", url}`) |
| POST | `/api/places/sync` | the same: clone, then keep in sync |
| POST | `/api/places/move` | `{key, from: {kind: "machine", …}, to: [{machine, path}], delete?}` |
| POST | `/api/places/remove` | `{key, from: {kind: "machine", …}, delete?}` |
| POST | `/api/places/look` | every machine looks at its repositories again |
| POST | `/api/places/tasks/:id/retry` | start a failed task again |
| DELETE | `/api/places/tasks/:id` | forget a task that is over |
| GET, PUT | `/api/sync/bundles/:batch?machine=` | a batch's bundle, to and from the machines it names (their sync token) |

## As built

- **Server:** one thread (`sync::server`) leads every repo's group on one hyper-log file
  (`<data>/sync/sync.log`), keeps each repo's tree beside it (`<id>.tree.json`) and its contents in
  `<data>/sync/blobs`, and the list of repos and copies in `repos.json`. It takes a copy's steps in
  that copy's order, once their contents are on the server, and adds or removes one learner at a
  time to match the copies. It compacts each log well within the room hyper-log keeps for a group,
  leaving about 2 MiB of entries for copies a little behind; one further behind takes an image.
- **Runner:** one thread (`sync::runner`) holds `<state>/sync`: the machine's own hyper-log file, a
  learner and a saved state per copy, and a content store. Every two seconds it looks at each folder
  that has taken everything committed, sends new contents over HTTP, then the steps; it fetches the
  contents committed steps name, writes them, and states durable only what the folder took.
- **Links:** Raft messages ride `ServerMsg::Sync`/`AgentMsg::Sync` on a lane of their own, an image
  in parts of 512 KiB. Contents go over `GET`/`PUT /api/sync/blobs/:hash?machine=`, with a token the
  server signs for each machine and gives it with its copies.
- **Tested** on a scratch server and two runners: a first sync (a 5 MB file, the exec bit, a link,
  `node_modules` left out), edits and deletes both ways, a conflict and its resolution, a runner and
  the server restarted mid-way, 30,000 files in 3 s, three rounds of deleting and making them again,
  a copy added after compaction (an image), and a copy removed.

## Phases

1. **Engine** (built): scanning a folder into an index with exclusions, detecting changes (polling
   size, mtime and hash; inotify and FSEvents later), the blob store, and applying steps with the
   conflict rule.
2. **Group** (built): a hyper-durable replica per synced repo; the server as voter, copies as
   learners; relay over kiln's links; API and UI.
3. **Git refs.**
4. **Voters on machines**, with fleet membership: no server needed.
