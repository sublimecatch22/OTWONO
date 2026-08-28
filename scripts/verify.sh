#!/usr/bin/env bash
#
# Everything CI runs, in one command. Use it before pushing.

set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run() {
  echo
  echo "── $1 ──"
  shift
  "$@"
}

run "Formatting (TypeScript)"  npm run format:check
run "Types (TypeScript)"       npm run typecheck
run "Tests (TypeScript)"       npm run test
run "Formatting (Rust)"        cargo fmt --all -- --check
run "Lints (Rust)"             cargo clippy --workspace --all-targets -- -D warnings
run "Tests (Rust)"             cargo test --workspace
run "Tests (WordPress plugin)" php wordpress/tests/run-tests.php
run "Tests (plugin vs a live relay)" ./scripts/run-wordpress-live-tests.sh

# The end-to-end suite needs the service binary and the built web assets.
run "Build (service)"          cargo build -p otwono-local-service
run "Build (web)"              npm run build
run "Tests (end to end)"       npx playwright test

echo
echo "Everything passed."
