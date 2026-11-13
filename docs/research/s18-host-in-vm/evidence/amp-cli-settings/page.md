---
lastModified: 2026-11-02
---

# Configuration

Amp reads settings from these locations:

- User settings:
  - macOS: `~/.config/amp/settings.json` or `~/.config/amp/settings.jsonc`
  - Linux: `~/.config/amp/settings.json` or `~/.config/amp/settings.jsonc`
  - Windows: `%USERPROFILE%\.config\amp\settings.json` or
    `%USERPROFILE%\.config\amp\settings.jsonc`
- Workspace settings: the nearest `.amp/settings.json` or `.amp/settings.jsonc`, searched upward
  from your current working directory to the repository root, or the current directory outside a
  Git repository
- Custom user settings: pass `--settings-file <path>` to point Amp at a different user settings
  file

Run `amp config edit` to open your user settings file in `$EDITOR`, or add `--workspace` to edit
workspace settings.

When the same setting appears in multiple places, workspace settings override user settings.

All settings use the `amp.` prefix.

## Settings

Add the <a href="/cli-settings.schema.json" data-sveltekit-reload>Amp settings JSON Schema</a> to
your settings file to get validation, descriptions, and completions in editors that support JSON
Schema:

```json
{
	"$schema": "https://ampcode.com/cli-settings.schema.json",
	"amp.showCosts": true
}
```

- **`amp.fuzzy.alwaysIncludePaths`**

  **Type:** `array`, **Default:** `[]`

  Glob patterns for paths that should always be included in fuzzy file search, even if they are
  ignored by Git. This is useful for build output directories or generated files you want to
  reference with `@` mentions.

  Examples: `["dist/**", "node_modules/@myorg/**"]`

- **`amp.showCosts`**

  **Type:** `boolean`, **Default:** `true`

  Show cost information for threads in the CLI while working. Workspace admins can also hide costs
  for all workspace members in [workspace settings](/workspace).

- **`amp.git.commit.ampThread.enabled`**

  **Type:** `boolean`, **Default:** `true`

  Add the Amp thread trailer to Git commits. When disabled, commits made by the agent will not
  include the `Amp-Thread-ID: <thread-url>` trailer. You can also set the environment variable
  `AMP_DISABLE_AMP_THREAD_TRAILER=1`. Amp links a pushed commit to its thread only through this
  trailer, so while it is disabled, new commits are not linked to their threads and do not mark
  those threads as shipped. While disabled, Amp also refuses to run a `git commit`
  whose message writes the trailer by hand and asks the agent to remove it; the same applies to
  `Co-authored-by: Amp` when `amp.git.commit.coauthor.enabled` is off.

- **`amp.git.commit.coauthor.enabled`**

  **Type:** `boolean`, **Default:** `true`

  Add Amp as a coauthor in Git commits. When disabled, commits made by the agent will not include
  the `Co-authored-by: Amp <amp@ampcode.com>` trailer. In orbs, use **Commit Author** in a
  personal project's settings or **Credit Amp as Co-author** in a workspace project's settings
  instead. In orbs whose project Git identity is Amp, Amp is already the
  commit author and this trailer is never added; the thread creator is credited as a co-author
  instead.

- **`amp.keymap`**

  **Type:** `object`, **Default:** `{}`

  Customize the CLI keymap. Keymap entries in user settings override entries in workspace settings.
  See [Customize the Keymap](/docs/cli/keybindings#customize-the-keymap) for more information.

- **`amp.mcpServers`**

  **Type:** `object`

  Model Context Protocol servers that expose tools. See the [MCP documentation](/docs/customize/mcp).

- **`amp.notifications.enabled`**

  **Type:** `boolean`, **Default:** `true`

  Play notification sounds when the agent completes a task or is blocked waiting for user input.
  Over SSH, or when `AMP_FORCE_BEL` is set, Amp sends a terminal bell instead of using host audio.

- **`amp.remoteThreadCreation.enabled`**

  **Type:** `boolean`, **Default:** `false`

  Let ampcode.com create new threads that open in the interactive Amp TUI on this machine, in the
  directory where it was started. Toggle it from the TUI command palette with
  `amp: enable remote creation of threads`.

- **`amp.runner.autoUpdate.enabled`**

  **Type:** `boolean`, **Default:** `true`

  Let `amp --no-tui` runners install CLI updates and restart into them. A runner restarts once
  no thread is running there, at most once every 12 hours, and keeps its command-line flags.
  Setting `amp.updates.mode` to `"disabled"` also turns this off.

- **`amp.runner.env.enabled`**

  **Type:** `boolean`, **Default:** `false`

  Let `amp --no-tui` runners inject the Secrets & Env Vars configured on ampcode.com into the
  shell commands, MCP servers, and plugins of the threads they serve. Same as starting the runner with
  `--amp-env`. See [Secrets & Env Vars on runners](/docs/cli/runners#secrets-env-vars).

- **`amp.skills.disableClaudeCodeSkills`**

  **Type:** `boolean`, **Default:** `false`

  Disable loading skills from Claude Code directories (`.claude/skills/`,
  `~/.claude/skills/`, and `~/.claude/plugins/cache/`). This does not affect `.agents/skills/`,
  `~/.config/agents/skills/`, `~/.agents/skills/`, `~/.config/amp/skills/`, `amp.skills.path`,
  built-in skills, personal skills, or workspace skills.

- **`amp.skills.disableGlobalAgentsSkills`**

  **Type:** `boolean`, **Default:** `false`

  Disable loading skills from the global agent-agnostic directories (`~/.config/agents/skills/`
  and `~/.agents/skills/`). Use this when another tool owns `~/.config/agents/` and you want Amp
  to read only `~/.config/amp/skills/` or `amp.skills.path`. This does not affect
  `~/.config/amp/skills/`, `.agents/skills/`, Claude Code directories, `amp.skills.path`,
  built-in skills, personal skills, or workspace skills. `amp skill add --global` installs to
  `~/.config/agents/skills/` and warns that those skills are not loaded while this is set.

- **`amp.skills.path`**

  **Type:** `string`

  Path to additional directories containing skills. This setting supports colon-separated paths,
  or semicolon-separated paths on Windows. Use `~` for your home directory. Relative paths
  resolve against the workspace root, so a workspace `.amp/settings.json` can use
  `.amp/skills`. Example: `~/my-skills:/shared/team-skills`

- **`amp.terminal.copyOnSelect`**

  **Type:** `boolean`, **Default:** `true`

  By default, the Amp TUI copies the selection to the clipboard. When set to `false`, selecting
  text in the thread transcript does not copy it automatically. Press <kbd>Ctrl+C</kbd> to copy an
  active transcript selection manually.

  Either way, the Amp TUI handles mouse input itself, so your terminal's own selection and
  right-click menu do not apply while it runs. Hold <kbd>Shift</kbd> while you drag or right-click
  to use them; this works in most terminals, including Windows Terminal, and in Zellij. Over SSH
  and on Linux without a display server, Amp copies through your terminal with OSC 52.

- **`amp.terminal.detailsExpandedByDefault`**

  **Type:** `boolean`, **Default:** `false`

  Expand thinking and tool call details by default in the CLI transcript. Press
  <kbd>Alt+T</kbd> to collapse or expand details for the current session.

- **`amp.thread.autoArchiveOnQuit`**

  **Type:** `boolean`, **Default:** `false`

  Automatically archive open CLI threads when quitting Amp. This applies to the active thread and
  any background threads connected in the current CLI session.

- **`amp.tools.disable`**

  **Type:** `array`, **Default:** `[]`

  Disable specific tools by name. Use `builtin:toolname` to disable only the built-in tool with that
  name while allowing an MCP server to provide a tool by the same name. Glob patterns using `*` are
  supported.

- **`amp.mcpPermissions`**

  **Type:** `array`, **Default:** `[]`

  Allow or block MCP servers that match a pattern. Amp applies the first matching rule. If no rule
  matches an MCP server, Amp allows it.

  - For a remote MCP server, use the `url` key to match the server endpoint.
  - For a local MCP server, use the `command` and `args` keys to match an executable command and its
    arguments.

  Here are some examples:

  ```json
  "amp.mcpPermissions": [
    // Allow specific trusted MCP servers
    { "matches": { "command": "npx", "args": "* @playwright/mcp@*" }, "action": "allow" },
    { "matches": { "url": "https://mcp.trusted.com/mcp" }, "action": "allow" },
    // Block potentially risky MCP servers
    { "matches": { "command": "python", "args": "*bad_command*" }, "action": "reject" },
    { "matches": { "url": "*/malicious.com*" }, "action": "reject" }
  ]
  ```

  The following rules block all MCP servers:

  ```json
  "amp.mcpPermissions": [
    { "matches": { "command": "*" }, "action": "reject" },
    { "matches": { "url": "*" }, "action": "reject" }
  ]
  ```

- **`amp.updates.mode`**

  **Type:** `string`, **Default:** `"auto"`

  Control update checking. `"warn"` shows update notifications. `"disabled"` turns off update
  checks. `"auto"` installs updates automatically. Setting the `AMP_SKIP_UPDATE_CHECK=1`
  environment variable overrides this setting and disables all update checks.

<a id="enterprise-managed-settings"></a>

## Managed Settings

Workspace administrators can provide managed settings in two ways.

You can add one JSON document in Workspace Settings, on the Managed Settings page. This document uses the same `amp.` keys as `managed-settings.json`. Each save creates a
new revision that records the author and time. Administrators can view the revision history and
compare any two revisions. The workspace document is available to workspaces on any plan that
restrict members to a single workspace.

You cannot set these keys in the workspace document:

- `amp.url` could make the client unable to reach the server that provides the settings.
- `amp.proxy` could make the client unable to reach the server that provides the settings.
- `amp.mcpServers` is configured through the server MCP registry and hosted MCP servers instead.
- `amp.workspaces` is local to one machine.
- `amp.mcpTrustedServers` is local to one machine.

Clients receive workspace changes within about 10 minutes or the next time they start.

Amp also applies `amp.tools.disable` and `amp.tools.enable` from the workspace document on its
servers, so they cover threads that run in orbs, including threads started from Slack, and tools
that run on Amp's servers, such as `code_exec` and `manage_amp`. Running threads pick up changes
within about a minute.

To see which settings your workspace manages, open the CLI command palette with Ctrl+O, or type
`/` in an empty prompt, and run `settings: show managed`. It lists each managed key, its value,
and whether it replaces or merges with a local value.

When the workspace document and your local settings set the same scalar setting, the workspace value
wins. Lists are combined with workspace entries first. Objects are merged key by key, with workspace
values winning conflicts. The local `managed-settings.json` file merges in the same way and takes
precedence over the workspace document.

You can also deploy a local `managed-settings.json` file to each machine that runs Amp. Put the
file in the path for the operating system:

- macOS: `/Library/Application Support/ampcode/managed-settings.json`
- Linux: `/etc/ampcode/managed-settings.json`
- Windows: `%ProgramData%\ampcode\managed-settings.json`

Before distributing a managed settings file, validate it against the <a href="/cli-settings.schema.json" data-sveltekit-reload>Amp settings JSON Schema</a>,
using your editor's schema association or a JSON Schema validator. In `amp.mcpPermissions`, each rule's
`matches` must be an object, such as `{"command":"*"}`, not a string. The workspace settings
editor rejects invalid MCP permissions without changing the saved policy.

Both the local file and the workspace document support one additional field:

- **`amp.admin.compatibilityDate`**

  **Type:** `string`

  The date used to determine which migrations Amp must apply for backward compatibility. Use the
  `YYYY-MM-DD` format, such as `2024-01-15`.

## Proxies and Certificates

When using the Amp CLI in a corporate network with proxy servers or custom certificates, set these
standard Node.js environment variables in your shell profile or CI environment as needed:

```bash
export HTTP_PROXY=your-proxy-url
export HTTPS_PROXY=your-proxy-url
export NODE_EXTRA_CA_CERTS=/path/to/your/certificates.pem
```
