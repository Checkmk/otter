#!/bin/bash
# Test polling script that returns a new hash each time
# Usage:
#   ./polling-simple.sh --poll                    # Return event hashes
#   ./polling-simple.sh --context <hash> <dir>   # Write event metadata

if [[ "$1" == "--poll" ]]; then
  # Return a new hash each time (simulates continuous events)
  timestamp=$(date +%s%N)
  echo "[\"event-${timestamp}\"]"
  exit 0
elif [[ "$1" == "--context" ]]; then
  hash="$2"
  context_dir="$3"

  # Write test metadata
  cat > "$context_dir/metadata.txt" << EOF
Event: $hash
Timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF
  exit 0
else
  echo "Usage: $0 [--poll | --context <hash> <dir>]" >&2
  exit 1
fi
