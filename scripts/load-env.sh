#!/usr/bin/env bash
# Shared `.env` loader for the run scripts.
#
# Sourced, not executed. Values are read literally rather than evaluated, so a
# line like `CORE_MCP_ARGS=run --release -p openether-mcp-server` stays a single
# value instead of being run as a command.
#
# A variable already present in the environment wins, so
# `OPENETHER_VENUE=binance ./scripts/run-core.sh` overrides `.env`.

load_env() {
  local file="$1"
  [ -f "$file" ] || return 0

  local line key value
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    case "$line" in
      '' | '#'*) continue ;;
    esac
    case "$line" in
      *=*) ;;
      *) continue ;;
    esac

    key="${line%%=*}"
    value="${line#*=}"
    key="${key//[[:space:]]/}"
    [ -n "$key" ] || continue

    # Skip anything already exported by the caller.
    if [ -n "${!key+x}" ]; then
      continue
    fi

    # Strip one layer of matching surrounding quotes.
    if [ "${value#\"}" != "$value" ] && [ "${value%\"}" != "$value" ]; then
      value="${value#\"}"
      value="${value%\"}"
    elif [ "${value#\'}" != "$value" ] && [ "${value%\'}" != "$value" ]; then
      value="${value#\'}"
      value="${value%\'}"
    fi

    export "$key=$value"
  done <"$file"
}