async fn cmd_list(
    db: &Database,
    source: Option<String>,
    workspace: Option<String>,
    limit: i64,
    after: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    include_all: bool,
    json: bool,
) -> Result<()> {
    let dedup_across_sources = source.is_none();
    let workspace = workspace.map(|value| format!("%{value}%"));
    let opts = hstry_core::db::ListConversationsOptions {
        source_id: source,
        workspace,
        after,
        before,
        updated_after: None,
        limit: Some(if dedup_across_sources {
            expanded_list_limit(limit)
        } else {
            limit
        }),
    };

    let mut fetched = db.list_conversation_previews(opts).await?;
    if !include_all {
        fetched.retain(|preview| !is_continuation_fragment(preview.first_user_message.as_deref()));
    }

    let previews = if dedup_across_sources {
        let mut deduped = dedup_conversation_previews(fetched);
        if limit > 0 {
            deduped.truncate(limit as usize);
        }
        deduped
    } else {
        fetched
    };

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(previews),
            error: None,
        });
    }

    let display = previews
        .into_iter()
        .map(|preview| {
            let title = display_title_for_list(
                preview.conversation.title.as_deref(),
                preview.first_user_message.as_deref(),
            );
            pretty::ConversationDisplay {
                id: preview.conversation.id,
                source_id: preview.conversation.source_id,
                workspace: preview.conversation.workspace,
                created_at: preview.conversation.created_at,
                title,
                readable_id: preview.conversation.readable_id,
            }
        })
        .collect::<Vec<_>>();

    pretty::print_conversations(&display);

    Ok(())
}

async fn cmd_list_peek(
    db: &Database,
    source: Option<String>,
    workspace: Option<String>,
    limit: i64,
    after: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    last_assistant_chars: Option<usize>,
) -> Result<()> {
    let dedup_across_sources = source.is_none();
    let workspace_filter = workspace.map(|value| format!("%{value}%"));
    let opts = hstry_core::db::ListConversationsOptions {
        source_id: source,
        workspace: workspace_filter,
        after,
        before,
        updated_after: None,
        limit: Some(if dedup_across_sources {
            expanded_list_limit(limit)
        } else {
            limit
        }),
    };

    let previews = if dedup_across_sources {
        let mut deduped = dedup_conversation_previews(db.list_conversation_previews(opts).await?);
        if limit > 0 {
            deduped.truncate(limit as usize);
        }
        deduped
    } else {
        db.list_conversation_previews(opts).await?
    };

    let mut cfg = hstry_core::peek::PeekConfig::default();
    if let Some(n) = last_assistant_chars {
        cfg.last_assistant_chars = n;
    }

    let mut bundles = Vec::with_capacity(previews.len());
    for preview in previews {
        let messages = db.get_messages(preview.conversation.id).await?;
        let bundle = hstry_core::peek::build_peek(&preview.conversation, &messages, &cfg);
        bundles.push(bundle);
    }

    emit_json(JsonResponse {
        ok: true,
        result: Some(bundles),
        error: None,
    })
}

async fn cmd_peek(db: &Database, id: &str, chars: Option<usize>, json: bool) -> Result<()> {
    let conv = resolve_conversation_by_id(db, id).await?;
    let messages = db.get_messages(conv.id).await?;

    let mut cfg = hstry_core::peek::PeekConfig::default();
    if let Some(n) = chars {
        cfg.last_assistant_chars = n;
    }
    let bundle = hstry_core::peek::build_peek(&conv, &messages, &cfg);

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(bundle),
            error: None,
        });
    }

    print_peek_text(&bundle);
    Ok(())
}

fn print_peek_text(b: &hstry_core::peek::PeekBundle) {
    println!("{} [{}]", b.id, b.source);
    if let Some(m) = &b.model {
        println!("Model: {m}");
    }
    println!(
        "Created: {} ({} min, {} msgs)",
        b.created_at.format("%Y-%m-%d %H:%M"),
        b.duration_min,
        b.message_count
    );
    println!(
        "Counts: user={}, assistant={}, tool_calls={}",
        b.counts.user, b.counts.assistant, b.counts.tool_calls
    );
    if !b.tools.is_empty() {
        let tools: Vec<String> = b
            .tools
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        println!("Tools: {}", tools.join(", "));
    }
    if !b.files_touched.is_empty() {
        println!("Files touched:");
        for f in &b.files_touched {
            println!("  {f}");
        }
    }
    if !b.bash_sample.is_empty() {
        println!("Bash sample:");
        for c in &b.bash_sample {
            println!("  $ {c}");
        }
    }
    if let Some(s) = &b.first_user {
        println!("\nFirst user: {s}");
    }
    if let Some(s) = &b.last_user
        && Some(s) != b.first_user.as_ref()
    {
        println!("Last user:  {s}");
    }
    if let Some(s) = &b.last_assistant {
        println!("Last asst:  {s}");
    }
}

async fn cmd_show(db: &Database, id: &str, json: bool, message_idx: Option<i32>) -> Result<()> {
    let conv = resolve_conversation_by_id(db, id).await?;

    let mut messages = db.get_messages(conv.id).await?;
    if let Some(idx) = message_idx {
        messages.retain(|m| m.idx == idx);
        if messages.is_empty() {
            anyhow::bail!("Message index {idx} not found");
        }
    }
    if json {
        let details = hstry_core::models::ConversationWithMessages {
            conversation: conv,
            messages: messages
                .into_iter()
                .map(|message| hstry_core::models::MessageWithExtras {
                    message,
                    tool_calls: Vec::new(),
                    attachments: Vec::new(),
                })
                .collect(),
        };
        return emit_json(JsonResponse {
            ok: true,
            result: Some(details),
            error: None,
        });
    }

    let title = conv.title.as_deref().unwrap_or("(untitled)");
    println!("Title: {title}");
    println!("Created: {created}", created = conv.created_at);
    println!("Source: {source}", source = conv.source_id);
    if let Some(ws) = &conv.workspace {
        println!("Workspace: {ws}");
    }
    println!();

    for msg in messages {
        println!("--- {role} ---", role = msg.role);
        println!("{content}", content = msg.content);
        println!();
    }

    Ok(())
}

async fn cmd_source(
    db: &Database,
    runner: &AdapterRunner,
    command: SourceCommand,
    json: bool,
) -> Result<()> {
    match command {
        SourceCommand::Add {
            path,
            adapter,
            id,
            input,
        } => {
            let input = read_input::<SourceAddInput>(input)?;
            let path_str = input
                .as_ref()
                .map_or_else(|| path.to_string_lossy().to_string(), |v| v.path.clone());
            let (input_adapter, input_id) = input
                .as_ref()
                .map_or((None, None), |v| (v.adapter.clone(), v.id.clone()));
            let adapter = input_adapter.or(adapter);
            let id = input_id.or(id);

            // Auto-detect adapter if not specified
            let adapter_name = if let Some(a) = adapter {
                a
            } else {
                let mut best_adapter = None;
                let mut best_confidence = 0.0f32;

                for adapter_name in runner.list_adapters() {
                    if let Some(adapter_path) = runner.find_adapter(&adapter_name)
                        && let Ok(Some(confidence)) = runner.detect(&adapter_path, &path_str).await
                        && confidence > best_confidence
                    {
                        best_confidence = confidence;
                        best_adapter = Some(adapter_name);
                    }
                }

                best_adapter.ok_or_else(|| {
                    anyhow::anyhow!("Could not auto-detect adapter for path: {path_str}")
                })?
            };

            let source_id = id.unwrap_or_else(|| {
                let uuid = uuid::Uuid::new_v4().to_string();
                let short = uuid.split('-').next().unwrap_or(uuid.as_str());
                format!("{adapter_name}-{short}")
            });

            // trx-gzfh: route through the source-registration chokepoint.
            // This enforces the five invariant rules (no duplicate, no
            // sub-path, no super-path, no file paths, no cross-harness
            // territory) before any source row is created.
            let canonical_roots = resolve_canonical_roots(runner).await;
            let existing = db.list_sources().await?;
            let source = match hstry_core::source_registry::validate_new_source(
                &adapter_name,
                &path_str,
                source_id.clone(),
                serde_json::Value::Object(serde_json::Map::default()),
                &canonical_roots,
                &existing,
                |p| p.is_dir(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    if json {
                        return emit_json(JsonResponse::<()> {
                            ok: false,
                            result: None,
                            error: Some(e.to_string()),
                        });
                    }
                    anyhow::bail!("{e}");
                }
            };

            db.upsert_source(&source).await?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(source),
                    error: None,
                });
            }
            println!("Added source: {source_id} ({adapter_name})");
        }
        SourceCommand::List => {
            let sources = db.list_sources().await?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(sources),
                    error: None,
                });
            }
            if sources.is_empty() {
                println!("No sources configured.");
            } else {
                for source in sources {
                    let path = source.path.as_deref().unwrap_or("-");
                    println!(
                        "{id} | {adapter} | {path}",
                        id = source.id,
                        adapter = source.adapter
                    );
                }
            }
        }
        SourceCommand::Remove { id, input } => {
            let input = read_input::<SourceRemoveInput>(input)?;
            let id = input.as_ref().map_or(id, |v| v.id.clone());
            db.remove_source(&id).await?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({ "id": id })),
                    error: None,
                });
            }
            println!("Removed source: {id}");
        }
        SourceCommand::Cleanup { auto_remove } => {
            let sources = db.list_sources().await?;

            // Group by (adapter, path_normalized)
            use std::collections::HashMap;
            let mut groups: HashMap<(String, String), Vec<hstry_core::models::Source>> =
                HashMap::new();

            for source in &sources {
                let path_normalized = source
                    .path
                    .as_deref()
                    .map(|p| p.trim_end_matches('/'))
                    .unwrap_or("")
                    .to_lowercase();

                groups
                    .entry((source.adapter.clone(), path_normalized))
                    .or_default()
                    .push(source.clone());
            }

            // Find duplicates
            let mut to_remove: Vec<String> = Vec::new();
            let mut total_duplicates = 0usize;

            for ((adapter, path), mut group) in groups {
                if group.len() > 1 {
                    total_duplicates += group.len() - 1;
                    if !json {
                        println!(
                            "Found {n} duplicate sources for {adapter}:{path}",
                            n = group.len(),
                            adapter = adapter,
                            path = path
                        );
                    }

                    // Sort: keep the shortest/most canonical ID (e.g., "pi" over "pi-558c036f")
                    group.sort_by(|a, b| {
                        // Prefer non-generated IDs (shorter, no dashes)
                        let a_score = if a.id.contains('-') { 2 } else { 0 };
                        let b_score = if b.id.contains('-') { 2 } else { 0 };
                        a_score.cmp(&b_score).then(a.id.len().cmp(&b.id.len()))
                    });

                    if !json {
                        println!("  Keeping: {id}", id = group[0].id);
                    }

                    // Keep first, mark rest for removal
                    for source in group.iter().skip(1) {
                        to_remove.push(source.id.clone());
                        if !json {
                            println!(
                                "  Would remove: {id} (path: {path})",
                                id = source.id,
                                path = source.path.as_deref().unwrap_or("-")
                            );
                        }
                    }
                }
            }

            if total_duplicates == 0 {
                if !json {
                    println!("No duplicate sources found.");
                }
                return Ok(());
            }

            if auto_remove {
                if !json {
                    println!(
                        "\nRemoving {count} duplicate sources...",
                        count = to_remove.len()
                    );
                }

                let mut conversations_removed = 0i64;
                let mut messages_removed = 0i64;

                for source_id in &to_remove {
                    // Get counts before removing
                    let (conv_count, msg_count) =
                        db.count_source_data(source_id).await.unwrap_or((0, 0));

                    conversations_removed += conv_count;
                    messages_removed += msg_count;

                    db.remove_source(source_id).await?;
                }

                if !json {
                    println!(
                        "Removed {sources} sources, {convs} conversations, {msgs} messages",
                        sources = to_remove.len(),
                        convs = conversations_removed,
                        msgs = messages_removed
                    );
                } else {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({
                            "removed_sources": to_remove.len(),
                            "removed_conversations": conversations_removed,
                            "removed_messages": messages_removed,
                            "source_ids": to_remove,
                        })),
                        error: None,
                    });
                }
            } else if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "duplicate_count": total_duplicates,
                        "source_ids": to_remove,
                    })),
                    error: None,
                });
            }
        }
        SourceCommand::PruneCursor {
            dry_run,
            auto_remove,
        } => {
            let sources = db.list_sources().await?;
            let cursor_sources: Vec<_> = sources.iter().filter(|s| s.adapter == "cursor").collect();

            if cursor_sources.is_empty() {
                if !json {
                    println!("No Cursor sources configured.");
                }
                return Ok(());
            }

            let has_global_storage = cursor_sources
                .iter()
                .any(|s| cursor_source_path_rank(s.path.as_deref().unwrap_or("")) == 0);

            let mut to_remove: Vec<String> = Vec::new();
            let mut keep: Vec<String> = Vec::new();

            if has_global_storage {
                for source in &cursor_sources {
                    let rank = cursor_source_path_rank(source.path.as_deref().unwrap_or(""));
                    if rank == 0 {
                        keep.push(source.id.clone());
                    } else {
                        to_remove.push(source.id.clone());
                    }
                }
            } else {
                let min_rank = cursor_sources
                    .iter()
                    .map(|s| cursor_source_path_rank(s.path.as_deref().unwrap_or("")))
                    .min()
                    .unwrap_or(1);

                let mut candidates: Vec<_> = cursor_sources
                    .iter()
                    .filter(|s| {
                        cursor_source_path_rank(s.path.as_deref().unwrap_or("")) == min_rank
                    })
                    .collect();
                candidates.sort_by(|a, b| a.id.cmp(&b.id));

                if let Some(best) = candidates.first() {
                    keep.push(best.id.clone());
                    for source in &cursor_sources {
                        if source.id != best.id {
                            to_remove.push(source.id.clone());
                        }
                    }
                }
            }

            if !json {
                println!("Cursor sources: {}", cursor_sources.len());
                for id in &keep {
                    println!("  Keep: {id}");
                }
                for id in &to_remove {
                    if let Some(source) = cursor_sources.iter().find(|s| &s.id == id) {
                        println!("  Remove: {id} ({})", source.path.as_deref().unwrap_or("-"));
                    }
                }
            }

            if to_remove.is_empty() {
                if !json {
                    println!("No redundant Cursor sources to prune.");
                }
                return Ok(());
            }

            if dry_run || !auto_remove {
                if !json && !dry_run {
                    println!(
                        "Run with --auto-remove to delete redundant sources (after `hstry dedup --cross-source`)."
                    );
                } else if !json && dry_run {
                    println!("Dry run: would remove {} Cursor sources.", to_remove.len());
                } else if json {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({
                            "dry_run": dry_run,
                            "keep": keep,
                            "remove": to_remove,
                        })),
                        error: None,
                    });
                }
                return Ok(());
            }

            let mut conversations_removed = 0i64;
            let mut messages_removed = 0i64;
            for source_id in &to_remove {
                let (conv_count, msg_count) =
                    db.count_source_data(source_id).await.unwrap_or((0, 0));
                conversations_removed += conv_count;
                messages_removed += msg_count;
                db.remove_source(source_id).await?;
            }

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "removed_sources": to_remove.len(),
                        "removed_conversations": conversations_removed,
                        "removed_messages": messages_removed,
                        "keep": keep,
                        "source_ids": to_remove,
                    })),
                    error: None,
                });
            }

            println!(
                "Removed {} Cursor sources ({} conversations, {} messages)",
                to_remove.len(),
                conversations_removed,
                messages_removed
            );
        }
    }
    Ok(())
}

fn cmd_adapters(
    runner: &AdapterRunner,
    config: &Config,
    config_path: &Path,
    command: Option<AdapterCommand>,
    json: bool,
) -> Result<()> {
    let mut config = config.clone();
    match command.unwrap_or(AdapterCommand::List) {
        AdapterCommand::List => {
            let adapters = runner.list_adapters();
            let statuses: Vec<AdapterStatus> = adapters
                .into_iter()
                .map(|adapter| AdapterStatus {
                    enabled: config.adapter_enabled(&adapter),
                    name: adapter,
                })
                .collect();
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(statuses),
                    error: None,
                });
            }
            if statuses.is_empty() {
                println!("No adapters found.");
            } else {
                println!("Available adapters:");
                for adapter in statuses {
                    let status = if adapter.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    println!("  {name} ({status})", name = adapter.name);
                }
            }
        }
        AdapterCommand::Add { path, input } => {
            let input = read_input::<AdapterAddInput>(input)?;
            let path = input
                .as_ref()
                .map(|v| PathBuf::from(&v.path))
                .unwrap_or(path);
            let expanded = Config::expand_path(&path.to_string_lossy());
            if !config.adapter_paths.contains(&expanded) {
                config.adapter_paths.push(expanded);
                config.save_to_path(config_path)?;
                if json {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({
                            "adapter_paths": config.adapter_paths,
                        })),
                        error: None,
                    });
                }
                println!("Added adapter path to config.");
            } else {
                if json {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({
                            "adapter_paths": config.adapter_paths,
                        })),
                        error: None,
                    });
                }
                println!("Adapter path already present in config.");
            }
        }
        AdapterCommand::Enable { name, input } => {
            let input = read_input::<AdapterToggleInput>(input)?;
            let name = input.as_ref().map_or(name, |v| v.name.clone());
            upsert_adapter_config(&mut config, &name, true);
            config.save_to_path(config_path)?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(AdapterStatus {
                        name,
                        enabled: true,
                    }),
                    error: None,
                });
            }
            println!("Enabled adapter: {name}");
        }
        AdapterCommand::Disable { name, input } => {
            let input = read_input::<AdapterToggleInput>(input)?;
            let name = input.as_ref().map_or(name, |v| v.name.clone());
            upsert_adapter_config(&mut config, &name, false);
            config.save_to_path(config_path)?;
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(AdapterStatus {
                        name,
                        enabled: false,
                    }),
                    error: None,
                });
            }
            println!("Disabled adapter: {name}");
        }
        AdapterCommand::Update {
            adapter,
            repo,
            force,
        } => {
            let expected_ref = format!("v{}", env!("CARGO_PKG_VERSION"));

            let mut repos_to_update: Vec<_> = config
                .adapter_repos
                .iter()
                .filter(|r| r.enabled)
                .filter(|r| repo.as_ref().is_none_or(|name| &r.name == name))
                .cloned()
                .collect();

            if repos_to_update.is_empty() {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some("No matching enabled repositories found".to_string()),
                    });
                }
                println!("No matching enabled repositories found.");
                return Ok(());
            }

            let mut config_changed = false;
            for repo in &mut repos_to_update {
                if let AdapterRepoSource::Git { url, git_ref, .. } = &mut repo.source
                    && official_adapter_ref_needs_update(url, git_ref, &expected_ref)
                {
                    *git_ref = expected_ref.clone();
                    config_changed = true;
                }
            }

            if config_changed {
                if let Some(repo_override) = repo.as_ref() {
                    config
                        .adapter_repos
                        .iter_mut()
                        .filter(|r| &r.name == repo_override)
                        .for_each(|r| {
                            if let AdapterRepoSource::Git { url, git_ref, .. } = &mut r.source
                                && official_adapter_ref_needs_update(url, git_ref, &expected_ref)
                            {
                                *git_ref = expected_ref.clone();
                            }
                        });
                } else {
                    for r in &mut config.adapter_repos {
                        if let AdapterRepoSource::Git { url, git_ref, .. } = &mut r.source
                            && official_adapter_ref_needs_update(url, git_ref, &expected_ref)
                        {
                            *git_ref = expected_ref.clone();
                        }
                    }
                }
                config.save_to_path(config_path)?;
            }

            let adapter_root = adapter_root_dir(&config)?;
            std::fs::create_dir_all(&adapter_root)?;

            let mut updated_repos = Vec::new();

            for repo in &repos_to_update {
                let repo_result =
                    update_repo_adapters(repo, &adapter_root, adapter.as_deref(), force)?;
                updated_repos.push(repo_result);
            }

            adapter_manifest::validate_adapter_manifest(&config.adapter_paths)?;

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({
                        "adapter_root": adapter_root,
                        "repos": updated_repos,
                    })),
                    error: None,
                });
            }

            println!("Updated adapters in {}", adapter_root.display());
            for repo_result in &updated_repos {
                println!(
                    "  {name}: {count} adapters",
                    name = repo_result.name,
                    count = repo_result.adapters.len()
                );
            }
        }
        AdapterCommand::Repo { command } => {
            cmd_adapter_repo(&mut config, config_path, command, json)?;
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct RepoUpdateResult {
    name: String,
    adapters: Vec<String>,
    source: String,
}

fn adapter_root_dir(config: &Config) -> Result<PathBuf> {
    if let Some(path) = config.adapter_paths.first() {
        return Ok(path.clone());
    }

    let config_dir = Config::default_config_path()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve config directory"))?
        .to_path_buf();
    Ok(config_dir.join("adapters"))
}

fn official_adapter_ref_needs_update(url: &str, git_ref: &str, expected_ref: &str) -> bool {
    url == hstry_core::config::DEFAULT_ADAPTER_REPO && git_ref != expected_ref
}

fn update_repo_adapters(
    repo: &AdapterRepo,
    adapter_root: &Path,
    filter: Option<&str>,
    force: bool,
) -> Result<RepoUpdateResult> {
    match &repo.source {
        AdapterRepoSource::Git { url, git_ref, path } => {
            let temp_dir = tempfile::tempdir()?;
            let target = temp_dir.path();

            let mut cmd = ProcessCommand::new("git");
            cmd.arg("clone")
                .arg("--depth")
                .arg("1")
                .arg("--branch")
                .arg(git_ref)
                .arg(url)
                .arg(target);

            let output = cmd.output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("Failed to clone adapters repo: {stderr}");
            }

            let src_root = target.join(path);
            let source_label = format!("git:{url}@{git_ref}");
            let adapters = copy_adapters_from(&src_root, adapter_root, filter, force)?;

            Ok(RepoUpdateResult {
                name: repo.name.clone(),
                adapters,
                source: source_label,
            })
        }
        AdapterRepoSource::Local { path } => {
            let src_root = PathBuf::from(path);
            let source_label = format!("local:{path}");
            let adapters = copy_adapters_from(&src_root, adapter_root, filter, force)?;

            Ok(RepoUpdateResult {
                name: repo.name.clone(),
                adapters,
                source: source_label,
            })
        }
        AdapterRepoSource::Archive { url, .. } => {
            anyhow::bail!("Archive adapter repositories are not supported yet: {url}");
        }
    }
}
