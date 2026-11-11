#!/usr/bin/env bash
# Start the core engine and the dashboard dev server together.
#
#   ./scripts/dev.sh
#
# Ctrl-C stops both. No Docker, no containers: two local processes.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cleanup() {
  if [ -n "${ui_pid:-}" ]; then
    kill "$ui_pid" 2>/dev/null || true
  fi
  if [ -n "${core_pid:-}" ]; then
    kill "$core_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

"$root/scripts/run-core.sh" &
core_pid=$!

"$root/scripts/run-ui.sh" &
ui_pid=$!

echo
echo "core  → http://127.0.0.1:${CORE_PORT:-8080}"
echo "ui    → http://127.0.0.1:5173"
echo "press Ctrl-C to stop both"

wait -n "$core_pid" "$ui_pid"
