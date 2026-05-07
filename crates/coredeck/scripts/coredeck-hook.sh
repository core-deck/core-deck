#!/bin/sh
# coredeck-hook — generic Claude Code hook shim for the daemon.
#
# Usage: coredeck-hook.sh <event-name> [max-time-seconds]
#
# Reads the hook payload from stdin, POSTs it to the daemon's
# /hooks/<event-name> endpoint, and writes the response to stdout
# unchanged. PermissionRequest relies on that pass-through so Claude
# Code receives the allow/deny envelope verbatim; observational hooks
# get an empty 200 OK and ignore stdout.
#
# `max-time-seconds` caps curl's wall-clock wait. Defaults to 5s — a
# safety backstop for observational hooks where the daemon should
# respond in milliseconds. PermissionRequest passes a much higher
# value (the user needs time to react) and pairs it with a `timeout`
# field on the hook entry itself so Claude Code's own 60s default
# doesn't fire first.
#
# When the daemon is offline (curl ECONNREFUSED, timeout, etc.) the
# script swallows the error and exits 0 so Claude Code doesn't surface
# a hook failure on every turn. Claude Code treats non-zero from a
# command hook as a block signal — exactly the wrong UX here.

set -u

EVENT="${1:-}"
MAX_TIME="${2:-5}"
if [ -z "$EVENT" ]; then
    exit 0
fi

DAEMON_URL="${COREDECK_DAEMON_URL:-http://127.0.0.1:19384}"
curl -s -m "$MAX_TIME" -X POST "$DAEMON_URL/hooks/$EVENT" \
    -H 'Content-Type: application/json' \
    --data-binary @- 2>/dev/null

exit 0
