async fn cmd_resume(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    id: Option<String>,
    search_query: Option<String>,
    agent_override: Option<String>,
    source_filter: Option<String>,
    workspace_filter: Option<String>,
    after_str: Option<String>,
    before_str: Option<String>,
    limit: i64,
    dry_run: bool,
    allow_unverified: bool,
    pick: bool,
    json_output: bool,
) -> Result<()> {
    use hstry_core::db::ListConversationsOptions;

    // Machine-readable resume is a plan, never a false claim that a process launched.
    let dry_run = dry_run || json_output;
    let agent_name = agent_override
        .as_deref()
        .unwrap_or(&config.resume.default_agent);

    let agent_config = config
        .resume
        .agents
        .get(agent_name)
        .ok_or_else(|| {
            let available: Vec<_> = config.resume.agents.keys().collect();
            anyhow::anyhow!(
                "No resume configuration for agent '{agent_name}'. Available: {available:?}\n\
                 Add [resume.agents.{agent_name}] to your config.toml"
            )
        })?
        .clone();

    // Parse date filters
    let after = after_str.as_deref().map(parse_date_filter).transpose()?;
    let before = before_str.as_deref().map(parse_date_filter).transpose()?;

    // Interactive fzf picker mode
    if pick {
        return run_fzf_picker(
            db,
            runner,
            config,
            source_filter,
            workspace_filter,
            after,
            before,
            limit,
            agent_override,
            dry_run,
            allow_unverified,
            json_output,
        )
        .await;
    }

    // Step 1: Resolve the conversation
    let conversation = if let Some(ref id_str) = id {
        // Direct ID lookup
        resolve_conversation_by_id(db, id_str).await?
    } else if let Some(ref query) = search_query {
        // Search and pick
        let workspace_filter_like = workspace_filter.as_ref().map(|v| format!("%{v}%"));
        let conversations = db
            .list_conversation_summaries(ListConversationsOptions {
                source_id: source_filter.clone(),
                workspace: workspace_filter_like.clone(),
                after,
                before,
                updated_after: None,
                limit: Some(limit),
            })
            .await?;

        // Filter by search query (fuzzy match on title + first message)
        let query_lower = query.to_lowercase();
        let mut matches: Vec<_> = conversations
            .into_iter()
            .filter(|cs| {
                let title_match = cs
                    .conversation
                    .title
                    .as_ref()
                    .is_some_and(|t| t.to_lowercase().contains(&query_lower));
                let msg_match = cs
                    .first_user_message
                    .as_ref()
                    .is_some_and(|m| m.to_lowercase().contains(&query_lower));
                let workspace_match = cs
                    .conversation
                    .workspace
                    .as_ref()
                    .is_some_and(|w| w.to_lowercase().contains(&query_lower));
                title_match || msg_match || workspace_match
            })
            .collect();

        if matches.is_empty() {
            let search_opts = hstry_core::db::SearchOptions {
                source_id: source_filter.clone(),
                workspace: workspace_filter.clone(),
                limit: Some(limit),
                ..Default::default()
            };
            let hits = db.search(query, search_opts).await?;
            if hits.is_empty() {
                anyhow::bail!("No conversations found matching '{query}'");
            }
            // Use the top hit's conversation
            let top_hit = &hits[0];
            db.get_conversation(top_hit.conversation_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Conversation not found in database"))?
        } else if matches.len() == 1 {
            matches.remove(0).conversation
        } else {
            // Multiple matches: show numbered list for user to pick
            if json_output {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(
                        &matches
                            .iter()
                            .enumerate()
                            .map(|(i, cs)| {
                                serde_json::json!({
                                    "index": i + 1,
                                    "id": cs.conversation.id,
                                    "title": cs.conversation.title,
                                    "source": cs.conversation.source_id,
                                    "workspace": cs.conversation.workspace,
                                    "created_at": cs.conversation.created_at,
                                    "messages": cs.message_count,
                                })
                            })
                            .collect::<Vec<_>>(),
                    ),
                    error: None,
                });
            }

            eprintln!(
                "Found {} conversations matching '{query}':\n",
                matches.len()
            );
            for (i, cs) in matches.iter().enumerate() {
                let title = cs
                    .conversation
                    .title
                    .as_deref()
                    .or(cs.first_user_message.as_deref())
                    .unwrap_or("(untitled)");
                let truncated = if title.len() > 80 {
                    format!("{}...", &title[..77])
                } else {
                    title.to_string()
                };
                let source = &cs.conversation.source_id;
                let date = cs.conversation.created_at.format("%Y-%m-%d %H:%M");
                let workspace = cs.conversation.workspace.as_deref().unwrap_or("");
                let ws_short = workspace
                    .strip_prefix("/Users/")
                    .and_then(|s| s.split_once('/').map(|(_, rest)| format!("~/{rest}")))
                    .unwrap_or_else(|| workspace.to_string());

                eprintln!("  {i:>3}) [{source}] {date}  {ws_short}", i = i + 1);
                eprintln!("       {truncated}");
            }
            eprintln!();

            // Read user selection
            eprint!("Select (1-{}): ", matches.len());
            std::io::stderr().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let choice: usize = input
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid selection"))?;
            if choice == 0 || choice > matches.len() {
                anyhow::bail!("Selection out of range");
            }
            matches.remove(choice - 1).conversation
        }
    } else {
        // No ID and no search: list recent and pick
        let workspace_filter_like = workspace_filter.as_ref().map(|v| format!("%{v}%"));
        let conversations = db
            .list_conversation_summaries(ListConversationsOptions {
                source_id: source_filter.clone(),
                workspace: workspace_filter_like,
                after,
                before,
                updated_after: None,
                limit: Some(limit),
            })
            .await?;

        if conversations.is_empty() {
            anyhow::bail!("No conversations found. Try adjusting filters.");
        }

        if json_output {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(
                    &conversations
                        .iter()
                        .enumerate()
                        .map(|(i, cs)| {
                            serde_json::json!({
                                "index": i + 1,
                                "id": cs.conversation.id,
                                "title": cs.conversation.title,
                                "source": cs.conversation.source_id,
                                "workspace": cs.conversation.workspace,
                                "created_at": cs.conversation.created_at,
                                "messages": cs.message_count,
                            })
                        })
                        .collect::<Vec<_>>(),
                ),
                error: None,
            });
        }

        eprintln!("Recent conversations:\n");
        for (i, cs) in conversations.iter().enumerate() {
            let title = cs
                .conversation
                .title
                .as_deref()
                .or(cs.first_user_message.as_deref())
                .unwrap_or("(untitled)");
            let truncated = if title.len() > 80 {
                format!("{}...", &title[..77])
            } else {
                title.to_string()
            };
            let source = &cs.conversation.source_id;
            let date = cs.conversation.created_at.format("%Y-%m-%d %H:%M");
            let workspace = cs.conversation.workspace.as_deref().unwrap_or("");
            let ws_short = workspace
                .strip_prefix("/Users/")
                .and_then(|s| s.split_once('/').map(|(_, rest)| format!("~/{rest}")))
                .unwrap_or_else(|| workspace.to_string());

            eprintln!("  {i:>3}) [{source}] {date}  {ws_short}", i = i + 1);
            eprintln!("       {truncated}");
        }
        eprintln!();

        eprint!("Select (1-{}): ", conversations.len());
        std::io::stderr().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let choice: usize = input
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid selection"))?;
        if choice == 0 || choice > conversations.len() {
            anyhow::bail!("Selection out of range");
        }
        conversations[choice - 1].conversation.clone()
    };

    // Step 2: Determine source adapter
    let source = db
        .get_source(&conversation.source_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("Source '{}' not found in database", conversation.source_id)
        })?;
    let source_adapter = &source.adapter;

    // Step 3: Check for same-agent fast path
    let original_file = conversation
        .metadata
        .get("file")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    let is_same_agent = source_adapter == &agent_config.format;
    let original_exists = original_file.as_ref().is_some_and(|p| p.is_file());

    if is_same_agent && original_exists {
        let Some(session_path) = original_file.as_ref() else {
            anyhow::bail!("Missing original session file metadata");
        };
        let workspace = conversation.workspace.as_deref().unwrap_or(".");

        if dry_run {
            if json_output {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(&serde_json::json!({
                        "action": "direct_resume",
                        "launched": false,
                        "native_verification": "not_checked",
                        "argv": build_resume_command(&agent_config, session_path, &conversation)?,
                        "agent": agent_name,
                        "session_path": session_path,
                        "workspace": workspace,
                        "conversation_id": conversation.id,
                        "title": conversation.title,
                    })),
                    error: None,
                });
            }
            println!("Would resume directly (same agent, original file exists):");
            println!("  Agent:    {agent_name}");
            println!("  Session:  {}", session_path.display());
            println!("  Workspace: {workspace}");
            let cmd = build_resume_command(&agent_config, session_path, &conversation)?;
            println!("  Arguments: {cmd:?}");
            return Ok(());
        }

        if !json_output {
            eprintln!("Resuming {} session directly in {workspace}", agent_name);
        }

        let cmd = build_resume_command(&agent_config, session_path, &conversation)?;
        launch_agent(&cmd, workspace, json_output)?;
        return Ok(());
    }

    // Step 4: Convert and place
    let adapter_path = runner.find_adapter(&agent_config.format).ok_or_else(|| {
        anyhow::anyhow!(
            "No adapter found for format '{}'. Is it installed and enabled?",
            agent_config.format
        )
    })?;

    // A cross-agent continuation is a new native session, with explicit provenance.
    let mut target_conversation = conversation.clone();
    target_conversation.external_id = Some(resume::fresh_session_id(&agent_config.format));
    target_conversation.metadata["hstry_origin"] = serde_json::json!({"conversation_id":conversation.id,"source":conversation.source_id,"external_id":conversation.external_id});

    // Load messages and build export conversation
    let messages = db.get_messages(conversation.id).await?;
    let parsed_messages: Vec<ParsedMessage> = messages
        .into_iter()
        .map(|m| ParsedMessage {
            role: m.role.to_string(),
            content: m.content,
            created_at: m.created_at.map(|dt| dt.timestamp_millis()),
            model: m.model,
            tokens: m.tokens,
            cost_usd: m.cost_usd,
            parts: Some(m.parts_json),
            tool_calls: None,
            metadata: Some(m.metadata),
        })
        .collect();

    let export_conv = ExportConversation {
        external_id: target_conversation.external_id.clone(),
        readable_id: conversation.readable_id.clone(),
        title: conversation.title.clone(),
        created_at: conversation.created_at.timestamp_millis(),
        updated_at: conversation.updated_at.map(|dt| dt.timestamp_millis()),
        model: conversation.model.clone(),
        provider: conversation.provider.clone(),
        workspace: conversation.workspace.clone(),
        tokens_in: conversation.tokens_in,
        tokens_out: conversation.tokens_out,
        cost_usd: conversation.cost_usd,
        messages: parsed_messages,
        metadata: Some(target_conversation.metadata.clone()),
        version: Some(u64::try_from(conversation.version).unwrap_or(0)),
        message_count: Some(u32::try_from(conversation.message_count).unwrap_or(0)),
    };

    let export_opts = ExportOptions {
        format: agent_config.format.clone(),
        pretty: Some(false),
        include_tools: Some(true),
        include_attachments: Some(true),
    };

    let result = runner
        .export(&adapter_path, vec![export_conv], export_opts)
        .await?;

    // Step 5: Place the exported file(s) in the agent's native session directory
    let session_dir = Config::expand_path(&agent_config.session_dir);
    let placed_paths = place_exported_session(&result, &session_dir, &target_conversation, true)?;

    if placed_paths.is_empty() {
        anyhow::bail!("Export produced no files to place");
    }

    let primary_path = &placed_paths[0];
    let workspace = conversation.workspace.as_deref().unwrap_or(".");

    if dry_run {
        if json_output {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(&serde_json::json!({
                    "action": "convert_and_resume",
                    "launched": false,
                    "native_verification": "not_checked",
                    "fidelity": "adapter_projection_not_runtime_state",
                    "warnings": ["Native load/continuation is not verified for this installed agent version; runtime state, permissions, reasoning signatures and attachment availability may not survive conversion"],
                    "target_session_id": target_conversation.external_id,
                    "origin": target_conversation.metadata["hstry_origin"],
                    "requires_allow_unverified": true,
                    "argv": build_resume_command(&agent_config, primary_path, &target_conversation)?,
                    "agent": agent_name,
                    "source_adapter": source_adapter,
                    "target_format": agent_config.format,
                    "placed_files": placed_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "workspace": workspace,
                    "conversation_id": conversation.id,
                    "title": conversation.title,
                })),
                error: None,
            });
        }
        println!("Would convert and resume:");
        println!("  Agent:    {agent_name}");
        println!("  Source:   {source_adapter}");
        println!("  Format:   {}", agent_config.format);
        for p in &placed_paths {
            println!("  Placed:   {}", p.display());
        }
        println!("  Workspace: {workspace}");
        let cmd = build_resume_command(&agent_config, primary_path, &target_conversation)?;
        println!("  Arguments: {cmd:?}");
        return Ok(());
    }

    if !allow_unverified {
        anyhow::bail!(
            "Converted transcripts have not been verified against this installed agent version. Inspect --dry-run --json; use --allow-unverified only to explicitly accept this risk. No session files were written."
        );
    }
    let command = build_resume_command(&agent_config, primary_path, &target_conversation)?;
    which::which(&command[0])
        .map_err(|_| anyhow::anyhow!("Target executable is not installed: {}", command[0]))?;
    if !Path::new(workspace).is_dir() {
        anyhow::bail!("Recorded workspace does not exist; no session files were written");
    }
    place_exported_session(&result, &session_dir, &target_conversation, false)?;
    eprintln!(
        "Warning: conversion is an adapter projection; native continuation is not verified for this installed agent version."
    );
    if !json_output {
        eprintln!(
            "Converted {source_adapter} -> {} ({} file(s))",
            agent_config.format,
            placed_paths.len()
        );
    }

    launch_agent(&command, workspace, json_output)?;
    Ok(())
}

/// Resolve a conversation by UUID, partial UUID, or external_id.
async fn resolve_conversation_by_id(db: &Database, id_str: &str) -> Result<Conversation> {
    // Try full UUID first
    if let Ok(uuid) = uuid::Uuid::parse_str(id_str)
        && let Some(conv) = db.get_conversation(uuid).await?
    {
        return Ok(conv);
    }

    // Try partial UUID match or external_id match
    let all = db
        .list_conversations(hstry_core::db::ListConversationsOptions {
            limit: None,
            ..Default::default()
        })
        .await?;

    let matches: Vec<_> = all
        .into_iter()
        .filter(|c| {
            let id_match = c.id.to_string().starts_with(id_str);
            let ext_match = c
                .external_id
                .as_ref()
                .is_some_and(|e| e.starts_with(id_str) || e == id_str);
            let readable_match = c
                .readable_id
                .as_deref()
                .is_some_and(|r| r == id_str || r.starts_with(&format!("{id_str}-")));
            id_match || ext_match || readable_match
        })
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No conversation found matching '{id_str}'"),
        1 => matches
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No conversation found matching '{id_str}'")),
        n => anyhow::bail!(
            "Ambiguous ID '{id_str}': matched {n} conversations. Use a longer prefix."
        ),
    }
}

#[derive(Debug, Serialize)]
struct RemoveConversationResult {
    id: uuid::Uuid,
    source_id: String,
    external_id: Option<String>,
    title: Option<String>,
    messages: i64,
    removed: bool,
}

async fn cmd_remove(
    db: &Database,
    id: &str,
    yes: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let conversation = resolve_conversation_by_id(db, id).await?;
    let messages = db.count_messages_for_conversation(conversation.id).await?;
    let should_remove = yes && !dry_run;

    if should_remove {
        db.delete_conversations_batch(&[conversation.id]).await?;
    }

    let result = RemoveConversationResult {
        id: conversation.id,
        source_id: conversation.source_id,
        external_id: conversation.external_id,
        title: conversation.title,
        messages,
        removed: should_remove,
    };

    if json_output {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(result),
            error: None,
        });
    }

    let title = result.title.as_deref().unwrap_or("Untitled conversation");
    if should_remove {
        println!(
            "Removed '{title}' ({id}) and {messages} message(s)",
            id = result.id
        );
    } else {
        println!("Would remove:");
        println!("  Title:    {title}");
        println!("  ID:       {}", result.id);
        println!("  Source:   {}", result.source_id);
        println!("  Messages: {messages}");
        println!();
        println!("Run `hstry remove {} --yes` to confirm.", result.id);
    }

    Ok(())
}

/// Parse argument templates before substituting source data.
fn build_resume_command(
    agent_config: &hstry_core::config::AgentResumeConfig,
    session_path: &Path,
    conversation: &Conversation,
) -> Result<Vec<String>> {
    let id_string = conversation.id.to_string();
    let session_id = conversation.external_id.as_deref().unwrap_or(&id_string);
    resume::arguments(
        &agent_config.command,
        session_path,
        session_id,
        conversation.workspace.as_deref().unwrap_or("."),
    )
}

/// Place exported session files into the agent's native session directory.
///
/// Adapters may include a `root` prefix in their file paths (e.g., `sessions/...`).
/// Since `session_dir` already points to the target directory, we strip the root prefix.
fn place_exported_session(
    result: &hstry_runtime::ExportResult,
    session_dir: &Path,
    conversation: &Conversation,
    dry_run: bool,
) -> Result<Vec<PathBuf>> {
    let id = conversation
        .external_id
        .clone()
        .unwrap_or_else(|| conversation.id.to_string());
    let mut result = result.clone();
    if let Some(origin) = conversation.metadata.get("hstry_origin") {
        // Kept outside native message files, which may discard unknown metadata.
        result.files.get_or_insert_with(Vec::new).push(hstry_runtime::runner::ExportFile {
            path:format!(".hstry-origin/{id}.json"),
            content:serde_json::to_string(&serde_json::json!({"origin":origin,"target_session_id":id,"target_format":result.format}))?,
            encoding:None,
        });
    }
    resume::place(&result, session_dir, &id, dry_run)
}

/// Launch an agent process in the given workspace directory.
fn launch_agent(argv: &[String], workspace: &str, json_output: bool) -> Result<()> {
    if json_output {
        anyhow::bail!("JSON resume must return a plan without launching");
    }
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("Empty resume command"))?;
    eprintln!("Launching: {argv:?}");
    eprintln!("Workspace: {workspace}");

    let status = ProcessCommand::new(program)
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to launch '{program}': {e}"))?;

    if !status.success() {
        anyhow::bail!("Agent exited with status: {status}");
    }

    Ok(())
}

async fn cmd_index(_config: &Config, db: &Database, rebuild: bool, json: bool) -> Result<()> {
    let total = if rebuild {
        db.rebuild_search_fts().await?
    } else {
        0
    };

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(serde_json::json!({
                "indexed": total,
                "rebuild": rebuild,
                "backend": "sqlite-fts5",
            })),
            error: None,
        });
    }

    if rebuild {
        println!("Rebuilt SQLite FTS search index ({total} messages).");
    } else {
        println!("Search uses SQLite FTS5 and stays up to date via triggers.");
    }

    Ok(())
}

async fn cmd_stats(db: &Database, json: bool) -> Result<()> {
    let sources = db.list_sources().await?;
    let conv_count = db.count_conversations().await?;
    let msg_count = db.count_messages().await?;
    let sources_count = i64::try_from(sources.len()).unwrap_or(i64::MAX);
    let per_source = db.get_source_stats().await?;
    let activity = db.get_activity_stats(30).await?;

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(StatsSummary {
                sources: sources_count,
                conversations: conv_count,
                messages: msg_count,
                per_source,
                activity,
            }),
            error: None,
        });
    }

    // Header
    println!("\x1b[1mDatabase Statistics\x1b[0m");
    println!();

    // Totals
    println!("\x1b[1;34mTotals\x1b[0m");
    println!("  Sources:       {sources_count}");
    println!("  Conversations: {conv_count}");
    println!("  Messages:      {msg_count}");
    println!();

    // Activity
    println!("\x1b[1;34mActivity\x1b[0m");
    println!("  Today:      {:>6} conversations", activity.today);
    println!("  This week:  {:>6} conversations", activity.week);
    println!("  This month: {:>6} conversations", activity.month);
    println!();

    // Per-source stats
    if !per_source.is_empty() {
        println!("\x1b[1;34mPer Source\x1b[0m");
        println!(
            "  {:<15} {:<12} {:>8} {:>10} {:>12}",
            "SOURCE", "ADAPTER", "CONVS", "MSGS", "LAST SYNC"
        );
        println!("  {}", "-".repeat(60));
        for stats in &per_source {
            let last_sync = stats
                .last_sync_at
                .map(pretty::relative_time_short)
                .unwrap_or_else(|| "never".to_string());
            println!(
                "  {:<15} {:<12} {:>8} {:>10} {:>12}",
                truncate_title(&stats.source_id, 15),
                truncate_title(&stats.adapter, 12),
                stats.conversations,
                stats.messages,
                last_sync
            );
        }
        println!();
    }

    // Date range
    let oldest = per_source.iter().filter_map(|s| s.oldest).min();
    let newest = per_source.iter().filter_map(|s| s.newest).max();
    if let (Some(oldest), Some(newest)) = (oldest, newest) {
        println!("\x1b[1;34mDate Range\x1b[0m");
        println!("  Oldest: {}", oldest.format("%Y-%m-%d"));
        println!("  Newest: {}", newest.format("%Y-%m-%d"));
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct DedupResult {
    duplicates_found: usize,
    conversations_removed: usize,
    messages_removed: usize,
    dry_run: bool,
}
