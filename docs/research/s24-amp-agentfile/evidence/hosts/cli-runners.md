---
lastModified: 2026-11-08
---

# Runners

You can start Amp threads remotely from [ampcode.com](https://ampcode.com) on any machine where you
can run `amp`.

<!-- prettier-ignore -->
<video controls autoplay muted loop playsinline width="1972" height="1080" src="https://static.ampcode.com/docs/agents-anywhere-demo-no-tui.mp4" type="video/mp4"></video>

To do that, you need an Amp instance that serves as a _runner_.

## Start a Runner

Run `amp --no-tui` to start a runner-only Amp instance. It waits for and runs remotely created
threads in the current directory without opening the TUI. To serve more than one directory, see
[Serve Multiple Directories](#serve-multiple-directories).

Pass `--runner-id <id>` to give it a stable runner ID:

```shell-session
$ amp --no-tui --runner-id grandmas-garage-server
```

Runner IDs must be valid hostnames and are case-insensitive. Amp preserves the casing you provide.

<a id="share-your-network"></a>

### Orb Network Routing

Many sites block cloud IPs. With Orb Network Routing, your orbs browse the web through your own
computer's connection.

```shell-session
$ amp --no-tui --runner-id home-mac --share-network
```

On a Mac, you can use the [app's switch](/docs/apps/mac#share-your-network) instead.
Then, in an orb thread's **•••** menu, pick `home-mac` under **Orb › Network Routing**. Keep the
runner running and its computer awake while orbs use it.

Workspace admins can turn Orb Network Routing off for every member under **Orbs › Orb Network
Routing** in the workspace settings. While it is off, orbs stop routing through runners, and
runners stop sharing their network.

To have the agent do this for you, add this to an `AGENTS.md`, or to your
[Global AGENTS.md](/docs/customize/agents-md) for every orb:

```markdown
Before using the web in an orb, run `amp net route home-mac`.
```

To access the runner's terminals from ampcode.com, pass `--remote-control-terminal`:

```shell-session
$ amp --no-tui --runner-id grandmas-garage-server --remote-control-terminal
```

### Portals on Your Runner

Start the runner with `amp --no-tui --remote-control-portals` to give its threads a Portal tab,
in any browser or the Amp iPhone and iPad apps. Use it to open the dev server Amp just started,
try the app it built, or show a preview to your team, straight from the thread. In the Mac app,
turn on [Allow Portals](/docs/apps/mac#allow-portals) instead.

Ask Amp in a thread on the runner to start your app, then open it from the thread's
[**Portal**](/docs/orbs/portals) tab.

You can open every portal on your runner, and so can every member of a workspace you
[share the runner with](#share-a-runner-with-your-workspace). Anyone who can write to a thread
on the runner sees its Portal tab. Runner portals cannot be made public, and their pages have no
[review button](/docs/orbs/portals#review-and-comment).

<a id="share-the-runners-desktop"></a>

### See and Control the Runner's Desktop

Start the runner with `amp --no-tui --desktop` to give its threads a Desktop tab, in any browser
or the Amp iPhone and iPad apps. Use it to try the app Amp just built, click through a dialog that
is holding up a thread, or watch Amp drive a native app and take over.

<!-- prettier-ignore -->
<video class="docs-media-light" controls loop muted playsinline preload="none" data-autoplay-in-view width="2560" height="1026" aria-label="The Amp macOS app with a thread beside its runner's shared Mac desktop. Clicking Open the iOS app in the transcript opens the Desktop tab, where the cursor opens a thread in the iPhone Simulator and taps its header, showing Archive at the top of the menu." poster="https://static.ampcode.com/docs/runners-desktop-ios-light-poster.avif" src="https://static.ampcode.com/docs/runners-desktop-ios-light.mp4"></video>
<video class="docs-media-dark" controls loop muted playsinline preload="none" data-autoplay-in-view width="2560" height="1026" aria-label="The Amp macOS app with a thread beside its runner's shared Mac desktop. Clicking Open the iOS app in the transcript opens the Desktop tab, where the cursor opens a thread in the iPhone Simulator and taps its header, showing Archive at the top of the menu." poster="https://static.ampcode.com/docs/runners-desktop-ios-dark-poster.avif" src="https://static.ampcode.com/docs/runners-desktop-ios-dark.mp4"></video>

On macOS, threads see and control the Mac's display. In the Mac app, turn on
[Share This Mac's Display](/docs/apps/mac#share-this-macs-display) instead. On Linux,
the runner starts its own virtual desktop, so your screen stays untouched. Anyone who can write to
a thread on the runner can see and control its desktop.

The Desktop and Terminal tabs connect to the runner over WebRTC: directly when your networks
allow it, or through a TURN relay. If neither connects within 5 seconds, they fall back to Amp's
relay.

On macOS, the runner asks for Screen Recording and Accessibility, and restarts its helper after
you grant them. Check what the runner can share, or ask again, with:

```shell-session
$ amp runner desktop status
$ amp runner desktop allow screen-recording
$ amp runner desktop allow accessibility
```

These permissions cover the Desktop tab. If you start the runner over SSH and want threads
themselves to see and control the Mac, for example to take screenshots, also turn on `amp` under
Screen Recording and Accessibility in System Settings > Privacy & Security. The runner prints the
path to add if `amp` is not listed.

<details>
<summary><strong>macOS Details</strong></summary>

Desktop sharing needs an Apple Silicon Mac on macOS 15 or later, running the arm64 Amp CLI.

Amp installs `Amp Desktop Helper.app` under `~/.amp/bin/amp-desktop-helper/<version>`, matching
the CLI version. The helper captures the screen, injects input, and asks macOS for Screen
Recording and Accessibility access. If macOS does not grant access from the prompt, turn on Amp
Desktop Helper in System Settings > Privacy & Security. Granting Screen Recording makes macOS quit
and reopen the helper; the runner notices and starts a new helper session on its own.

The helper's permissions cover only the Desktop tab. Commands that threads run themselves, such
as `screencapture` or `osascript`, get the permissions of the app that started the runner, such
as your terminal app. Over SSH, macOS would check the SSH server (`sshd-keygen-wrapper` or
`sshd-session`), which every SSH session shares, so a runner started over SSH with `--desktop`
restarts itself to make macOS check `amp` instead. It then asks macOS for Screen Recording and
Accessibility for `amp` and prints the path of the `amp` binary. Turn on `amp` under both in
System Settings > Privacy & Security to let threads see and control the Mac themselves; if it is
not listed, add that path with +. This runner and its threads no longer get the permissions of
SSH sessions, such as Full Disk Access for remote users.

If the Mac has more than one display, choose which one to share from the Desktop tab's menu.

</details>

<details>
<summary><strong>Linux Details</strong></summary>

The virtual desktop is a headless [labwc](https://labwc.github.io/) compositor with a 1280x720
output, streamed by [Waymote](https://github.com/rockorager/waymote), the same stack that orb
desktops use. Threads served by the runner inherit `WAYLAND_DISPLAY`, `DISPLAY`, and
`PULSE_SERVER` for that desktop, so graphical programs they start open there and appear in the
Desktop tab. Nothing from your own session is captured, and no permission prompt is needed.

The runner needs `labwc`, `wlr-randr`, and `ffmpeg` from your distribution on its `PATH` (an
orb's bundled build under `/opt/amp-wayland` is found first). On x64 Linux the runner downloads
Waymote (`waymote-gateway` and `waymote-streamd`) itself the first time `--desktop` starts, into
`~/.amp/bin/waymote/<waymote-version>`, unless they are already on `PATH`; on other architectures
install them from the [Waymote releases](https://github.com/rockorager/waymote/releases).
`dbus-run-session` and PulseAudio (`pulseaudio` and `pactl`) are optional; without PulseAudio the
desktop streams video only. `amp runner desktop status` lists which of these the runner finds.

Each runner keeps its compositor, logs, and sockets under
`~/.cache/amp/runner-desktop/<runner-id>`, so two runners on one machine get separate desktops.
A runner started inside an orb whose desktop is already running attaches to that desktop instead
of starting another one.

The desktop renders in software by default. To try GPU rendering, set
`AMP_DESKTOP_WLR_RENDERER` to a [wlroots renderer](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/docs/env_vars.md)
such as `gles2` or `vulkan` (for example, `AMP_DESKTOP_WLR_RENDERER=gles2 amp --no-tui --desktop`).
This is experimental: the runner needs access to a GPU render node (`/dev/dri/renderD*`), and if
the renderer cannot start, the desktop fails to start instead of falling back to software. The
runner ignores `WLR_RENDERER` from your own session.

Sharing an existing GNOME, KDE, X11, or other desktop session from a Linux runner is not yet
supported.

</details>

## Spawning Runner Threads

Use `amp -x` with `--executor runner:<id>` to [spawn a runner thread from the CLI](/docs/cli/execute-mode#spawning-runner-threads):

```shell-session
$ amp -x "Run the tests and fix any failures" --executor runner:grandmas-garage-server
```

## Serve Multiple Directories

By default, a runner serves threads in the directory where it was started. One runner can also
serve several directories, so you can pick any of them when you create a thread from
ampcode.com.

<div class="docs-media-stage">
  <img class="docs-transparent-media docs-media-light" width="1824" style="aspect-ratio: 1824/1183" src="https://static.ampcode.com/docs/runners-directory-picker-light.png" alt="The new thread composer with the runner mac-mini selected and the picker open, listing the projects amp, sandcastle, and marketing-site and the folders code, dotfiles, and scratch" />
  <img class="docs-transparent-media docs-media-dark" width="1824" style="aspect-ratio: 1824/1183" src="https://static.ampcode.com/docs/runners-directory-picker-dark.png" alt="The new thread composer with the runner mac-mini selected and the picker open, listing the projects amp, sandcastle, and marketing-site and the folders code, dotfiles, and scratch" />
</div>

These flags control which directories a runner serves:

| Flag                           | What it does                                                              |
| ------------------------------ | ------------------------------------------------------------------------- |
| `--dir <path>`                 | Serves one more directory. Repeat for each directory.                     |
| `--discover-dirs`              | Serves every Git checkout up to two levels beneath the current directory. |
| `--discover-dirs=<path>`       | Scans a different directory. Repeat to scan several.                      |
| `--discover-depth <levels>`    | Scans deeper than two levels.                                             |
| `--discover-exclude <pattern>` | Skips directories that match a `.gitignore`-style pattern.                |
| `--no-serve-cwd`               | Disables automatic serving of the current working directory.              |

### Add Directories With `--dir`

Pass `--dir <path>` for each additional directory. Any existing directory works; it does not need
to be a Git checkout:

```shell-session
$ amp --no-tui --runner-id mac-mini --dir ~/code/amp --dir ~/code/sandcastle
```

### Discover Git Checkouts

Pass `--discover-dirs` to serve every Git checkout found up to two levels beneath the current
directory:

```shell-session
$ cd ~/code
$ amp --no-tui --runner-id mac-mini --discover-dirs
```

On macOS and Linux, the runner watches for filesystem changes, so a repository you clone or a
worktree you add appears automatically without waiting for a periodic scan. Deleted checkouts
are removed automatically too. On other platforms, or if filesystem watching fails, the runner
rescans once a minute. Discovery skips hidden directories, `node_modules`, and symbolic links,
and serves at most 500 checkouts. Past that limit, main checkouts take precedence over linked
worktrees; use `--discover-exclude` to skip directories you do not need, or `amp runner dirs add`
to serve a specific directory regardless of the limit.

Git worktrees are served like any other checkout. Discovery also reads each repository's
worktree list (what `git worktree list` shows), so worktrees that live inside the checkout, such
as `repo/.worktrees/<branch>`, or under a bare clone, such as `org/repo/worktrees/<branch>`, are
served when they are beneath the scanned directory, however deep. A bare clone itself has no
working tree and is not served.

### Scan Multiple Directories

Pass `--discover-dirs=<path>` to scan a different directory instead, and repeat the flag to scan
several. A `--discover-dirs` without a path among them still stands for the current directory:

```shell-session
$ amp --no-tui --runner-id mac-mini --discover-dirs=~/work --discover-dirs=~/code --discover-dirs
```

Write the path with `=`. A space-separated path would also be accepted, but `--discover-dirs`
then takes any following argument, such as a subcommand, as the path.

### Scan Deeper

Pass `--discover-depth <levels>` (1 to 10) to scan deeper. Discovery does not look inside a
checkout, so a layout such as `~/code/<org>/<team>/<repo>` needs a depth of 3 from `~/code`:

```shell-session
$ cd ~/code
$ amp --no-tui --runner-id mac-mini --discover-depth 3
```

The depth applies to every scanned directory. `--discover-depth` on its own implies
`--discover-dirs` for the current directory.

### Exclude Directories

Pass `--discover-exclude <pattern>` to skip directories, and repeat the flag to skip several:

```shell-session
$ cd ~/code
$ amp --no-tui --runner-id mac-mini --discover-exclude dotfiles --discover-exclude 'work/legacy'
```

Patterns work like `.gitignore` entries:

- A name without a slash, such as `dotfiles`, skips every directory with that name at any depth.
- A pattern with a slash, such as `work/legacy` or `clients/*/vendor`, is relative to each
  scanned directory.
- An absolute or `~/` path skips that one directory.

Nothing inside a skipped directory is served, including worktrees registered there.
`--discover-exclude` on its own implies `--discover-dirs` for the current directory.

### Change Directories While the Runner Runs

While the runner is running, change the directories it serves from another terminal on the same
machine:

```shell-session
$ amp runner dirs add ~/code/new-repo
$ amp runner dirs list
$ amp runner dirs remove ~/code/dotfiles
```

Directories added this way are served again when you restart the runner from the same
directory. Pass `--runner-id <id>` to these commands when more than one runner is running on the
machine.

### Serve Only Selected Directories

Pass `--no-serve-cwd` to stop automatically serving the directory where you start the runner:

```shell-session
$ amp --no-tui --runner-id mac-mini --no-serve-cwd --discover-dirs=~/code
```

Directories selected by `--dir` or `--discover-dirs`, and directories added with
`amp runner dirs add`, are still served. You can explicitly serve the current directory with
`--dir .`. You can also start with no served directories and add them while the runner runs.
While the runner serves no directories, it does not register with Amp, so it does not appear
when you create a thread. It registers as soon as you add a directory.

New threads can only start in served directories. If the current directory is not served,
requests that omit a directory or explicitly target it are rejected. This selects where a thread
starts; it does not restrict the files or commands a running thread can access.

## Create a Directory or Project

When you are controlling a runner remotely, you may want to start work in a new directory
instead of one that already exists. You can create that directory from ampcode.com without
going back to a terminal on the runner's machine. If the work needs its own repository, you
can create an Amp-hosted project at the same time.

<div class="docs-media-stage">
  <img class="docs-transparent-media docs-media-light" width="1824" style="aspect-ratio: 1824/856" src="https://static.ampcode.com/docs/runners-create-composer-light.png" alt="The new thread composer with the mac-mini runner selected and playing-with-jev typed in the open folder picker, showing New Directory and New Project at ~/code/playing-with-jev" />
  <img class="docs-transparent-media docs-media-dark" width="1824" style="aspect-ratio: 1824/856" src="https://static.ampcode.com/docs/runners-create-composer-dark.png" alt="The new thread composer with the mac-mini runner selected and playing-with-jev typed in the open folder picker, showing New Directory and New Project at ~/code/playing-with-jev" />
</div>

**New Directory** creates an empty folder on the runner's machine. It does not initialize Git
or create an Amp project. Use it for work that does not need a repository yet.

**New Project** creates an [Amp-hosted project](/docs/projects) with the name you typed. It also
creates the folder on your runner, initializes a Git repository there, and sets its `origin`
to the new project's repository. It does not create a GitHub repository or push your files.

Both actions create the folder directly inside the directory where you started the runner.
Selecting a different folder in the picker does not change that parent directory. In the
screenshot, the destination is `~/code/playing-with-jev`. You can use the new folder
immediately without restarting the runner or adding a `--dir` flag.

## Create a Worktree

When several threads should work in the same repository at the same time, give each one its
own [Git worktree](https://git-scm.com/docs/git-worktree). A runner can create the worktree for
you from ampcode.com, so the thread starts in a fresh checkout and your main checkout stays as
it is.

In the directory picker, every Git checkout the runner serves offers **New Worktree** next to
**Current**. Press Tab on the highlighted repository, click **New Worktree**, or tap it on a
phone.

<div class="docs-media-stage">
  <img class="docs-transparent-media docs-media-light" width="1824" style="aspect-ratio: 1824/1312" src="https://static.ampcode.com/docs/runners-worktree-picker-light.png" alt="The new thread composer with the runner mac-mini selected and the directory picker open. The amp repository is highlighted and offers Current or New Worktree." />
  <img class="docs-transparent-media docs-media-dark" width="1824" style="aspect-ratio: 1824/1312" src="https://static.ampcode.com/docs/runners-worktree-picker-dark.png" alt="The new thread composer with the runner mac-mini selected and the directory picker open. The amp repository is highlighted and offers Current or New Worktree." />
</div>

Then name the worktree. The form suggests a name from your prompt and shows what the runner
will create before you confirm:

<div class="docs-media-stage">
  <img class="docs-transparent-media docs-media-light" width="1824" style="aspect-ratio: 1824/1029" src="https://static.ampcode.com/docs/runners-worktree-form-light.png" alt="The worktree form: New worktree of amp, name fix-flaky-login-test, directory ~/code/amp-fix-flaky-login-test, branch fix-flaky-login-test, from current HEAD, and a Use Worktree button" />
  <img class="docs-transparent-media docs-media-dark" width="1824" style="aspect-ratio: 1824/1029" src="https://static.ampcode.com/docs/runners-worktree-form-dark.png" alt="The worktree form: New worktree of amp, name fix-flaky-login-test, directory ~/code/amp-fix-flaky-login-test, branch fix-flaky-login-test, from current HEAD, and a Use Worktree button" />
</div>

When you create the thread, the runner runs `git worktree add` on its machine:

- The worktree is a sibling of the repository named `<repository>-<name>`. For `~/code/amp`
  and the name `fix-flaky-login-test`, that is `~/code/amp-fix-flaky-login-test`.
- It checks out a new branch with the same name. The runner fetches the current branch's
  configured upstream first. If the current branch is behind, the new worktree starts at
  that latest commit. Local commits ahead of the upstream are preserved.
- Uncommitted changes stay in the original checkout.
- Amp creates the worktree when you create the thread and starts the thread there. If
  creation fails, no thread is created.

The original checkout's branch and files stay unchanged. If the local and remote branches
have diverged, merge or rebase them before creating a worktree. A failed fetch stops creation
rather than using an old commit. Branches without an upstream and detached checkouts use
their current `HEAD` without fetching. This requires an updated runner.

Names may contain letters, numbers, hyphens, and underscores, and must start with a letter or
number. Leave the name empty to name the worktree from your prompt. If the branch or the
directory already exists, Amp adds `-2`, `-3`, and so on. Nothing is overwritten.

Amp remembers **New Worktree** for that runner and repository. Later threads you start there
also get a new worktree, named from their prompt, until you choose **Current**. A name you type
applies only to the next thread.

Worktrees can only be created from a main Git checkout. A bare repository or a linked worktree
does not offer **New Worktree**. Worktrees stay on disk after the thread ends unless you remove
them as described next.

Agents can create worktrees too. [Puck](/docs/puck) and threads that use the `create_thread`
tool pass `executor: "runner"`, the `runner_id`, and a `worktree` name; the runner creates the
worktree the same way and the new thread starts in it.

### Archive a Thread and Remove Its Worktree

A thread that runs in a worktree the runner created has an extra action, **Archive and Remove
Worktree**, in the thread's menu, in the command palette, in the context menu of the thread in
the sidebar, and behind the archive button on the thread's sidebar row. After you confirm, Amp
archives the thread, then asks the runner to run `git worktree remove` and delete the branch.
The directory disappears from the picker.

Removal never forces anything:

- A worktree with uncommitted or untracked changes is kept, and the toast tells you why.
- A branch with commits that have not been merged yet is kept.
- If the runner is offline, the thread is still archived and the worktree stays on disk.

Unarchiving the thread does not recreate the worktree.

Agents can do the same: ask the agent to archive the thread and remove its worktree. The same
rules apply. The thread is already archived at that point, so if the runner keeps the worktree,
the thread does not say why. The worktree stays on disk, and you can remove it with
`git worktree remove`.

## Secrets & Env Vars

By default, threads on a runner see only the environment variables of the runner process. That
is the environment of the shell or service that started `amp --no-tui`. To change a variable,
you have to change it on the machine and restart the runner.

But a runner can also use the [Secrets & Env Vars](/docs/orbs/handling-secrets) that you
configure on ampcode.com.

This is the same set of variables that orbs get. It lets you manage the environment of a runner
from the web or the Amp apps, e.g., to give a runner on a remote machine a new API key without
logging in to that machine.

This is off by default. Turn it on with the `--amp-env` flag:

```bash
amp --no-tui --amp-env
```

Setting [`amp.runner.env.enabled`](/docs/cli/settings) to `true` in the runner's settings file
does the same without the flag. Either one enables it.

### How It Works

Every time a thread starts on the runner, the runner asks ampcode.com for the variables that
apply to that thread and adds them to the environment of the shell commands and the stdio MCP
servers that the thread runs. This works like an orb, which gets its variables when it starts.
The variables are sent only to the runner that serves the thread, and only for the directory the
thread runs in.

Variables come from four places. When the same variable is set in more than one place, the
value from the place higher in this list wins:

1. Your personal Secrets & Env Vars on ampcode.com.
2. The project's Secrets & Env Vars, for the project that matches the thread's directory.
3. The workspace's Secrets & Env Vars.
4. The environment of the runner process.

So a project variable replaces a variable of the same name that the runner process already had,
and a personal variable replaces both. Variables that exist only in the runner process stay as
they are.

### What to Expect

- You do not need to restart the runner after you change a variable on ampcode.com. The next
  thread that starts in that directory gets the new value. Threads already running there keep the
  old value until a new thread starts in the same directory.
- The variables are set per served directory. The most recent thread to start in a directory
  sets the environment for every thread in that directory.
- MCP servers and plugins started for a directory also get these variables. When the variables
  for a directory change, the runner restarts the directory's MCP servers and plugins so that
  they start with the new values.

## Share a Runner with Your Workspace

A runner can serve the other members of your workspace, not just you. That is useful for a
machine with hardware or access the rest of the team needs, e.g., a GPU machine or a Mac that
builds and signs your app.

Start the runner with `--share`:

```shell-session
$ amp --no-tui --runner-id macos-builder --share
```

On a Mac, you can turn on [**Share With Workspace**](/docs/apps/mac#share-with-your-workspace)
in the Amp app's Runner tab instead.

Before the runner starts, it prints a warning that workspace members will be able to run code on
the machine as you. The runner is shared with your workspace for as long as it runs. To stop
sharing it, restart it without `--share`; this also turns off multiplayer on every thread that
was created on it, so other members lose access to your machine right away. Every member of the
workspace sees it in the location picker on ampcode.com under **Shared Runners**, with your name
and avatar. The picker also shows how many threads the runner is running, counting every
member's threads.

<div class="docs-media-stage">
  <img class="docs-transparent-media docs-media-light" width="1824" style="aspect-ratio: 1824/1185" src="https://static.ampcode.com/docs/runners-shared-picker-light.png?v=2" alt="The location picker in the new thread composer, listing your own devbox under Runners and gpu-runner (Allison) and macos-builder (Monty) under Shared Runners, each with its owner's avatar and running-thread count." />
  <img class="docs-transparent-media docs-media-dark" width="1824" style="aspect-ratio: 1824/1185" src="https://static.ampcode.com/docs/runners-shared-picker-dark.png?v=2" alt="The location picker in the new thread composer, listing your own devbox under Runners and gpu-runner (Allison) and macos-builder (Monty) under Shared Runners, each with its owner's avatar and running-thread count." />
</div>

### How It Works

- When another member picks your runner, Amp creates the thread as their thread and your runner
  runs it. Your runner gets access to that thread only.
- Other members' threads on a shared runner are always visible to the whole workspace. Your
  workspace's default thread visibility must be set to the workspace for members to create them.
- Other members see the directories your runner serves, but not your threads on it.
- The thread owner can turn on [multiplayer](/docs/collaborate/multiplayer) for a thread on a
  shared runner, so other workspace members can prompt it and use its terminal and files. Sharing
  a runner does not turn on multiplayer by itself.
- Your own threads on the runner can be made multiplayer when you start them in the workspace you
  share the runner with, whatever project they are in. Your other threads on it stay personal.
- With [`--amp-env`](#secrets-env-vars), a shared runner adds the workspace and project Secrets &
  Env Vars, but not your personal ones. This also applies to your own threads on the runner.
- Creating directories, projects, and worktrees on a shared runner is not supported. Other
  members pick from the directories it already serves.

### Caution! Achtung! Attention! Attenzione!

- Threads from other members run on your machine as the operating-system user that started the
  runner. They can read and change that user's files and use its credentials and logins. Only
  share a runner with people you trust to run code on that machine.
- Threads can work in the same directory at the same time and overwrite each other's changes.
- Stopping sharing does not undo changes that threads already made, and it does not stop
  programs they already started on the machine.

### Turn Off Runner Sharing for a Workspace

Workspace admins can turn off **Allow Members to Share Runners** under **Member Settings** in
the workspace settings. While it is off:

- Starting a runner with `--share` fails with an error that your workspace admin has disabled
  sharing runners. Start it without `--share` to use it only yourself.
- Runners that were already shared stop being shared right away. Members no longer see them
  under **Shared Runners** and cannot start new threads on them, multiplayer is turned off on the
  threads created on them, and threads already running on them lose access to the runner within
  five minutes.

Turning the setting back on does not share those runners again. Their owners must restart them
with `--share`.

## Keep a Runner Updated

A long-running `amp --no-tui` runner updates itself. It checks for a new release about once an
hour and installs it. Once no thread is running on it, it restarts into the new version, at most
once every 12 hours. The restart keeps the runner ID, the served directories, and the other
command-line flags. If threads stay attached for a day, the runner restarts once none of them is
in the middle of a turn.

Set [`amp.runner.autoUpdate.enabled`](/docs/cli/settings) to
`false` to turn this off. Setting `amp.updates.mode` to `"disabled"` also turns it off. Runners
installed with Homebrew report the new version but do not install it; update them with `brew`
and restart the runner.

<a id="using-a-runner"></a>

## Runners vs. Orbs

Runners and orbs are two different ways to give an agent a machine.

An [orb](/docs/orbs) is a machine that Amp creates for a thread. You can start as many orbs as
you have tasks, and each one is separate from the others. You pick its
[size](/docs/orbs/sizes-and-costs) when you create the thread. When the agent is done and you
stop using the thread, the orb goes to sleep, and a sleeping orb costs nothing. It wakes up
again when you send the next message, with your files and services still in place. You never
set up, update, or maintain the machine.

A runner runs on a machine you already have. You start `amp --no-tui` on it, and Amp creates
threads there with the directories, tools, and logins that are on that machine. Amp does not create,
size, pause, or wake the machine. The runner is available while the process runs and the
machine is on, and every thread you create on it shares that one machine.

Use orbs when you want a fresh machine for every task. Use a runner when the work has to happen
on a specific machine, e.g., one with hardware or network access that an orb cannot have.

Inside a thread, runners support some of what orbs support:

| Feature                                                        | Runner Support                                                                                                                                                                                                                                                                              |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **[Lifecycle](/docs/orbs/sizes-and-costs#when-an-orb-pauses)** | Amp pauses an orb when it is idle and wakes it when you need it again. A runner runs on a machine you control, and Amp does not start, stop, or pause that machine.                                                                                                                         |
| **[Creating Threads](/docs/threads#start-a-thread)**           | A runner creates threads on its own machine, in the directory where it was started or any directory it [serves](#serve-multiple-directories).                                                                                                                                               |
| **[Changes](/docs/orbs/shipping)**                             | Available automatically when the runner directory is a Git or Jujutsu checkout. No runner flag is needed.                                                                                                                                                                                   |
| **[Files](/docs/orbs/files-and-attachments)**                  | Available automatically while the runner is connected. It browses the runner directory, and no runner flag is needed.                                                                                                                                                                       |
| **[Terminal](/docs/threads#inside-a-thread)**                  | Available when you start the runner with `--remote-control-terminal` (in the [Mac](/docs/apps/mac#allow-terminal-access) or [Linux](/docs/apps/linux#built-in-runner) app, turn on **Allow Terminal Access**). Restart the runner with this flag, then reload the thread if it is disabled. |
| **[Portal](/docs/orbs/portals)**                               | Available when you start the runner with `--remote-control-portals` (in the Mac app, turn on [Allow Portals](/docs/apps/mac#allow-portals)). Read [Portals on Your Runner](#portals-on-your-runner).                                                                                        |
| **[Desktop](/docs/orbs/portals#desktop)**                      | Available on macOS and Linux when you start the runner with `--desktop`. Read [See and Control the Runner's Desktop](#see-and-control-the-runners-desktop).                                                                                                                                 |
| **[Multiplayer](/docs/collaborate/multiplayer)**               | Supported on [shared runners](#share-a-runner-with-your-workspace) only. On a runner you share, your own threads qualify when you start them in the workspace you shared the runner with, whatever project they are in. Threads on a runner you have not shared cannot be made multiplayer. |

## Troubleshoot a Runner

- If the runner does not appear in the location picker, confirm that `amp --no-tui` is still
  running and signed in to the same Amp account as the web app.
- If **Changes** is unavailable, start the runner inside a Git or Jujutsu checkout.
- If **Files** cannot load, confirm that the runner is connected and can still read its working
  directory.
- If **Terminal** says access is disabled, restart the runner with
  `--remote-control-terminal`, then reload the thread page.
- If the thread has no **Portal** tab, restart the runner with `--remote-control-portals`, then
  reload the thread page.

## TUI as Runner

You can turn every interactive Amp TUI into a runner too by adding this setting:

```json
// ~/.config/amp/settings.json

{
	"amp.remoteThreadCreation.enabled": true
}
```

You can also use the command `amp: enable remote creation of threads` to turn it on. Each TUI
accepts new threads in the directory where you started it.

<!-- prettier-ignore -->
<video controls autoplay muted loop playsinline width="1976" height="1080" src="https://static.ampcode.com/docs/agents-anywhere-demo-1.mp4" type="video/mp4"></video>
