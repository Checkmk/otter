# Configuration Reference

## Table of Contents

1. [Workflow Structure](#workflow-structure)
2. [Workflow Types](#workflow-types)
3. [Step Types](#step-types)
4. [Workspace Configuration](#workspace-configuration)
5. [Resource Limits](#resource-limits)
6. [Triggers](#triggers)
7. [Examples](#examples)

---

## Workflow Structure

Workflows are TOML files in `~/.config/orchestr8r/workflows/`. Each workflow defines a name, type, optional trigger (for triggered workflows), and a sequence of steps.

```toml
name = "my-workflow"
type = "triggered"  # or "looping"

[workspace]         # optional; see Workspace Configuration below
type = "fixed"
path = "/home/user/my-project"

[resources]         # optional; see Resource Limits below
cpu_quota = "200%"

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
- If `[workspace]` is omitted, all steps execute in the run's isolated scratch directory (`~/.local/share/orchestr8r/runs/<run-id>/`).
- See [Workspace Configuration](#workspace-configuration) for all variants including per-run script setup.

---

## Workflow Types

### `looping`

Loops continuously while the daemon runs. After each iteration completes, the workflow restarts from the first step. Typically ends with a checkpoint to allow user control.

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
- All environment variables are inherited from the daemon process

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
- `message` (required): Prompt sent to the agent.
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
- Sends a desktop notification via the system notification daemon
- Does not pause the workflow
- If no `message` is provided, a generic "Workflow step completed" notification is sent
- Notification sending is fire-and-forget; failures do not stop the workflow

---

## Workspace Configuration

The optional `[workspace]` table controls where steps execute. If omitted, each run uses an isolated scratch directory (`~/.local/share/orchestr8r/runs/<run-id>/`).

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
- Relative paths are resolved relative to the daemon's working directory

### `script`

A command is run before each workflow run. Its stdout (trimmed) is used as the workspace path.

```toml
[workspace]
type = "script"
command = ["setup-workspace.sh"]
```

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
BRANCH="orchestr8r-${RUN_ID:0:8}"
git -C ~/my-repo worktree add "/tmp/ws-$RUN_ID" -b "$BRANCH"
echo "/tmp/ws-$RUN_ID"
```

**Behavior by workflow type:**
- **Triggered workflows**: script runs once per trigger event (per run)
- **Looping workflows**: script runs once per iteration

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
orchestr8r start on-demand       # Fire the trigger via CLI
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

**Example:**
```toml
name = "jira-ticket"
type = "triggered"

[trigger]
type = "polling"
poll_command = ["poll-jira", "--poll"]
context_command = ["poll-jira", "--context"]
interval_secs = 600

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
- **No workspace / `scratch`** → Trigger pre-allocates a run directory and creates `<run-dir>/trigger-context/`. Persists in `~/.local/share/orchestr8r/runs/<run-id>/trigger-context/`.
- **No `context_command`** → No context directory is created; the workflow runs without trigger-context files.

**Seen-hash persistence:**
- Hashes returned by `poll_command` are stored at `<data-dir>/triggers/<workflow-name>-seen.json` as a sorted JSON array
- Hashes are marked as seen immediately after polling; if the `context_command` fails, the hash is still considered seen (logged as a warning, not re-polled)
- The seen-hash file survives daemon restarts

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

Example script and workflow are available in the [examples](examples/) directory:
- `polling-simple.sh`
- `polling-trigger.toml`

---

## Example Workflows

Can be found in the [examples](examples/) directory.