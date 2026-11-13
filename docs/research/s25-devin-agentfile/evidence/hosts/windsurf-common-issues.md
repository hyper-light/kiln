> ## Documentation Index
> Fetch the complete documentation index at: https://docs.devin.ai/llms.txt
> Use this file to discover all available pages before exploring further.

# Common Devin Desktop Issues

> Troubleshoot common Devin Desktop Editor issues including rate limiting, MacOS security warnings, Windows updates, Linux crashes, and terminal problems.

### General FAQ

> [accordion] I subscribed to Pro so why am I stuck on the Free tier?
>
> First, give it a few minutes to update. If that doesn't work, try logging out of Devin Desktop on the website, restarting your IDE, and logging back into Devin Desktop. Additionally, please make sure you have the latest version of Devin Desktop installed.

> [accordion] How do I cancel my Pro/Teams subscription?
>
> You can cancel your paid plan by going to your Profile by clicking your icon on the top right of the [Devin Desktop website](https://windsurf.com/profile).
>
> To cancel your Pro subscription, navigate to the `Billing` page in the navigation panel on the left and click "Cancel Plan".
>
> To cancel your Teams subscription, navigate to the `Manage Team` page in the navigation panel on the left and click "Cancel Plan".

> [accordion] How do I disable code snippet telemetry?
>
> As mentioned on our [security page](https://docs.devin.ai/admin/security.md#how-is-your-data-used-to-improve-devin), you can opt out of code snippet telemetry by going to your [account settings](https://windsurf.com/settings). For more information, please visit our [Terms of Service](https://windsurf.com/terms-of-service-individual).

> [accordion] How do I delete my account?
>
> Reach out to [support](https://windsurf.com/support/) to delete your account.

> [accordion] How do I request a feature?
>
> You can share feature requests and feedback through our community channels:
> [Reddit](https://www.reddit.com/r/windsurf/), [Discord](https://discord.com/invite/3XFf78nAx5), or [Twitter/X](https://x.com/windsurf).
>
> You can also reach out to us via our [support platform](https://windsurf.com/support/).

### I'm experiencing rate limiting issues

We're subject to rate limits and unfortunately sometimes hit capacity for the premium models we work with. We are actively working on getting these limits increased and fairly distributing the capacity that we have!

This should not be an issue forever. If you get this error, please wait a few moments and try again.

### Pylance or Pyright isn't working / Python syntax highlighting is broken or subpar

We've gone ahead and developed a [Pyright extension specifically for Devin Desktop](https://docs.devin.ai/desktop/advanced.md#windsurf-extensions). Please search for "Windsurf Pyright" or paste `@id:codeium.windsurfPyright` into the extension search.

### How do I download Diagnostic logs to send to the Devin Desktop support team?

You can download diagnostic logs by going to your Cascade Panel, tapping the three dots in the top right corner, and then clicking "Download Diagnostics".

![](https://mintcdn.com/cognitionai/WE51TvKjsiyk1ZHl/desktop/assets/windsurf/windsurf-download-diagnostics.png?fit=max\&auto=format\&n=WE51TvKjsiyk1ZHl\&q=85\&s=dc7a5b0e63ef1fcefee74c8e052a0adc)

### On MacOS, I see a pop-up: 'Devin Desktop' is damaged and cannot be opened.

This pop-up is due to a false positive in MacOS security features. You can usually resolve this by going to "System Settings -> Privacy & Security" and clicking "Allow" or "Open anyway" for Devin Desktop. If this fails or is not possible, try the following steps:

1. Ensure that Devin Desktop is placed under your `/Applications` folder and that you are running it from there.
2. Check your processor type: if your Mac has an Intel chip, make sure you have the Intel version. If it's Apple Silicon (like M1, M2 or M3), make sure you have the Apple Silicon version. You can select the processor type from the [Mac download page](https://windsurf.com/windsurf/download_mac).
3. Try redownloading the DMG and reinstalling from [the official download page](https://windsurf.com/windsurf/download_mac), as the failing security feature is usually triggered on download.
4. Make sure Devin Desktop (and the "Devin Desktop is Damaged" pop-up) is closed, and run `xattr -c "/Applications/Devin.app/"`.

### I received an error message about updates on Windows, or updates are not appearing on Windows.

For example:

> Updates are disabled because you are running the user-scope installation of Devin Desktop as Administrator.

We cannot auto-update Devin Desktop when it is run as Administrator. Please re-run Devin Desktop with User scope to update.

### On macOS, Remote SSH fails with "Undefined error: 0" but SSH works from Terminal

If Remote SSH in Devin Desktop fails immediately while the same SSH connection works from Terminal, VS Code, or other applications, this is usually caused by macOS blocking Devin Desktop's local network access.

In the Remote - SSH output log (View → Output → Remote - SSH), you will see:

```
debug1: Connecting to <hostname> port 22.
ssh: connect to host <hostname> port 22: Undefined error: 0
```

followed by `SSH server closed unexpectedly. Error code: 255`.

The `Undefined error: 0` message (rather than "Connection refused" or "Network unreachable") is the key indicator — this is the error macOS returns when an application is not granted Local Network permission under Privacy & Security.

To fix this:

1. Open **System Settings → Privacy & Security → Local Network**.
2. Find **Devin Desktop** in the list and **enable** the toggle.
3. Restart Devin Desktop and retry the connection.

If Devin Desktop does not appear in the Local Network list, try initiating an SSH connection from Devin Desktop first to trigger the macOS permission prompt. If the prompt was previously dismissed and the toggle does not appear, deleting and reinstalling Devin Desktop will re-trigger the prompt on next launch.

### What domains should I allowlist for network filters/firewalls, VPNs, or proxies?

If you're using any network filtering, firewalls, VPN services, or working in environments with restricted network access, you may experience connectivity issues with Devin Desktop. To ensure smooth operation, please allowlist the following domains in your network configuration:

**For all customers, Devin Desktop requires (backend domains):**

* `.codeiumdata.com`
* `update.windsurf.com`
* `.windsurf.com`
* `.codeium.com`
* `.googleapis.com` (authentication)
* `apis.google.com` (authentication)
* `static.devin.ai`

**Cognition Platform customers additionally require (backend domains):**

* `app.devin.ai` (webapp, session URLs, and resource downloads)
* `api.devin.ai` (API backend and the ACP live WebSocket)

If you use wildcard rules, `.devin.ai` covers both. Cognition Platform customers on a `devinenterprise.com` domain should allowlist the equivalent hosts (or `.devinenterprise.com`).

**User-accessible domains (changelog, documentation, and support):**

* `docs.devin.ai` or `docs.devinenterprise.com` (changelog and documentation, after June 2, 2026)
* `decagon.ai` (support)

### On Linux, Devin Desktop quietly doesn't launch, or crashes on launch

This is usually due to an Electron permissions issue, which VSCode also has, and is expected when using the tarball on Linux.

The easiest way to fix it is to run the following:

```bash theme={null}
sudo chown root:root /path/to/windsurf/chrome-sandbox
sudo chmod 4755 /path/to/windsurf/chrome-sandbox
```

You should then be able to launch Devin Desktop. You can also just run `devin-desktop` with the flag `--no-sandbox`, though we don't encourage this.

If this fails, then try the below.

### I received an error message saying 'Devin Desktop failed to start'

> [!WARNING]
>
> Warning: deleting these folders will remove your conversation history and local settings!

Delete the following folder:

Windows: `C:\Users\<YOUR_USERNAME>\.codeium\windsurf\cascade`

Linux/Mac: `~/.codeium/windsurf/cascade`

and try restarting the IDE.

### My Cascade panel goes blank

Please reach out to us if this happens! A screen recording would be much appreciated. This can often be solved by clearing your chat history (`~/.codeium/windsurf/cascade`).

### Terminal session appears stuck in Cascade

If a terminal command has finished running in the terminal but Cascade still shows the session as in progress or stuck, this can be caused by several issues:

**Default terminal profile not set**

This may be caused by the default terminal profile not being explicitly set. To resolve this, you can set the default terminal profile in your Editor settings.

Open the Settings UI (Cmd/Ctrl + ,), search for "terminal default profile", and set the appropriate value for your operating system. Alternatively, you can add the following to your `settings.json`:

For macOS:

```json theme={null}
"terminal.integrated.defaultProfile.osx": "zsh"
```

For Windows:

```json theme={null}
"terminal.integrated.defaultProfile.windows": "PowerShell"
```

For Linux:

```json theme={null}
"terminal.integrated.defaultProfile.linux": "bash"
```

Replace the value with your preferred shell (e.g., `bash`, `zsh`, `PowerShell`, `Command Prompt`, etc.).

**Customized zsh themes**

In some cases, a heavily customized zsh theme (for example, themes from Oh My Zsh, Powerlevel10k, or other prompt frameworks) can also cause Cascade to think a command is still running even after it finishes. To check if this is the issue:

1. Open your `~/.zshrc` file in a text editor.
2. Temporarily disable your theme by commenting out lines that set or load it, such as `ZSH_THEME="..."`, `source ~/.p10k.zsh`, or `eval "$(oh-my-posh init zsh)"`.
3. Save the file, restart Devin Desktop (or open a new terminal in Devin Desktop), and run a command again.

If the terminal session no longer appears stuck in Cascade, you can either keep a simpler theme in `~/.zshrc`, or create a separate, minimal zsh configuration used only by the Devin Desktop terminal so your other terminals can continue using the more complex theme.

**Systemd terminal context tracking (Linux)**

On some newer Linux distributions (reported on Fedora 43 and later), the shell startup chain (`~/.bashrc` → `/etc/bashrc` → `/etc/profile.d/80-systemd-osc-context.sh`) can enable systemd "terminal context tracking," which emits OSC 3008 escape sequences via `PS0` or `PROMPT_COMMAND`. These extra control sequences can interfere with Cascade's output parsing, causing a command to appear stuck or resulting in captured output that looks missing or truncated—even though the terminal displays it correctly.

To work around this issue, prevent the OSC context sequences from being emitted in the Cascade terminal by not sourcing `/etc/bashrc` from your `~/.bashrc`, or by creating a minimal shell configuration file used only for Devin Desktop/Cascade.

### Docker Container Not Visible in Remote Explorer When Using WSL

When connecting to Docker containers inside WSL, the remote explorer window may not display available containers to connect to, requiring users to use the command palette workaround. Use Cmd+P (macOS) or Ctrl+P (Windows) → "Dev Containers: Attach to Running Container" to see the full list of running containers.


This documentation is built and hosted on [Mintlify](https://mintlify.com), a developer documentation platform.