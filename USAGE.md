# Configuration Reference

## Table of Contents

1. [Workflow Structure](#workflow-structure)
2. [Workflow Types](#workflow-types)
3. [Step Types](#step-types)
4. [Finally Steps](#finally-steps)
5. [Workspace Configuration](#workspace-configuration)
6. [Resource Limits](#resource-limits)
7. [Sandbox Configuration](#sandbox-configuration)
8. [Triggers](#triggers)
9. [Inputs: params and secrets](#inputs-params-and-secrets)
10. [Workflow Management](#workflow-management)
11. [Service Management](#service-management)
12. [Theming](#theming)
13. [Examples](#examples)

---

## Workflow Structure

Workflows are TOML files in `~/.config/otter/workflows/`. Each workflow defines a name, type, optional trigger (for triggered workflows), and a sequence of steps.

```toml
name = "my-workflow"
type = "triggered"  # or "looping"

[workspace]         # optional; see Workspace Configuration below
type = "fixed"
path = "/home/user/my-project"

[resources]         # optional; see Resource Limits below
cpu_quota = "200%"

[sandbox]           # optional; see Sandbox Configuration below

[trigger]  # optional; required if type = "triggered"
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "Step 1"]

[[steps]]
type = "checkpoint"
message = "Review output?"
```

**Workspace behavior:**
- If `[workspace]` is omitted, all steps execute in the run's isolated scratch directory (`~/.local/share/otter/runs/<run-id>/`).
- See [Workspace Configuration](#workspace-configuration) for all variants including per-run script setup.

---

## Workflow Types

### `looping`

Loops continuously while the service runs. After each iteration completes, the workflow restarts from the first step. Typically ends with a checkpoint to allow user control.

```toml
name = "continuous-task"
type = "looping"

[[steps]]
type = "shell"
command = ["./check-status.sh"]

[[steps]]
type = "checkpoint"
message = "Continue to next iteration?"
```

**Behavior:**
- Runs all steps in order
- After last step completes, immediately restarts (or waits if the last step is a checkpoint)
- Use a checkpoint at the end to control when the next iteration starts
- No trigger is used; the workflow loops continuously

### `triggered`

Runs once per trigger event. After completing, the workflow waits idle until the next trigger fires. Events that arrive while a run is in progress are **queued** — the next run starts automatically when the current one finishes.

```toml
name = "on-demand-task"
type = "triggered"

[trigger]
type = "manual"

[[steps]]
type = "agent"
provider = "claude"
message = "Do something"
```

**Behavior:**
- Requires a `[trigger]` section
- Runs once per trigger event
- Only one instance runs at a time; subsequent trigger events are queued
- Workflow transitions to `completed` after finishing successfully

---

## Step Types

All steps have a `type` field specifying their behavior. The following step types are supported:

### `shell`

Runs an arbitrary shell command. Fails the workflow (stops execution) on non-zero exit.

**Fields:**
- `command` (required): Array of strings; the command and arguments to execute

**Example:**
```toml
[[steps]]
type = "shell"
command = ["bash", "-c", "echo 'Hello' && exit 0"]
```

**Behavior:**
- Runs the command in a shell
- Captures stdout/stderr and logs them
- Non-zero exit code causes the workflow to fail and stop
- All environment variables are inherited from the service process

---

### `checkpoint`

Pauses the workflow and waits for human input via the TUI. The user can:
- **Continue** → resume to the next step
- **Stop** → halt the workflow
- **Feedback** → provide text that is sent as a follow-up message to the most recent agent session (if available)

**Fields:**
- `message` (optional): Text displayed in the TUI checkpoint prompt

**Example:**
```toml
[[steps]]
type = "checkpoint"
message = "Review the logs."
```

**Behavior:**
- Pauses execution; the workflow is marked `paused` in the TUI
- If feedback is provided and the previous step was an `agent` step, the feedback is sent to that agent's session as a new message
- If feedback is provided but there is no active agent session, feedback is ignored
- The workflow can be manually stopped from the TUI or CLI

---

### `agent`

Runs an AI CLI tool (Claude, Copilot, or custom) with a message. Supports persistent named sessions that span multiple agent steps within a run.

**Fields:**
- `provider` (optional): `"claude"` or `"copilot"`. Mutually exclusive with `command`.
- `command` (optional): Escape hatch; arbitrary CLI command array. Mutually exclusive with `provider`.
- `message` (required unless `message_file` is set): Prompt sent to the agent.
- `message_file` (optional): Path to a file whose contents are used as the prompt. Resolved relative to the workflow package directory. Mutually exclusive with `message`; only allowed on `agent` steps.
- `session` (optional): Session name. Steps sharing the same session name resume the same conversation within a workflow run.
- `allowed_tools` (optional): List of tool names the agent may use.
  - **Claude**: maps to `--allowed-tools <comma-separated-list>` (e.g., `["Write", "Read"]` → `--allowed-tools Write,Read`)
  - **Copilot**: maps to `--allow-tool=<name>` per entry
- `permission_mode` (Claude-only, optional): Passed as `--permission-mode <value>` (e.g., `"acceptEdits"`)

**Examples:**

Claude with built-in provider:
```toml
[[steps]]
type = "agent"
provider = "claude"
allowed_tools = ["Write", "Read", "Bash"]
permission_mode = "acceptEdits"
message = "Implement the following feature..."
session = "implementation"
```

Copilot:
```toml
[[steps]]
type = "agent"
provider = "copilot"
allowed_tools = ["read_file", "create_file"]
message = "Review this code."
```

Custom CLI (escape hatch):
```toml
[[steps]]
type = "agent"
command = ["aider", "--model", "gpt-4"]
message = "Fix the bug in main.rs"
```

Prompt sourced from a file in the workflow package:
```toml
[[steps]]
type = "agent"
provider = "claude"
message_file = "prompts/implement-feature.md"
```

**Behavior:**
- If `provider` is used, the provider is invoked as a subprocess
- If `command` is used, the command is invoked as-is
- If `session` is specified, the session is created on first use and resumed on subsequent `agent` steps with the same `session` name
- Sessions persist for the entire workflow run; checkpoints and other steps do not affect their lifecycle
- If `session` is not specified, a temporary session is created for that step alone and discarded after
- Agent output (stdout) is captured and logged
- Checkpoint feedback is sent to the active agent session as a follow-up message
- The agent's response to feedback is presented in the checkpoint again until the user continues or stops

---

### `notify`

Sends a desktop notification and immediately continues to the next step. Does not pause execution.

**Fields:**
- `message` (optional): Text displayed in the notification

**Example:**
```toml
[[steps]]
type = "notify"
message = "Build completed successfully!"
```

**Behavior:**
- Sends a desktop notification via the system notification service
- Does not pause the workflow
- If no `message` is provided, a generic "Workflow step completed" notification is sent
- Notification sending is fire-and-forget; failures do not stop the workflow

---

## Finally Steps

The optional `[[finally]]` section defines steps that run after all main steps complete, regardless of outcome. This is useful for cleanup tasks such as releasing workspace resources that would otherwise be skipped when a step fails.

**Fields:**
- All `[[steps]]` fields apply (same step types: `shell`, `agent`, `notify`, `checkpoint`)
- `on` (optional): List of outcomes that trigger this step. Values: `"success"`, `"failed"`, `"stopped"`. If omitted, runs for all outcomes.

**Example:**
```toml
[[finally]]
type = "shell"
command = ["workspace-pool.sh", "release"]
# no `on` — runs for all outcomes

[[finally]]
type = "notify"
message = "Done!"
on = ["success"]
```

**Behavior:**
- If a finally step fails, a warning is logged and execution continues to the next finally step; the run's final status is unchanged
- If workspace setup itself fails before any steps run, finally steps are skipped
- `checkpoint` steps in `[[finally]]` degrade gracefully: if no UI is connected the step is skipped with a warning

---

## Workspace Configuration

The optional `[workspace]` table controls where steps execute. If omitted, each run uses an isolated scratch directory (`~/.local/share/otter/runs/<run-id>/`).

### `scratch` (default)

Steps run in the per-run scratch directory. Equivalent to omitting `[workspace]` entirely.

```toml
[workspace]
type = "scratch"
```

### `fixed`

Steps run in an existing directory on disk.

```toml
[workspace]
type = "fixed"
path = "/home/user/my-project"
```

**Behavior:**
- Path is canonicalized at run time; an error is raised if it does not exist or is not a directory
- Relative paths are resolved relative to the service's working directory

### `script`

A command is run before each workflow run. Its stdout (trimmed) is used as the workspace path.

```toml
[workspace]
type = "script"
command = ["setup-workspace.sh"]
requires = ["GITHUB_TOKEN"]   # optional
```

**Fields:**
- `command` (required): The command to run
- `requires` (optional): Entries from `[require]` to inject as env vars — see [Inputs: params and secrets](#inputs-params-and-secrets)

**Script contract:**
- Invoked as: `<command> <workflow-name> <run-id>`
- Must print exactly one path to stdout (trailing newlines are trimmed)
- Must exit 0; non-zero exit fails the run before any steps execute
- The returned path must exist and be a directory

**Example script (`setup-workspace.sh`):**
```bash
#!/bin/bash
WORKFLOW=$1
RUN_ID=$2
BRANCH="otter-${RUN_ID:0:8}"
git -C ~/my-repo worktree add "/tmp/ws-$RUN_ID" -b "$BRANCH"
echo "/tmp/ws-$RUN_ID"
```

**Behavior by workflow type:**
- **Triggered workflows**: script runs once per trigger event (per run)
- **Looping workflows**: script runs once per iteration

### `git`

Creates a git worktree against a local base repo, checked out at a given ref before any steps run. Two modes:

- **Unpooled** (default): each run gets a fresh worktree inside the run's scratch dir, removed at end of run. Simple, isolated, no cache reuse across runs.
- **Pooled** (add `[workspace.pool]`): a set of locked, reusable worktree slots persisted under `pool.dir`. Build caches (Bazel, Cargo target dirs, etc.) inside the worktree survive across runs. Slots grow on demand and are released back to the pool at end of run.

```toml
[workspace]
type = "git"
base_repo = "/home/user/my-repo"      # required; path to local git repo
ref = "origin/main"                   # optional; default = base_repo HEAD

[workspace.pool]                      # optional; presence enables pooling
dir = "/home/user/otter-slots"        # required; where slot worktrees live
keep_directory_on = ["failed"]        # optional; default = []
```

**Fields:**

- `base_repo` (required): Path to a local git repo. Canonicalized at runtime; must exist.
- `ref` (optional): Any git ref valid in `base_repo` (branch, tag, SHA, `HEAD`, `origin/main`, ...). Default: `HEAD`.
- `pool.dir` (required when `[workspace.pool]` is set): Directory under which slot worktrees and their lock dirs live. Created if missing.
- `pool.keep_directory_on` (optional): List of run outcomes (`"success"`, `"failed"`, `"stopped"`) for which the slot's lock should be **kept** after the run, so the directory can be inspected post-mortem. Default `[]` (always release).

**Behavior:**

- **Unpooled**: `git -C <base_repo> worktree add --detach <scratch>/worktree <ref>`. Cleanup: `git worktree remove --force` at end of run (also drops the registration in `.git/worktrees/`).
- **Pooled**: slots are namespaced per base repo at `pool.dir/<base-repo-ns>/slot-N`, so workflows pointing at different repos can safely share one `pool.dir`. Within a namespace, acquire scans `slot-0`, `slot-1`, ... for a free slot; if all locked, creates `slot-N`. Locking uses an atomic `mkdir <slot>.lock` (cross-platform). The slot is reset to `ref` on acquire (`checkout --detach`, `reset --hard`, `clean -fd`). Stale locks (older than 24h) are broken automatically on the next acquire.
- Trigger-context, when used with polling triggers, is written to `<workspace>/trigger-context/` like any non-scratch workspace.

**Note:** `[workspace.pool]` is only valid when `type = "git"`. Combining it with `scratch`, `fixed`, or `script` is rejected at workflow-load time.

---

## Resource Limits

The optional `[resources]` table controls resource usage for all subprocess steps in a workflow
(agent steps and shell steps). If omitted, steps run without CPU limits.

### `cpu_quota`

Limits total CPU time for the entire process tree of each spawned subprocess (the agent CLI and
all child processes it spawns).

**Fields:**
- `cpu_quota` (optional): CPU quota in systemd `CPUQuota` format. `"100%"` = 1 core, `"200%"` = 2 cores, etc.

**Example:**
```toml
[resources]
cpu_quota = "200%"  # cap the whole run to 2 CPU cores
```

**Behavior:**
- Requires Linux with a running systemd user instance (`systemd --user`)
- Implemented via `systemd-run --scope --user -p CPUQuota=<value>` wrapping each subprocess invocation
- The cgroup quota applies to the subprocess and all its children (e.g. `claude` → `bash` → `testing framework`)
- If `systemd-run` is not available, a warning is logged and the workflow runs without CPU limiting

---

## Inputs: params and secrets

Workflows often need values that differ per install — a repo path, an API token, a username. Declare each one in `[require]`; `otter workflow install` prompts for it and either saves the value to a sidecar (non-sensitive) or stores it encrypted in the OS keyring (sensitive). The installed `workflow.toml` is left intact as a template; references are resolved when the daemon loads it.

```toml
[require.REPO_PATH]
description = "Path to your local repo"
default = "~/work/my-repo"        # optional; non-sensitive entries only

[require.JIRA_PAT]
description = "Personal access token from id.atlassian.com → API tokens"
sensitive = true                  # optional; default false

[workspace]
type = "git"
base_repo = "{{REPO_PATH}}"       # ← non-sensitive: substituted by daemon

[[steps]]
type = "shell"
command = ["./deploy.sh"]
requires = ["JIRA_PAT"]           # ← sensitive: injected as env var at runtime
```

**Non-sensitive entries** can be referenced anywhere in the workflow as `{{NAME}}` — `[workspace] base_repo`, `pool.dir`, `path`, agent messages, shell argv, polling commands. Resolved values are saved to `<workflow>/.otter-state/values.toml` (a flat `KEY = "value"` table). The daemon reads this sidecar and substitutes `{{NAME}}` into the workflow when it loads. The on-disk `workflow.toml` always stays as the original template, so `cat` shows exactly what the author shipped.

**Sensitive entries** (`sensitive = true`) are prompted with hidden input and stored encrypted at `~/.config/otter/secrets.age`. The encryption key lives in the OS keyring (libsecret on Linux, Keychain on macOS, Credential Manager on Windows).

**Injecting values into subprocess env** — `requires = ["NAME", ...]` on `[[steps]]`, `[[finally]]`, polling `[trigger]`, or script `[workspace]` injects the resolved value into the subprocess env under the declared name. Sensitive names are fetched from the keyring; non-sensitive names are read from `<workflow>/.otter-state/values.toml` on each invocation, so `otter workflow configure` edits take effect on the next run without a daemon reload. The subprocess otherwise sees a clean environment (only `PATH`, `HOME`, `USER`, `TMPDIR`, etc., are kept).

**Rules:**

- Names match `^[A-Z][A-Z0-9_]*$` and may not shadow safe system vars (`PATH`, `HOME`, `USER`, `SHELL`, `TMPDIR`, `PWD`, `LANG`, `LC_*`).
- `{{NAME}}` references only work for non-sensitive entries (substituting a secret would write it to disk in cleartext).
- `requires = [...]` accepts both sensitive and non-sensitive entries.
- Sensitive entries are global by name: two workflows that both declare `[require.JIRA_PAT]` share one keyring entry — the second install finds it already set and skips the prompt.

**Commands:**

`[require]` values are gathered at install time and editable afterwards with `otter workflow configure` — see [Workflow management](#installing). Secrets are also addressable directly:

```bash
otter secret set <name> <value>                  # store or overwrite a secret directly
otter secret get <name>                          # print value
otter secret list                                # list all stored secret names
otter secret delete <name>                       # remove
```

**⚠ Back up your keyring.** The encryption key lives only in the OS keyring. If the keyring is wiped (OS reinstall, machine migration, accidental delete), `secrets.age` becomes permanently unreadable — there is no recovery path.

---

## Sandbox Configuration

The optional `[sandbox]` table enables filesystem and process isolation for shell and agent steps using rootless [Podman](https://podman.io) containers with security hardening (dropped capabilities, read-only rootfs, tmpfs for writable areas).

**Prerequisites:** Podman must be installed and accessible. If Podman is not available, sandboxed steps fail.

### Workflow-level configuration

```toml
[sandbox]
image = "localhost/agentbox:latest"  # optional; default image
network = "none"                      # optional; "bridge" (default) or "none"
```

The presence of `[sandbox]` enables sandboxing for all shell and agent steps. To use the defaults (default image, bridge networking), an empty `[sandbox]` section suffices.

**Fields:**
- `image` (optional): Container image to use. Defaults to `localhost/agentbox:latest`
- `network` (optional): Network mode — `"bridge"` (default, allows network access) or `"none"` (full network isolation)

### Per-step override

Individual steps can opt in or out of sandboxing:

```toml
[sandbox]

[[steps]]
type = "agent"
provider = "claude"
message = "Review code."
# inherits sandbox from workflow level

[[steps]]
type = "shell"
command = ["git", "push"]
sandbox = false                       # opt out for this step
```

**Resolution logic:** When `[sandbox]` is defined, all shell and agent steps are sandboxed unless the step sets `sandbox = false`. When `[sandbox]` is absent, steps run unsandboxed unless a step sets `sandbox = true`.

### Workspace and scripts

- The workspace directory is bind-mounted at `/workspace` inside the container
- If the workflow has companion scripts (package directory), the scripts directory is bind-mounted at `/opt/scripts:ro` and added to `PATH`

### Building the default image

The `agentbox` CLI can build and manage the default container image:

```bash
agentbox build                        # build default image
agentbox build --tag my-image:v1      # build with custom tag
agentbox export-template              # export Containerfile for customization
```

### Standalone usage

`agentbox` can also be used standalone to run any command in a sandbox:

```bash
# Interactive sandboxed Claude Code session
agentbox run --workspace ~/my-project -- claude

# With network isolation
agentbox run --workspace ~/my-project --network none -- claude

# Non-interactive YOLO mode
agentbox run --no-tty --workspace ~/my-project -- claude --permission-mode bypassPermissions --print "fix the bug"
```

---

## Triggers

Triggers define how a `triggered` workflow is started. Only `triggered` workflows require a trigger.

### `manual`

Triggered explicitly via CLI or TUI.

**Fields:**
- `type` (required): `"manual"`

**Example:**
```toml
name = "on-demand"
type = "triggered"

[trigger]
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "Running on demand"]
```

**Usage:**
```bash
otter start on-demand       # Fire the trigger via CLI
```

Or click "Start" in the TUI dashboard.

**Behavior:**
- The workflow waits idle until explicitly triggered
- Each trigger fires one instance of the workflow
- If a trigger arrives while a run is in progress, the event is queued

---

### `polling`

Polls an external event source on a configurable interval.

**Fields:**
- `type` (required): `"polling"`
- `poll_command` (required): Array of strings; the command to run on each poll cycle. stdout must be a JSON array of strings (event identifiers/hashes), exit 0 on success
- `context_command` (optional): Array of strings; the command to run for each new hash. Invoked as `<context_command> <hash> <context-dir>`, which should write trigger context files to `<context-dir>`. If omitted, no context directory is created and the workflow runs without trigger-context files.
- `interval_secs` (optional, default: 600): Polling interval in seconds
- `requires` (optional): Entries from `[require]` to inject as env vars into both `poll_command` and `context_command` — see [Inputs: params and secrets](#inputs-params-and-secrets)

**Example:**
```toml
name = "jira-ticket"
type = "triggered"

[require.JIRA_PAT]
description = "Jira personal access token"
sensitive = true

[trigger]
type = "polling"
poll_command = ["poll-jira", "--poll"]
context_command = ["poll-jira", "--context"]
interval_secs = 600
requires = ["JIRA_PAT"]

[[steps]]
type = "shell"
command = ["cat", "trigger-context/issue.json"]

[[steps]]
type = "agent"
provider = "claude"
message = "Implement the JIRA issue described in trigger-context/issue.json."
```

**Context directory location (adaptive):**
- **`fixed` or `script` workspace** → Context is written to `<workspace>/trigger-context/`. For `script` workspaces, the workspace is set up first (via the script), then the context command runs inside it.
- **No workspace / `scratch`** → Trigger pre-allocates a run directory and creates `<run-dir>/trigger-context/`. Persists in `~/.local/share/otter/runs/<run-id>/trigger-context/`.
- **No `context_command`** → No context directory is created; the workflow runs without trigger-context files.

**Seen-hash persistence:**
- Hashes returned by `poll_command` are stored at `<data-dir>/triggers/<workflow-name>-seen.json` as a sorted JSON array
- Hashes are marked as seen immediately after polling; if the `context_command` fails, the hash is still considered seen (logged as a warning, not re-polled)
- The seen-hash file survives service restarts

**Behavior:**
- Runs `poll_command` on the configured interval
- Parses stdout as a JSON array of strings (event identifiers/hashes)
- For each new hash (not in the seen-hash file):
  1. Adds it to seen-hash file and persists immediately
  2. If `context_command` is set: creates context directory and runs `<context_command> <hash> <context-dir>`
  3. If context command exits non-zero, logs a warning and skips (hash remains marked seen)
  4. Sends a `TriggerEvent` with the hash as payload
- Each new hash fires exactly one workflow run; multiple hashes from one poll cycle fire multiple runs (queued sequentially)
- Polling continues indefinitely; errors do not stop the trigger

**Script contracts:**

```bash
# Poll command — run as-is, no extra arguments appended
$ <poll_command...>
# stdout: JSON array of strings (e.g., ["hash1", "hash2"])
# exit 0 = success; non-zero = skip this poll cycle

# Context command (optional) — hash and context-dir appended as positional args
$ <context_command...> <hash> <context-dir>
# Creates files in <context-dir> with trigger-specific data
# exit 0 = success; non-zero = skip this hash (already marked seen)
```

Example script and workflow are available in the [examples/polling-simple](examples/polling-simple) directory.

---

## Workflow management

A **workflow package** is a directory containing a `workflow.toml` plus any companion scripts used by the workflow's steps. Companion scripts in the package directory are automatically prepended to `PATH` when any step in that workflow runs.

### Package layout

```
my-workflow/
├── workflow.toml          # required — the workflow definition
├── poll.sh                # companion script referenced by trigger or steps
└── setup-workspace.sh     # companion script for workspace provisioning
```

### Optional metadata fields

`workflow.toml` may include top-level metadata fields:

```toml
name = "my-workflow"
type = "triggered"
schema = 1
version = "1.2.0"     # human-readable package version (used for marketplace update detection)
description = "..."   # one-liner shown in marketplace install previews
```

**`schema`** declares the minimum otter workflow schema version required to run this workflow. If the installed otter does not support the version, it logs a warning and skips the workflow.

### Installing

`otter workflow install <target>` is idempotent: it installs the workflow if it's new, or upgrades it in place if it's already installed — preserving saved `[require]` values, leaving keyring secrets untouched, and only prompting for newly-introduced keys.

`<target>` accepts:

- **a local path** — `.toml` file or package directory.
- **`<name>@<marketplace>`** — package in a registered marketplace; the README and `[require]` table are previewed with a y/n confirm before any prompts. See [Marketplaces](#marketplaces).
- **bare `<name>`** — already-installed workflow; re-resolves the package from the marketplace it was installed from. Re-installing at the same upstream version is a no-op.

```bash
otter workflow install ./my-workflow/             # local package directory
otter workflow install ./my-workflow.toml         # local .toml file (no companion scripts)
otter workflow install jira-sync@acme             # from a registered marketplace
otter workflow install jira-sync                  # refresh from origin marketplace
otter workflow install jira-sync --force          # discard saved values and reinstall fresh
otter workflow list                               # list installed workflows
```

The installed workflow lives at `~/.config/otter/workflows/<name>/`.

### Reconfiguring and removing

```bash
otter workflow configure <name>                   # re-prompt for [require] values; Enter keeps current
otter workflow configure <name> --reset-secrets   # also re-prompt sensitive entries

otter workflow remove <name>                      # delete the installed workflow
```

### Auto-start on daemon startup

```bash
otter workflow enable <name>    # start automatically on next daemon start
otter workflow disable <name>   # stop auto-starting
```

---

## Marketplaces

A **marketplace** is a git repository publishing a curated set of installable workflow packages. Register one once, then install workflows from it by name (see [Workflow management → Installing](#installing) for the install command itself). The daemon `git fetch`es each registered marketplace on startup; new upstream versions surface as `update available: X.Y.Z` annotations in `otter status`, and `otter workflow install <name>` pulls them in.

> ⚠ Marketplaces are user-vetted. `marketplace add` clones whatever URL you
> point it at, and the workflows from a marketplace run arbitrary shell and
> agent steps once installed. Inspect the marketplace's source before adding it.

### Registering and unregistering

```bash
otter marketplace add <git-url>                   # register under the manifest's declared name
otter marketplace remove <marketplace>            # unregister & delete the clone
```

Removing a marketplace leaves already-installed workflows in place; they're surfaced in `otter status` as `origin marketplace removed`.

### Marketplace repo contract

A marketplace is a git repo with a single registry file at the root:

```toml
# .otter-marketplace.toml
schema = 1
name = "acme"                # required canonical identity (alphanumeric, `-`, `_`)

[[workflow]]
path = "workflows/hello-world"

[[workflow]]
path = "workflows/jira-sync"
wip = true   # listed but hidden from default browse output
```

The `name` is the handle under which the marketplace is registered locally and the value recorded in every installed workflow's origin sidecar — it's the marketplace's stable identity across machines.

Each `path` points at a workflow **package directory** containing the required `workflow.toml` (with `version` for update detection, optional `description` for listings) plus an optional `README.md` (rendered verbatim as the install preview before prompts).

This repository doubles as a working marketplace — see the top-level [.otter-marketplace.toml](.otter-marketplace.toml) and the packages under [examples/](examples/) for a copy-and-edit starting point.

---

## Service Management

The otter service must be running before the TUI or CLI commands can connect to it.
For boot-time persistence (start on login), enable the platform service:

```bash
otter status            # show (service) status
otter service start     # start the service for this session only
otter service stop      # stop the running service
otter service restart   # stop (if running) and start the service again
otter service enable    # start service and register start-on-boot
otter service disable   # disable automatic startup and stop the service
```

---

## Theming

The TUI ships with built-in `dark` (default) and `light` themes. Select one in `~/.config/otter/config.toml`:

```toml
[theme]
mode = "auto"   # "dark" | "light" | "auto" | "<custom-name>"
```

- `dark` / `light` — use the bundled theme of that name.
- `auto` — follow the OS color-scheme preference, read once at TUI launch. Restart the TUI to pick up an OS change.
- `<custom-name>` — load `~/.config/otter/themes/<custom-name>.toml`.

### Custom themes

Seed files for the bundled themes are written to `~/.config/otter/themes/dark.toml` and `light.toml` on first launch. Copy one, edit, and select it by name:

```bash
cp ~/.config/otter/themes/light.toml ~/.config/otter/themes/my-theme.toml
# then set mode = "my-theme" in config.toml
```

- Colors are 6-digit `#rrggbb` hex; omitted fields fall back to the bundled dark palette.
- Editing `dark.toml` / `light.toml` has no effect — those modes always use the bundled palette. Use a custom name to override.

---

## Example Workflows

Can be found in the [examples](examples/) directory.
