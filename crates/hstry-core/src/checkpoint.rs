//! Rolling compressed checkpoints of a hub SQLite database.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

use crate::config::CheckpointConfig;
use crate::db::Database;
use crate::error::{Error, Result};

const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub stem: String,
    pub weekly: bool,
    pub conversations: i64,
    pub messages: i64,
    pub sources: i64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub integrity: String,
    pub live_database: String,
}

#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub manifest: CheckpointManifest,
    pub archive_path: PathBuf,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CheckpointRecord {
    pub path: PathBuf,
    pub compressed_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub weekly: bool,
}

/// Choose which checkpoint archives to delete to stay under `max_total_bytes`.
///
/// Drops oldest non-weekly files first, then oldest weeklies beyond `keep_weekly`.
/// Always leaves at least one checkpoint.
pub fn plan_prune(
    mut records: Vec<CheckpointRecord>,
    max_total_bytes: u64,
    keep_weekly: usize,
) -> Vec<PathBuf> {
    records.sort_by_key(|r| r.created_at);
    let mut total: u64 = records.iter().map(|r| r.compressed_bytes).sum();
    let mut to_delete = Vec::new();
    let keep_weekly = keep_weekly.max(1);

    while total > max_total_bytes && records.len() > 1 {
        let idx = records.iter().position(|r| !r.weekly);
        let Some(idx) = idx else {
            break;
        };
        let removed = records.remove(idx);
        total = total.saturating_sub(removed.compressed_bytes);
        to_delete.push(removed.path);
    }

    while total > max_total_bytes && records.len() > 1 {
        let weekly_count = records.iter().filter(|r| r.weekly).count();
        if records[0].weekly && weekly_count <= keep_weekly {
            break;
        }
        let removed = records.remove(0);
        total = total.saturating_sub(removed.compressed_bytes);
        to_delete.push(removed.path);
    }

    to_delete
}

pub fn default_restore_path(database: &Path) -> PathBuf {
    let stem = database
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("hstry");
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.restore.db"))
}

pub fn ingest_lock_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(".ingest.lock");
    PathBuf::from(path)
}

pub fn acquire_ingest_lock(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    fs4::fs_std::FileExt::lock_exclusive(&file)
        .map_err(|e| Error::Other(format!("failed to lock hub ingest: {e}")))?;
    Ok(file)
}

fn archive_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.db.zst"))
}

fn manifest_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.json"))
}

pub fn list_checkpoints(dir: &Path) -> Result<Vec<CheckpointInfo>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<CheckpointManifest>(&text) else {
            continue;
        };
        let archive = archive_path(dir, &manifest.stem);
        if !archive.exists() {
            continue;
        }
        out.push(CheckpointInfo {
            manifest,
            archive_path: archive,
            manifest_path: path,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.manifest.created_at));
    Ok(out)
}

pub fn latest_checkpoint_at(dir: &Path) -> Result<Option<DateTime<Utc>>> {
    Ok(list_checkpoints(dir)?
        .into_iter()
        .map(|c| c.manifest.created_at)
        .max())
}

pub fn checkpoint_due(dir: &Path, interval_secs: u64) -> Result<bool> {
    match latest_checkpoint_at(dir)? {
        None => Ok(true),
        Some(latest) => {
            let elapsed = Utc::now()
                .signed_duration_since(latest)
                .num_seconds()
                .max(0) as u64;
            Ok(elapsed >= interval_secs.max(60))
        }
    }
}

pub async fn create_checkpoint(
    db: &Database,
    database_path: &Path,
    config: &CheckpointConfig,
    weekly: Option<bool>,
) -> Result<CheckpointInfo> {
    let dir = config.resolve_dir(database_path);
    fs::create_dir_all(&dir)?;

    let created_at = Utc::now();
    let stem = format!("hstry-{}", created_at.format("%Y%m%d-%H%M%S"));
    let weekly = weekly.unwrap_or(created_at.weekday() == Weekday::Sun);

    let _lock = acquire_ingest_lock(&ingest_lock_path(database_path))?;

    let raw_path = dir.join(format!("{stem}.db"));
    db.backup_to(&raw_path).await?;

    let copy = Database::open(&raw_path).await?;
    let integrity = copy.integrity_check().await?;
    let conversations = copy.count_conversations().await?;
    let messages = copy.count_messages().await?;
    let sources = copy.count_sources().await?;
    copy.close().await;

    if integrity != "ok" {
        let _ = fs::remove_file(&raw_path);
        return Err(Error::Other(format!(
            "checkpoint integrity_check failed: {integrity}"
        )));
    }

    let uncompressed_bytes = fs::metadata(&raw_path)?.len();
    let zst_path = archive_path(&dir, &stem);
    {
        let input = BufReader::new(File::open(&raw_path)?);
        let output = BufWriter::new(File::create(&zst_path)?);
        zstd::stream::copy_encode(input, output, 3)?;
    }
    let compressed_bytes = fs::metadata(&zst_path)?.len();
    fs::remove_file(&raw_path)?;

    let manifest = CheckpointManifest {
        version: MANIFEST_VERSION,
        created_at,
        stem: stem.clone(),
        weekly,
        conversations,
        messages,
        sources,
        uncompressed_bytes,
        compressed_bytes,
        integrity,
        live_database: database_path.to_string_lossy().to_string(),
    };
    let manifest_path = manifest_path(&dir, &stem);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    Ok(CheckpointInfo {
        manifest,
        archive_path: zst_path,
        manifest_path,
    })
}

pub fn prune_checkpoints(dir: &Path, config: &CheckpointConfig) -> Result<Vec<PathBuf>> {
    let listed = list_checkpoints(dir)?;
    let records: Vec<CheckpointRecord> = listed
        .iter()
        .map(|c| CheckpointRecord {
            path: c.archive_path.clone(),
            compressed_bytes: c.manifest.compressed_bytes,
            created_at: c.manifest.created_at,
            weekly: c.manifest.weekly,
        })
        .collect();
    let targets = plan_prune(records, config.max_total_bytes, config.keep_weekly);
    let mut deleted = Vec::new();
    for archive in targets {
        let stem = archive
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".db.zst"))
            .unwrap_or("");
        let json = manifest_path(dir, stem);
        let _ = fs::remove_file(&json);
        if archive.exists() {
            fs::remove_file(&archive)?;
        }
        deleted.push(archive);
    }
    Ok(deleted)
}

/// Remove `path` for restore, retrying transient Windows locks.
///
/// 32: ERROR_SHARING_VIOLATION, 5: ERROR_ACCESS_DENIED. Any other error, or
/// a lock that does not clear within the bound, propagates: production
/// restore must never silently continue with a stale file in place.
/// (Shared shape with PR #30 so it rebases cleanly; the gate adds
/// tests/workflow/guard infrastructure, not a second implementation.)
pub(crate) fn safe_remove_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut attempts = 0;
    loop {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                attempts += 1;
                let code = e.raw_os_error();
                // 32: ERROR_SHARING_VIOLATION, 5: ERROR_ACCESS_DENIED (Windows transient file locks)
                if (code == Some(32) || code == Some(5)) && attempts < 20 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return Err(Error::Io(e));
            }
        }
    }
}

fn safe_create_file(path: &Path) -> std::io::Result<File> {
    let mut attempts = 0;
    loop {
        match File::create(path) {
            Ok(f) => return Ok(f),
            Err(e) => {
                attempts += 1;
                let code = e.raw_os_error();
                if (code == Some(32) || code == Some(5)) && attempts < 10 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                return Err(e);
            }
        }
    }
}

pub fn restore_checkpoint(dir: &Path, stem: &str, dest: &Path) -> Result<PathBuf> {
    let archive = archive_path(dir, stem);
    if !archive.exists() {
        return Err(Error::NotFound(format!(
            "checkpoint archive not found: {}",
            archive.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // All database handles to `dest` must be closed before this call (see
    // `Database::close`, which checkpoints WAL first). Every removal below
    // is strict: a sidecar that cannot be cleared fails the restore instead
    // of letting a new database be written next to a stale WAL/SHM.
    safe_remove_file(dest)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = dest.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar = PathBuf::from(sidecar_os);
        safe_remove_file(&sidecar)?;
    }
    let input = BufReader::new(File::open(&archive)?);
    let output = BufWriter::new(safe_create_file(dest).map_err(Error::Io)?);
    zstd::stream::copy_decode(input, output)?;
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, bytes: u64, days_ago: i64, weekly: bool) -> CheckpointRecord {
        CheckpointRecord {
            path: PathBuf::from(name),
            compressed_bytes: bytes,
            created_at: Utc::now() - chrono::Duration::days(days_ago),
            weekly,
        }
    }

    #[test]
    fn prune_deletes_oldest_dailies_first() {
        let records = vec![
            rec("d1", 4, 4, false),
            rec("d2", 4, 3, false),
            rec("w1", 4, 2, true),
            rec("d3", 4, 1, false),
        ];
        let deleted = plan_prune(records, 10, 4);
        assert_eq!(deleted, vec![PathBuf::from("d1"), PathBuf::from("d2")]);
    }

    #[test]
    fn prune_keeps_weeklies_until_cap_forces_it() {
        let records = vec![
            rec("w1", 6, 3, true),
            rec("w2", 6, 2, true),
            rec("w3", 6, 1, true),
        ];
        let deleted = plan_prune(records, 10, 2);
        assert_eq!(deleted, vec![PathBuf::from("w1")]);
    }

    #[test]
    fn prune_always_keeps_one() {
        let records = vec![rec("only", 20, 0, false)];
        let deleted = plan_prune(records, 10, 4);
        assert!(deleted.is_empty());
    }

    #[tokio::test]
    async fn create_list_restore_roundtrip() {
        use crate::models::Source;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("live.db");
        let db = Database::open(&db_path).await.unwrap();
        db.upsert_source(&Source {
            id: "cursor-abc".to_string(),
            adapter: "cursor".to_string(),
            path: Some("/tmp".to_string()),
            last_sync_at: None,
            config: serde_json::json!({}),
        })
        .await
        .unwrap();

        let cfg = CheckpointConfig {
            enabled: true,
            dir: Some(dir.path().join("checkpoints")),
            max_total_bytes: 10 * 1024 * 1024,
            ..Default::default()
        };

        let created = create_checkpoint(&db, &db_path, &cfg, Some(false))
            .await
            .unwrap();
        assert_eq!(created.manifest.integrity, "ok");
        assert_eq!(created.manifest.sources, 1);
        db.close().await;

        let listed = list_checkpoints(&cfg.dir.clone().unwrap()).unwrap();
        assert_eq!(listed.len(), 1);

        let restore_path = dir.path().join("restored.db");
        restore_checkpoint(
            &cfg.dir.clone().unwrap(),
            &created.manifest.stem,
            &restore_path,
        )
        .unwrap();
        let restored = Database::open(&restore_path).await.unwrap();
        assert_eq!(restored.count_sources().await.unwrap(), 1);
        restored.close().await;
    }
}
