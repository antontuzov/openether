#!/usr/bin/env bash
# Compile the momentum strategy to WASM for the core's sandbox.
#
# The strategy crate is excluded from the cargo workspace on purpose: it is
# built for the wasm32 target, not for the host.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
strategy="$root/rust/crates/strategies/momentum"
out="$root/rust/target/wasm"

rustup target add wasm32-unknown-unknown >/dev/null

cd "$strategy"
cargo build --release --target wasm32-unknown-unknown

mkdir -p "$out"
cp "$strategy/target/wasm32-unknown-unknown/release/openether_strategy_momentum.wasm" \
   "$out/momentum.wasm"

echo "strategy → $out/momentum.wasm"
echo "run the core with: OPENETHER_STRATEGY_WASM=$out/momentum.wasm ./scripts/run-core.sh"
