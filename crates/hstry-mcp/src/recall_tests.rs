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
    let server = McpServer::new(Config::default(), db, None);
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

#[tokio::test]
async fn refresh_local_never_uploads_or_modifies_remote_watermarks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("mcp_refresh.db");
    let db = hstry_core::Database::open(&db_path).await?;
    let source = hstry_core::models::Source {
        id: "local-source".into(),
        adapter: "pi".into(),
        path: None,
        last_sync_at: None,
        config: serde_json::json!({
            "local_cursor": 10,
            "last_confirmed_cursor": 5,
            "pending": true,
        }),
    };
    db.upsert_source(&source).await?;

    let mut config = Config {
        database: db_path.clone(),
        ..Default::default()
    };
    config.sync.mode = hstry_core::config::SyncMode::Satellite;
    config.sync.auto_sync = true;
    config.sync.hub_remote = Some("hub".into());
    config.remotes.push(hstry_core::config::RemoteConfig {
        name: "hub".into(),
        host: "hub.example.com".into(),
        port: None,
        identity_file: None,
        database_path: None,
        enabled: true,
    });

    let server = McpServer::new(config, db, None);

    // Call search with refresh_local: true
    let request = serde_json::from_value(serde_json::json!({
        "query": "nonexistent query",
        "scope": "local",
        "refresh_local": true,
    }))?;
    let _output = server.search(Parameters(request)).await;

    // Verify remote push state was never touched:
    let verify_db = hstry_core::Database::open(&db_path).await?;
    // 1. No push watermark created or advanced, and no push error recorded
    assert!(
        verify_db
            .get_search_state("push_watermark:hub")
            .await?
            .is_none()
    );
    assert!(
        verify_db
            .get_search_state("push_last_confirmed:hub")
            .await?
            .is_none()
    );
    assert!(
        verify_db
            .get_search_state("push_last_contact:hub")
            .await?
            .is_none()
    );
    assert!(
        verify_db
            .get_search_state("push_last_error:hub")
            .await?
            .is_none()
    );

    // 2. Source confirmed cursor and pending status remain intact
    let updated_source = verify_db
        .get_source("local-source")
        .await?
        .expect("source exists");
    let cfg = updated_source.config;
    assert_eq!(
        cfg.get("last_confirmed_cursor").and_then(|v| v.as_i64()),
        Some(5)
    );
    assert_eq!(cfg.get("pending").and_then(|v| v.as_bool()), Some(true));
    assert!(cfg.get("last_confirmed_remote_at").is_none());
    assert!(cfg.get("last_contact_at").is_none());
    assert!(cfg.get("last_error").is_none());

    Ok(())
}
