#!/usr/bin/env bash
# Runs the mw_client addon tests against a freshly built server.
# Usage: GODOT=/path/to/godot godot/run_tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."
GODOT="${GODOT:-godot}"
TCP_PORT="${TCP_PORT:-17878}"
UDP_PORT="${UDP_PORT:-17879}"

cargo build --quiet --bin network_manager
./target/debug/network_manager --tcp-addr "127.0.0.1:$TCP_PORT" --udp-addr "127.0.0.1:$UDP_PORT" &
SERVER=$!
trap 'kill -TERM $SERVER 2>/dev/null || true' EXIT
sleep 0.5

"$GODOT" --headless --path godot --import >/dev/null 2>&1 || true
OUT=$("$GODOT" --headless --path godot -s res://tests/run.gd -- "$TCP_PORT" "$UDP_PORT" 2>&1) || STATUS=$?
echo "$OUT"
# Godot can exit 0 on script errors, so require the explicit success marker.
grep -q "^ALL PASSED" <<<"$OUT" && [ "${STATUS:-0}" -eq 0 ]
