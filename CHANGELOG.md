# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This repository is a personal fork of [byteowlz/hstry](https://github.com/byteowlz/hstry).
Upstream last consumed here is `v0.5.25` (`a4be7c9`). Keep fork version at `1.0.0`.

## [Unreleased]

### Added

- Hub-side satellite ingest: `hstry hub ingest` merges a delta sqlite into the live hub under `{device_id}:` while holding an ingest lock. `hstry remote sync --direction push` no longer SCP-overwrites the hub file.
- Rolling hub checkpoints: `hstry checkpoint create|list|restore|prune`, daily/weekly tags, 10 GiB compressed cap. Hub service creates them when `[checkpoint] enabled`.
- Antigravity 2.0 app, agy CLI, and IDE 1 stores (`~/.gemini/antigravity*`, SQLite + brain transcript). Legacy `~/.gemini/tmp` JSONL remains.
- DeepSeek Harness adapter (`dsh`) for `~/.dsh/sessions` (`session.jsonl` / concatenated zstd).
- Zcode / ZAI adapter (`zcode`) for `~/.zcode` (`cli/db/db.sqlite` session / message / part).
- Restore runbook: [docs/restore.md](docs/restore.md) (hub snapshots to Google Drive; search-only restore on this PC).
- Upstream v0.5.25 recall: exact/needle/regex with bounded evidence, shared bounded reads, installable retrieval skill (`hstry skill`), and opt-in content-free search diagnostics.

### Changed

- Satellite push exports only conversations with `updated_at` since the last successful ingest (watermark in `search_state`). `--full` keeps the legacy whole-file replace for recovery.
- Satellite search still queries the hub remote (`sync.hub_remote`, trx-1xsa) but defaults to `--scope all` so local staging is read too. Pass `--scope local` for this machine only, `--scope remote` for hub-only.
- Search JSON now returns a report object; upgrade service/API/remote binaries together. `show` is bounded by default; use `--full` for legacy output.
- Resume uses structured arguments, fresh conversion identities, origin sidecars, and no-overwrite placement. JSON returns a non-executing plan; converted launches require `--allow-unverified`.

### Fixed

- Restore the global `--no-color` flag (`console::set_colors_enabled(false)`). Scripts that pass `hstry --no-color` no longer die on an unexpected clap argument.
- Satellite default search reads the local database. An unreachable hub keeps those local hits (with a warning) instead of failing the whole search.
- Antigravity SQLite conversations read timestamps from `step_payload` field 5 (`CortexStepMetadata` Timestamp, or a unix varint) when `metadata` is empty, instead of using wall-clock `Date.now()` per row.
- Default remote search (CLI and TUI) uses `sync.hub_remote` and errors if that name is missing, instead of silently querying every remote. Explicit `--remote` is unchanged.
- Windows MSVC debug builds reserve an 8MB stack so `hstry --help` no longer hits `STATUS_STACK_OVERFLOW`.
- TUI compilation with remote search reports and OpenCode conversion layout/session IDs.

## [1.0.0] - 2026-08-21

Fork 1.0: local agent archive + NAS hub/satellite. Canonical remote is `andrew05060414/chronicle`.

### Added

- Windows as a first-class collector: Cursor Composer import, Node adapter runtime, `scripts/update-adapters.ps1`.
- Adapters: `qclaw` (QClaw / OpenClaw), `workbuddy`, `antigravity` (Gemini CLI sessions under `~/.gemini/tmp`, CLI-only).
- `sync.device_id` namespaces satellite pushes (`arknights` / `macbook`) so Windows and Mac do not overwrite each other on the hub.
- Remote sync fixes for hub paths that contain spaces, Windows SCP, and refreshing source metadata on repeat push.
- Docs for personal NAS hub / Mac satellite setup (fork-only paths).

### Changed

- Adapter canonical-root checks normalize Windows path separators.
- Ignore local adapter test artifacts (`dry-run*.txt`, `parse-limit*.json`).

### Not in 1.0

- Upstream TUI overhaul (Resume / grouping / images) — current three-pane TUI remains.
- Live ChatGPT / Gemini / Copilot web ingest (export adapters already exist for takeout files).
- Antigravity IDE 2.0 session store.

## [0.5.25] - 2026-09-06

Upstream release consumed by this fork while remaining at `1.0.0`.

### Added

- Exact/needle/regex recall with bounded evidence, stable anchors, and explicit scope/completeness reporting.
- Shared bounded reads across CLI, HTTP, MCP, and SSH, including field continuations and linked tool interactions.
- Installable retrieval skill and opt-in content-free search diagnostics.

### Changed

- Search JSON now returns a report object; upgrade service/API/remote binaries together. `show` is bounded by default; use `--full` for legacy output.
- Resume uses structured arguments, fresh conversion identities, origin sidecars, and no-overwrite placement. JSON returns a non-executing plan; converted launches require `--allow-unverified`.

### Fixed

- TUI compilation with remote search reports and OpenCode conversion layout/session IDs.

## [0.5.16] - 2026-04-26

### Changed

- **TUI left pane**: Group source entries by adapter instead of individual source ID. One entry per adapter (e.g., one `pi`, one `claude-code`) even when multiple source IDs exist for the same adapter.
- **`hstry list`**: Automatically deduplicate conversations across sources by `external_id`/`readable_id`/`platform_id`. Canonical source IDs (e.g. `pi`) take precedence over `import-pi` style IDs.
- **`hstry import`**: Reuse existing source when importing to a path that already has one, or default to canonical adapter ID instead of `import-<adapter>`. Prevents duplicate source creation.
- **Startup performance**: Replace `COUNT(*)` scans on FTS5 tables with `SELECT 1 LIMIT 1` probes during DB init. Eliminates multi-second startup latency on large databases.

## [0.5.15] - 2026-01-28

### Added

- (previous releases)
