#!/bin/bash
# coredeck-register — Claude Code SessionStart hook script for coredeck-claude.
#
# Reads $COREDECK_WRAPPER_ID from the environment (set by the wrapper before
# exec'ing claude) and `session_id` from the SessionStart hook's stdin
# payload, then POSTs {wrapper_id, session_id} to the daemon so it can
# correlate the Claude session to the wrapper instance.
#
# Exits silently when not running under a wrapper (so it's a no-op for
# normal `claude` invocations).

set -u

INPUT=$(cat)

WRAPPER_ID="${COREDECK_WRAPPER_ID:-}"
if [ -z "$WRAPPER_ID" ]; then
    exit 0
fi

if command -v jq >/dev/null 2>&1; then
    SESSION_ID=$(printf '%s' "$INPUT" | jq -r '.session_id // empty')
else
    SESSION_ID=$(printf '%s' "$INPUT" | grep -o '"session_id":"[^"]*"' | head -1 | cut -d'"' -f4)
fi

if [ -z "${SESSION_ID:-}" ]; then
    exit 0
fi

DAEMON_URL="${COREDECK_DAEMON_URL:-http://127.0.0.1:19384}"
curl -s -m 2 -X POST "$DAEMON_URL/wrapper/register" \
    -H 'Content-Type: application/json' \
    -d "{\"wrapper_id\":\"$WRAPPER_ID\",\"session_id\":\"$SESSION_ID\"}" \
    >/dev/null 2>&1 || true

exit 0
