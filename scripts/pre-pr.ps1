# Pre-PR memory-integrity gate (Windows): same safety contract as
# scripts/pre-pr.sh and CI, runnable before a PR is considered ready.
#
# Safety contract:
# - HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, and XDG_STATE_HOME are redirected
#   to a fresh temp directory so no command can resolve the real user
#   config/home/database.
# - Known live/archive roots (e.g. D:/Data/hstry) are refused up front.
# - Only synthetic sentinel data in isolated temp databases is used.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/pre-pr.ps1
# Or: just pre-pr-windows
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Fail([string]$Message) {
  Write-Error $Message
  exit 1
}

function Refuse-LivePath([string]$Path) {
  if ([string]::IsNullOrEmpty($Path)) { return }
  $normalized = $Path.Replace("\", "/").ToLowerInvariant()
  if ($normalized -eq "d:/data/hstry" -or $normalized.StartsWith("d:/data/hstry/")) {
    Fail("pre-pr: REFUSING known live/archive path: $Path")
  }
}

$IsolatedHome = Join-Path ([System.IO.Path]::GetTempPath()) ("hstry-pre-pr-home." + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $IsolatedHome | Out-Null
$env:HOME = $IsolatedHome
$env:XDG_CONFIG_HOME = Join-Path $IsolatedHome ".config"
$env:XDG_DATA_HOME = Join-Path $IsolatedHome ".local/share"
$env:XDG_STATE_HOME = Join-Path $IsolatedHome ".local/state"
New-Item -ItemType Directory -Path $env:XDG_CONFIG_HOME, $env:XDG_DATA_HOME, $env:XDG_STATE_HOME | Out-Null
Write-Host "pre-pr: isolated HOME=$IsolatedHome"

# Central test-guard: flag + operator-configured roots (not compiled in).
# A leaked flag with an empty root list refuses nothing.
$env:HSTRY_ENFORCE_TEST_DB_GUARD = "1"
if (-not $env:HSTRY_TEST_BLOCKED_DB_ROOTS) {
  $env:HSTRY_TEST_BLOCKED_DB_ROOTS = "D:/Data/hstry"
}

try {
  Refuse-LivePath($env:HSTRY_DATABASE)

  Write-Host "== pre-pr: cargo fmt --all --check"
  cargo fmt --all --check

  Write-Host "== pre-pr: cargo check --workspace --all-targets"
  cargo check --workspace --all-targets

  Write-Host "== pre-pr: cargo clippy --workspace --all-targets -- -D warnings"
  cargo clippy --workspace --all-targets -- -D warnings

  Write-Host "== pre-pr: cargo test --workspace --all-targets --no-fail-fast"
  cargo test --workspace --all-targets --no-fail-fast

  Write-Host "== pre-pr: memory-integrity gate (explicit)"
  cargo test -p hstry-core --test memory_integrity --no-fail-fast

  if (Get-Command bun -ErrorAction SilentlyContinue) {
    Write-Host "== pre-pr: adapter fixtures (bun)"
    bun run adapters/cursor/test.js
    bun run adapters/gemini-cli/test.js
    bun run adapters/workbuddy/test.js
  } else {
    Write-Host "pre-pr: bun not installed; skipping bun adapter fixtures (CI runs them)"
  }

  if (Get-Command node -ErrorAction SilentlyContinue) {
    Write-Host "== pre-pr: adapter regressions (node)"
    # Run each committed *.regression.mjs with node; any failure fails the gate.
    $files = Get-ChildItem -Path "adapters" -Filter "*.regression.mjs" -Recurse | Sort-Object FullName
    if ($files.Count -eq 0) {
      Write-Host "adapter-regressions: no *.regression.mjs files committed; nothing to run."
    } else {
      Write-Host ("adapter-regressions: executing {0} Node regression file(s)" -f $files.Count)
      foreach ($f in $files) {
        Write-Host "--- node $($f.FullName)"
        node $f.FullName
      }
      Write-Host ("adapter-regressions: all {0} file(s) passed" -f $files.Count)
    }
  } else {
    Fail("pre-pr: node not installed; cannot run *.regression.mjs gate")
  }

  Write-Host "pre-pr: gate passed"
} finally {
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $IsolatedHome
}
