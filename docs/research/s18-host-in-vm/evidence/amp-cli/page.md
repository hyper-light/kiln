---
lastModified: 2026-09-25
---

# Getting Started

The Amp CLI runs in your terminal. It can run the Amp agent on your machine, in an
[orb](/docs/orbs), or on a connected [runner](/docs/cli/runners).

The CLI supports macOS, Linux, and Windows through WSL. On Windows, we recommend
[WezTerm](https://wezterm.org/install/windows.html#for-winget-users) or
[Alacritty](https://alacritty.org/) instead of Windows Terminal for fully functional clipboard
support.

<!-- prettier-ignore -->
<video width="1280" height="814" controls autoplay loop muted playsinline preload="metadata" aria-label="The same Orb thread controlled from the Amp TUI" src="https://static.ampcode.com/docs/cli-getting-started-tui.mp4"></video>

## Install

Here's how you can install the Amp CLI:

Mac/Linux/WSL:

```
curl -fsSL https://ampcode.com/install.sh | bash
```

Windows:

```
powershell -c "irm https://ampcode.com/install.ps1 | iex"
```

Homebrew:

```
brew install ampcode/tap/ampcode
```

<div class="mt-3 text-sm italic text-muted-foreground">
  If your workspace has its own installation instructions, sign in and open the <a href="/app">download page</a>. Amp will show the instructions for that workspace.
</div>

## Use the CLI

After installing, run `amp` to start the Amp CLI.

Without any arguments, it runs in interactive mode:

```shell-session
$ amp
```

If you pipe input to the CLI, it uses the input as the first user message in interactive mode:

```shell-session
$ echo "commit all my changes" | amp
```

To see more of what the CLI can do, run `amp --help`.

## Manage CLI accounts

Run `amp login` to add an account and make it active for the current Amp server. The CLI keeps accounts
for each server separate. Use `amp account list` to see saved accounts and `amp account switch
<email-or-user-id>` to choose the account used by future CLI commands.

Each running CLI process stays with the account it started with. Switching accounts does not
change an already running process. Start a new `amp` process after switching.

Run `amp logout` to remove the active saved account, `amp logout <email-or-user-id>` to remove a
specific account, or `amp logout --all` to remove all saved CLI accounts on every Amp server.
Removing the active account does not select another account. Use `amp account switch` or log in again.
This does not sign out browser or native app sessions. If `AMP_API_KEY` is set, it remains active
in that shell and takes precedence over saved accounts until you unset it.

## Shell Completions

Amp provides Tab completion for commands, aliases, options, and declared argument choices in
Bash, Zsh, Fish, and PowerShell. Setup reads the command definitions from the installed Amp version.
Pressing Tab runs a small JavaScript resolver with Amp's bundled Bun runtime, without starting
the Amp application. You do not need Usage, a separate Bun installation, or a command spec.
Completions work without signing in or connecting to the network.

For Bash 4 or later, install and load [bash-completion](https://github.com/scop/bash-completion),
then add this line to `~/.bashrc` after loading it:

```bash
source <(amp completions bash)
```

For Zsh, add this line to `~/.zshrc` after your existing `compinit` call. If you have not enabled
Zsh completions, first run `autoload -Uz compinit; compinit` in that file.

```zsh
source <(amp completions zsh)
```

For Fish, add this to `~/.config/fish/config.fish`:

```fish
if status is-interactive
    amp completions fish | source
end
```

For PowerShell 7, add this line to your `$PROFILE` file:

```powershell
amp completions pwsh | Out-String | Invoke-Expression
```

Open a new shell or run the setup line in your current shell. Remove any earlier Amp completion
setup to avoid conflicts. After updating Amp, open a new shell or run the setup line again to
refresh the command definitions. Amp does not change your shell configuration.

## Attach Images

Press <kbd>Ctrl+V</kbd> to paste an image from your clipboard into the prompt.
Use <kbd>Ctrl+V</kbd>, not <kbd>Cmd+V</kbd>, even on macOS. You can also drag and drop image files
from your file manager into the terminal to attach them.

If your terminal intercepts <kbd>Ctrl+V</kbd>, press <kbd>Ctrl+O</kbd> and run
`prompt: paste image from clipboard`. See [CLI Keybindings](/docs/cli/keybindings) for more shortcuts.

## Choose Where the Thread Runs

By default, `amp` starts a local thread. Use `--executor` to open the interactive TUI with a new
thread that will run locally, in an orb, or on one of your [runners](/docs/cli/runners):

```shell-session
$ amp --executor local
$ amp --executor orb
$ amp --executor runner:grandmas-garage-server
```

Orb threads use the Amp project that matches the current directory's Git remote. If no project
matches, Amp warns you and uses a no-project orb. A runner ID identifies a connected runner started
with `amp --no-tui --runner-id <id>`.

Runner threads use the runner's starting directory by default. Pass `--runner-dir <absolute-path>`
to choose another directory served by that runner:

```shell-session
$ amp --executor runner:grandmas-garage-server --runner-dir /home/me/code/my-app
```

## Update

Amp automatically checks and installs new versions in the background. You don't have to do anything except regularly restart Amp.

You can also manually check for updates:

```bash
amp update
```

You can check which version you have by running:

```bash
amp version
```

## Connect an Editor

Amp can connect to VS Code, Cursor, Windsurf, Zed, and Neovim. Open the editor, run `amp`, then open the command palette with <kbd>Ctrl+O</kbd> and select `ide connect`.

The integration lets Amp:

- See the current open file and selection, so Amp can better understand the context of your prompt
- Edit files through your IDE, with full undo support

Neovim also requires the [Amp Neovim plugin](https://github.com/ampcode/amp.nvim).
