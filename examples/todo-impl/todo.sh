#!/usr/bin/env bash
#
# Companion script for the `todo-impl` workflow.
#
# A "task" is a single top-level Markdown heading (`# ...`) plus its body, up to
# the next top-level heading. A task's identity is the SHA-256 of its *title*
# (heading text with any status marker stripped), so adding/removing a marker
# never changes the identity.
#
# Status is encoded as a marker right after the `# `:
#   (none)    pending  -> eligible to run
#   (DRAFT)   draft    -> authored by you; "not ready yet", skipped by --poll
#   (LOCK)    locked   -> a run is in progress (or crashed mid-run)
#   (DONE)    done     -> implemented successfully
#   (FAIL)    failed   -> the run failed or was stopped
#
# Only *pending* tasks (no marker) are emitted by --poll, so otter never picks
# up a draft and never re-runs a locked/done/failed task. To run a draft, remove
# its (DRAFT) marker. To re-queue a failed task, clear its (FAIL) marker.
#
# Usage:
#   todo.sh --poll                     # JSON array of pending task ids
#   todo.sh --context <id> <ctx-dir>   # write section.md + id, mark task (LOCK)
#   todo.sh --done <id>                # mark task done   ((LOCK) -> (DONE))
#   todo.sh --fail <id>                # mark task failed ((LOCK) -> (FAIL))
#
# Reads TODO_PATH from the environment (injected via the workflow's `requires`).

set -euo pipefail

MARKERS=("(DRAFT)" "(LOCK)" "(DONE)" "(FAIL)")

todo_path() {
  local p="${TODO_PATH:?TODO_PATH not set}"
  # Expand a leading ~ since requires-injected values are stored verbatim.
  printf '%s' "${p/#\~/$HOME}"
}

id_of() { # title -> short stable id
  printf '%s' "$1" | sha256sum | cut -c1-16
}

# Split a heading line ("# ...") into MARKER and TITLE globals.
parse_heading() {
  local rest="${1#\# }" m
  MARKER=""
  TITLE="$rest"
  for m in "${MARKERS[@]}"; do
    if [[ "$rest" == "$m "* ]]; then
      MARKER="$m"
      TITLE="${rest#"$m" }"
      break
    fi
  done
}

is_h1() { [[ "$1" == "# "* ]]; }

cmd_poll() {
  local file ids=() line
  file="$(todo_path)"
  [[ -f "$file" ]] || { echo "[]"; return 0; }
  while IFS= read -r line || [[ -n "$line" ]]; do
    if is_h1 "$line"; then
      parse_heading "$line"
      [[ -z "$MARKER" ]] && ids+=("$(id_of "$TITLE")")
    fi
  done <"$file"

  local out="[" first=1 id
  for id in "${ids[@]:-}"; do
    [[ -z "$id" ]] && continue
    [[ $first -eq 1 ]] && first=0 || out+=","
    out+="\"$id\""
  done
  out+="]"
  printf '%s\n' "$out"
}

# Find the index of the heading whose id == target. With require_pending=1, only
# an unmarked heading matches. Sets FOUND to the index or -1.
find_index() {
  local target="$1" require_pending="$2" i
  FOUND=-1
  for i in "${!LINES[@]}"; do
    is_h1 "${LINES[$i]}" || continue
    parse_heading "${LINES[$i]}"
    [[ "$require_pending" == "1" && -n "$MARKER" ]] && continue
    if [[ "$(id_of "$TITLE")" == "$target" ]]; then
      FOUND=$i
      return 0
    fi
  done
  return 0
}

write_back() { # rewrite TODO from LINES
  local file="$1" tmp
  tmp="$(mktemp)"
  printf '%s\n' "${LINES[@]}" >"$tmp"
  mv "$tmp" "$file"
}

cmd_context() {
  local target="$1" ctx_dir="$2" file start end i
  file="$(todo_path)"
  mapfile -t LINES <"$file"

  find_index "$target" 1
  [[ "$FOUND" -ge 0 ]] || { echo "no pending task with id $target" >&2; return 1; }

  # Section = heading line up to (but not including) the next H1.
  start="$FOUND"
  end="${#LINES[@]}"
  for ((i = start + 1; i < ${#LINES[@]}; i++)); do
    if is_h1 "${LINES[$i]}"; then end=$i; break; fi
  done

  mkdir -p "$ctx_dir"
  printf '%s\n' "${LINES[@]:start:end-start}" >"$ctx_dir/section.md"
  printf '%s' "$target" >"$ctx_dir/id"

  # Mark the task locked.
  parse_heading "${LINES[$start]}"
  LINES[$start]="# (LOCK) $TITLE"
  write_back "$file"
}

set_marker() {
  local target="$1" marker="$2" file
  file="$(todo_path)"
  mapfile -t LINES <"$file"
  find_index "$target" 0
  [[ "$FOUND" -ge 0 ]] || { echo "no task with id $target" >&2; return 0; }
  parse_heading "${LINES[$FOUND]}"
  LINES[$FOUND]="# $marker $TITLE"
  write_back "$file"
}

case "${1:-}" in
  --poll)    cmd_poll ;;
  --context) cmd_context "${2:?id}" "${3:?ctx-dir}" ;;
  --done)    set_marker "${2:?id}" "(DONE)" ;;
  --fail)    set_marker "${2:?id}" "(FAIL)" ;;
  *)
    echo "Usage: $0 [--poll | --context <id> <dir> | --done <id> | --fail <id>]" >&2
    exit 1
    ;;
esac
