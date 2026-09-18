#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.no_color {
        console::set_colors_enabled(false);
    }

    // Initialize logging
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_log_filter(cli.verbose)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // Load config
    let config_path = cli.config.unwrap_or_else(Config::default_config_path);
    let config = Config::ensure_at(&config_path)?;

    match cli.command {
        Command::Search {
            query,
            max_chars,
            snippet_chars,
            raw,
            trace_file,
            offset,
            limit,
            source,
            workspace,
            mode,
            scope,
            remote,
            role,
            no_tools,
            dedup,
            include_system,
            after,
            before,
            model,
            harness_filter,
            tag,
            compact,
            input,
        } => {
            skill::warn_if_stale();
            let input = read_input::<SearchInput>(input)?;
            let query = input.as_ref().map_or(query, |v| v.query.clone());
            let limit = input.as_ref().and_then(|v| v.limit).unwrap_or(limit);
            let source = input.as_ref().and_then(|v| v.source.clone()).or(source);
            let workspace = input
                .as_ref()
                .and_then(|v| v.workspace.clone())
                .or(workspace);
            let mode = input.as_ref().and_then(|v| v.mode).unwrap_or(mode);
            let scope = SearchScopeArg::from(
                config.resolve_search_scope(
                    input
                        .as_ref()
                        .and_then(|v| v.scope)
                        .or(scope)
                        .map(Into::into),
                ),
            );
            let remotes = input
                .as_ref()
                .and_then(|v| v.remotes.clone())
                .unwrap_or(remote);
            let offset = input.as_ref().and_then(|v| v.offset).unwrap_or(offset);
            let after = input.as_ref().and_then(|v| v.after.clone()).or(after);
            let before = input.as_ref().and_then(|v| v.before.clone()).or(before);
            let role = input.as_ref().and_then(|v| v.role.clone()).unwrap_or(role);
            let model = input.as_ref().and_then(|v| v.model.clone()).or(model);
            let harness_filter = input
                .as_ref()
                .and_then(|v| v.harness_filter.clone())
                .or(harness_filter);
            let tag = input.as_ref().and_then(|v| v.tag.clone()).or(tag);
            cmd_search_fast(
                &config,
                &query,
                limit,
                source,
                workspace,
                mode,
                scope,
                remotes,
                role,
                no_tools,
                dedup,
                include_system,
                after,
                before,
                model,
                harness_filter,
                tag,
                compact,
                cli.json,
                hstry_core::recall::Budget {
                    total: max_chars,
                    snippet: snippet_chars,
                },
                raw,
                offset,
                trace_file,
            )
            .await
        }
        Command::Sync {
            source,
            parallel,
            input,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            let input = read_input::<SyncInput>(input)?;
            let source = input.as_ref().and_then(|v| v.source.clone()).or(source);
            let parallel = input.and_then(|v| v.parallel).or(parallel);
            cmd_sync(&db, &runner, &config, source, parallel, cli.json).await
        }
        Command::Import {
            path,
            adapter,
            source_id,
            dry_run,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_import(
                &db, &runner, &config, path, adapter, source_id, dry_run, cli.json,
            )
            .await
        }
        Command::Index { rebuild } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_index(&config, &db, rebuild, cli.json).await
        }
        Command::List {
            source,
            workspace,
            limit,
            after,
            before,
            input,
            peek,
            peek_chars,
            all,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let input = read_input::<ListInput>(input)?;
            let source = input.as_ref().and_then(|v| v.source.clone()).or(source);
            let workspace = input
                .as_ref()
                .and_then(|v| v.workspace.clone())
                .or(workspace);
            let limit = input.as_ref().and_then(|v| v.limit).unwrap_or(limit);
            let after = input.as_ref().and_then(|v| v.after.clone()).or(after);
            let before = input.as_ref().and_then(|v| v.before.clone()).or(before);
            let after_dt = after.as_deref().map(parse_date_filter).transpose()?;
            let before_dt = before.as_deref().map(parse_date_filter).transpose()?;
            if peek {
                cmd_list_peek(
                    &db, source, workspace, limit, after_dt, before_dt, peek_chars,
                )
                .await
            } else {
                cmd_list(
                    &db, source, workspace, limit, after_dt, before_dt, all, cli.json,
                )
                .await
            }
        }
        Command::Read {
            id,
            input,
            remote,
            options,
        } => {
            let request = read_input::<read_cli::ReadInput>(input)?;
            let (id, options) = if let Some(r) = request {
                (r.id, r.options)
            } else {
                (
                    id.ok_or_else(|| anyhow::anyhow!("Provide a conversation ID or --input"))?,
                    options.into(),
                )
            };
            let page = if let Some(name) = remote {
                let peer = config
                    .remotes
                    .iter()
                    .find(|r| r.name == name && r.enabled)
                    .ok_or_else(|| anyhow::anyhow!("Unknown remote {name}"))?;
                hstry_core::remote::read_remote(peer, &id, &options).await?
            } else {
                let db = Database::open(&config.database).await?;
                let conv = resolve_conversation_by_id(&db, &id).await?;
                db.read_page(conv.id, options).await?
            };
            println!("{}", page.to_wire()?);
            Ok(())
        }
        Command::Show {
            id,
            input,
            message_idx,
            full,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let input = read_input::<ShowInput>(input)?;
            let id = input
                .map(|v| v.id)
                .or(id)
                .ok_or_else(|| anyhow::anyhow!("Provide a conversation ID or --input"))?;
            if full {
                cmd_show(&db, &id, cli.json, message_idx).await
            } else {
                let conv = resolve_conversation_by_id(&db, &id).await?;
                let page = db
                    .read_page(
                        conv.id,
                        hstry_core::read::ReadOptions {
                            message_idx,
                            ..Default::default()
                        },
                    )
                    .await?;
                println!("{}", page.to_wire()?);
                Ok(())
            }
        }
        Command::Peek { id, chars } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_peek(&db, &id, chars, cli.json).await
        }
        Command::Remove { id, yes, dry_run } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_remove(&db, &id, yes, dry_run, cli.json).await
        }
        Command::Source { command } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_source(&db, &runner, command, cli.json).await
        }
        Command::Adapters { command } => {
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_adapters(&runner, &config, &config_path, command, cli.json)
        }
        Command::Service { command } => match command {
            ServiceCommand::Status => {
                let status = service::get_service_status(&config_path)?;
                if cli.json {
                    emit_json(JsonResponse {
                        ok: true,
                        result: Some(status),
                        error: None,
                    })
                } else {
                    service::cmd_service(&config_path, ServiceCommand::Status).await
                }
            }
            ServiceCommand::Run => service::cmd_service(&config_path, ServiceCommand::Run).await,
            other => {
                service::cmd_service(&config_path, other).await?;
                if cli.json {
                    let status = service::get_service_status(&config_path)?;
                    emit_json(JsonResponse {
                        ok: true,
                        result: Some(status),
                        error: None,
                    })
                } else {
                    Ok(())
                }
            }
        },
        Command::Scan => {
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_scan(&runner, &config, cli.json).await
        }
        Command::Quickstart => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_quickstart(&db, &runner, &config, &config_path, cli.json).await
        }
        Command::Export {
            format,
            conversations,
            source,
            workspace,
            role,
            output,
            session_files,
            pretty,
        } => {
            adapter_manifest::validate_adapter_manifest(&config.adapter_paths)?;
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_export(
                &db,
                &runner,
                &format,
                &conversations,
                source,
                workspace,
                role,
                output,
                session_files,
                pretty,
                cli.json,
            )
            .await
        }
        Command::Resume {
            id,
            search,
            agent,
            source,
            workspace,
            after,
            before,
            limit,
            dry_run,
            allow_unverified,
            pick,
        } => {
            adapter_manifest::validate_adapter_manifest(&config.adapter_paths)?;
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_resume(
                &db,
                &runner,
                &config,
                id,
                search,
                agent,
                source,
                workspace,
                after,
                before,
                limit,
                dry_run,
                allow_unverified,
                pick,
                cli.json,
            )
            .await
        }
        Command::Stats => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_stats(&db, cli.json).await
        }
        Command::Dedup {
            dry_run,
            source,
            cross_source,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_dedup(&db, dry_run, source, cross_source, cli.json).await
        }
        Command::Mmry { command } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_mmry(&db, command, cli.json).await
        }
        Command::Remote { command } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_remote(&db, &config, &config_path, command, cli.json).await
        }
        Command::Hub { command } => cmd_hub(&config, command, cli.json).await,
        Command::Checkpoint { command } => cmd_checkpoint(&config, command, cli.json).await,
        Command::Web { command } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            cmd_web(&db, &config, &config_path, command, cli.json).await
        }
        Command::Skill { command } => skill::run(command),
        Command::Config { command } => cmd_config(&config, &config_path, command, cli.json),
        Command::Reseed {
            source,
            dedup,
            no_index,
            no_bulk,
            dry_run,
            drop_source,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_reseed(
                &db,
                &runner,
                &source,
                dedup,
                !no_index,
                !no_bulk,
                dry_run,
                drop_source,
                cli.json,
            )
            .await
        }
        Command::Verify { source, repair } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
                anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
            })?;
            let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());
            cmd_verify(&db, &runner, source, repair, cli.json).await
        }
        Command::Backup {
            dry_run,
            target,
            encrypt,
            nas_remote,
        } => {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, &config);
            backup::run(
                &db,
                &config,
                &config_path,
                backup::BackupOpts {
                    dry_run,
                    encrypt,
                    targets: target,
                    nas_remote,
                },
                cli.json,
            )
            .await
        }
        Command::Skills { command } => skills::run(command),
        Command::Tui => skills::run_tui(),
    }
}

/// Ensure sources from config file are in the database.
///
/// trx-gzfh: New sources declared in `config.toml` are validated through
/// the source-registration chokepoint before they hit the database, so a
/// hand-edited config can't introduce a duplicate / sub-path / cross-
/// harness source. Updates to *already-registered* sources (path or
/// adapter changes) are validated against the rest of the source set as
/// well. Bad rows in config cause `ensure_config_sources` to fail loudly
/// rather than silently registering a spurious source.
async fn ensure_config_sources(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
) -> Result<()> {
    let canonical_roots = resolve_canonical_roots(runner).await;

    for source in &config.sources {
        let existing = db.get_source(&source.id).await?;
        let expanded_path = hstry_core::Config::expand_path(&source.path);
        let normalized_path = expanded_path
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();

        // The validator inspects the *current* sources table excluding
        // any row we are about to update. Without this filter, every
        // legitimate update would trip Rule 1 (duplicate path).
        let others: Vec<Source> = db
            .list_sources()
            .await?
            .into_iter()
            .filter(|s| s.id != source.id)
            .collect();

        let entry = match existing {
            Some(mut entry) => {
                let existing_normalized = entry
                    .path
                    .as_deref()
                    .map(|p| p.trim_end_matches('/').to_string())
                    .unwrap_or_default();
                let adapter_changed = entry.adapter != source.adapter;
                let path_changed = existing_normalized != normalized_path;

                if adapter_changed || path_changed {
                    // Re-validate against the invariant before mutating.
                    let validated = hstry_core::source_registry::validate_new_source(
                        &source.adapter,
                        &normalized_path,
                        source.id.clone(),
                        entry.config.clone(),
                        &canonical_roots,
                        &others,
                        |p| p.is_dir(),
                    )
                    .map_err(|e| {
                        anyhow::anyhow!("config source '{id}' rejected: {e}", id = source.id)
                    })?;
                    entry.adapter = validated.adapter;
                    entry.path = validated.path;
                    entry.last_sync_at = None;
                    if let serde_json::Value::Object(mut cfg) = entry.config.clone() {
                        cfg.remove("cursor");
                        entry.config = serde_json::Value::Object(cfg);
                    }
                }
                entry
            }
            None => hstry_core::source_registry::validate_new_source(
                &source.adapter,
                &normalized_path,
                source.id.clone(),
                serde_json::Value::Object(serde_json::Map::default()),
                &canonical_roots,
                &others,
                |p| p.is_dir(),
            )
            .map_err(|e| anyhow::anyhow!("config source '{id}' rejected: {e}", id = source.id))?,
        };
        db.upsert_source(&entry).await?;
    }
    Ok(())
}

fn default_sync_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().min(4))
        .unwrap_or(4)
}

/// Per-source result captured during a concurrent sync so the final summary can
/// be rendered in a stable, grouped order instead of interleaved line-by-line.
enum SyncOutcome {
    Synced {
        id: String,
        adapter: String,
        conversations: usize,
        messages: usize,
    },
    UpToDate,
    Failed {
        id: String,
        adapter: String,
        message: String,
    },
}

async fn sync_sources(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    source_filter: Option<String>,
    parallel: Option<usize>,
    print: bool,
) -> Result<Vec<sync::SyncStats>> {
    adapter_manifest::validate_adapter_manifest(&config.adapter_paths)?;

    // Ensure sources from config are in the database
    ensure_config_sources(db, runner, config).await?;

    let sources = db.list_sources().await?;

    if sources.is_empty() {
        if print {
            println!("No sources configured. Use 'hstry source add <path>' to add a source.");
        }
        return Ok(Vec::new());
    }

    let mut sources_to_sync = Vec::new();
    let mut disabled = 0usize;
    for source in sources {
        if let Some(ref filter) = source_filter
            && &source.id != filter
        {
            continue;
        }

        if !config.adapter_enabled(&source.adapter) {
            disabled += 1;
            continue;
        }

        sources_to_sync.push(source);
    }

    if sources_to_sync.is_empty() {
        if print && disabled > 0 {
            println!(
                "{}",
                console::style(format!("{disabled} adapter(s) disabled in config")).dim()
            );
        }
        return Ok(Vec::new());
    }

    let total = sources_to_sync.len();
    let parallelism = parallel.unwrap_or_else(default_sync_parallelism).max(1);
    let parallelism = parallelism.min(total.max(1));
    let stats = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let outcomes = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let pb = if print {
        let bar = indicatif::ProgressBar::new_spinner();
        bar.set_style(
            indicatif::ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message(format!("Syncing 0/{total} sources..."));
        Some(bar)
    } else {
        None
    };

    stream::iter(sources_to_sync)
        .for_each_concurrent(parallelism, |mut source| {
            let stats = Arc::clone(&stats);
            let outcomes = Arc::clone(&outcomes);
            let done = Arc::clone(&done);
            let pb = pb.clone();
            async move {
                let outcome = sync_one_source(db, runner, &mut source, &stats).await;

                let mut outcomes = outcomes.lock().await;
                outcomes.push(outcome);
                if let Some(bar) = pb.as_ref() {
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    bar.set_message(format!("Syncing {n}/{total} sources..."));
                }
            }
        })
        .await;

    if let Some(bar) = pb {
        bar.finish_and_clear();
    }

    if print {
        let outcomes = outcomes.lock().await;
        print_sync_summary(&outcomes, disabled);
    }

    Ok(stats.lock().await.clone())
}

/// Sync a single source, resetting a stale cursor when the source has no
/// surviving conversations. Pushes successful stats onto the shared accumulator
/// and returns the outcome for the end-of-run summary.
async fn sync_one_source(
    db: &Database,
    runner: &AdapterRunner,
    source: &mut Source,
    stats: &Arc<tokio::sync::Mutex<Vec<sync::SyncStats>>>,
) -> SyncOutcome {
    if source.last_sync_at.is_some() {
        match db.count_source_data(&source.id).await {
            Ok((0, _)) => {
                source.last_sync_at = None;
                if let serde_json::Value::Object(mut config) = source.config.clone() {
                    config.remove("cursor");
                    source.config = serde_json::Value::Object(config);
                }
            }
            Ok(_) => {}
            Err(err) => {
                return SyncOutcome::Failed {
                    id: source.id.clone(),
                    adapter: source.adapter.clone(),
                    message: err.to_string(),
                };
            }
        }
    }

    match sync::sync_source(db, runner, source).await {
        Ok(result) => {
            if result.conversations > 0 {
                let outcome = SyncOutcome::Synced {
                    id: source.id.clone(),
                    adapter: source.adapter.clone(),
                    conversations: result.conversations,
                    messages: result.messages,
                };
                stats.lock().await.push(result);
                outcome
            } else {
                stats.lock().await.push(result);
                SyncOutcome::UpToDate
            }
        }
        Err(err) => SyncOutcome::Failed {
            id: source.id.clone(),
            adapter: source.adapter.clone(),
            message: err.to_string(),
        },
    }
}

/// Render the grouped, color-styled sync summary: failures first (so they're
/// not lost in scrollback), then newly synced sources, then a single collapsed
/// line for everything that was already up to date.
fn print_sync_summary(outcomes: &[SyncOutcome], disabled: usize) {
    let mut up_to_date = 0usize;
    let mut synced = 0usize;
    let mut total_conversations = 0usize;
    let mut failed = 0usize;

    for outcome in outcomes {
        if let SyncOutcome::Failed {
            id,
            adapter,
            message,
        } = outcome
        {
            failed += 1;
            eprintln!(
                "{} {} {}",
                console::style("✗").red().bold(),
                console::style(format!("{id} ({adapter})")).bold(),
                console::style(message).red()
            );
        }
    }

    for outcome in outcomes {
        if let SyncOutcome::Synced {
            id,
            adapter,
            conversations,
            messages,
        } = outcome
        {
            synced += 1;
            total_conversations += conversations;
            println!(
                "{} {} {}",
                console::style("✓").green().bold(),
                console::style(format!("{id} ({adapter})")).bold(),
                console::style(format!(
                    "{conversations} conversations, {messages} messages"
                ))
                .dim()
            );
        } else if matches!(outcome, SyncOutcome::UpToDate) {
            up_to_date += 1;
        }
    }

    let mut notes = Vec::new();
    if up_to_date > 0 {
        notes.push(format!("{up_to_date} up to date"));
    }
    if disabled > 0 {
        notes.push(format!("{disabled} disabled"));
    }
    if !notes.is_empty() {
        println!("{}", console::style(notes.join(", ")).dim());
    }

    let summary = if synced > 0 {
        format!("Synced {total_conversations} conversations across {synced} source(s)")
    } else if failed > 0 {
        "No conversations synced".to_string()
    } else {
        "Everything up to date".to_string()
    };
    println!("{}", console::style(summary).bold());
}

async fn cmd_sync(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    source_filter: Option<String>,
    parallel: Option<usize>,
    json: bool,
) -> Result<()> {
    let stats = sync_sources(db, runner, config, source_filter, parallel, !json).await?;

    if json {
        let total_sources = stats.len();
        let total_conversations = stats.iter().map(|s| s.conversations).sum();
        let total_messages = stats.iter().map(|s| s.messages).sum();
        return emit_json(JsonResponse {
            ok: true,
            result: Some(SyncSummary {
                sources: stats,
                total_sources,
                total_conversations,
                total_messages,
            }),
            error: None,
        });
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct ImportResult {
    adapter: String,
    confidence: f32,
    source_id: String,
    conversations: usize,
    messages: usize,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct DetectionResult {
    adapter: String,
    confidence: f32,
}
