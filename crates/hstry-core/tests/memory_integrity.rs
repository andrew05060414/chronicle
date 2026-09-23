//! Pre-PR memory-integrity smoke gate for the authoritative archive.
//!
//! Every test here runs against an isolated temporary HOME/config/database
//! created by [`hstry_core::test_guard::isolated_temp_db`]. No test reads or
//! writes a live archive, a NAS hub, or any remote: synthetic sentinel data
//! only, local temp files only, no network/SSH.
//!
//! Coverage contract (GitHub issue #33):
//! - fresh DB init + required schema availability;
//! - deterministic sentinel seed + retrieval after ingest;
//! - `PRAGMA quick_check` + required schema/table checks;
//! - checkpoint create -> mutate scratch -> restore to scratch ->
//!   quick_check + sentinel/row-count preservation;
//! - WAL/SHM-sensitive lifecycle with all handles closed before replacement
//!   (Windows file-handle semantics); on Windows only, restore over still-
//!   open handles must fail safely and succeed after release;
//! - fetched/full-sync database validation helpers against local temp files;
//! - prior-schema builder that migrates/opens and preserves sentinels;
//! - live/archive paths are refused before any database work runs, centrally
//!   enforced at the `Database::open` boundary while
//!   `HSTRY_ENFORCE_TEST_DB_GUARD` is set (proven before mutation).

use std::path::{Path, PathBuf};

use hstry_core::Database;
use hstry_core::checkpoint::{create_checkpoint, restore_checkpoint};
use hstry_core::config::CheckpointConfig;
use hstry_core::ingest::ingest_batch;
use hstry_core::models::Source;
use hstry_core::parsed::{ParsedConversation, ParsedMessage};
use hstry_core::test_guard::{
    BLOCKED_ROOTS_ENV_VAR, GUARD_ENV_VAR, assert_test_path_safe, isolated_temp_db,
};

const SENTINEL_A: &str = "hstry-integrity-sentinel-alpha-7f3a9c";
const SENTINEL_B: &str = "hstry-integrity-sentinel-beta-41d2e8";
const SENTINEL_MESSAGE: &str = "hstry-integrity-sentinel-message-9be671";

fn sentinel_conversation(external_id: &str, title: &str) -> ParsedConversation {
    ParsedConversation {
        external_id: Some(external_id.to_string()),
        readable_id: None,
        title: Some(title.to_string()),
        created_at: 1_700_000_000_000,
        updated_at: None,
        model: None,
        provider: None,
        workspace: None,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
        messages: vec![
            ParsedMessage {
                role: "user".to_string(),
                content: format!("{SENTINEL_MESSAGE} user {external_id}"),
                created_at: None,
                model: None,
                tokens: None,
                cost_usd: None,
                parts: None,
                tool_calls: None,
                metadata: None,
            },
            ParsedMessage {
                role: "assistant".to_string(),
                content: format!("{SENTINEL_MESSAGE} assistant {external_id}"),
                created_at: None,
                model: None,
                tokens: None,
                cost_usd: None,
                parts: None,
                tool_calls: None,
                metadata: None,
            },
        ],
        metadata: None,
        version: None,
        message_count: None,
        parent_external_id: None,
        parent_message_idx: None,
        fork_type: None,
    }
}

async fn seed_sentinels(db: &Database) -> anyhow::Result<()> {
    db.upsert_source(&Source {
        id: "integrity-gate".to_string(),
        adapter: "integrity-gate".to_string(),
        path: None,
        last_sync_at: None,
        config: serde_json::json!({}),
    })
    .await?;
    ingest_batch(
        db,
        "integrity-gate",
        vec![
            sentinel_conversation("sentinel-a", SENTINEL_A),
            sentinel_conversation("sentinel-b", SENTINEL_B),
        ],
    )
    .await?;
    Ok(())
}

async fn assert_sentinels_present(db: &Database) -> anyhow::Result<()> {
    anyhow::ensure!(db.quick_check().await? == "ok", "quick_check must be ok");
    db.require_schema().await?;
    anyhow::ensure!(
        db.count_conversations().await? == 2,
        "sentinel conversation count must be preserved"
    );
    anyhow::ensure!(
        db.count_messages().await? == 4,
        "sentinel message count must be preserved"
    );
    for (external_id, title) in [("sentinel-a", SENTINEL_A), ("sentinel-b", SENTINEL_B)] {
        let conv = db
            .get_conversation_by_reference(
                Some("integrity-gate"),
                Some(external_id),
                None,
                None,
                None,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("sentinel conversation missing: {external_id}"))?;
        anyhow::ensure!(
            conv.title.as_deref() == Some(title),
            "sentinel title mismatch for {external_id}"
        );
        let messages = db.get_messages(conv.id).await?;
        anyhow::ensure!(messages.len() == 2, "sentinel messages missing");
        for msg in &messages {
            anyhow::ensure!(
                msg.content.contains(SENTINEL_MESSAGE),
                "sentinel content missing in retrieved message"
            );
        }
    }
    Ok(())
}

fn checkpoint_config(dir: &Path) -> CheckpointConfig {
    CheckpointConfig {
        enabled: true,
        dir: Some(dir.join("checkpoints")),
        max_total_bytes: 64 * 1024 * 1024,
        ..Default::default()
    }
}

/// Write a stale sidecar fixture, waiting (bounded) for the OS to release
/// the memory-mapped section after close. On Windows the `-shm` file can
/// stay mapped briefly after the last handle closes (os error 1224,
/// user-mapped section open); the retry establishes the fixture only after
/// the mapping is gone, so the stale state is legitimate and never
/// truncates a live mapping.
async fn write_stale_sidecar(slot: &Path, suffix: &str) -> anyhow::Result<()> {
    let target = format!("{}{suffix}", slot.display());
    let mut last_err = String::new();
    for _ in 0..100 {
        match std::fs::write(&target, b"stale-sidecar-fixture") {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = format!("{e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    anyhow::bail!("could not establish stale sidecar fixture {target}: {last_err}")
}

#[test]
fn live_archive_paths_are_refused_before_any_db_work() {
    const ROOT: &str = "c:/hstry-test-blocked-root";
    temp_env::with_var(BLOCKED_ROOTS_ENV_VAR, Some(ROOT), || {
        for blocked in [
            "c:/hstry-test-blocked-root/staging.db",
            "c:\\hstry-test-blocked-root\\staging.db",
            "c:/hstry-test-blocked-root/hstry.db",
            "c:/hstry-test-blocked-root/nested/archive.db",
        ] {
            assert!(
                std::panic::catch_unwind(|| assert_test_path_safe(Path::new(blocked))).is_err(),
                "blocked path was not refused: {blocked}"
            );
        }
    });
    let (_tmp, safe) = isolated_temp_db("integrity-gate-guard");
    assert_test_path_safe(&safe);
}

#[tokio::test]
async fn sentinel_roundtrip_with_quick_check_and_schema() -> anyhow::Result<()> {
    let (_tmp, db_path) = isolated_temp_db("integrity-gate-roundtrip");
    let db = Database::open(&db_path).await?;
    seed_sentinels(&db).await?;
    assert_sentinels_present(&db).await?;
    db.close().await;
    Ok(())
}

#[tokio::test]
async fn checkpoint_mutate_restore_preserves_sentinels() -> anyhow::Result<()> {
    let (_tmp, db_path) = isolated_temp_db("integrity-gate-checkpoint");
    let tmp_dir: PathBuf = _tmp.path().to_path_buf();
    let cfg = checkpoint_config(&tmp_dir);

    let db = Database::open(&db_path).await?;
    seed_sentinels(&db).await?;
    let created = create_checkpoint(&db, &db_path, &cfg, Some(false)).await?;
    anyhow::ensure!(
        created.manifest.integrity == "ok",
        "checkpoint must record a clean integrity check"
    );
    db.close().await;

    // Restore to scratch, then mutate the scratch copy.
    let scratch = tmp_dir.join("scratch.db");
    restore_checkpoint(
        &tmp_dir.join("checkpoints"),
        &created.manifest.stem,
        &scratch,
    )?;
    let scratch_db = Database::open(&scratch).await?;
    scratch_db
        .upsert_source(&Source {
            id: "mutation".to_string(),
            adapter: "mutation".to_string(),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        })
        .await?;
    ingest_batch(
        &scratch_db,
        "mutation",
        vec![sentinel_conversation("mutant", "mutant-title")],
    )
    .await?;
    anyhow::ensure!(
        scratch_db.count_conversations().await? == 3,
        "scratch mutation must land"
    );
    scratch_db.close().await;

    // Restore over the mutated scratch database. Restore must replace the old
    // contents and clear stale sidecars before reopening the restored copy.
    let stale_wal = format!("{}-wal", scratch.display());
    write_stale_sidecar(&scratch, "-wal").await?;
    restore_checkpoint(
        &tmp_dir.join("checkpoints"),
        &created.manifest.stem,
        &scratch,
    )?;
    anyhow::ensure!(
        !Path::new(&stale_wal).exists(),
        "stale WAL sidecar must be removed by restore"
    );

    let restored = Database::open(&scratch).await?;
    assert_sentinels_present(&restored).await?;
    anyhow::ensure!(
        restored.count_conversations().await? == 2,
        "restore must discard the scratch-only mutation"
    );
    restored.close().await;
    Ok(())
}

#[tokio::test]
async fn wal_shm_lifecycle_closed_before_replace() -> anyhow::Result<()> {
    // Real product restore semantics: restores target a scratch/sibling slot
    // (e.g. `<stem>.restore.db` via `default_restore_path`), never an open
    // live database. On main, `checkpoint restore --live` still holds the
    // pool open across the replacement -- that overwrite-while-open sequence
    // is a separately owned defect (PR #30), so the gate must not model it.
    //
    // This test reuses one slot path across two incarnations, which is what
    // the product does every time it restores into the same `*.restore.db`
    // slot: open the slot, close it (WAL checkpointed on close), plant stale
    // `-wal`/`-shm` sidecars simulating an unclean prior lifecycle, then
    // restore over the closed slot. The restore must succeed, clear the
    // sidecars so they can never replay over the restored file, and preserve
    // sentinels with a clean `quick_check`.
    let (_tmp, db_path) = isolated_temp_db("integrity-gate-lifecycle");
    let tmp_dir: PathBuf = _tmp.path().to_path_buf();
    let cfg = checkpoint_config(&tmp_dir);

    let db = Database::open(&db_path).await?;
    seed_sentinels(&db).await?;
    let created = create_checkpoint(&db, &db_path, &cfg, Some(false)).await?;
    db.close().await;

    // First incarnation of the restore slot: previously hosted live handles,
    // now released via `close`.
    let slot = tmp_dir.join("restore-slot.db");
    let prior = Database::open(&slot).await?;
    prior.close().await;

    // Stale sidecars: restore must clear them (Windows also treats a
    // leftover sidecar as a lock on the restore target). The fixture write
    // waits for the post-close unmap so it never truncates a live mapping
    // (os error 1224).
    let stale_wal = format!("{}-wal", slot.display());
    let stale_shm = format!("{}-shm", slot.display());
    write_stale_sidecar(&slot, "-wal").await?;
    write_stale_sidecar(&slot, "-shm").await?;
    restore_checkpoint(&tmp_dir.join("checkpoints"), &created.manifest.stem, &slot)?;
    anyhow::ensure!(
        !Path::new(&stale_wal).exists() && !Path::new(&stale_shm).exists(),
        "stale WAL/SHM sidecars must be removed by restore"
    );

    let reopened = Database::open(&slot).await?;
    assert_sentinels_present(&reopened).await?;
    reopened.close().await;
    Ok(())
}

/// Windows-only: restoring over a database whose handles are still open must
/// fail safely instead of writing a partial file; after the handles are
/// released the same restore succeeds with sentinels intact.
///
/// Windows denies replacing a file with live handles (os error 32), so the
/// refusal half is Windows-specific by OS semantics -- POSIX permits
/// replacing open files. The success-after-release half runs on all
/// platforms inside `wal_shm_lifecycle_closed_before_replace`.
#[cfg(windows)]
#[tokio::test]
async fn restore_refuses_open_handles_and_succeeds_after_close() -> anyhow::Result<()> {
    let (_tmp, db_path) = isolated_temp_db("integrity-gate-refusal");
    let tmp_dir: PathBuf = _tmp.path().to_path_buf();
    let cfg = checkpoint_config(&tmp_dir);

    let db = Database::open(&db_path).await?;
    seed_sentinels(&db).await?;
    let created = create_checkpoint(&db, &db_path, &cfg, Some(false)).await?;
    db.close().await;

    let slot = tmp_dir.join("refusal-slot.db");
    restore_checkpoint(&tmp_dir.join("checkpoints"), &created.manifest.stem, &slot)?;

    // Hold the slot open: handles are genuinely live here by construction.
    let held = Database::open(&slot).await?;
    anyhow::ensure!(
        held.count_conversations().await? == 2,
        "slot must hold the restored sentinels before the refusal attempt"
    );
    let refused = restore_checkpoint(&tmp_dir.join("checkpoints"), &created.manifest.stem, &slot);
    anyhow::ensure!(
        refused.is_err(),
        "restore over open handles must fail on Windows"
    );
    held.close().await;

    // Handles released: the same restore now succeeds and nothing partial
    // was left behind by the refused attempt.
    restore_checkpoint(&tmp_dir.join("checkpoints"), &created.manifest.stem, &slot)?;
    let reopened = Database::open(&slot).await?;
    assert_sentinels_present(&reopened).await?;
    reopened.close().await;
    Ok(())
}

/// Central-enforcement proof: with `HSTRY_ENFORCE_TEST_DB_GUARD` set and a
/// synthetic blocked root, opening a path under that root is rejected at
/// `Database::open` before any filesystem or database mutation — the
/// blocked root itself must not appear.
///
/// Synchronous test driving a fresh runtime so the flag mutation (via
/// `temp_env`, which restores it afterwards) never overlaps an ambient
/// async context. Sibling tests open isolated temp paths, which pass the
/// guard either way. The blocked root is injected, never a real live disk.
#[test]
fn open_rejects_blocked_live_path_before_mutation() {
    let (tmp, _) = isolated_temp_db("integrity-gate-open-guard");
    let blocked_root = tmp.path().join("blocked-root-must-not-be-created");
    let db_path = blocked_root.join("staging.db");
    let root_str = blocked_root.to_string_lossy().into_owned();
    temp_env::with_vars(
        [
            (GUARD_ENV_VAR, Some("1")),
            (BLOCKED_ROOTS_ENV_VAR, Some(root_str.as_str())),
        ],
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build single-threaded test runtime");
            let result = rt.block_on(Database::open(&db_path));
            assert!(
                result.is_err(),
                "guarded open of a configured blocked path must fail"
            );
            assert!(
                !db_path.exists() && !blocked_root.exists(),
                "refused open must not mutate the filesystem"
            );
        },
    );
}

#[tokio::test]
async fn fetched_db_validation_helpers_use_local_temp_files_only() -> anyhow::Result<()> {
    // No SSH, no NAS, no remotes: validation opens local temp files read-only.
    let (_tmp, db_path) = isolated_temp_db("integrity-gate-validate");
    let db = Database::open(&db_path).await?;
    seed_sentinels(&db).await?;
    db.close().await;
    Database::validate_hstry_database_file(&db_path).await?;

    let garbage = _tmp.path().join("garbage.db");
    std::fs::write(&garbage, b"this is not a sqlite database")?;
    anyhow::ensure!(
        Database::validate_hstry_database_file(&garbage)
            .await
            .is_err(),
        "corrupt fixture must be rejected"
    );

    let wrong_schema = _tmp.path().join("wrong-schema.db");
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite:{}?mode=rwc", wrong_schema.display()))
            .await?;
        sqlx::query("CREATE TABLE not_conversations (id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await?;
        pool.close().await;
    }
    anyhow::ensure!(
        Database::validate_hstry_database_file(&wrong_schema)
            .await
            .is_err(),
        "sqlite file without the archive schema must be rejected"
    );
    Ok(())
}

/// Deterministic prior-schema builder: a v001-only database with sentinel
/// rows, built from the repo's own versioned `001` migration plus the
/// `schema_migrations` version marker. `Database::open` must migrate it to
/// the current schema without losing sentinel data.
///
/// Migration contract (see `crates/hstry-core/migrations/README.md`): every
/// new `NNN_*.sql` migration is appended to both the migrations directory
/// and the embedded list in `db.rs`, is idempotent (`IF NOT EXISTS` /
/// PRAGMA-guarded `ALTER TABLE`), and never removes the required tables
/// checked by `Database::require_schema`. This test automatically picks up
/// new migrations by comparing against the migration-file count.
#[tokio::test]
async fn prior_schema_migrates_and_preserves_sentinels() -> anyhow::Result<()> {
    const V1_SCHEMA: &str = include_str!("../migrations/001_initial_schema.sql");

    let (_tmp, db_path) = isolated_temp_db("integrity-gate-migration");
    {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite:{}?mode=rwc", db_path.display()))
            .await?;
        sqlx::raw_sql(V1_SCHEMA).execute(&pool).await?;
        // `schema_migrations` is created by `Database::init` (see
        // `schema::SCHEMA`), not by `001`; recreate that exact step here so
        // the builder represents a genuine v1-era database.
        sqlx::raw_sql(hstry_core::schema::SCHEMA)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (1, '001_initial_schema.sql', 1)",
        )
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO sources (id, adapter, path, last_sync_at, config) VALUES ('integrity-gate', 'integrity-gate', NULL, NULL, '{}')")
            .execute(&pool)
            .await?;
        let conv_id = "11111111-1111-1111-1111-111111111111";
        sqlx::query(
            "INSERT INTO conversations (id, source_id, external_id, readable_id, title, created_at, updated_at, model, workspace, tokens_in, tokens_out, cost_usd, metadata)
             VALUES (?, 'integrity-gate', 'legacy-sentinel', 'legacy-readable', ?, 1700000000000, NULL, NULL, NULL, NULL, NULL, NULL, '{}')",
        )
        .bind(conv_id)
        .bind(SENTINEL_A)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, idx, role, content, parts_json, created_at, model, tokens, cost_usd, metadata)
             VALUES ('22222222-2222-2222-2222-222222222222', ?, 0, 'user', ?, '[]', NULL, NULL, NULL, NULL, '{}')",
        )
        .bind(conv_id)
        .bind(format!("{SENTINEL_MESSAGE} legacy"))
        .execute(&pool)
        .await?;
        pool.close().await;
    }

    let db = Database::open(&db_path).await?;
    anyhow::ensure!(
        db.quick_check().await? == "ok",
        "migrated DB must pass quick_check"
    );
    db.require_schema().await?;
    let conv = db
        .get_conversation_by_reference(
            Some("integrity-gate"),
            Some("legacy-sentinel"),
            None,
            None,
            None,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("legacy sentinel conversation lost during migration"))?;
    anyhow::ensure!(
        conv.title.as_deref() == Some(SENTINEL_A),
        "legacy sentinel title lost during migration"
    );
    let messages = db.get_messages(conv.id).await?;
    anyhow::ensure!(
        messages.len() == 1 && messages[0].content.contains(SENTINEL_MESSAGE),
        "legacy sentinel message lost during migration"
    );

    // Every versioned migration file must have been applied exactly once.
    let migration_files = std::fs::read_dir(format!("{}/migrations", env!("CARGO_MANIFEST_DIR")))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sql"))
        .count() as i64;
    let probe = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("sqlite:{}?mode=ro", db_path.display()))
        .await?;
    // NOTE: `db` is still open; read the version through a second handle.
    let (max_version,): (i64,) = sqlx::query_as("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&probe)
        .await?;
    probe.close().await;
    anyhow::ensure!(
        max_version == migration_files,
        "applied migration version {max_version} must equal migration file count {migration_files}"
    );
    db.close().await;
    Ok(())
}
