# Orchestr8r

Automate multi-step tasks (with AI support) triggered by real-world events.

- A **workflow** is a series of steps defined in TOML — either **indefinite** (loops continuously while the service is up) or **triggered** (runs once per trigger event)
- Each workflow has exactly one running instance at a time — an indefinite workflow only starts the next iteration after the previous one completes
- **Checkpoint steps** pause for human input; the TUI presents continue / stop / feedback actions
- **Agent steps** drive Claude (or any AI CLI) with persistent named sessions across steps within a run
- A TUI dashboard monitors running workflows, views logs, and controls execution

## Prerequisites

- Rust toolchain (`cargo`)
- [Claude CLI](https://docs.anthropic.com/en/docs/claude-code) (`claude`) — required for `agent` steps

## Installation

```
cargo build --release
# Binary is at: target/release/orchestr8r
```

## Quick start

1. **Create the workflow directory:**

   ```
   mkdir -p ~/.config/orchestr8r/workflows
   ```

2. **Add a workflow** (e.g. `~/.config/orchestr8r/workflows/hello.toml`):

   ```toml
   name = "hello-world"
   kind = "indefinite"

   [[steps]]
   type = "shell"
   command = ["echo", "Hello from orchestr8r!"]

   [[steps]]
   type = "checkpoint"
   message = "Continue to next iteration?"
   ```

3. **Start the daemon** (in a separate terminal or as a background service):

   ```
   orchestr8r daemon
   ```

4. **Open the dashboard:**

   ```
   orchestr8r
   ```

   or start a workflow directly:

   ```
   orchestr8r start hello-world
   ```

---

## Usage

Workflows are loaded from `~/.config/orchestr8r/workflows/*.toml` when
the daemon starts.

### Start the daemon

```
orchestr8r daemon
```

The daemon starts headless — all workflows are dormant until explicitly started. A Unix socket at `~/.local/share/orchestr8r/orchestr8r.sock` is the only control interface.

### Open the dashboard

```
orchestr8r
```

The dashboard connects to the daemon via the Unix socket. It shows workflow states, live step output, and checkpoint prompts. Prints a helpful error with instructions if no daemon is running.

### Control workflows via CLI

```
orchestr8r help                 # show all commands
orchestr8r status               # list all workflows and their state
orchestr8r start <name>         # start a dormant workflow
orchestr8r pause <name>         # pause a running indefinite workflow between iterations
orchestr8r resume <name>        # resume a paused workflow
orchestr8r stop <name>          # stop a running workflow
```
---

## Workflow definition

Workflows are TOML files with a name, a kind, and a list of steps.

### Workflow kinds

- **`indefinite`** — loops continuously; each iteration runs all steps end-to-end, then restarts. A `checkpoint` step at the end of the loop lets you stop or continue each cycle.
- **`triggered`** — runs once per trigger event; waits idle between firings.

### Step types

| Type         | Description                                                               | Fields                               |
| ------------ | ------------------------------------------------------------------------- | ------------------------------------ |
| `workspace`  | Sets the working directory for subsequent steps                           | `path` (required)                    |
| `agent`      | Runs an AI CLI tool with a message; supports named persistent sessions    | `command`, `message` (required); `session` (optional) |
| `shell`      | Runs an arbitrary shell command; fails the workflow on non-zero exit      | `command` (required)                 |
| `checkpoint` | Pauses for human review; TUI presents continue / stop / feedback actions | `message` (optional)                 |
| `notify`     | Sends a desktop notification and continues                               | `message` (optional)                 |

#### `agent` sessions

The optional `session` field on `agent` steps groups steps into a shared conversation within a single run. Steps with the same session name share context — the `command` value is only required on the first step that creates the session. Steps without a `session` get an isolated per-step session.

```toml
[[steps]]
type = "agent"
command = ["claude", "--print"]
session = "planner"
message = "Read plan.md and create an implementation plan."

[[steps]]
type = "agent"
session = "planner"           # resumes the same session — no command needed
message = "Summarise the plan in one sentence."
```

---

## Examples

### Minimal shell workflow

```toml
# ~/.config/orchestr8r/workflows/hello-world.toml
name = "hello-world"
kind = "indefinite"

[[steps]]
type = "shell"
command = ["echo", "Hello from orchestr8r!"]

[[steps]]
type = "checkpoint"
message = "Continue to next iteration?"
```

### AI architecture sync (indefinite)

Continuously keeps the codebase aligned with a target architecture document. Found in `examples/arch-sync.toml`.

```toml
name = "arch-sync"
kind = "indefinite"

[[steps]]
type = "workspace"
path = "."

[[steps]]
type = "agent"
command = ["claude", "--allowed-tools", "Write,Read"]
message = "Save the final plan to plan.md (overwriting it if required): Review README.md, TARGET_ARCHITECTURE.md and the current code. Identify the best next step to align them and create an implementation plan."

[[steps]]
type = "checkpoint"
message = "Review the agent's plan."

[[steps]]
type = "agent"
command = ["claude", "--permission-mode", "acceptEdits"]
message = "Read plan.md and implement it."

[[steps]]
type = "checkpoint"
message = "Review the implementation."
```

At each checkpoint the TUI shows **Continue**, **Stop**, or **Feedback** — choosing Feedback lets you type a correction that is passed back to the agent before continuing.

### Triggered workflow (manual)

Runs once per explicit `start` call. Found in `examples/hello-triggered.toml`.

```toml
name = "hello-triggered"
kind = "triggered"

[trigger]
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "triggered workflow ran!"]
```

---

## Triggers

| Type     | Description                                                           |
| -------- | --------------------------------------------------------------------- |
| `manual` | Fired explicitly via `orchestr8r start <name>` or management console  |
| ...      | More to be added (mail? react to code reviews?)                       |

Triggered workflows declare their trigger in an inline `[trigger]` table:

```toml
name = "my-workflow"
kind = "triggered"

[trigger]
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "hello"]
```
