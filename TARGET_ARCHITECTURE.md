# Orchestr8r — Target Architecture

## Overview

Orchestr8r is a **Rust-native workflow automation service** that executes multi-step AI agent tasks in response to real-world events. It runs as a local daemon with a TUI dashboard, deployable on a local machine or self-hosted server with a path to cloud environments.

---

## Core Principles

- **Built in Rust** — the entire service, daemon, plugins, and TUI are written in Rust
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
message = "Review the comments above. Accept to post, reject to save to file."
notify = ["desktop"]
```

- **Indefinite workflows** loop continuously; the next iteration begins only after the previous completes
- **Triggered workflows** start on an event and run to completion (or failure)
- Exactly one instance of each workflow runs at a time

### Step Types (built-in, trait-based)

| Step Type    | Description                                                               |
| ------------ | ------------------------------------------------------------------------- |
| `container`  | Launches a container via the `ContainerRuntime` trait                     |
| `checkpoint` | Pauses for human input; sends notification, awaits accept/reject/feedback |
| `agent`      | Invokes an `AgentRunner` (e.g., shells out to a CLI agent tool)           |
| `worktree`   | Creates a git worktree for isolated work                                  |
| `notify`     | Sends a notification without pausing                                      |
| `shell`      | Runs an arbitrary shell command in a sandbox                              |

All step types are defined as Rust structs implementing a `StepExecutor` trait. The architecture does not block adding dynamically loaded step plugins in the future (e.g., via `libloading` or WASM), but the initial implementation uses built-in steps only.

```rust
#[async_trait]
pub trait StepExecutor: Send + Sync {
    async fn execute(&self, ctx: &StepContext) -> Result<StepOutput, StepError>;
    fn step_type(&self) -> &'static str;
}
```

---

## Trigger System

Triggers are **first-class plugins** implementing a `TriggerSource` trait:

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
| `webhook`    | Embedded HTTP listener (e.g., `axum`)    |
| `file-watch` | `notify` crate (inotify/FSEvents/kqueue) |
| `email`      | IMAP IDLE (push-based)                   |
| `manual`     | TUI or CLI command                       |

Triggers are registered at startup from workflow TOML config. Each trigger maintains its own async task and sends `TriggerEvent` messages to the workflow scheduler via a channel.

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

Initial implementation: **Docker** via the `bollard` crate. Podman (Docker-compatible API) works without code changes by pointing to the Podman socket.

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

Initial implementation: **SQLite** via `rusqlite` or `sqlx` with the SQLite feature. Database file stored at `~/.local/share/orchestr8r/state.db` (XDG-compliant).

---

## Secrets Management

### Design Goals

- Secrets are **never passed to agent processes directly** through environment inspection
- Agents receive secrets only via **explicit injection** into their container/process at runtime
- The core service reads secrets; agent subprocesses see only what is declared in the workflow step's `secrets` list

### Implementation

```rust
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<SecretValue, SecretError>;
    fn set(&self, key: &str, value: SecretValue) -> Result<(), SecretError>;
    fn list_keys(&self) -> Result<Vec<String>, SecretError>;
}
```

Initial implementation: **Encrypted local file** using `age` encryption (via the `age` crate). The encryption key is derived from a passphrase stored in the OS keychain (`keyring` crate) so the user is not prompted on every start. Secrets file: `~/.local/share/orchestr8r/secrets.age`.

---

## Notification System

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError>;
}
```

Initial implementation: **desktop notifications** via the `notify-rust` crate (supports Linux, macOS, Windows). Checkpoint steps block workflow progression until the user responds via the TUI dashboard.

Future notifier implementations: email, webhook.

---

## AI Agent Integration

Agent steps implement `StepExecutor` via an `AgentRunner` abstraction:

```rust
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, spec: AgentSpec) -> Result<AgentOutput, AgentError>;
}
```

**Recommended initial implementation:** subprocess-based runner that shells out to CLI agent tools (e.g., `claude`, `aider`, custom scripts). This is provider-agnostic by construction — any agent that can be invoked as a CLI tool is supported. Direct API integration (Anthropic, OpenAI) can be added as additional `AgentRunner` implementations without changing the interface.

---

## TUI Dashboard

Built with **`ratatui`**. The TUI is the primary user interface for:

- Viewing running and completed workflow instances with live log streaming
- Responding to checkpoint steps (accept / reject / provide feedback)
- Managing workflow enable/disable
- Viewing plugin/trigger status

The TUI communicates with the core daemon via an **in-process channel** when running as a single binary, or via a **Unix domain socket / named pipe** when the daemon runs headlessly and the TUI attaches separately.

---

## Configuration

All configuration lives in `~/.config/orchestr8r/` (XDG-compliant):

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
- The `SecretStore` is only accessible by the core daemon process — agents cannot enumerate or read secrets beyond what is explicitly injected
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
