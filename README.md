# Orchestr8r

Automate multi-step tasks (with AI support) triggered by real-world events.

- **Workflows** are TOML-defined series of steps — either **looping** (loops) or **triggered** (event-driven)
- **Agent steps** drive Claude, Copilot or any AI CLI across steps
- **Checkpoints** pause for human review and feedback
- **Workspace** isolates agents in per-run scratch directories, a fixed directory or a per-run script-provisioned directory
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

```bash
# Install workflow
orchestr8r workflow install examples/hello-world.toml

# Start background service
orchestr8r service start

# Open the TUI
orchestr8r

# or start workflow via CLI:
orchestr8r start hello-world

# Stop background service
orchestr8r service stop
```

## Common Commands

```bash
orchestr8r                         # open the TUI dashboard
orchestr8r help                    # show all commands
orchestr8r status                  # list service and workflow state
orchestr8r service start           # start background service for this session
orchestr8r service stop            # stop background service
orchestr8r workflow install <path> # install a workflow (.toml or package dir)
orchestr8r start <name>            # start or resume a workflow
orchestr8r stop <name>             # stop a running workflow
```

## Example Workflows

Can be found in the [examples/](examples/) directory.

## Configuration Reference

A reference for all possible configurations (step types, triggers and
workspace configuration) can be found in the [configuration reference](CONFIGURATION_REFERENCE.md).
