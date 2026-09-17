//! Isolation guards for tests touching the authoritative archive.
//!
//! Chronicle is the authoritative conversation/memory SQLite archive. Tests
//! must only ever open databases inside an isolated temporary directory and
//! must never resolve to a known live/archive location (for example
//! `D:/Data/hstry/staging.db`). CI isolation alone is not sufficient: every
//! test fails fast here, before executing any database work, when a blocked
//! path is detected.

use std::path::{Path, PathBuf};

/// Normalized (lowercase, forward-slash) live archive root. Any test path
/// equal to or under this root is refused.
const BLOCKED_ROOT: &str = "d:/data/hstry";

/// Known live archive files, matched by exact normalized path.
const BLOCKED_FILES: &[&str] = &[
    "d:/data/hstry/staging.db",
    "d:/data/hstry/hstry.db",
    "d:/data/hstry/archive.db",
    "d:/data/hstry/archive-restore.db",
];

fn normalize_str(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn normalize(path: &Path) -> String {
    normalize_str(&path.to_string_lossy())
}

fn is_blocked(raw: &str) -> bool {
    let normalized = normalize_str(raw);
    if BLOCKED_FILES.contains(&normalized.as_str()) {
        return true;
    }
    if normalized == BLOCKED_ROOT || normalized.starts_with("d:/data/hstry/") {
        return true;
    }
    false
}

/// Panic before any database work when `path` resolves to a known
/// live/archive location.
pub fn assert_test_path_safe(path: &Path) {
    let normalized = normalize(path);
    assert!(
        !is_blocked(&normalized),
        "refusing to run a test against a known live/archive database path: {}",
        path.display()
    );
}

/// Create an isolated temporary directory holding a database file named
/// `{prefix}.db`, asserting the resulting path is safe. The returned
/// [`tempfile::TempDir`] must be kept alive for the duration of the test.
pub fn isolated_temp_db(prefix: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create isolated temp dir for test database");
    let path = dir.path().join(format!("{prefix}.db"));
    assert_test_path_safe(&path);
    (dir, path)
}

/// Remove a database file plus its SQLite sidecars (`-wal`, `-shm`,
/// `-journal`), retrying the main file for Windows file-handle release.
///
/// SQLite may briefly keep the file locked after the last connection closes
/// on Windows (os error 32). Retrying a bounded number of times keeps
/// cleanup deterministic without masking real failures: after the retries
/// are exhausted the original error propagates.
pub async fn remove_db_files_with_retry(path: &Path) -> std::io::Result<()> {
    remove_sidecars(path);
    let mut last_err = None;
    for _ in 0..50 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("failed to remove test database")))
}

/// Best-effort removal of `-wal`/`-shm`/`-journal` sidecars next to `path`.
/// Missing files are ignored; other errors are ignored so stale sidecars
/// can never fail an otherwise healthy cleanup or restore.
pub fn remove_sidecars(path: &Path) {
    let base = path.as_os_str().to_os_string();
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = base.clone();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        let _ = std::fs::remove_file(sidecar);
    }
}

/// Close a [`crate::Database`] (flushing WAL via `Database::close`), then
/// remove its file and sidecars with Windows-tolerant retries.
pub async fn close_and_remove_db(db: crate::Database, path: &Path) -> std::io::Result<()> {
    db.close().await;
    remove_db_files_with_retry(path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_known_live_paths() {
        assert!(is_blocked("d:/data/hstry/staging.db"));
        assert!(is_blocked("d:\\data\\hstry\\staging.db"));
        assert!(is_blocked("D:/Data/HSTRY/STAGING.DB"));
        assert!(is_blocked("d:/data/hstry/nested/dir/hstry.db"));
        assert!(is_blocked("d:/data/hstry"));
    }

    #[test]
    fn allows_isolated_temp_paths() {
        assert!(!is_blocked("/tmp/hstry-test-123.db"));
        assert!(!is_blocked("c:/users/test/appdata/local/temp/hstry.db"));
    }

    #[test]
    #[should_panic(expected = "refusing to run a test")]
    fn guard_panics_on_live_path() {
        assert_test_path_safe(Path::new("D:/Data/hstry/staging.db"));
    }
}
