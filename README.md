# Orchestr8r

Automate multi-step AI agent tasks triggered by real-world events.

- A **workflow** is a series of steps defined in config, executing inside a container — either **indefinite** (always running while the service is up, looping continuously) or **triggered** (started by an event)
- Each workflow has exactly one running instance at a time — an indefinite workflow only starts the next loop after the previous one completes, never spawning parallel containers
- Workflows are composed of **step plugins** — reusable, installable units of work (e.g., launch agent, spin up container, create worktree, send notification, wait for approval)
- **Triggers** are first-class for event-driven workflows: cron/scheduled, event-driven (email, webhook, file change), or manual
- **Checkpoint steps** pause a workflow for human input — the available actions (continue, stop, feedback) are presented automatically by the UI
- A dashboard lets you monitor running workflows, view logs, and manage step plugins
- Secrets and credentials are managed centrally and injected into containers at runtime
- Example indefinite workflow (always running):
  - Launch custom agent that compares actual codebase to a target_architecture.md and suggest an implementation plan to bring the two together
  - Checkpoint: review the plan
  - Launch a new agent session to implement the plan
  - Checkpoint: notify the Orchestr8r user to review the implementation
  - Push the implementation as a PR
- Example event-driven workflow (email trigger: new PR review requested):
  - Spin up a new container
  - Create worktree with PR
  - Launch custom agent to review the PR and generate review comments
  - Checkpoint: review the comments

## Usage

```
cargo run -- <workflow.toml>
```

### Workflow definition

Workflows are defined in TOML. Each workflow has a name, a kind, and a list of steps:

```toml
name = "arch-sync"
kind = "indefinite"

[[steps]]
type = "workspace"
path = "."

[[steps]]
type = "agent"
command = ["claude", "--print"]
message = "Review the code and create a plan. Write it to plan.md."

[[steps]]
type = "checkpoint"
message = "Review the plan."
```

### Step types

| Type         | Description                                                           | Required fields      |
| ------------ | --------------------------------------------------------------------- | -------------------- |
| `workspace`  | Sets the working directory for subsequent steps                       | `path`               |
| `agent`      | Runs a CLI command with a message as the final argument, saves output | `command`, `message`; optional `session` |
| `shell`      | Runs an arbitrary command                                             | `command`            |
| `checkpoint` | Pauses for human input; actions are shown by the UI                   | `message` (optional) |

### Workflow kinds

- **`indefinite`** — loops continuously; each iteration runs all steps, then starts over until shutdown or rejection
- **`triggered`** — runs once per trigger event; waits idle between firings

### Triggers

| Type     | Description                                              | Required fields |
| -------- | -------------------------------------------------------- | --------------- |
| `manual` | Fired explicitly via the `trigger` subcommand            | —               |

Triggered workflows define their trigger inline:

```toml
# trigger.toml

name = "my-workflow"
kind = "triggered"

[trigger]
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "triggered!"]
```

#### Example usage

```
cargo run -- run trigger.toml
cargo run -- trigger my-workflow
```

The running instance picks up the signal and executes the workflow steps once.
