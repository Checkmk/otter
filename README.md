# Orchestr8r

Automate multi-step tasks (with AI support) triggered by real-world events.

- **Workflows** are TOML-defined series of steps — either **indefinite** (loops) or **triggered** (event-driven)
- **Agent steps** drive Claude, Copilot or any AI CLI across steps
- **Checkpoints** pause for human review and feedback
- **Workspace** isolates agents in per-run scratch directories unless explicitly configured via a `workspace` flag
- **TUI dashboard** monitors workflows, views logs, and controls execution

## Build

### Prerequisites

- Rust toolchain (`cargo`)
- For agent steps: Claude Code, Copilot CLI, or any AI CLI tool

### Install

```bash
cargo build --release
# Binary is at: target/release/orchestr8r
```

## Quick Start

1. Create workflow directory:

   ```bash
   mkdir -p ~/.config/orchestr8r/workflows
   ```

2. Add a workflow (e.g. `~/.config/orchestr8r/workflows/hello.toml`)

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

3. Start the daemon:

   ```
   orchestr8r daemon
   ```

4. Open the dashboard:

   ```
   orchestr8r
   ```

   or start a workflow directly:

   ```
   orchestr8r start hello-world
   ```

The dashboard connects to the daemon via a Unix socket. It shows workflow states, live step output, and checkpoint prompts. If no daemon is running, it prints a helpful error with setup instructions.

## Common Commands

```bash
orchestr8r                   # open the TUI dashboard
orchestr8r help              # show all commands
orchestr8r daemon            # start the background daemon
orchestr8r status            # list all workflows and their state
orchestr8r start <name>      # start a workflow
orchestr8r pause <name>      # pause running indefinite workflow
orchestr8r resume <name>     # resume a paused workflow
orchestr8r stop <name>       # stop a running workflow
```

## Example Workflows

Can be found in the [examples/](examples/) directory.

## Configuration Reference

A reference for all possible configurations (step types, triggers and
workspace configuration) can be found in the [configuration reference](CONFIGURATION_REFERENCE.md).
