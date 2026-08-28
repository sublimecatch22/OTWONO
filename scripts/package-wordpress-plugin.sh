#!/usr/bin/env bash
#
# Build the installable WordPress plugin ZIP.
#
# The archive contains exactly what WordPress needs and nothing else: no tests,
# no build artefacts, no dotfiles. A ZIP that installs and runs with no further
# steps is the whole point.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
plugin_dir="$root/wordpress/otwono-ai-connector"
out_dir="${1:-$root/releases}"
zip_path="$out_dir/otwono-ai-connector.zip"

if [[ ! -f "$plugin_dir/otwono-ai-connector.php" ]]; then
  echo "error: the plugin source is not where it should be ($plugin_dir)" >&2
  exit 1
fi

echo "Checking PHP syntax…"
missing_php=0
command -v php >/dev/null 2>&1 || missing_php=1
if [[ $missing_php -eq 0 ]]; then
  while IFS= read -r file; do
    php -l "$file" > /dev/null || { echo "error: $file has a syntax error" >&2; exit 1; }
  done < <(find "$plugin_dir" -name '*.php')
else
  echo "  php is not installed here; skipping the syntax check."
fi

mkdir -p "$out_dir"
rm -f "$zip_path"

# Build from a staging copy so the archive's top-level directory is the plugin
# slug, which is what WordPress expects from an uploaded ZIP.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

cp -R "$plugin_dir" "$staging/otwono-ai-connector"
find "$staging" \( -name '.*' -o -name '*.log' -o -name 'node_modules' \) -prune -exec rm -rf {} + 2>/dev/null || true

( cd "$staging" && zip -r -q -X "$zip_path" otwono-ai-connector )

echo
echo "Built: $zip_path"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$zip_path" | tee "$zip_path.sha256"
elif command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$zip_path" | tee "$zip_path.sha256"
fi

echo
echo "Install it through WordPress Admin → Plugins → Add New → Upload Plugin."
