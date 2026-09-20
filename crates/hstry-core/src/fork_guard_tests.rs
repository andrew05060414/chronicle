//! Fork-only regression guards for invariants that upstream merges must not remove.
//!
//! Keep these tests in a file upstream does not own so an upstream rewrite of
//! remote/config code cannot silently delete both an invariant and its proof.

use std::path::Path;

use crate::Database;
use crate::config::SyncConfig;
use crate::remote::expand_remote_path_command;

#[test]
fn remote_path_expansion_never_evaluates_command_substitution() {
    let malicious = "~/archive/$(touch fork-guard-pwned).db";
    let command = expand_remote_path_command(malicious);

    assert!(!command.contains("eval"));
    assert!(command.contains("printf"));
    assert!(
        command.contains("'$(touch fork-guard-pwned).db'"),
        "command substitution must remain literal: {command}"
    );
}

#[test]
fn generated_device_identity_is_persisted_and_never_unknown() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("device-id");
    let config = SyncConfig::default();

    let first = config.device_namespace_at(&path)?;
    let second = config.device_namespace_at(&path)?;

    assert_eq!(first, second, "device identity must be stable across reads");
    assert_ne!(first, "unknown");
    let raw_uuid = first
        .strip_prefix("device-")
        .expect("generated namespace must use device-<uuid>");
    uuid::Uuid::parse_str(raw_uuid).expect("generated device identity must contain a UUID");
    Ok(())
}

#[tokio::test]
async fn fetched_archive_validation_requires_sqlite_integrity_and_schema() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let valid = tmp.path().join("valid.db");
    let db = Database::open(&valid).await?;
    db.close().await;

    Database::validate_hstry_database_file(&valid).await?;

    let garbage = tmp.path().join("garbage.db");
    std::fs::write(&garbage, b"not a sqlite database")?;
    assert!(
        Database::validate_hstry_database_file(Path::new(&garbage))
            .await
            .is_err(),
        "garbage must never pass fetched-hub validation"
    );

    Ok(())
}
