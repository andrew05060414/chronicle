use hstry_core::Database;
use hstry_core::models::Source;

#[tokio::test]
async fn read_only_open_refuses_missing_database_without_creating_it() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("missing.db");

    let result = Database::open_read_only(&path).await;

    assert!(result.is_err(), "missing read-only database must fail");
    assert!(!path.exists(), "read-only open must not create an empty archive");
    Ok(())
}

#[tokio::test]
async fn read_only_open_can_read_but_cannot_write() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("archive.db");

    let writable = Database::open(&path).await?;
    writable
        .upsert_source(&Source {
            id: "read-only-sentinel".to_string(),
            adapter: "test".to_string(),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        })
        .await?;
    writable.close().await;

    let readonly = Database::open_read_only(&path).await?;
    let sources = readonly.list_sources().await?;
    assert!(sources.iter().any(|s| s.id == "read-only-sentinel"));

    let write = readonly
        .upsert_source(&Source {
            id: "should-fail".to_string(),
            adapter: "test".to_string(),
            path: None,
            last_sync_at: None,
            config: serde_json::json!({}),
        })
        .await;
    assert!(write.is_err(), "read-only handle must reject writes");

    readonly.close().await;
    Ok(())
}
