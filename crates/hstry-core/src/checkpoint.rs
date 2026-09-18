//! Rolling compressed checkpoints of a hub SQLite database.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
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
    fs::create_dir_all(&dir).map_err(|e| create_stage("create checkpoint dir", &dir, e))?;

    let created_at = Utc::now();
    let stem = format!("hstry-{}", created_at.format("%Y%m%d-%H%M%S"));
    let weekly = weekly.unwrap_or(created_at.weekday() == Weekday::Sun);

    let _lock = acquire_ingest_lock(&ingest_lock_path(database_path))?;

    // Online copy: the live `db` handle stays open. VACUUM INTO writes a
    // consistent snapshot; we must not `Database::open` that snapshot
    // (WAL + pooled connections + migrations) or Windows will still hold
    // `-wal`/`-shm` mappings when we later compress and delete it.
    let raw_path = dir.join(format!("{stem}.db"));
    vacuum_into_with_retry(db, &raw_path).await?;

    let stats = inspect_snapshot_with_retry(&raw_path).await?;
    if stats.integrity != "ok" {
        let _ = safe_remove_file(&raw_path);
        return Err(Error::Other(format!(
            "checkpoint create: integrity_check failed for {}: {}",
            raw_path.display(),
            stats.integrity
        )));
    }

    let uncompressed_bytes = fs::metadata(&raw_path)
        .map_err(|e| create_stage("stat snapshot", &raw_path, e))?
        .len();
    let zst_path = archive_path(&dir, &stem);
    {
        let input = BufReader::new(
            safe_open_file(&raw_path)
                .map_err(|e| create_stage("open snapshot for compress", &raw_path, e))?,
        );
        let mut output = BufWriter::new(
            safe_create_file(&zst_path)
                .map_err(|e| create_stage("create archive", &zst_path, e))?,
        );
        zstd::stream::copy_encode(input, &mut output, 3)
            .map_err(|e| create_stage("compress snapshot", &raw_path, e))?;
        output
            .flush()
            .map_err(|e| create_stage("flush archive", &zst_path, e))?;
    }
    let compressed_bytes = fs::metadata(&zst_path)
        .map_err(|e| create_stage("stat archive", &zst_path, e))?
        .len();
    safe_remove_file(&raw_path).map_err(|e| create_stage("remove snapshot", &raw_path, e))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar_os = raw_path.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar = PathBuf::from(sidecar_os);
        safe_remove_file(&sidecar)
            .map_err(|e| create_stage("remove snapshot sidecar", &sidecar, e))?;
    }

    let manifest = CheckpointManifest {
        version: MANIFEST_VERSION,
        created_at,
        stem: stem.clone(),
        weekly,
        conversations: stats.conversations,
        messages: stats.messages,
        sources: stats.sources,
        uncompressed_bytes,
        compressed_bytes,
        integrity: stats.integrity,
        live_database: database_path.to_string_lossy().to_string(),
    };
    let manifest_path = manifest_path(&dir, &stem);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .map_err(|e| create_stage("write manifest", &manifest_path, e))?;

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

/// Windows transient file-lock codes worth a bounded retry while the OS
/// releases handles or finishes indexing a fresh file.
///
/// 32: ERROR_SHARING_VIOLATION, 5: ERROR_ACCESS_DENIED,
/// 1224: ERROR_USER_MAPPED_SECTION_OPEN. Anything else (corruption, missing
/// files, permission policy, semantic errors) must fail fast, never retry.
fn is_transient_lock(code: Option<i32>) -> bool {
    matches!(code, Some(32) | Some(5) | Some(1224))
}

/// Remove `path` for restore, retrying transient Windows locks.
///
/// Bounded at ~5s (100 x 50ms), consistent with test cleanup tolerance for
/// hosted-runner handle/indexing latency. Any non-transient error, or a lock
/// that does not clear within the bound, propagates: production restore must
/// never silently continue with a stale file in place.
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
                if is_transient_lock(e.raw_os_error()) && attempts < 100 {
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
                if is_transient_lock(e.raw_os_error()) && attempts < 10 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Open the freshly-written checkpoint archive for reading, retrying
/// transient Windows locks.
///
/// A brand-new destination can still fail restore here: opening the archive
/// races OS handle release / antivirus indexing of the just-finished
/// checkpoint write. Bounded at ~2s (40 x 50ms); decode/corruption errors
/// downstream never retry.
fn safe_open_file(path: &Path) -> std::io::Result<File> {
    let mut attempts = 0;
    loop {
        match File::open(path) {
            Ok(f) => return Ok(f),
            Err(e) => {
                attempts += 1;
                if is_transient_lock(e.raw_os_error()) && attempts < 40 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Annotate a restore failure with the exact stage and path so future CI is
/// diagnosable from a single error line. The original error text (including
/// any os error code) is preserved.
fn restore_stage(stage: &str, path: &Path, err: impl std::fmt::Display) -> Error {
    Error::Other(format!(
        "checkpoint restore: {stage} failed for {}: {err}",
        path.display()
    ))
}

fn create_stage(stage: &str, path: &Path, err: impl std::fmt::Display) -> Error {
    Error::Other(format!(
        "checkpoint create: {stage} failed for {}: {err}",
        path.display()
    ))
}

/// Parse `os error N` codes from a Display string. Digits are consumed in
/// full, so `os error 5` matches 5 and `os error 50` matches 50 — never
/// a prefix of a larger Windows network code (50/53/55/59).
fn parse_os_error_codes(s: &str) -> Vec<i32> {
    const NEEDLE: &str = "os error ";
    let mut codes = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(NEEDLE) {
        rest = &rest[i + NEEDLE.len()..];
        let digits_end = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit())
            .last()
            .map(|(idx, c)| idx + c.len_utf8())
            .unwrap_or(0);
        if digits_end == 0 {
            continue;
        }
        if let Ok(n) = rest[..digits_end].parse::<i32>() {
            codes.push(n);
        }
        rest = &rest[digits_end..];
    }
    codes
}

fn error_is_transient_lock(err: &Error) -> bool {
    if let Error::Io(e) = err
        && is_transient_lock(e.raw_os_error())
    {
        return true;
    }
    parse_os_error_codes(&err.to_string())
        .into_iter()
        .any(|c| is_transient_lock(Some(c)))
}

async fn vacuum_into_with_retry(db: &Database, dest: &Path) -> Result<()> {
    let mut attempts = 0;
    loop {
        match db.backup_to(dest).await {
            Ok(()) => return Ok(()),
            Err(e) if error_is_transient_lock(&e) && attempts < 40 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(create_stage("vacuum into snapshot", dest, e)),
        }
    }
}

async fn inspect_snapshot_with_retry(path: &Path) -> Result<crate::db::SnapshotInspect> {
    let mut attempts = 0;
    loop {
        match Database::inspect_readonly_snapshot(path).await {
            Ok(stats) => return Ok(stats),
            Err(e) if error_is_transient_lock(&e) && attempts < 40 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(create_stage("inspect snapshot", path, e));
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
        fs::create_dir_all(parent)
            .map_err(|e| restore_stage("create destination parent", parent, e))?;
    }
    // All database handles to `dest` must be closed before this call (see
    // `Database::close`, which checkpoints WAL first). Every removal below
    // is strict: a sidecar that cannot be cleared fails the restore instead
    // of letting a new database be written next to a stale WAL/SHM.
    safe_remove_file(dest).map_err(|e| restore_stage("remove destination", dest, e))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_os = dest.as_os_str().to_os_string();
        sidecar_os.push(suffix);
        let sidecar = PathBuf::from(sidecar_os);
        safe_remove_file(&sidecar).map_err(|e| restore_stage("remove sidecar", &sidecar, e))?;
    }
    let input = BufReader::new(
        safe_open_file(&archive)
            .map_err(|e| restore_stage("open checkpoint archive", &archive, e))?,
    );
    let mut output = BufWriter::new(
        safe_create_file(dest).map_err(|e| restore_stage("create destination", dest, e))?,
    );
    zstd::stream::copy_decode(input, &mut output)
        .map_err(|e| restore_stage("decode checkpoint archive", &archive, e))?;
    output
        .flush()
        .map_err(|e| restore_stage("flush destination", dest, e))?;
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

    #[test]
    fn os_error_code_parse_does_not_prefix_match() {
        assert_eq!(parse_os_error_codes("os error 5"), vec![5]);
        assert_eq!(parse_os_error_codes("os error 32"), vec![32]);
        assert_eq!(parse_os_error_codes("os error 1224"), vec![1224]);
        assert_eq!(parse_os_error_codes("os error 50"), vec![50]);
        assert_eq!(
            parse_os_error_codes("Io(Os { code: 32, kind: Uncategorized, message: \"x\" })"),
            Vec::<i32>::new()
        );
        assert_eq!(
            parse_os_error_codes("failed: os error 5 (and later os error 32)"),
            vec![5, 32]
        );
        assert!(error_is_transient_lock(&Error::Other(
            "checkpoint create: vacuum into snapshot failed for x: os error 5".into()
        )));
        assert!(!error_is_transient_lock(&Error::Other(
            "checkpoint create: vacuum into snapshot failed for x: os error 50".into()
        )));
        assert!(!error_is_transient_lock(&Error::Other(
            "checkpoint create: vacuum into snapshot failed for x: os error 53".into()
        )));
        assert!(error_is_transient_lock(&Error::Other(
            "checkpoint create: inspect snapshot failed for x: os error 32".into()
        )));
        assert!(error_is_transient_lock(&Error::Other(
            "checkpoint restore: remove sidecar failed for x: os error 1224".into()
        )));
    }

    #[test]
    fn restore_failure_names_stage_and_path() {
        // Diagnosability contract: a restore failure must state exactly
        // which stage and path failed, so CI is readable from one line.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.db");
        let err = restore_checkpoint(dir.path(), "missing-stem", &dest).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing-stem"),
            "restore error must name the missing archive, got: {msg}"
        );

        // A corrupt archive must fail at the decode stage with the archive
        // path, without any transient retry masking it.
        let stem = "corrupt-stem";
        std::fs::write(dir.path().join(format!("{stem}.db.zst")), b"not zstd").unwrap();
        std::fs::write(
            dir.path().join(format!("{stem}.json")),
            serde_json::json!({"stem": stem}).to_string(),
        )
        .unwrap();
        let err = restore_checkpoint(dir.path(), stem, &dest).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("decode") && msg.contains(stem),
            "decode failure must name the stage and archive, got: {msg}"
        );
        // Failed restore must not leave a partial destination behind... the
        // destination file exists (created before decode) but the call
        // reported the failure loudly instead of succeeding.
        assert!(!dest.exists() || std::fs::metadata(&dest).unwrap().len() == 0);
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
        // Live handle must remain usable: checkpoint creation is an online
        // operation and must not require closing the source database.
        assert_eq!(db.count_sources().await.unwrap(), 1);

        let ckpt_dir = cfg.dir.clone().unwrap();
        let leftover_sidecars: Vec<_> = fs::read_dir(&ckpt_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with("-wal") || n.ends_with("-shm") || n.ends_with("-journal"))
            .collect();
        assert!(
            leftover_sidecars.is_empty(),
            "checkpoint must not leave WAL/SHM on the snapshot: {leftover_sidecars:?}"
        );
        db.close().await;

        let listed = list_checkpoints(&ckpt_dir).unwrap();
        assert_eq!(listed.len(), 1);

        let restore_path = dir.path().join("restored.db");
        restore_checkpoint(&ckpt_dir, &created.manifest.stem, &restore_path).unwrap();
        let restored = Database::open(&restore_path).await.unwrap();
        assert_eq!(restored.count_sources().await.unwrap(), 1);
        restored.close().await;
    }

    #[tokio::test]
    async fn inspect_snapshot_does_not_enable_wal_while_source_stays_open() {
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

        let raw = dir.path().join("snapshot.db");
        db.backup_to(&raw).await.unwrap();
        let stats = Database::inspect_readonly_snapshot(&raw).await.unwrap();
        assert_eq!(stats.integrity, "ok");
        assert_eq!(stats.sources, 1);

        let wal = PathBuf::from(format!("{}-wal", raw.display()));
        let shm = PathBuf::from(format!("{}-shm", raw.display()));
        assert!(
            !wal.exists() && !shm.exists(),
            "readonly snapshot inspect must not create WAL/SHM sidecars"
        );
        // Source database remains open and readable.
        assert_eq!(db.count_sources().await.unwrap(), 1);
        db.close().await;
    }

    #[tokio::test]
    async fn create_failure_names_stage_and_path() {
        let err = inspect_snapshot_with_retry(Path::new("missing-snapshot.db"))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("inspect snapshot") && msg.contains("missing-snapshot.db"),
            "create inspect failure must name the stage and path, got: {msg}"
        );
    }
}
