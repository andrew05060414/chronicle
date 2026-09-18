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

/// Environment flag enabling central test-guard enforcement at the DB-open
/// boundary. Pre-PR (`scripts/pre-pr.sh` / `scripts/pre-pr.ps1`) and CI set
/// `HSTRY_ENFORCE_TEST_DB_GUARD=1`; production and ad-hoc debug runs leave
/// it unset and are completely unaffected.
pub const GUARD_ENV_VAR: &str = "HSTRY_ENFORCE_TEST_DB_GUARD";

/// Whether central guard enforcement is enabled for this process.
pub fn guard_enabled() -> bool {
    std::env::var(GUARD_ENV_VAR)
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Central enforcement hook for the database-open boundary
/// ([`crate::Database::open`]).
///
/// When the guard flag is enabled and `path` is a known live/archive
/// location, this returns an error BEFORE any filesystem or database
/// mutation (callers invoke it before `create_dir_all` or connecting).
/// Any test — present or future — that opens a database through the common
/// boundary is therefore refused up front; per-test
/// [`assert_test_path_safe`] / [`isolated_temp_db`] remain as a second
/// layer that also applies when the flag is unset.
pub fn check_path_for_open(path: &Path) -> Result<(), String> {
    if guard_enabled() && is_blocked(&normalize(path)) {
        return Err(format!(
            "refusing to open known live/archive database path in guarded test mode: {}",
            path.display()
        ));
    }
    Ok(())
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
///
/// TEST CLEANUP ONLY. Missing files are ignored; other errors are ignored so
/// stale sidecars can never fail an otherwise healthy test teardown.
/// Production restore must not use this helper: it uses strict per-sidecar
/// removal (`checkpoint::safe_remove_file`) that propagates failures.
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

    #[test]
    fn open_boundary_rejects_blocked_paths_only_while_flag_is_set() {
        let blocked = Path::new("D:/Data/hstry/staging.db");
        let safe = Path::new("/tmp/hstry-test-guard.db");
        temp_env::with_var(GUARD_ENV_VAR, Some("1"), || {
            assert!(guard_enabled());
            assert!(check_path_for_open(blocked).is_err());
            assert!(check_path_for_open(safe).is_ok());
        });
        temp_env::with_var(GUARD_ENV_VAR, None::<&str>, || {
            assert!(!guard_enabled());
            // Flag unset: the boundary passes everything through, so normal
            // production/debug usage is unaffected.
            assert!(check_path_for_open(blocked).is_ok());
        });
    }
}
