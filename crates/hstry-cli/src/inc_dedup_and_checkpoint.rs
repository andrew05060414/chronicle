async fn cmd_dedup(
    db: &Database,
    dry_run: bool,
    source_filter: Option<String>,
    cross_source: bool,
    json: bool,
) -> Result<()> {
    use std::collections::HashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    if cross_source && source_filter.is_some() {
        anyhow::bail!("--cross-source cannot be combined with --source");
    }

    let sources: HashMap<String, hstry_core::models::Source> = if cross_source {
        db.list_sources()
            .await?
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect()
    } else {
        HashMap::new()
    };

    let opts = hstry_core::db::ListConversationsOptions {
        source_id: source_filter,
        workspace: None,
        after: None,
        before: None,
        updated_after: None,
        limit: None,
    };

    let conversations = db.list_conversations(opts).await?;

    if !json {
        let mode = if cross_source {
            "cross-source (harness + external_id)"
        } else {
            "per-source content hash"
        };
        println!(
            "Scanning {} conversations for duplicates ({mode})...",
            conversations.len()
        );
    }

    let mut to_remove: Vec<uuid::Uuid> = Vec::new();
    let mut duplicates_found = 0usize;

    if cross_source {
        let mut groups: HashMap<String, Vec<Conversation>> = HashMap::new();
        for conv in conversations {
            groups
                .entry(conversation_identity_key(&conv))
                .or_default()
                .push(conv);
        }

        for mut convs in groups.into_values() {
            if convs.len() <= 1 {
                continue;
            }
            duplicates_found += convs.len() - 1;
            let mut best = convs.remove(0);
            for candidate in convs {
                if should_replace_conversation_with_sources(&candidate, &best, &sources) {
                    to_remove.push(best.id);
                    best = candidate;
                } else {
                    to_remove.push(candidate.id);
                }
            }
        }
    } else {
        // Group conversations by a hash of their full content
        let mut groups: HashMap<u64, Vec<Conversation>> = HashMap::new();

        for conv in conversations {
            let messages = db.get_messages(conv.id).await?;

            // Hash all message content for accurate dedup
            let mut hasher = DefaultHasher::new();
            conv.source_id.hash(&mut hasher);
            for msg in &messages {
                msg.role.to_string().hash(&mut hasher);
                msg.content.hash(&mut hasher);
            }
            let hash = hasher.finish();

            groups.entry(hash).or_default().push(conv);
        }

        for (_key, mut convs) in groups {
            if convs.len() > 1 {
                duplicates_found += convs.len() - 1;
                // Sort by updated_at descending, keep the most recent
                convs.sort_by(|a, b| {
                    let a_time = a.updated_at.unwrap_or(a.created_at);
                    let b_time = b.updated_at.unwrap_or(b.created_at);
                    b_time.cmp(&a_time)
                });
                // Keep first (most recent), mark rest for removal
                for conv in convs.into_iter().skip(1) {
                    to_remove.push(conv.id);
                }
            }
        }
    }

    if !json && !to_remove.is_empty() {
        println!("Found {} duplicate conversations", duplicates_found);
    }

    // Count messages that will be removed (use lightweight count query)
    let mut messages_removed = 0usize;
    for conv_id in &to_remove {
        let count = db.count_messages_for_conversation(*conv_id).await?;
        let count = usize::try_from(count.max(0)).unwrap_or(usize::MAX);
        messages_removed = messages_removed.saturating_add(count);
    }

    if !dry_run && !to_remove.is_empty() {
        // Batch delete all duplicates in a single transaction
        db.delete_conversations_batch(&to_remove).await?;
    }

    let result = DedupResult {
        duplicates_found,
        conversations_removed: to_remove.len(),
        messages_removed,
        dry_run,
    };

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(result),
            error: None,
        });
    }

    if to_remove.is_empty() {
        println!("No duplicates found.");
    } else if dry_run {
        println!(
            "Would remove {} conversations ({} messages)",
            result.conversations_removed, result.messages_removed
        );
        println!("Run without --dry-run to actually remove them.");
    } else {
        println!(
            "Removed {} duplicate conversations ({} messages)",
            result.conversations_removed, result.messages_removed
        );
    }

    Ok(())
}

// =============================================================================
// Reseed / Verify (trx-hjjw)
// =============================================================================

#[derive(Debug, Serialize)]
struct ReseedResult {
    source: String,
    purged_conversations: i64,
    purged_messages: i64,
    imported_conversations: usize,
    imported_messages: usize,
    deduped_messages: i64,
    indexed_messages: usize,
    bulk_mode: bool,
    dry_run: bool,
}

async fn cmd_reseed(
    db: &Database,
    runner: &AdapterRunner,
    source_id: &str,
    do_dedup: bool,
    do_index: bool,
    bulk_mode: bool,
    dry_run: bool,
    drop_source: bool,
    json: bool,
) -> Result<()> {
    let Some(source) = db.get_source(source_id).await? else {
        if json {
            return emit_json(JsonResponse::<()> {
                ok: false,
                result: None,
                error: Some(format!("Source '{source_id}' not found")),
            });
        }
        anyhow::bail!("Source '{source_id}' not found");
    };

    if !json {
        println!(
            "Reseeding source '{source_id}' ({adapter})",
            adapter = source.adapter
        );
        if dry_run {
            println!("  (dry run — nothing will be modified)");
        }
    }

    if dry_run {
        let (convs, msgs) = db.count_source_data(source_id).await?;
        if json {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(ReseedResult {
                    source: source_id.to_string(),
                    purged_conversations: convs,
                    purged_messages: msgs,
                    imported_conversations: 0,
                    imported_messages: 0,
                    deduped_messages: 0,
                    indexed_messages: 0,
                    bulk_mode,
                    dry_run: true,
                }),
                error: None,
            });
        }
        println!("  Would purge {convs} conversations / {msgs} messages");
        println!("  Would re-import from {:?}", source.path);
        if do_dedup {
            println!("  Would run conversation-local dedup");
        }
        if do_index {
            println!("  Would rebuild search index for source");
        }
        return Ok(());
    }

    // Pre-purge counts inform the result.
    let (pre_convs, pre_msgs) = db.count_source_data(source_id).await?;

    // IMPORTANT: purge BEFORE begin_bulk_reseed(). begin_bulk_reseed drops
    // idx_messages_conv_idx to speed up inserts, but that also makes the
    // cascading DELETEs below table-scan. Deleting first keeps the index
    // online for the purge and only drops it for the re-import.
    let purge = db.purge_source(source_id, drop_source).await?;
    if !json {
        println!(
            "  Purged {} conversations / {} messages / {} events",
            purge.conversations, purge.messages, purge.message_events
        );
    }

    // Re-create the source if it was dropped. Otherwise, we still need a
    // fresh copy with last_sync_at=None and cursor cleared — leaving them set
    // makes the adapter filter on "only rows since last time" and skip every
    // session in the source, which is the exact bug we are trying to recover
    // from (trx-hjjw rationale).
    let mut reimport_source = source.clone();
    reimport_source.last_sync_at = None;
    if let serde_json::Value::Object(mut map) = reimport_source.config {
        map.remove("cursor");
        map.remove("file_fingerprint");
        map.remove("watermark_at_ms");
        reimport_source.config = serde_json::Value::Object(map);
    }
    db.upsert_source(&reimport_source).await?;

    if bulk_mode {
        db.begin_bulk_reseed().await?;
    }

    // Re-import via the standard sync path. We deliberately reuse sync_source
    // (rather than cmd_import) because it understands cursor / batched
    // streaming for the Pi adapter. A progress spinner shows running totals
    // so the operator has immediate feedback on a multi-minute reseed.
    use indicatif::{ProgressBar, ProgressStyle};
    let pb: Option<ProgressBar> = if json {
        None
    } else {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message("Importing...");
        Some(bar)
    };

    let stats = if let Some(bar) = pb.as_ref() {
        let cb_box: Box<dyn Fn(usize, usize) + Send + Sync> =
            Box::new(move |convs: usize, msgs: usize| {
                bar.set_message(format!(
                    "Importing... {convs} conversations / {msgs} messages"
                ));
            });
        let cb_ref: sync::ProgressCallback<'_> = cb_box.as_ref();
        sync::sync_source_with_progress(db, runner, &reimport_source, Some(cb_ref)).await?
    } else {
        sync::sync_source_with_progress(db, runner, &reimport_source, None).await?
    };
    if let Some(bar) = pb {
        bar.finish_with_message(format!(
            "Imported {} conversations / {} messages",
            stats.conversations, stats.messages
        ));
    }

    let mut deduped = 0i64;
    if do_dedup {
        let pb = if json {
            None
        } else {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(
                indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                    .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message("Deduplicating turns...");
            Some(bar)
        };
        deduped = db
            .dedup_messages_for_source(Some(source_id), 5, false)
            .await?;
        if let Some(bar) = pb {
            bar.finish_with_message(format!("Dedup removed {deduped} duplicate turns"));
        }
    }

    if bulk_mode {
        db.end_bulk_reseed().await?;
    }

    let mut indexed = 0usize;
    if do_index {
        let pb = if json {
            None
        } else {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(
                indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                    .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message("Rebuilding SQLite FTS search index...");
            Some(bar)
        };
        indexed = db.rebuild_search_fts().await?;
        if let Some(bar) = pb {
            bar.finish_with_message(format!("Indexed {indexed} messages"));
        }
    }

    let _ = pre_convs;
    let _ = pre_msgs;
    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(ReseedResult {
                source: source_id.to_string(),
                purged_conversations: purge.conversations,
                purged_messages: purge.messages,
                imported_conversations: stats.conversations,
                imported_messages: stats.messages,
                deduped_messages: deduped,
                indexed_messages: indexed,
                bulk_mode,
                dry_run: false,
            }),
            error: None,
        });
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct VerifyDrift {
    source: String,
    db_conversations: i64,
    db_messages: i64,
    on_disk_conversations: usize,
    on_disk_messages: usize,
    drifted: bool,
    repaired: bool,
}

#[derive(Debug, Serialize)]
struct VerifyResult {
    sources: Vec<VerifyDrift>,
    total_drifted: usize,
    total_repaired: usize,
}

async fn cmd_verify(
    db: &Database,
    runner: &AdapterRunner,
    source_filter: Option<String>,
    repair: bool,
    json: bool,
) -> Result<()> {
    let mut targets: Vec<Source> = Vec::new();
    if let Some(id) = source_filter.clone() {
        if let Some(src) = db.get_source(&id).await? {
            targets.push(src);
        }
    } else {
        for src in db.list_sources().await? {
            targets.push(src);
        }
    }

    let mut report = VerifyResult {
        sources: Vec::new(),
        total_drifted: 0,
        total_repaired: 0,
    };

    for source in &targets {
        let Some(adapter_path) = runner.find_adapter(&source.adapter) else {
            continue;
        };
        let Some(path) = source.path.as_ref() else {
            continue;
        };

        let parsed = match runner
            .parse(
                &adapter_path,
                path,
                hstry_runtime::runner::ParseOptions {
                    since: None,
                    limit: None,
                    include_tools: true,
                    include_attachments: false,
                    cursor: None,
                    batch_size: None,
                },
            )
            .await
        {
            Ok(c) => c,
            Err(err) => {
                if !json {
                    eprintln!("  {} ({}): parse error: {err}", source.id, source.adapter);
                }
                continue;
            }
        };
        let on_disk_convs = parsed.len();
        let on_disk_msgs: usize = parsed.iter().map(|c| c.messages.len()).sum();

        let (db_convs, db_msgs) = db.count_source_data(&source.id).await?;
        let on_disk_convs_i64 = i64::try_from(on_disk_convs).unwrap_or(i64::MAX);
        let on_disk_msgs_i64 = i64::try_from(on_disk_msgs).unwrap_or(i64::MAX);
        let drifted = on_disk_convs_i64 != db_convs || on_disk_msgs_i64 != db_msgs;

        let mut repaired = false;
        if drifted && repair {
            // Reseed the drifted source. Use the same defaults as `cmd_reseed`.
            cmd_reseed(db, runner, &source.id, true, true, true, false, false, true)
                .await
                .ok();
            repaired = true;
            report.total_repaired += 1;
        }
        if drifted {
            report.total_drifted += 1;
        }

        if !json {
            let marker = if drifted { "DRIFT" } else { "OK" };
            println!(
                "  [{marker}] {id}: db={db_convs}c/{db_msgs}m disk={on_disk_convs}c/{on_disk_msgs}m{}",
                if repaired { " (repaired)" } else { "" },
                id = source.id
            );
        }

        report.sources.push(VerifyDrift {
            source: source.id.clone(),
            db_conversations: db_convs,
            db_messages: db_msgs,
            on_disk_conversations: on_disk_convs,
            on_disk_messages: on_disk_msgs,
            drifted,
            repaired,
        });
    }

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(report),
            error: None,
        });
    }
    Ok(())
}

// =============================================================================
// Config Commands
// =============================================================================

fn cmd_config(
    config: &Config,
    config_path: &Path,
    command: Option<ConfigCommand>,
    json: bool,
) -> Result<()> {
    match command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => {
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(config),
                    error: None,
                });
            }

            // Pretty print the config as TOML
            let toml_str = toml::to_string_pretty(config)
                .map_err(|e| anyhow::anyhow!("Failed to serialize config: {e}"))?;
            println!("{toml_str}");
        }

        ConfigCommand::Path => {
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "path": config_path,
                        "exists": config_path.exists(),
                    })),
                    error: None,
                });
            }

            let config_path_display = config_path.display();
            println!("{config_path_display}");
        }

        ConfigCommand::Edit => {
            // Ensure config file exists
            if !config_path.exists() {
                config.save_to_path(config_path)?;
            }

            // Get editor from EDITOR or VISUAL env var, fallback to common editors
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| {
                    // Try common editors
                    for editor in &["nano", "vim", "vi", "notepad"] {
                        if which::which(editor).is_ok() {
                            return editor.to_string();
                        }
                    }
                    "nano".to_string()
                });

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "editor": editor,
                        "path": config_path,
                    })),
                    error: None,
                });
            }

            let status = std::process::Command::new(&editor)
                .arg(config_path)
                .status()?;

            if !status.success() {
                anyhow::bail!("Editor exited with non-zero status");
            }
        }
    }

    Ok(())
}

// =============================================================================
// Remote Commands
// =============================================================================

#[derive(Debug, serde::Serialize)]
struct RemoteStatus {
    name: String,
    host: String,
    enabled: bool,
    database_path: Option<String>,
    cached: bool,
    cache_path: Option<String>,
    cache_size_bytes: Option<u64>,
    cache_modified: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct RemoteFetchSummary {
    remotes: Vec<hstry_core::remote::FetchResult>,
    total_bytes: u64,
}

#[derive(Debug, serde::Serialize)]
struct RemoteSyncSummary {
    results: Vec<hstry_core::remote::SyncResult>,
    total_conversations_added: usize,
    total_conversations_updated: usize,
    total_messages_added: usize,
}

async fn cmd_hub(config: &Config, command: HubCommand, json: bool) -> Result<()> {
    match command {
        HubCommand::Ingest {
            file,
            namespace,
            delete,
        } => {
            if !file.exists() {
                anyhow::bail!("delta file not found: {}", file.display());
            }
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, config);
            let lock_path = hstry_core::checkpoint::ingest_lock_path(&config.database);
            let namespace = hstry_core::config::sanitize_device_namespace(&namespace);
            let result = hstry_core::remote::ingest_into_hub(&db, &file, &namespace, &lock_path)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            db.close().await;
            if delete {
                std::fs::remove_file(&file)?;
            }
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(result),
                    error: None,
                });
            }
            println!(
                "Ingested {ns}: added {added} conversations, updated {updated}, {messages} messages",
                ns = namespace,
                added = result.conversations_added,
                updated = result.conversations_updated,
                messages = result.messages_added
            );
        }
    }
    Ok(())
}

async fn cmd_checkpoint(config: &Config, command: CheckpointCommand, json: bool) -> Result<()> {
    use hstry_core::checkpoint::{
        create_checkpoint, default_restore_path, list_checkpoints, prune_checkpoints,
        restore_checkpoint,
    };

    let dir = config.checkpoint.resolve_dir(&config.database);
    match command {
        CheckpointCommand::Create { weekly } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, config);
            let weekly = if weekly { Some(true) } else { None };
            let created = create_checkpoint(&db, &config.database, &config.checkpoint, weekly)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            db.close().await;
            prune_checkpoints(&dir, &config.checkpoint).map_err(|e| anyhow::anyhow!("{e}"))?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(created.manifest),
                    error: None,
                });
            }
            println!(
                "Checkpoint {} ({})",
                created.manifest.stem,
                format_bytes(created.manifest.compressed_bytes)
            );
        }
        CheckpointCommand::List => {
            let listed = list_checkpoints(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            if json {
                let manifests: Vec<_> = listed.iter().map(|c| &c.manifest).collect();
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(manifests),
                    error: None,
                });
            }
            if listed.is_empty() {
                println!("No checkpoints in {}", dir.display());
                return Ok(());
            }
            println!(
                "{:<22} {:>8} {:>8} {:>8} {:>10} TAGS",
                "STEM", "CONVS", "MSGS", "SRCS", "SIZE"
            );
            for item in listed {
                let tag = if item.manifest.weekly {
                    "weekly"
                } else {
                    "daily"
                };
                println!(
                    "{:<22} {:>8} {:>8} {:>8} {:>10} {tag}",
                    item.manifest.stem,
                    item.manifest.conversations,
                    item.manifest.messages,
                    item.manifest.sources,
                    format_bytes(item.manifest.compressed_bytes)
                );
            }
        }
        CheckpointCommand::Restore { stem, output, live } => {
            let dest = if live {
                config.database.clone()
            } else {
                output.unwrap_or_else(|| default_restore_path(&config.database))
            };
            if dest
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("staging.db"))
            {
                anyhow::bail!("refusing to restore a checkpoint onto staging.db");
            }
            if live {
                let db = Database::open(&config.database).await?;
                apply_storage_config(&db, config);
                create_checkpoint(&db, &config.database, &config.checkpoint, Some(false))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                db.close().await;
            }
            let restored =
                restore_checkpoint(&dir, &stem, &dest).map_err(|e| anyhow::anyhow!("{e}"))?;
            let check_db = Database::open(&restored).await?;
            let integrity = check_db.integrity_check().await?;
            check_db.close().await;
            if integrity != "ok" {
                anyhow::bail!("restored database failed integrity check: {integrity}");
            }
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "stem": stem,
                        "path": restored,
                        "live": live
                    })),
                    error: None,
                });
            }
            println!("Restored {stem} -> {}", restored.display());
            if live {
                println!("Live hub replaced. Restart `hstry service` if it was running.");
            }
        }
        CheckpointCommand::Prune => {
            let deleted =
                prune_checkpoints(&dir, &config.checkpoint).map_err(|e| anyhow::anyhow!("{e}"))?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(deleted),
                    error: None,
                });
            }
            if deleted.is_empty() {
                println!("Nothing to prune.");
            } else {
                println!("Pruned {} checkpoint(s).", deleted.len());
            }
        }
    }
    Ok(())
}
