# Otter

Automate multi-step tasks (with AI support) triggered by real-world events.

- **Workflows** are TOML-defined series of steps — either **looping** (loops) or **triggered** (event-driven)
- **Agent steps** drive Claude, Copilot or any AI CLI across steps
- **Checkpoints** pause for human review and feedback
- **Workspace** isolates agents in per-run scratch directories, a fixed directory, a script-provisioned directory, or a per-run git worktree
- **Sandbox** optionally runs agent and shell steps in rootless Podman containers with security hardening
- **TUI dashboard** monitors workflows, views logs, and controls execution with theming support

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
otter workflow install examples/hello-world

# Start background service
otter service start

# Open the TUI
otter

# or start workflow via CLI:
otter start hello-world

# Stop background service
otter service stop
```

## Marketplace

Install workflows by name from any git repo that publishes them. This
repository doubles as one:

```bash
otter marketplace add "$(pwd)"
otter workflow install hello-world@otter-examples
```

See [USAGE.md](USAGE.md#marketplaces) for details.

## Common Commands

```bash
otter                                     # open the TUI dashboard
otter help                                # show all commands
otter status                              # list service, workflow, and marketplace state
otter service start                       # start background service for this session
otter service stop                        # stop background service
otter workflow install <path>             # install a workflow from disk
otter workflow install <n>@<marketplace>  # install from a registered marketplace
otter marketplace add <git-url>           # register a marketplace
otter start <name>                        # start or resume a workflow
otter stop <name>                         # stop a running workflow
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
