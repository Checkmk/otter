# Orchestr8r — Target Architecture

## Overview

Orchestr8r is a **Rust-native workflow automation service** that executes multi-step AI agent tasks in response to real-world events. It runs as a local daemon with a TUI dashboard, deployable on a local machine or self-hosted server with a path to cloud environments.

---

## Core Principles

- **Built in Rust** — no FFI boundaries; a single language across the entire codebase
- **Zero required external services** — works fully offline with SQLite and local container runtime
- **Pluggable by design** — storage, container runtime, triggers, and notifications are abstracted behind traits
- **Security boundary** — secrets are isolated from agent processes; agents receive only what they need via injection

---

## Architecture Layers

```
┌─────────────────────────────────────────────────────┐
│                    TUI Dashboard                    │  (ratatui)
├─────────────────────────────────────────────────────┤
│                   orchestr8r-core                   │
│  ┌────────────┐  ┌──────────────┐  ┌─────────────┐  │
│  │  Scheduler │  │ Workflow     │  │  Step       │  │
│  │  & Trigger │  │ Engine       │  │  Executor   │  │
│  │  Manager   │  │              │  │             │  │
│  └────────────┘  └──────────────┘  └─────────────┘  │
├─────────────────────────────────────────────────────┤
│               Plugin & Trait Layer                  │
│  ContainerRuntime │ StorageBackend │ Notifier       │
│  TriggerSource    │ SecretStore    │ AgentRunner    │
├─────────────────────────────────────────────────────┤
│              Built-in Implementations               │
│  Docker/Podman    │ SQLite         │ Desktop notif  │
│  Cron/Webhook/    │                │                │
│  File/Email       │                │                │
└─────────────────────────────────────────────────────┘
```

---

## Crate Structure

```
orchestr8r/
├── orchestr8r-core/        # Core engine, scheduler, workflow runner
├── orchestr8r-tui/         # ratatui-based TUI dashboard
├── orchestr8r-plugin-api/  # Shared traits for steps, triggers, notifiers
├── orchestr8r-runtime/     # Container runtime abstraction + Docker/Podman impl
├── orchestr8r-storage/     # StorageBackend trait + SQLite impl
├── orchestr8r-secrets/     # SecretStore trait + encrypted local file impl
├── orchestr8r-triggers/    # TriggerSource trait + built-in trigger impls
├── orchestr8r-notify/      # Notifier trait + desktop notification impl
└── orchestr8r-cli/         # Binary entrypoint, config loading, daemon setup
```

---

## Plugin Model

All extensibility points (step executors, trigger sources, container runtimes, storage backends, notifiers, secret stores, agent runners) are defined as Rust traits. In the initial implementation, all built-in implementations are compiled directly into the binary — there is no dynamic plugin loading (no `.so` files, WASM modules, or subprocess-based plugins).

The trait boundaries are intentionally designed to allow dynamic loading to be introduced later (e.g., via WASM sandboxing or a subprocess protocol) without restructuring the core engine.

---

## Workflow Engine

### Workflow Definition (TOML)

```toml
[[workflows]]
name = "pr-review"
trigger = "email-pr-review"    # references a trigger plugin
kind = "triggered"             # or "indefinite"

[[workflows.steps]]
type = "container"
image = "my-agent:latest"
command = ["review-pr"]
secrets = ["GITHUB_TOKEN"]

[[workflows.steps]]
type = "checkpoint"
message = "Review the comments."
notify = ["desktop"]
```

- **Indefinite workflows** loop continuously; the next iteration begins only after the previous completes
- **Triggered workflows** start on an event and run to completion (or failure)
- Exactly one instance of each workflow runs at a time; if a trigger fires while an instance is running, the event is **queued** and processed when the current run finishes

### Step Types

| Step Type    | Description                                                                           |
| ------------ | ------------------------------------------------------------------------------------- |
| `container`  | Launches a container via the `ContainerRuntime` trait                                 |
| `checkpoint` | Pauses for human input; awaits accept/reject/feedback in the TUI (see Agent Sessions) |
| `agent`      | Invokes an `AgentRunner` within an agent session (see Agent Sessions)                 |
| `worktree`   | Creates a git worktree for isolated work; cleanup is a dedicated plugin/step          |
| `notify`     | Sends a notification without pausing                                                  |
| `shell`      | Runs an arbitrary shell command in a sandbox                                          |

```rust
#[async_trait]
pub trait StepExecutor: Send + Sync {
    async fn execute(&self, ctx: &StepContext) -> Result<StepOutput, StepError>;
    fn step_type(&self) -> &'static str;
}
```

---

## Trigger System

```rust
#[async_trait]
pub trait TriggerSource: Send + Sync {
    fn name(&self) -> &str;
    async fn subscribe(&self, tx: Sender<TriggerEvent>) -> Result<(), TriggerError>;
}
```

Built-in trigger implementations:

| Trigger      | Mechanism                                |
| ------------ | ---------------------------------------- |
| `cron`       | `tokio-cron-scheduler` or similar        |
| `webhook`    | Embedded HTTP listener (`axum`)          |
| `file-watch` | `notify` crate (inotify/FSEvents/kqueue) |
| `email`      | IMAP IDLE (push-based)                   |
| `manual`     | TUI or CLI command                       |

---

## Container Runtime Abstraction

```rust
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    async fn run(&self, spec: ContainerSpec) -> Result<ContainerHandle, RuntimeError>;
    async fn wait(&self, handle: ContainerHandle) -> Result<ExitStatus, RuntimeError>;
    async fn logs(&self, handle: &ContainerHandle) -> Result<LogStream, RuntimeError>;
    async fn stop(&self, handle: &ContainerHandle) -> Result<(), RuntimeError>;
}
```

Initial implementation: **Docker** via the `bollard` crate. Podman works without code changes by pointing to the Podman socket.

---

## Storage Layer

```rust
pub trait StorageBackend: Send + Sync {
    fn save_workflow_run(&self, run: &WorkflowRun) -> Result<()>;
    fn load_workflow_runs(&self, filter: RunFilter) -> Result<Vec<WorkflowRun>>;
    fn append_log(&self, run_id: RunId, entry: LogEntry) -> Result<()>;
    fn save_checkpoint(&self, checkpoint: &CheckpointState) -> Result<()>;
    // ...
}
```

Initial implementation: **SQLite** via `sqlx` (sqlite feature) or `rusqlite`. Database at `~/.local/share/orchestr8r/state.db`.

---

## Secrets Management

```rust
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<SecretValue, SecretError>;
    fn set(&self, key: &str, value: SecretValue) -> Result<(), SecretError>;
    fn list_keys(&self) -> Result<Vec<String>, SecretError>;
}
```

Initial implementation: **encrypted local file** using `age`. The encryption key is derived from a passphrase stored in the OS keychain (`keyring` crate). Secrets file: `~/.local/share/orchestr8r/secrets.age`.

Agents receive secrets only via explicit injection at runtime — only secrets declared in the step's `secrets` list are visible to the subprocess.

---

## Notification System

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError>;
}
```

Initial implementation: **desktop notifications** via `notify-rust`. Notifications are **informational only** — the user must open the TUI to respond to a checkpoint.

---

## AI Agent Integration

### Agent Sessions

An **agent session** is a long-lived conversational context that persists across steps within a workflow run. Sessions are identified by an explicit `session` field on agent steps. This enables:

- **Multi-prompt sequences:** Multiple `agent` steps with the same `session` name send sequential prompts to the same session, preserving full conversation context
- **Checkpoint feedback:** When a checkpoint follows an agent step and the user chooses "feedback", the feedback text is sent as an additional prompt to the agent's session — the agent responds, and the checkpoint re-presents the result. This loop repeats until the user accepts or rejects.

An agent step **without** a `session` field starts a fresh, single-use session that is discarded after the step completes. An agent step **with** a `session` field creates the session on first use and resumes it on subsequent steps with the same name. The `command` field is only required on the first agent step that creates the session.

A checkpoint always targets the most recent agent session (whether named or single-use). Feedback is only available when the preceding agent session is still alive — i.e., when the previous step was an agent, or a named session is still open.

```toml
[[steps]]
type = "agent"
session = "planner"
command = ["claude", "--print"]
message = "Review the code and create a plan."

[[steps]]
type = "checkpoint"
message = "Review the plan."

[[steps]]
type = "agent"
session = "planner"
message = "Now implement the plan."
```

Session lifecycle:
1. An `agent` step with a `session` name creates the session (first use) or resumes it
2. An `agent` step without `session` creates a temporary session scoped to that step
3. Named sessions stay alive for the entire workflow run — checkpoints and other steps do not affect their lifecycle
4. All sessions are cleaned up when the workflow run completes (or fails)

### AgentRunner Trait

```rust
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn start(&self, spec: AgentSpec) -> Result<AgentSession, AgentError>;
    async fn prompt(&self, session: &mut AgentSession, message: &str) -> Result<AgentOutput, AgentError>;
    async fn stop(&self, session: AgentSession) -> Result<(), AgentError>;
}
```

Initial implementation: subprocess runner that shells out to CLI agent tools (e.g., `claude`, `aider`, custom scripts). Direct API integration (Anthropic, OpenAI) can be added as additional `AgentRunner` implementations.

---

## TUI Dashboard

The TUI is the primary user interface for:

- Viewing running and completed workflow instances with live log streaming
- Responding to checkpoint steps (accept / reject / provide feedback)
- Managing workflow enable/disable
- Viewing plugin/trigger status

The TUI communicates with the core daemon via an **in-process channel** when running as a single binary, or via a **Unix domain socket / named pipe** when attaching to a headless daemon.

---

## Configuration

All configuration lives in `~/.config/orchestr8r/`:

```
~/.config/orchestr8r/
├── config.toml          # Global daemon settings
└── workflows/
    ├── pr-review.toml
    └── arch-sync.toml
```

`config.toml` example:

```toml
[storage]
backend = "sqlite"
path = "~/.local/share/orchestr8r/state.db"

[secrets]
backend = "age-file"
path = "~/.local/share/orchestr8r/secrets.age"

[runtime]
backend = "docker"
socket = "/var/run/docker.sock"

[notifications]
default = ["desktop"]
```

---

## Inter-Step Data Flow

Steps communicate via a **shared scratch directory** scoped to each workflow run (`~/.local/share/orchestr8r/runs/<run-id>/`), available to every step via `StepContext`.

- Container and agent steps receive it as a bind-mounted volume
- Shell steps receive it as a working directory or environment variable
- The directory is retained after the run and subject to a configurable retention policy

**Open concern:** The current `workspace` step conflates "working directory" (e.g., a git repo the agent modifies) with "inter-step data passing" (e.g., `output_file`). These should eventually be separated — the workspace is where agents operate, while step artifacts (plans, outputs) should live in the scratch directory to keep the workspace clean. The exact boundary is TBD.

---

## Crash Recovery

In-progress runs are **not automatically resumed** on restart. The daemon marks any non-terminal runs as `failed` and sends a desktop notification for each. The user can re-trigger from the TUI or CLI.

Auto-resume is deferred: it requires step-level idempotency guarantees and careful handling of partial container/worktree state.

---

## Deployment Model

| Mode               | Description                                                                                                                    |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| **Local daemon**   | `orchestr8r daemon` runs as a user systemd service or background process; TUI attaches on demand                               |
| **Self-hosted**    | Same binary, deployed on a VPS; TUI connects over SSH                                                                          |
| **Cloud (future)** | Storage backend swapped to Postgres; secrets backend swapped to a vault; container runtime targets a remote Docker/k8s context |

---

## Security Model

- The daemon runs as the **current user** (no root required if Docker socket permissions allow it)
- Agent subprocesses run **inside containers** with only declared secrets injected
- Workflow TOML files are validated at load time with strict schema enforcement

---

## Key Dependencies (Rust Crates)

| Concern               | Crate                                 |
| --------------------- | ------------------------------------- |
| Async runtime         | `tokio`                               |
| TUI                   | `ratatui`                             |
| Docker API            | `bollard`                             |
| SQLite                | `sqlx` (sqlite feature) or `rusqlite` |
| TOML parsing          | `toml` + `serde`                      |
| Secret encryption     | `age`                                 |
| OS keychain           | `keyring`                             |
| File watching         | `notify`                              |
| Desktop notifications | `notify-rust`                         |
| HTTP (webhooks)       | `axum`                                |
| Error handling        | `thiserror`, `anyhow`                 |
| Logging               | `tracing`, `tracing-subscriber`       |
| CLI parsing           | `clap`                                |

---

## Open Questions / Future Work

- **Actionable notifications:** Add accept/reject/feedback actions directly in desktop notifications (platform permitting).
- **Auto-resume after crash:** Requires step-level idempotency guarantees and handling of partial container/worktree state.
- **Retry policy:** No per-step retry or backoff is defined. Configurable retry counts and dead-letter behavior are needed.
