#!/bin/bash
# Workspace setup script for orchestr8r.
# Invoked as: <script> <workflow-name> <run-id>
# Must print exactly one path to stdout and exit 0.

WORKFLOW=$1
RUN_ID=$2

BRANCH="orchestr8r-${RUN_ID:0:8}"
WORKTREE="/tmp/orchestr8r-ws-${RUN_ID:0:8}"

git -C ~/my-project worktree add "$WORKTREE" -b "$BRANCH"
echo "$WORKTREE"
