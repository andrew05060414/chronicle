//! Isolation guards for tests touching the authoritative archive.
//!
//! Chronicle is the authoritative conversation/memory SQLite archive. Tests
//! must only ever open databases inside an isolated temporary directory.
//! This module never hardcodes a machine-specific live root: blocked roots
//! come from [`BLOCKED_ROOTS_ENV_VAR`]. The open-boundary check in
//! [`crate::Database::open`] only fires when [`GUARD_ENV_VAR`] is also set.
//! A leaked guard flag with an empty root list refuses nothing, so
//! production cannot be bricked by the flag alone.

use std::path::{Path, PathBuf};

fn normalize_str(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn normalize(path: &Path) -> String {
    normalize_str(&path.to_string_lossy())
}

/// Roots supplied by [`BLOCKED_ROOTS_ENV_VAR`], normalized, trailing slashes
/// stripped. Empty / unset means nothing is blocked.
fn blocked_roots() -> Vec<String> {
    std::env::var(BLOCKED_ROOTS_ENV_VAR)
        .ok()
        .map(|v| {
            v.split(';')
                .map(|s| normalize_str(s.trim()).trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_blocked(raw: &str) -> bool {
    let normalized = normalize_str(raw);
    let trimmed = normalized.trim_end_matches('/');
    blocked_roots().into_iter().any(|root| {
        trimmed == root || normalized == root || normalized.starts_with(&format!("{root}/"))
    })
}

/// Environment flag enabling central test-guard enforcement at the DB-open
/// boundary. Pre-PR (`scripts/pre-pr.sh` / `scripts/pre-pr.ps1`) and CI set
/// `HSTRY_ENFORCE_TEST_DB_GUARD=1`; production and ad-hoc debug runs leave
/// it unset and are completely unaffected.
pub const GUARD_ENV_VAR: &str = "HSTRY_ENFORCE_TEST_DB_GUARD";

/// `;`-separated roots that [`check_path_for_open`] / [`assert_test_path_safe`]
/// refuse. Not a compiled-in machine path: CI and pre-PR scripts supply the
/// operator's live archive root; tests that prove the guard inject a
/// synthetic path of their own.
pub const BLOCKED_ROOTS_ENV_VAR: &str = "HSTRY_TEST_BLOCKED_DB_ROOTS";

/// Whether central guard enforcement is enabled for this process.
pub fn guard_enabled() -> bool {
    std::env::var(GUARD_ENV_VAR)
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Central enforcement hook for the database-open boundary
/// ([`crate::Database::open`]).
///
/// When the guard flag is enabled **and** `path` is under a root listed in
/// [`BLOCKED_ROOTS_ENV_VAR`], this returns an error BEFORE any filesystem
/// or database mutation (callers invoke it before `create_dir_all` or
/// connecting). Flag-only (empty root list) is a no-op, so a leaked flag
/// cannot refuse a production archive. Per-test [`assert_test_path_safe`] /
/// [`isolated_temp_db`] remain as a second layer against the same env list.
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

    const SYNTHETIC_ROOT: &str = "c:/hstry-test-blocked-root";

    #[test]
    fn blocks_configured_roots() {
        temp_env::with_var(BLOCKED_ROOTS_ENV_VAR, Some(SYNTHETIC_ROOT), || {
            assert!(is_blocked("c:/hstry-test-blocked-root/staging.db"));
            assert!(is_blocked("c:\\hstry-test-blocked-root\\staging.db"));
            assert!(is_blocked("C:/HSTRY-TEST-BLOCKED-ROOT/STAGING.DB"));
            assert!(is_blocked("c:/hstry-test-blocked-root/nested/dir/hstry.db"));
            assert!(is_blocked("c:/hstry-test-blocked-root"));
        });
    }

    #[test]
    fn allows_isolated_temp_paths() {
        temp_env::with_var(BLOCKED_ROOTS_ENV_VAR, Some(SYNTHETIC_ROOT), || {
            assert!(!is_blocked("/tmp/hstry-test-123.db"));
            assert!(!is_blocked("c:/users/test/appdata/local/temp/hstry.db"));
        });
    }

    #[test]
    fn empty_root_list_blocks_nothing() {
        temp_env::with_var(BLOCKED_ROOTS_ENV_VAR, None::<&str>, || {
            assert!(!is_blocked("c:/hstry-test-blocked-root/staging.db"));
            assert!(!is_blocked("d:/data/hstry/staging.db"));
        });
    }

    #[test]
    #[should_panic(expected = "refusing to run a test")]
    fn guard_panics_on_blocked_path() {
        temp_env::with_var(BLOCKED_ROOTS_ENV_VAR, Some(SYNTHETIC_ROOT), || {
            assert_test_path_safe(Path::new("c:/hstry-test-blocked-root/staging.db"));
        });
    }

    #[test]
    fn open_boundary_rejects_blocked_paths_only_while_flag_is_set() {
        let blocked = Path::new("c:/hstry-test-blocked-root/staging.db");
        let safe = Path::new("/tmp/hstry-test-guard.db");
        temp_env::with_vars(
            [
                (GUARD_ENV_VAR, Some("1")),
                (BLOCKED_ROOTS_ENV_VAR, Some(SYNTHETIC_ROOT)),
            ],
            || {
                assert!(guard_enabled());
                assert!(check_path_for_open(blocked).is_err());
                assert!(check_path_for_open(safe).is_ok());
            },
        );
        temp_env::with_vars(
            [
                (GUARD_ENV_VAR, None::<&str>),
                (BLOCKED_ROOTS_ENV_VAR, Some(SYNTHETIC_ROOT)),
            ],
            || {
                assert!(!guard_enabled());
                // Flag unset: the boundary passes everything through, so
                // normal production/debug usage is unaffected even if a
                // blocked-root list leaked into the environment.
                assert!(check_path_for_open(blocked).is_ok());
            },
        );
    }

    #[test]
    fn guard_flag_alone_does_not_refuse_without_roots() {
        // Production-safety: a leaked HSTRY_ENFORCE_TEST_DB_GUARD=1 with
        // no root list must not refuse any path, including a live archive.
        temp_env::with_vars(
            [
                (GUARD_ENV_VAR, Some("1")),
                (BLOCKED_ROOTS_ENV_VAR, None::<&str>),
            ],
            || {
                assert!(guard_enabled());
                assert!(check_path_for_open(Path::new("d:/data/hstry/staging.db")).is_ok());
                assert!(check_path_for_open(Path::new("c:/hstry-test-blocked-root/db")).is_ok());
            },
        );
    }
}
