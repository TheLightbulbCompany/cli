#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if [[ -n "${LIGHTBULB_BINARY:-}" ]]; then
  demo_binary="$LIGHTBULB_BINARY"
elif [[ -x ./bin/lightbulb-gateway ]]; then
  demo_binary=./bin/lightbulb-gateway
else
  cargo build --locked
  demo_binary=./target/debug/lightbulb-gateway
fi
supplier_port="${LIGHTBULB_SUPPLIER_PORT:-8081}"
gateway_port="${LIGHTBULB_GATEWAY_PORT:-8080}"
"$demo_binary" serve --role supplier --port "$supplier_port" --data-dir data/supplier &
supplier_pid=$!
cleanup() { kill "$supplier_pid" "${gateway_pid:-}" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
"$demo_binary" serve --role gateway --port "$gateway_port" --data-dir data/gateway --supplier-url "http://127.0.0.1:$supplier_port" &
gateway_pid=$!
printf 'Open http://127.0.0.1:%s — local simulation, no real purchases.\n' "$gateway_port"
wait -n "$supplier_pid" "$gateway_pid"
