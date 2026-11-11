#!/usr/bin/env bash
# Run the Rust core engine in paper mode.
#
#   ./scripts/run-core.sh              # paper venue, API on :8080
#   OPENETHER_VENUE=binance ./scripts/run-core.sh
#
# Live trading stays off unless OPENETHER_LIVE_TRADING is set, and the
# venue allowlist defaults to paper only.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=load-env.sh
source "$root/scripts/load-env.sh"
load_env "$root/.env"
cd "$root/rust"

export OPENETHER_VENUE="${OPENETHER_VENUE:-paper}"
export OPENETHER_SYMBOLS="${OPENETHER_SYMBOLS:-SOL/USDT,BTC/USDT,ETH/USDT}"
export CORE_HOST="${CORE_HOST:-127.0.0.1}"
export CORE_PORT="${CORE_PORT:-8080}"
export RUST_LOG="${RUST_LOG:-info}"

# The core runs from rust/, so a repo-relative strategy path in .env has to be
# made absolute before it is handed to the engine.
if [ -n "${OPENETHER_STRATEGY_WASM:-}" ]; then
  case "$OPENETHER_STRATEGY_WASM" in
    /*) ;;
    *) export OPENETHER_STRATEGY_WASM="$root/${OPENETHER_STRATEGY_WASM#./}" ;;
  esac
fi

echo "openEther core → http://$CORE_HOST:$CORE_PORT (venue: $OPENETHER_VENUE, live trading: ${OPENETHER_LIVE_TRADING:-off})"
exec cargo run --release --bin openether-core
