#!/usr/bin/env bash
# Render every Mermaid block in docs/ on a real browser and fail on a parse
# error. Mermaid only checks a diagram when it draws it, so a typo in a state
# label or an unsupported edge is invisible in the Markdown and breaks the
# rendered page. This script is that check, and it is the reason the docs use
# Mermaid.
#
# Usage: scripts/check-diagrams.sh [docs-dir]

set -euo pipefail

docs_dir="${1:-docs}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
case "$docs_dir" in
  /*) docs_abs="$docs_dir" ;;
  *) docs_abs="$root/$docs_dir" ;;
esac

if [ ! -d "$docs_abs" ]; then
  echo "no such docs directory: $docs_abs" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Mermaid's browser build, taken from the same package the CLI uses. The path is
# resolved once so the browser check and any local `mmdc` run agree on the
# version that decides what parses.
mermaid_cli="$(nix build --no-link --print-out-paths 'nixpkgs#mermaid-cli')"
mermaid_dist="$mermaid_cli/lib/node_modules/@mermaid-js/mermaid-cli/node_modules/mermaid/dist"
cp "$mermaid_dist/mermaid.min.js" "$work/mermaid.min.js"

mkdir -p "$work/blocks"
awk '
  /^```mermaid$/ { inblk = 1; n++; file = sprintf("'"$work"'/blocks/block-%03d.mmd", n); next }
  /^```$/ && inblk { inblk = 0; next }
  inblk { print > file }
' "$docs_abs"/*.md "$docs_abs"/*/*.md 2>/dev/null || true

count="$(find "$work/blocks" -name '*.mmd' | wc -l | tr -d ' ')"
if [ "$count" = "0" ]; then
  echo "no mermaid blocks under $docs_dir"
  exit 0
fi

cat > "$work/template.html" <<HTML
<!doctype html>
<html>
<head>
<meta charset="utf-8">
<script src="mermaid.min.js"></script>
<script>mermaid.initialize({ startOnLoad: false, theme: 'neutral' });</script>
</head>
<body></body>
</html>
HTML

cat > "$work/mermaid.spec.js" <<'SPEC'
const fs = require('fs');
const path = require('path');
const { test, expect } = require('playwright/test');

const dir = process.env.BLOCKS_DIR;
const page_url = 'file://' + process.env.TEMPLATE;
const files = fs.readdirSync(dir).filter((f) => f.endsWith('.mmd')).sort();

if (files.length === 0) {
  test('no diagrams', () => {});
}

for (const file of files) {
  test(`renders ${file}`, async ({ page }) => {
    const code = fs.readFileSync(path.join(dir, file), 'utf8');
    await page.goto(page_url);
    const failure = await page.evaluate(async (diagram) => {
      try {
        await window.mermaid.parse(diagram);
        const { svg } = await window.mermaid.render(
          'probe-' + Math.random().toString(36).slice(2),
          diagram,
        );
        return svg && svg.includes('<svg') ? null : 'no svg produced';
      } catch (e) {
        return String((e && e.message) || e);
      }
    }, code);
    expect(failure, `mermaid failed for ${file}`).toBeNull();
  });
}
SPEC

export BLOCKS_DIR="$work/blocks"
export TEMPLATE="$work/template.html"
echo "checking $count mermaid block(s) under $docs_dir"
cd "$work"
nix run --no-write-lock-file 'nixpkgs#playwright-test' -- \
  test mermaid.spec.js --reporter=list
