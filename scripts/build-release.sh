#!/usr/bin/env bash
#
# Build everything this machine can build, and write the release folder.
#
# On Linux this produces the .deb (and AppImage where the tooling allows), the
# WordPress plugin ZIP, checksums and release notes. Windows installers are
# built on Windows by scripts/build-windows.ps1 — see docs/RELEASE.md.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
out="$root/releases/$version"
mkdir -p "$out"

echo "OTWONO AI $version"
echo

echo "== Checks =="
npm run format:check
npm run typecheck
npm run test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
php wordpress/tests/run-tests.php

echo
echo "== Web assets =="
npm run build

echo
echo "== WordPress plugin =="
./scripts/package-wordpress-plugin.sh "$out"

echo
echo "== Desktop application =="
platform="$(uname -s)"
# One npm script per platform rather than a forwarded flag: npm treats an
# unknown `--flag value` pair as its own config, so the flag never arrives.
bundle_script=""
case "$platform" in
  Linux)  bundle_script="desktop:build:linux" ;;
  Darwin) bundle_script="desktop:build:macos" ;;
  *)      echo "  Unrecognised platform $platform; skipping the desktop bundle." ;;
esac

if [[ -n "$bundle_script" ]]; then
  if npm run "$bundle_script"; then
    find target/release/bundle -type f \( -name '*.deb' -o -name '*.AppImage' -o -name '*.dmg' \) \
      -exec cp {} "$out/" \;
  else
    echo "  The desktop bundle failed. The rest of the release folder is still valid." >&2
  fi
fi

if [[ -f "$root/RELEASE_NOTES.md" ]]; then
  cp "$root/RELEASE_NOTES.md" "$out/"
else
  echo "  RELEASE_NOTES.md is missing; write it before publishing." >&2
fi

echo
echo "== Checksums =="
( cd "$out" && find . -maxdepth 1 -type f ! -name 'SHA256SUMS' ! -name '*.sha256' -exec sha256sum {} + \
  | sed 's|\./||' > SHA256SUMS )
cat "$out/SHA256SUMS"

echo
echo "Release folder: $out"
