use anyhow::Result;
use chrono::Utc;
use hstry_core::Database;
use hstry_core::models::{Conversation, Source};
use hstry_core::recall::Provenance;
use uuid::Uuid;

fn temp_db_path() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let filename = format!("hstry-remote-ack-{}.db", Uuid::new_v4());
    path.push(filename);
    path
}

fn make_source(id: &str) -> Source {
    Source {
        id: id.to_string(),
        adapter: "pi".to_string(),
        path: Some(format!("/tmp/{id}")),
        last_sync_at: None,
        config: serde_json::json!({}),
    }
}

fn make_conversation(
    source_id: &str,
    ext: &str,
    created_at: chrono::DateTime<Utc>,
) -> Conversation {
    Conversation {
        id: Uuid::new_v4(),
        source_id: source_id.to_string(),
        external_id: Some(ext.to_string()),
        readable_id: None,
        platform_id: None,
        title: Some(format!("conv {ext}")),
        created_at,
        updated_at: Some(created_at),
        model: None,
        provider: None,
        workspace: None,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
        metadata: serde_json::json!({}),
        harness: None,
        version: 0,
        message_count: 0,
        parent_conversation_id: None,
        parent_message_idx: None,
        fork_type: None,
    }
}

#[tokio::test]
async fn test_duplicate_push_is_idempotent() -> Result<()> {
    let path = temp_db_path();
    let db = Database::open(&path).await?;
    let source = make_source("src-dedup");
    db.upsert_source(&source).await?;

    let conv = make_conversation("src-dedup", "c1", Utc::now());
    db.upsert_conversation(&conv).await?;
    let cursor = db
        .get_max_local_change_for_source("src-dedup")
        .await?
        .unwrap();

    let t1 = Utc::now();
    db.record_push_success("hub", cursor, t1).await?;

    let s1 = db.get_source("src-dedup").await?.unwrap();
    assert_eq!(
        s1.config
            .get("last_confirmed_cursor")
            .and_then(|v| v.as_i64()),
        Some(cursor)
    );
    assert_eq!(
        s1.config.get("pending").and_then(|v| v.as_bool()),
        Some(false)
    );

    // Duplicate push succeeds with identical cursor
    let t2 = t1 + chrono::Duration::seconds(10);
    db.record_push_success("hub", cursor, t2).await?;

    let s2 = db.get_source("src-dedup").await?.unwrap();
    assert_eq!(
        s2.config
            .get("last_confirmed_cursor")
            .and_then(|v| v.as_i64()),
        Some(cursor)
    );
    assert_eq!(
        s2.config.get("pending").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        s2.config
            .get("last_confirmed_remote_at")
            .and_then(|v| v.as_str()),
        Some(t2.to_rfc3339().as_str())
    );

    Ok(())
}

#[tokio::test]
async fn test_restart_reopen_db_keeps_pending_records() -> Result<()> {
    let path = temp_db_path();
    {
        let db = Database::open(&path).await?;
        let source = make_source("src-restart");
        db.upsert_source(&source).await?;

        let conv1 = make_conversation("src-restart", "c1", Utc::now());
        db.upsert_conversation(&conv1).await?;
        let cursor1 = db
            .get_max_local_change_for_source("src-restart")
            .await?
            .unwrap();
        db.record_push_success("hub", cursor1, Utc::now()).await?;

        // New record added, makes it pending
        let conv2 = make_conversation("src-restart", "c2", Utc::now());
        db.upsert_conversation(&conv2).await?;
        db.record_push_contact("hub", Utc::now()).await?;

        let s = db.get_source("src-restart").await?.unwrap();
        assert_eq!(
            s.config.get("pending").and_then(|v| v.as_bool()),
            Some(true)
        );
        db.close().await;
    }

    // Reopen DB from disk and verify pending state persisted
    {
        let db = Database::open(&path).await?;
        let s = db
            .get_source("src-restart")
            .await?
            .expect("source survived restart");
        assert_eq!(
            s.config.get("pending").and_then(|v| v.as_bool()),
            Some(true),
            "pending state must persist across database restart"
        );
        let prov = Provenance::from_source(&s, &serde_json::json!({}));
        assert_eq!(prov.pending, Some(true));
    }

    Ok(())
}

#[tokio::test]
async fn test_failed_push_leaves_ack_unchanged_and_persists_error() -> Result<()> {
    let path = temp_db_path();
    let db = Database::open(&path).await?;
    let source = make_source("src-fail");
    db.upsert_source(&source).await?;

    let conv1 = make_conversation("src-fail", "c1", Utc::now());
    db.upsert_conversation(&conv1).await?;
    let cursor1 = db
        .get_max_local_change_for_source("src-fail")
        .await?
        .unwrap();
    let initial_ack_time = Utc::now() - chrono::Duration::minutes(10);
    db.record_push_success("hub", cursor1, initial_ack_time)
        .await?;

    // Add another conversation, creating a pending change
    let conv2 = make_conversation("src-fail", "c2", Utc::now());
    db.upsert_conversation(&conv2).await?;

    // Attempt push which fails
    let fail_time = Utc::now();
    let error_text = "ssh connection reset by peer";
    db.record_push_error("hub", fail_time, error_text).await?;

    let s = db.get_source("src-fail").await?.unwrap();
    let cfg = &s.config;

    // Ack state MUST NOT advance
    assert_eq!(
        cfg.get("last_confirmed_cursor").and_then(|v| v.as_i64()),
        Some(cursor1),
        "failed upload must never advance confirmed cursor"
    );
    assert_eq!(
        cfg.get("last_confirmed_remote_at").and_then(|v| v.as_str()),
        Some(initial_ack_time.to_rfc3339().as_str()),
        "failed upload must never advance confirmed timestamp"
    );

    // Error and contact are recorded
    assert_eq!(
        cfg.get("last_error").and_then(|v| v.as_str()),
        Some(error_text)
    );
    assert_eq!(
        cfg.get("last_contact_at").and_then(|v| v.as_str()),
        Some(fail_time.to_rfc3339().as_str())
    );
    assert_eq!(cfg.get("pending").and_then(|v| v.as_bool()), Some(true));

    // Search state also reflects failure
    assert_eq!(
        db.get_search_state("push_last_error:hub").await?,
        Some(error_text.to_string())
    );
    assert_eq!(
        db.get_search_state("push_watermark:hub").await?,
        Some(cursor1.to_string())
    );

    // Provenance report reflects error and pending
    let prov = Provenance::from_source(&s, &serde_json::json!({}));
    assert_eq!(prov.last_error, Some(error_text.to_string()));
    assert_eq!(prov.pending, Some(true));

    Ok(())
}

#[tokio::test]
async fn test_unknown_stays_unknown_never_displayed_as_synced() -> Result<()> {
    let path = temp_db_path();
    let db = Database::open(&path).await?;
    let source = make_source("src-fresh");
    db.upsert_source(&source).await?;

    let s = db.get_source("src-fresh").await?.unwrap();
    let prov = Provenance::from_source(&s, &serde_json::json!({}));

    // Fresh source without sync/cursor history must have pending = None (unknown),
    // NEVER Some(false) (which would falsely display as synced).
    assert_eq!(
        prov.pending, None,
        "un-synced source must remain unknown, not false (synced)"
    );

    Ok(())
}
