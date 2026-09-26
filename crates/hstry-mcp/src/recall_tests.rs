use super::*;

#[tokio::test]
async fn search_and_expand_share_a_real_message_anchor() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = hstry_core::Database::open(&dir.path().join("mcp.db")).await?;
    db.upsert_source(&hstry_core::models::Source {
        id: "test".into(),
        adapter: "pi".into(),
        path: None,
        last_sync_at: None,
        config: serde_json::json!({}),
    })
    .await?;
    let conv = serde_json::from_value(
        serde_json::json!({"externalId":"test","createdAt":1767225600000_i64,"messages":[{"role":"tool","content":"needle@example.test FixtureOnlyValue"}]}),
    )?;
    hstry_core::ingest::ingest_batch(&db, "test", vec![conv]).await?;
    let server = McpServer::new(Config::default(), db);
    let request = serde_json::from_value(serde_json::json!({"query":"needle@example.test"}))?;
    let output = server.search(Parameters(request)).await;
    assert!(output.chars().count() <= 3000);
    let result: serde_json::Value = serde_json::from_str(&output)?;
    let hit = &result["result"]["hits"][0];
    let request = serde_json::from_value(
        serde_json::json!({"conversation_id":hit["conversation_id"],"message_idx":hit["message_idx"]}),
    )?;
    let output = server.expand(Parameters(request)).await;
    assert!(output.chars().count() <= 3000);
    let expanded: serde_json::Value = serde_json::from_str(&output)?;
    assert_eq!(
        expanded["result"]["records"][0]["text"],
        "needle@example.test FixtureOnlyValue"
    );
    Ok(())
}

/// Database plus WAL; `-shm` is excluded because WAL readers update it too.
fn archive_bytes(path: &std::path::Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    for suffix in ["-wal"] {
        if let Ok(extra) = std::fs::read(format!("{}{suffix}", path.display())) {
            bytes.extend(extra);
        }
    }
    bytes
}

/// MCP stays a read-only archive client (#47): no search scope, and no request
/// field such as the dropped `refresh_local`, may write to the archive.
#[tokio::test]
async fn search_never_writes_the_archive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mcp_read_only.db");
    let db = hstry_core::Database::open(&db_path).await?;
    db.upsert_source(&hstry_core::models::Source {
        id: "test".into(),
        adapter: "pi".into(),
        path: None,
        last_sync_at: None,
        config: serde_json::json!({}),
    })
    .await?;
    let conv = serde_json::from_value(
        serde_json::json!({"externalId":"ro","createdAt":1767225600000_i64,"messages":[{"role":"user","content":"read only sentinel"}]}),
    )?;
    hstry_core::ingest::ingest_batch(&db, "test", vec![conv]).await?;
    db.close().await;
    let before = archive_bytes(&db_path);

    let db = hstry_core::Database::open_read_only(&db_path).await?;
    let server = McpServer::new(Config::default(), db);
    for request in [
        serde_json::json!({"query": "sentinel"}),
        serde_json::json!({"query": "sentinel", "scope": "local"}),
        serde_json::json!({"query": "sentinel", "scope": "all"}),
        serde_json::json!({"query": "sentinel", "scope": "local", "refresh_local": true}),
    ] {
        let output = server
            .search(Parameters(serde_json::from_value(request)?))
            .await;
        assert!(output.contains("sentinel"), "{output}");
    }
    drop(server);

    assert_eq!(
        archive_bytes(&db_path),
        before,
        "MCP search wrote to the archive"
    );
    Ok(())
}
