# Configuration Reference

## Table of Contents

1. [Workflow Structure](#workflow-structure)
2. [Workflow Kinds](#workflow-kinds)
3. [Step Types](#step-types)
4. [Triggers](#triggers)
5. [Examples](#examples)

---

## Workflow Structure

Workflows are TOML files in `~/.config/orchestr8r/workflows/`. Each workflow defines a name, kind, optional trigger (for triggered workflows), and a sequence of steps.

```toml
name = "my-workflow"
kind = "indefinite"  # or "triggered"
workspace = "/home/user/my-project"  # optional

[trigger]  # optional; required if kind = "triggered"
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "Step 1"]

[[steps]]
type = "checkpoint"
message = "Review output?"
```

**Workspace behavior:**
- `workspace`: (optional) Absolute or relative path. All steps execute in this directory. Relative paths are resolved relative to the daemon's working directory.
- If `workspace` is omitted, all steps execute in the run's isolated scratch directory (`~/.local/share/orchestr8r/runs/<run-id>/`).

---

## Workflow Kinds

### `indefinite`

Loops continuously while the daemon runs. After each iteration completes, the workflow restarts from the first step. Typically ends with a checkpoint to allow user control.

```toml
name = "continuous-task"
kind = "indefinite"

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
- No trigger is used; the workflow runs indefinitely

### `triggered`

Runs once per trigger event. After completing, the workflow waits idle until the next trigger fires. Events that arrive while a run is in progress are **queued** — the next run starts automatically when the current one finishes.

```toml
name = "on-demand-task"
kind = "triggered"

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

## Triggers

Triggers define how a `triggered` workflow is started. Only `triggered` workflows require a trigger.

### `manual`

Triggered explicitly via CLI or TUI.

**Fields:**
- `type` (required): `"manual"`

**Example:**
```toml
name = "on-demand"
kind = "triggered"

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

Polls an external event source on a configurable interval. Uses a user-supplied script that adheres to a two-command interface for fetching event hashes and writing trigger-specific context.

**Fields:**
- `type` (required): `"polling"`
- `command` (required): Array of strings; the command to invoke. The command must support two modes:
  - `<command> --poll` → stdout: JSON array of strings (event identifiers), exit 0 on success
  - `<command> --context <hash> <context-dir>` → writes trigger context files to `<context-dir>`, exit 0 on success
- `interval_secs` (optional, default: 600): Polling interval in seconds

**Example:**
```toml
name = "jira-ticket"
kind = "triggered"

[trigger]
type = "polling"
command = ["poll-jira"]
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
- **No workspace configured** → Trigger pre-allocates a run directory and creates `<run-dir>/trigger-context/`. Persists in `~/.local/share/orchestr8r/runs/<run-id>/trigger-context/`.
- **Workspace configured** → Context is written to `<workspace>/trigger-context/` (stable across runs). Scripts may clean up if desired — orchestr8r does not remove context directories.

**Seen-hash persistence:**
- Hashes returned by `--poll` are stored at `<data-dir>/triggers/<workflow-name>-seen.json` as a sorted JSON array
- Hashes are marked as seen immediately after polling; if the `--context` command fails, the hash is still considered seen (logged as a warning, not re-polled)
- The seen-hash file survives daemon restarts

**Behavior:**
- Runs `<command> --poll` on the configured interval
- Parses stdout as a JSON array of strings (event identifiers/hashes)
- For each new hash (not in the seen-hash file):
  1. Adds it to seen-hash file and persists immediately
  2. Creates context directory and runs `<command> --context <hash> <context-dir>`
  3. If context command exits non-zero, logs a warning and skips (hash remains marked seen)
  4. Sends a `TriggerEvent` with the hash as payload
- Each new hash fires exactly one workflow run; multiple hashes from one poll cycle fire multiple runs (queued sequentially)
- Polling continues indefinitely; errors do not stop the trigger

**Script contract:**

The polling command must be executable and support these two modes:

```bash
# Mode 1: Poll for events
$ <command> --poll
# stdout: JSON array of strings (e.g., ["hash1", "hash2"])
# exit 0 = success; non-zero = skip this poll cycle

# Mode 2: Write context
$ <command> --context <hash> <context-dir>
# Creates files in <context-dir> with trigger-specific data
# exit 0 = success; non-zero = skip this hash (already marked seen)
```

Example script is available in the [examples](examples/) directory:
- `polling-simple.sh`

---

## Examples

Can be found in the [examples](examples/) directory.