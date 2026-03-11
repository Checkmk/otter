# Orchestr8r

Automate multi-step AI agent tasks triggered by real-world events.

- A **workflow** is a series of steps defined in config, executing inside a container — either **indefinite** (always running while the service is up, looping continuously) or **triggered** (started by an event)
- Each workflow has exactly one running instance at a time — an indefinite workflow only starts the next loop after the previous one completes, never spawning parallel containers
- Workflows are composed of **step plugins** — reusable, installable units of work (e.g., launch agent, spin up container, create worktree, send notification, wait for approval)
- **Triggers** are first-class for event-driven workflows: cron/scheduled, event-driven (email, webhook, file change), or manual
- **Checkpoint steps** are a built-in step type that pauses a workflow for human input — accept to continue, reject to pause
- A dashboard lets you monitor running workflows, view logs, and manage step plugins
- Secrets and credentials are managed centrally and injected into containers at runtime
- Example indefinite workflow (always running):
  - Launch custom agent that compares actual codebase to a target_architecture.md and suggest an implementation plan to bring the two together
  - Checkpoint: notify the Orchestr8r user about the plan
  - Launch custom agent to implement the plan
  - Checkpoint: notify the Orchestr8r user to review the implementation
  - Push the implementation as a PR
- Example event-driven workflow (email trigger: new PR review requested):
  - Spin up a new container
  - Create worktree with PR
  - Launch custom agent to review the PR and generate review comments
  - Checkpoint: notify the Orchestr8r user to review the comments, ask for acceptance to post them or save them to a markdown file
