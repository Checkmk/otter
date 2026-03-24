#!/bin/bash
# Workspace setup script for otter.
# Invoked as: <script> <workflow-name> <run-id>
# Must print exactly one path to stdout and exit 0.

WORKFLOW=$1
RUN_ID=$2

BRANCH="otter-${RUN_ID:0:8}"
WORKTREE="/tmp/otter-ws-${RUN_ID:0:8}"

git -C ~/my-project worktree add "$WORKTREE" -b "$BRANCH"
echo "$WORKTREE"
