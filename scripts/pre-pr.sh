#!/usr/bin/env bash
# Pre-PR memory-integrity gate: the same safety contract CI enforces,
# runnable locally before a PR is considered ready.
#
# Safety contract:
# - HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, and XDG_STATE_HOME are redirected
#   to a fresh temp directory so no command can resolve the real user
#   config/home/database.
# - Known live/archive roots (e.g. D:/Data/hstry) are refused up front.
# - Only synthetic sentinel data in isolated temp databases is used.
#
# Usage: ./scripts/pre-pr.sh   (on Windows: scripts/pre-pr.ps1)
# Pre-commit stays lightweight; this pre-PR gate is the hard gate.
set -euo pipefail

cd "$(dirname "$0")/.."

ISOLATED_HOME="$(mktemp -d "${TMPDIR:-/tmp}/hstry-pre-pr-home.XXXXXX")"
export HOME="$ISOLATED_HOME"
export XDG_CONFIG_HOME="$ISOLATED_HOME/.config"
export XDG_DATA_HOME="$ISOLATED_HOME/.local/share"
export XDG_STATE_HOME="$ISOLATED_HOME/.local/state"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"
echo "pre-pr: isolated HOME=$HOME"
trap 'rm -rf "$ISOLATED_HOME"' EXIT

# Central test-guard: flag + operator-configured roots (not compiled in).
# A leaked flag with an empty root list refuses nothing.
export HSTRY_ENFORCE_TEST_DB_GUARD=1
export HSTRY_TEST_BLOCKED_DB_ROOTS="${HSTRY_TEST_BLOCKED_DB_ROOTS:-D:/Data/hstry}"

refuse_live_path() {
  case "$(printf '%s' "$1" | tr '\\\\' '/' | tr '[:upper:]' '[:lower:]')" in
    d:/data/hstry|d:/data/hstry/*)
      echo "pre-pr: REFUSING known live/archive path: $1" >&2
      exit 1
      ;;
  esac
}

refuse_live_path "${HSTRY_DATABASE:-}"
refuse_live_path "${XDG_DATA_HOME:-}/hstry/hstry.db"

echo "== pre-pr: cargo fmt --all --check"
cargo fmt --all --check

echo "== pre-pr: cargo check --workspace --all-targets"
cargo check --workspace --all-targets

echo "== pre-pr: cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "== pre-pr: cargo test --workspace --all-targets --no-fail-fast"
cargo test --workspace --all-targets --no-fail-fast

echo "== pre-pr: memory-integrity gate (explicit)"
cargo test -p hstry-core --test memory_integrity --no-fail-fast

if command -v bun >/dev/null 2>&1; then
  echo "== pre-pr: adapter fixtures (bun)"
  bun run adapters/cursor/test.js
  bun run adapters/gemini-cli/test.js
  bun run adapters/workbuddy/test.js
else
  echo "pre-pr: bun not installed; skipping bun adapter fixtures (CI runs them)"
fi

if command -v node >/dev/null 2>&1; then
  echo "== pre-pr: adapter regressions (node)"
  bash ./scripts/ci/run-adapter-regressions.sh
else
  echo "pre-pr: node not installed; cannot run *.regression.mjs gate" >&2
  exit 1
fi

echo "pre-pr: gate passed"
