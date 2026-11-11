#!/usr/bin/env bash
# Run the AI agent (the "cortex") against the local core.
#
# The agent talks to the core over MCP: it spawns the Rust MCP server as a
# child process and calls execute_trade / get_portfolio / fetch_onchain_data
# through it. The core itself must be running (./scripts/run-core.sh).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=load-env.sh
source "$root/scripts/load-env.sh"
load_env "$root/.env"
cd "$root/ts"

if [ ! -d node_modules ]; then
  pnpm install
fi

export CORE_MCP_COMMAND="${CORE_MCP_COMMAND:-cargo}"
export CORE_MCP_ARGS="${CORE_MCP_ARGS:-run --release -p openether-mcp-server --manifest-path $root/rust/Cargo.toml}"
export AGENT_INTERVAL_MS="${AGENT_INTERVAL_MS:-15000}"

exec pnpm --filter @openether/agent start
