#!/usr/bin/env bash
# Run the dashboard dev server.
#
#   ./scripts/run-ui.sh
#
# Vite proxies /api and /ws to the core (see ts/packages/ui/vite.config.ts),
# so start the core first for the live view — without it the dashboard falls
# back to the in-browser simulator.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=load-env.sh
source "$root/scripts/load-env.sh"
load_env "$root/.env"
cd "$root/ts"

if [ ! -d node_modules ]; then
  pnpm install
fi

exec pnpm --filter @openether/ui dev
