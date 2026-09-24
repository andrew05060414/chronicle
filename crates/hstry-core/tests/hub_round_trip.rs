//! Fork invariant (#17): a satellite must not re-import its own archive when
//! it pulls the hub.
//!
//! This file is fork-only and deliberately separate from the upstream test
//! modules in `src/remote.rs`, so an upstream merge cannot delete the guard
//! along with the code it guards (#24).

use hstry_core::{
    Database,
    models::{Conversation, Message, Source},
    remote::{merge_databases, merge_databases_excluding},
};
use serde_json::json;

/// Write one source plus a one-message conversation under it.
async fn seed(db: &Database, source_id: &str, external_id: &str) -> anyhow::Result<()> {
    seed_with_id(db, source_id, external_id, uuid::Uuid::new_v4()).await
}

async fn seed_with_id(
    db: &Database,
    source_id: &str,
    external_id: &str,
    id: uuid::Uuid,
) -> anyhow::Result<()> {
    db.upsert_source(&Source {
        id: source_id.into(),
        adapter: "cursor".into(),
        path: None,
        last_sync_at: None,
        config: json!({}),
    })
    .await?;
    let conv: Conversation = serde_json::from_value(json!({
        "id": id,
        "source_id": source_id,
        "external_id": external_id,
        "title": format!("conversation in {source_id}"),
        "created_at": "2026-01-01T00:00:00Z",
        "metadata": {}
    }))?;
    db.upsert_conversation(&conv).await?;
    let msg: Message = serde_json::from_value(json!({
        "id": uuid::Uuid::new_v4(),
        "conversation_id": conv.id,
        "idx": 0,
        "role": "user",
        "content": format!("hello from {source_id}"),
        "parts_json": [],
        "metadata": {},
        "created_at": "2026-01-01T00:00:00Z"
    }))?;
    db.insert_message(&msg).await?;
    Ok(())
}

async fn source_ids(db: &Database) -> anyhow::Result<Vec<String>> {
    let mut ids: Vec<String> = db.list_sources().await?.into_iter().map(|s| s.id).collect();
    ids.sort();
    Ok(ids)
}

/// A hub as a satellite named `arknights` sees it: its own rows pushed there
/// earlier, one other device, and a source whose name merely starts with the
/// same characters.
async fn hub_fixture() -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("hub.db");
    let hub = Database::open(&path).await?;
    seed(&hub, "arknights:cursor-abc", "own-1").await?;
    seed(&hub, "macbook:cursor-xyz", "other-1").await?;
    seed(&hub, "arknights-old:cursor-legacy", "lookalike-1").await?;
    hub.close().await;
    Ok((dir, path))
}

#[tokio::test]
async fn pull_leaves_this_devices_own_rows_on_the_hub() -> anyhow::Result<()> {
    let (_hub_dir, hub_path) = hub_fixture().await?;

    let local_dir = tempfile::tempdir()?;
    let local = Database::open(&local_dir.path().join("local.db")).await?;
    seed(&local, "cursor-abc", "own-1").await?;

    // What `sync_from_remote` does on a satellite whose device namespace is
    // `arknights`, pulling the remote it calls `nas-lan`.
    let result = merge_databases_excluding(&local, &hub_path, "nas-lan", Some("arknights")).await?;

    // Only the other device's rows crossed over. Without the skip this would
    // be 3 sources and 3 conversations, one of them a second copy of
    // `cursor-abc` under `nas-lan:arknights:`.
    assert_eq!(result.sources_added, 2);
    assert_eq!(result.conversations_added, 2);

    assert_eq!(
        source_ids(&local).await?,
        vec![
            "cursor-abc".to_string(),
            "nas-lan:arknights-old:cursor-legacy".to_string(),
            "nas-lan:macbook:cursor-xyz".to_string(),
        ],
        "own rows must not return under a double namespace; a source that \
         merely starts with the same characters must still merge"
    );

    local.close().await;
    Ok(())
}

#[tokio::test]
async fn pull_without_a_namespace_still_re_imports_everything() -> anyhow::Result<()> {
    // Pins the defect itself, so a future refactor that drops the argument
    // fails here instead of silently restoring the round trip.
    let (_hub_dir, hub_path) = hub_fixture().await?;

    let local_dir = tempfile::tempdir()?;
    let local = Database::open(&local_dir.path().join("local.db")).await?;
    seed(&local, "cursor-abc", "own-1").await?;

    let result = merge_databases(&local, &hub_path, "nas-lan").await?;

    assert_eq!(result.sources_added, 3);
    assert!(
        source_ids(&local)
            .await?
            .contains(&"nas-lan:arknights:cursor-abc".to_string()),
        "unguarded merge is the behaviour #17 describes: the device's own \
         archive comes back under a second namespace"
    );

    local.close().await;
    Ok(())
}

#[tokio::test]
async fn push_still_namespaces_local_rows_into_the_hub() -> anyhow::Result<()> {
    // The push direction must keep prefixing: that is how the hub tells
    // devices apart. Guards against "fixing" #17 inside the shared merge.
    let local_dir = tempfile::tempdir()?;
    let local_path = local_dir.path().join("local.db");
    let local = Database::open(&local_path).await?;
    seed(&local, "cursor-abc", "own-1").await?;
    local.close().await;

    let hub_dir = tempfile::tempdir()?;
    let hub = Database::open(&hub_dir.path().join("hub.db")).await?;
    let result = merge_databases(&hub, &local_path, "arknights").await?;

    assert_eq!(result.sources_added, 1);
    assert_eq!(source_ids(&hub).await?, vec!["arknights:cursor-abc"]);

    hub.close().await;
    Ok(())
}

#[tokio::test]
async fn merge_survives_a_conversation_id_already_used_by_another_source() -> anyhow::Result<()> {
    // A device relays a hub row back to the hub: same conversation id, now under
    // a longer namespace. Reusing the id verbatim would violate the primary key
    // and fail every later push.
    let shared = uuid::Uuid::new_v4();

    let hub_dir = tempfile::tempdir()?;
    let hub = Database::open(&hub_dir.path().join("hub.db")).await?;
    seed_with_id(&hub, "macbook:cursor-xyz", "other-1", shared).await?;

    let delta_dir = tempfile::tempdir()?;
    let delta_path = delta_dir.path().join("delta.db");
    let delta = Database::open(&delta_path).await?;
    seed_with_id(&delta, "nas-lan:macbook:cursor-xyz", "other-1", shared).await?;
    delta.close().await;

    let result = merge_databases(&hub, &delta_path, "desktop").await?;
    assert_eq!(result.conversations_added, 1);

    let original = hub.get_conversation(shared).await?.expect("original row");
    assert_eq!(
        original.source_id, "macbook:cursor-xyz",
        "existing row untouched"
    );
    assert!(
        source_ids(&hub)
            .await?
            .contains(&"desktop:nas-lan:macbook:cursor-xyz".to_string())
    );

    hub.close().await;
    Ok(())
}
