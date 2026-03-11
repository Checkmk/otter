# Orchestr8r

The service that helps you manage AI workflows.

- A workflow is a series of steps that can run indefinitely or when triggered running in a docker container
- Example indefinite workflow:
  - Launch custom agent that compares actual codebase to a target_architecture.md and suggest an implementation plan to bring the two together
  - Notify the Orchestr8r user about the plan, ask for acceptance or interview for feedback until plan is accepted
  - Launch custom agent to implement the plan
  - Notify the Orchestr8r user to review the implementation, ask for acceptance or interview for feedback until implementation is accepted
  - Push the plan as a PR
- Example: When I get an email about a new PR to be reviewed
  - When I receive a specific type of email notifying me about a new requested review
  - Spin up a new container
  - Create worktree with PR
  - Launch custom agent to review the PR
  - Create the required review comments
  - Notify the Orchestr8r user to review the review comments, ask for acceptance to post them or store them in md file
- Note that each bullet point in a workflow is a reusable step corresponding to an instance of a step plugin
