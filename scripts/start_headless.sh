#!/usr/bin/env bash
# Launches Koharu in headless mode: shared runtime, HTTP API, no window.
# Usage: scripts/start_headless.sh [--port 9170] [additional koharu --headless args]
set -euo pipefail

profile="${KOHARU_PROFILE:-release}"
root="$(cd "$(dirname "$0")/.." && pwd)"
exe="$root/target/$profile/koharu"

if [ ! -x "$exe" ]; then
    echo "koharu executable not found at $exe; build it first (bun run build)" >&2
    exit 1
fi

exec "$exe" --headless "$@"
