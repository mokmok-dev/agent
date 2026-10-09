#!/usr/bin/env bash
# Needs bwrap, socat and rg on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
export SHELL="${SHELL:-$(command -v bash)}"
for probe in repro/0*.mjs; do
  printf '########## %s\n' "$probe"
  node "$probe"
  printf '\n'
done
