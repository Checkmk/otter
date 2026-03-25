# Otter

Automate multi-step tasks (with AI support) triggered by real-world events.

- **Workflows** are TOML-defined series of steps — either **looping** (loops) or **triggered** (event-driven)
- **Agent steps** drive Claude, Copilot or any AI CLI across steps
- **Checkpoints** pause for human review and feedback
- **Workspace** isolates agents in per-run scratch directories, a fixed directory or a per-run script-provisioned directory
- **Sandbox** optionally runs agent and shell steps in rootless Podman containers with security hardening
- **TUI dashboard** monitors workflows, views logs, and controls execution

## Build

### Prerequisites

- Rust toolchain (`cargo`)
- For agent steps: Claude Code, Copilot CLI, or any AI CLI tool
- For sandbox: [Podman](https://podman.io) (rootless)

### Install

```bash
cargo build --release
# Binary is at: target/release/otter
```

## Quick Start

```bash
# Install workflow
otter workflow install examples/hello-world.toml

# Start background service
otter service start

# Open the TUI
otter

# or start workflow via CLI:
otter start hello-world

# Stop background service
otter service stop
```

## Common Commands

```bash
otter                         # open the TUI dashboard
otter help                    # show all commands
otter status                  # list service and workflow state
otter service start           # start background service for this session
otter service stop            # stop background service
otter workflow install <path> # install a workflow (.toml or package dir)
otter start <name>            # start or resume a workflow
otter stop <name>             # stop a running workflow
```

## Example Workflows

Can be found in the [examples/](examples/) directory.

## User Data

| Purpose | Linux | Windows |
|---|---|---|
| Configuration & workflows | `~/.config/otter/` | `%APPDATA%\otter\` |
| State, logs & run scratch dirs | `~/.local/share/otter/` | `%APPDATA%\otter\` |

## Configuration Reference

A reference for all possible configurations (step types, triggers and
workspace configuration) can be found in the [usage guide](USAGE.md).
