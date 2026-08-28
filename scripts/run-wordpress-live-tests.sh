#!/usr/bin/env bash
#
# Run the WordPress plugin against a relay that is really running.
#
# Starts the relay binary against a throwaway database on a free loopback port,
# runs the live suite, then stops the relay and removes the database.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

binary="target/debug/otwono-relay"
if [ ! -x "$binary" ]; then
  echo "Building the relay…"
  cargo build -p otwono-relay --bin otwono-relay
fi

port="${OTWONO_RELAY_PORT:-8799}"
workdir="$(mktemp -d)"
trap 'kill "${relay_pid:-0}" 2>/dev/null || true; rm -rf "$workdir"' EXIT

OTWONO_RELAY_DB="$workdir/relay.sqlite3" \
OTWONO_RELAY_BIND="127.0.0.1:$port" \
RUST_LOG=warn \
  "$binary" >"$workdir/relay.log" 2>&1 &
relay_pid=$!

for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$port/health" >/dev/null; then break; fi
  sleep 0.2
done

if ! curl -sf "http://127.0.0.1:$port/health" >/dev/null; then
  echo "The relay did not start. Log:" >&2
  cat "$workdir/relay.log" >&2
  exit 1
fi

OTWONO_RELAY_URL="http://127.0.0.1:$port" php wordpress/tests/run-live-tests.php
