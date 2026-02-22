#!/bin/bash
# demarch-query — Shell wrapper for Demarch IPC queries.
# Usage: demarch-query <type> [--param=value ...]
#
# Intended for Codex agents that run shell commands natively.
# Writes an IPC query file, waits for the host to process it, prints the result.
#
# Examples:
#   demarch-query run_status
#   demarch-query search_beads --status=open
#   demarch-query run_events --limit=10 --since=2026-02-20T00:00:00Z
#   demarch-query search_beads --id=beads-abc123

set -euo pipefail

IPC_DIR="/workspace/ipc"
QUERIES_DIR="$IPC_DIR/queries"
RESPONSES_DIR="$IPC_DIR/responses"
TIMEOUT_SECONDS=30

if [ $# -lt 1 ]; then
  echo "Usage: demarch-query <type> [--param=value ...]" >&2
  echo "Types: run_status, sprint_phase, search_beads, spec_lookup, review_summary, next_work, run_events" >&2
  exit 1
fi

TYPE="$1"
shift

# Parse --key=value params into JSON object
PARAMS="{}"
for arg in "$@"; do
  if [[ "$arg" =~ ^--([a-z_]+)=(.+)$ ]]; then
    key="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[2]}"
    # Try to parse as number, otherwise quote as string
    if [[ "$value" =~ ^[0-9]+$ ]]; then
      PARAMS=$(echo "$PARAMS" | python3 -c "import sys,json; d=json.load(sys.stdin); d['$key']=$value; print(json.dumps(d))")
    else
      PARAMS=$(echo "$PARAMS" | python3 -c "import sys,json; d=json.load(sys.stdin); d['$key']='$value'; print(json.dumps(d))")
    fi
  else
    echo "Warning: ignoring unrecognized argument: $arg" >&2
  fi
done

# Generate UUID
UUID=$(python3 -c "import uuid; print(uuid.uuid4())")

# Build query JSON
QUERY=$(python3 -c "
import json, sys
print(json.dumps({
    'uuid': '$UUID',
    'type': '$TYPE',
    'params': json.loads('$PARAMS'),
    'timestamp': __import__('datetime').datetime.now().isoformat()
}, indent=2))
")

# Atomic write
mkdir -p "$QUERIES_DIR" "$RESPONSES_DIR"
QUERY_FILE="$QUERIES_DIR/$UUID.json"
echo "$QUERY" > "$QUERY_FILE.tmp"
mv "$QUERY_FILE.tmp" "$QUERY_FILE"

# Poll for response
RESPONSE_FILE="$RESPONSES_DIR/$UUID.json"
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT_SECONDS ]; do
  if [ -f "$RESPONSE_FILE" ]; then
    cat "$RESPONSE_FILE"
    rm -f "$RESPONSE_FILE" 2>/dev/null
    exit 0
  fi
  sleep 0.2
  ELAPSED=$((ELAPSED + 1))  # Approximate — each iteration is ~0.2s
  if [ $((ELAPSED % 5)) -eq 0 ]; then
    ELAPSED=$((ELAPSED))  # No-op for readability
  fi
done

# Timeout
rm -f "$QUERY_FILE" 2>/dev/null
echo '{"status":"error","result":"Query timed out — Demarch kernel may not be available."}' >&2
exit 1
