# Pre-PR Memory-Integrity Gate

This is the canonical pre-PR safety suite for Chronicle, the authoritative
conversation/memory SQLite archive. A green compile/test matrix is not
enough: a PR must not be able to silently corrupt, truncate, or make the
archive unrecoverable. Planning source of truth: GitHub issue #33.

## Local entry point

Unix/macOS (also works in Windows Git Bash):

```bash
./scripts/pre-pr.sh
```

Windows PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/pre-pr.ps1
```

Via just:

```bash
just pre-pr          # unix / git-bash
just pre-pr-windows  # Windows PowerShell
```

Pre-commit stays lightweight; this pre-PR gate is the hard gate.

## Safety contract

- `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_STATE_HOME` are
  redirected to a fresh temp directory, so no command can resolve the real
  user config/home/database.
- Central enforcement, not just convention: pre-PR scripts and CI set
  `HSTRY_ENFORCE_TEST_DB_GUARD=1` **and** `HSTRY_TEST_BLOCKED_DB_ROOTS`
  (operator-configured `;`-separated roots; this fork's scripts default
  to `D:/Data/hstry`). `Database::open` refuses those roots BEFORE any
  directory creation or connection. The library does not compile in a
  machine-specific path. The flag alone, with an empty root list, is a
  no-op — a leaked flag cannot refuse a production archive. Tests that
  prove the guard inject a synthetic root of their own and never assert
  that a real disk (e.g. `D:/Data`) does not exist. Per-test
  `hstry_core::test_guard::assert_test_path_safe` / `isolated_temp_db`
  remain as a second layer against the same env list.
- Only synthetic sentinel conversations/messages are seeded, in isolated
  temp databases. No network, SSH, NAS, or remotes anywhere in the suite.

## What CI runs on every PR

Workflow: `.github/workflows/ci.yml`.

| Job / step | Command | OS matrix |
|---|---|---|
| Test: formatting | `cargo fmt --all --check` | ubuntu, macOS 14, Windows |
| Test: check | `cargo check --workspace --all-targets` | ubuntu, macOS 14, Windows |
| Test: lint | `cargo clippy --workspace --all-targets -- -D warnings` | ubuntu, macOS 14, Windows |
| Test: full suite | `cargo test --workspace --all-targets --no-fail-fast` | ubuntu, macOS 14, Windows |
| Test: integrity (explicit log section) | `cargo test -p hstry-core --test memory_integrity --no-fail-fast` | ubuntu, macOS 14, Windows |
| Adapter fixtures (bun) | `bun run adapters/{cursor,gemini-cli,workbuddy}/test.js` | ubuntu |
| Adapter regressions (node) | `./scripts/ci/run-adapter-regressions.sh` (every committed `adapters/**/*.regression.mjs`) | ubuntu |

Windows is a mandatory storage-safety platform because file-handle
semantics differ (open SQLite files lock on Windows). No step uses
`continue-on-error` and no suite is allowed-to-fail.

## DB lifecycle simulated by `crates/hstry-core/tests/memory_integrity.rs`

1. Fresh DB init + required-schema check (`require_schema`).
2. Deterministic sentinel seed (2 conversations, 4 messages) + retrieval
   after ingest, verifying titles and sentinel content.
3. `PRAGMA quick_check` must return `ok` at every stage.
4. Checkpoint create -> restore to scratch -> mutate scratch (extra
   conversation lands) -> plant a stale WAL sidecar -> restore over the same
   scratch database -> `quick_check` plus sentinel and row-count preservation.
   The separate WAL/SHM lifecycle test also covers replacing an existing
   closed slot with stale sidecars.
5. WAL/SHM-sensitive lifecycle on the real product pattern: restores
   target a scratch/sibling slot (as `default_restore_path` does), never an
   open live database. The test reuses one slot across two incarnations
   (open -> close with WAL checkpoint -> plant stale `-wal`/`-shm` ->
   restore over the closed slot), asserting the replace succeeds, stale
   sidecars are cleared, and sentinels survive. The stale-sidecar fixture
   is established with a bounded wait so it never truncates a still-mapped
   section (Windows os error 1224). A Windows-only test additionally holds
   the slot open and asserts restore fails safely (no partial file) and
   then succeeds after release. Note: on `main`,
   `checkpoint restore --live` still replaces the file while the pool is
   open; that overwrite-while-open sequence is a separately owned defect
   (PR #30) and is deliberately not modeled here. `restore_checkpoint`
   removes the target and each `-wal`/`-shm` sidecar strictly via
   `safe_remove_file` (bounded ~15s retry on transient Windows locks
   32/5/1224, every other failure propagates) and opens the freshly
   written archive through a bounded transient-only retry, since that
   open races handle release/indexing on hosted Windows; decode and
   corruption errors never retry. Every restore filesystem step reports
   its stage and path, so CI is diagnosable from one error line. The
   shape matches PR #30, so it rebases cleanly; the gate adds
   tests/workflow/guard infrastructure, not a second restore
   implementation. No best-effort test helper is used in the production
   path. Checkpoint *creation* is an online operation: the live archive
   handle stays open. The snapshot is produced with `VACUUM INTO`, then
   inspected read-only (no WAL, no migrations, no pooled writer) so
   Windows never has to release a mapped `-wal`/`-shm` on that temp copy
   before compress/delete. Snapshot file ops reuse the same
   `safe_open_file` / `safe_create_file` / `safe_remove_file` transient
   retry (32/5/1224 only); corruption and integrity failures fail fast.
   Every create filesystem step reports its stage and path.
6. Fetched/full-sync validation helpers
   (`Database::validate_hstry_database_file`, read-only, local temp files
   only): a valid archive passes; a corrupt file and a SQLite file without
   the archive schema are rejected.
7. Migration protection: a deterministic v001-only database (built from the
   repo's own `001` migration + `schema_migrations` marker, with legacy
   sentinel rows) is opened/migrated by `Database::open`, then verified
   with `quick_check`, sentinel preservation, and applied-version equals
   migration-file count.

## Migration contract

The repo has an explicit schema-version mechanism: versioned
`crates/hstry-core/migrations/NNN_*.sql` files, tracked in the
`schema_migrations` table, applied from disk or from the embedded list in
`crates/hstry-core/src/db.rs`. The smallest maintainable contract for new
migrations:

1. Add `NNN_description.sql` (next number) using `IF NOT EXISTS` /
   PRAGMA-guarded `ALTER TABLE`.
2. Append it to the embedded list in `db.rs`.
3. Never remove the tables checked by `Database::require_schema`
   (`schema_migrations`, `sources`, `conversations`, `messages`).
4. The migration-protection test picks the new file up automatically and
   fails if it does not apply cleanly or loses sentinel data.

## Known baseline issue addressed

`ingest::tests::concurrent_batches_queue_without_database_locked_errors`
previously failed on Windows at final `remove_file` with os error 32
(file in use): the pool's connections were closed but the file removal had
no tolerance for Windows handle release, and the test used a bare
`temp_dir()` path with no isolation guard. Fixed precisely (no
`continue-on-error`, no ignored Windows failures):

- the test now uses `test_guard::isolated_temp_db` (tempdir + live-path
  refusal) and `test_guard::close_and_remove_db`, which closes the
  database (WAL checkpoint on close) and retries file removal with sidecar
  (`-wal`/`-shm`/`-journal`) cleanup;
- the sibling `outcome_distinguishes_created_from_updated_conversations`
  test had the same latent leak (never closed/removed its temp DB) and
  uses the same helper now.

## Runtime impact

The `memory_integrity` suite runs 6 tests against small synthetic
databases (a handful of rows, one zstd checkpoint roundtrip each in two
tests). Measured locally well under a few seconds of test time on top of
the normal workspace build; heavier scale/stress tests remain out of
scope for the PR gate and can run nightly.
