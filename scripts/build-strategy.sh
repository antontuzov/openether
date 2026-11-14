#!/usr/bin/env bash
# Compile every built-in strategy to WASM for the core's sandbox.
#
# The strategy crates are excluded from the cargo workspace on purpose: they are
# built for the wasm32 target, not for the host. Each artifact lands in
# rust/target/wasm under the name the strategy registry expects.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/rust/target/wasm"

rustup target add wasm32-unknown-unknown >/dev/null

build_strategy() {
  local dir="$1" crate="$2" artifact="$3"
  local strategy="$root/rust/crates/strategies/$dir"
  ( cd "$strategy" && cargo build --release --target wasm32-unknown-unknown )
  cp "$strategy/target/wasm32-unknown-unknown/release/$crate.wasm" "$out/$artifact.wasm"
  echo "strategy → $out/$artifact.wasm"
}

mkdir -p "$out"
build_strategy momentum       openether_strategy_momentum       momentum
build_strategy mean-reversion openether_strategy_mean_reversion mean-reversion
build_strategy breakout       openether_strategy_breakout       breakout

echo
echo "select one over HTTP:  curl -X POST localhost:8080/api/strategy/select \\"
echo "                         -H 'content-type: application/json' -d '{\"name\":\"breakout\"}'"
echo "or start the core with: OPENETHER_STRATEGY_WASM=$out/momentum.wasm ./scripts/run-core.sh"
