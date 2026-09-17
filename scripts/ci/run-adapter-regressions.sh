#!/usr/bin/env bash
# Run every committed Node-runnable adapter regression test.
#
# Files matched: adapters/**/*.regression.mjs (for example the Grok,
# Antigravity/protobuf, shared first-message, and Cursor regressions from
# PR #29). Each file is executed with plain `node` so the gate does not
# depend on Bun for these fixtures. Any failure fails the whole gate
# (no continue-on-error, no allowed-to-fail).
set -euo pipefail

cd "$(dirname "$0")/../.."

mapfile -t FILES < <(find adapters -name '*.regression.mjs' | sort)

if [ "${#FILES[@]}" -eq 0 ]; then
  echo "adapter-regressions: no *.regression.mjs files committed; nothing to run."
  exit 0
fi

echo "adapter-regressions: executing ${#FILES[@]} Node regression file(s)"
for f in "${FILES[@]}"; do
  echo "--- node $f"
  node "$f"
done
echo "adapter-regressions: all ${#FILES[@]} file(s) passed"
