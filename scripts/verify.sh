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

echo
echo "Everything passed."
