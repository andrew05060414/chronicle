async fn cmd_import(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    path: PathBuf,
    adapter: Option<String>,
    source_id: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let path_str = path.to_string_lossy().to_string();
    let expanded = Config::expand_path(&path_str);

    if !expanded.exists() {
        if json {
            return emit_json(JsonResponse::<()> {
                ok: false,
                result: None,
                error: Some(format!("Path not found: {path}", path = expanded.display())),
            });
        }
        anyhow::bail!("Path not found: {path}", path = expanded.display());
    }

    // Detect or use specified adapter
    let (adapter_name, confidence) = if let Some(name) = adapter {
        // Verify adapter exists
        if runner.find_adapter(&name).is_none() {
            if json {
                return emit_json(JsonResponse::<()> {
                    ok: false,
                    result: None,
                    error: Some(format!("Adapter '{name}' not found")),
                });
            }
            anyhow::bail!("Adapter '{name}' not found");
        }
        (name, 1.0f32)
    } else {
        // Auto-detect adapter
        if !json {
            println!("Detecting format for {path}...", path = expanded.display());
        }

        let mut best_match: Option<(String, f32)> = None;
        let mut all_matches: Vec<DetectionResult> = Vec::new();

        for adapter_name in runner.list_adapters() {
            if !config.adapter_enabled(&adapter_name) {
                continue;
            }

            if let Some(adapter_path) = runner.find_adapter(&adapter_name)
                && let Ok(Some(conf)) = runner
                    .detect(&adapter_path, &expanded.to_string_lossy())
                    .await
                && conf > 0.3
            {
                all_matches.push(DetectionResult {
                    adapter: adapter_name.clone(),
                    confidence: conf,
                });

                if best_match
                    .as_ref()
                    .is_none_or(|(_, best_conf)| conf > *best_conf)
                {
                    best_match = Some((adapter_name, conf));
                }
            }
        }

        // Sort by confidence descending
        all_matches.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if !json && all_matches.len() > 1 {
            println!("Detected formats:");
            for m in &all_matches {
                println!(
                    "  {adapter} ({confidence:.0}%)",
                    adapter = m.adapter,
                    confidence = m.confidence * 100.0
                );
            }
        }

        if let Some((name, conf)) = best_match {
            if !json {
                println!(
                    "Using adapter: {name} (confidence: {confidence:.0}%)",
                    confidence = conf * 100.0
                );
            }
            (name, conf)
        } else {
            if json {
                return emit_json(JsonResponse::<()> {
                    ok: false,
                    result: None,
                    error: Some("Could not detect format. Use --adapter to specify.".to_string()),
                });
            }
            anyhow::bail!(
                "Could not detect format for {path}. Use --adapter to specify.",
                path = expanded.display()
            );
        }
    };

    let Some(adapter_path) = runner.find_adapter(&adapter_name) else {
        anyhow::bail!("Adapter '{adapter_name}' not found");
    };

    let normalized_source_path = expanded.to_string_lossy().trim_end_matches('/').to_string();
    let source_id = if let Some(source_id) = source_id {
        source_id
    } else if let Some(existing) = db
        .get_source_by_adapter_path(&adapter_name, &normalized_source_path)
        .await?
    {
        existing.id
    } else {
        adapter_name.clone()
    };

    // Parse conversations
    let parse_opts = hstry_runtime::runner::ParseOptions {
        since: None,
        limit: None,
        include_tools: true,
        include_attachments: true,
        cursor: None,
        batch_size: None,
    };

    let conversations = runner
        .parse(&adapter_path, &expanded.to_string_lossy(), parse_opts)
        .await?;

    if conversations.is_empty() {
        if json {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(ImportResult {
                    adapter: adapter_name,
                    confidence,
                    source_id,
                    conversations: 0,
                    messages: 0,
                    dry_run,
                }),
                error: None,
            });
        }
        println!("No conversations found.");
        return Ok(());
    }

    let conv_count = conversations.len();
    let msg_count: usize = conversations.iter().map(|c| c.messages.len()).sum();

    if dry_run {
        if json {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(ImportResult {
                    adapter: adapter_name,
                    confidence,
                    source_id,
                    conversations: conv_count,
                    messages: msg_count,
                    dry_run: true,
                }),
                error: None,
            });
        }
        println!("Dry run: would import {conv_count} conversations ({msg_count} messages)");
        for conv in &conversations {
            let title = conv.title.as_deref().unwrap_or("Untitled");
            let msg_cnt = conv.messages.len();
            println!("  - {title} ({msg_cnt} messages)");
        }
        return Ok(());
    }

    // trx-gzfh: route through the source-registration chokepoint so
    // import cannot create spurious sources (e.g. a path inside another
    // harness's tree, an individual file, a duplicate of an existing
    // source). Idempotent for an already-registered source.
    let canonical_roots = resolve_canonical_roots(runner).await;
    let existing_sources = db.list_sources().await?;
    let source = match hstry_core::source_registry::validate_new_source(
        &adapter_name,
        &normalized_source_path,
        source_id.clone(),
        serde_json::json!({}),
        &canonical_roots,
        &existing_sources,
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

    // Import conversations
    if !json {
        println!("Importing {conv_count} conversations...");
    }

    let mut imported_convs = 0usize;
    let mut imported_msgs = 0usize;

    for conv in conversations {
        let mut conv_id = uuid::Uuid::new_v4();
        if let Some(external_id) = conv.external_id.as_deref()
            && let Some(existing) = db.get_conversation_id(&source_id, external_id).await?
        {
            conv_id = existing;
        }

        let hstry_conv = hstry_core::models::Conversation {
            id: conv_id,
            source_id: source_id.clone(),
            external_id: conv.external_id,
            readable_id: conv.readable_id,
            platform_id: None,
            title: conv.title,
            created_at: chrono::DateTime::from_timestamp_millis(conv.created_at)
                .unwrap_or_default()
                .with_timezone(&chrono::Utc),
            updated_at: conv.updated_at.and_then(|ts| {
                chrono::DateTime::from_timestamp_millis(ts).map(|dt| dt.with_timezone(&chrono::Utc))
            }),
            model: conv.model,
            provider: conv.provider,
            workspace: conv.workspace,
            tokens_in: conv.tokens_in,
            tokens_out: conv.tokens_out,
            cost_usd: conv.cost_usd,
            metadata: conv
                .metadata
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .unwrap_or_default(),
            harness: None,
            version: 0,
            message_count: 0,
            parent_conversation_id: None,
            parent_message_idx: conv.parent_message_idx,
            fork_type: conv.fork_type,
        };

        db.upsert_conversation(&hstry_conv).await?;

        for (idx, msg) in conv.messages.iter().enumerate() {
            let Ok(idx) = i32::try_from(idx) else {
                continue;
            };
            let parts_json = msg.parts.clone().unwrap_or_else(|| serde_json::json!([]));
            let role_str = msg.role.as_str();
            // Stable, content-addressable id (trx-hjjw.4) so re-imports of
            // the same source produce idempotent rows.
            let stable_id = hstry_core::stable_message_id(
                &source_id,
                hstry_conv.external_id.as_deref(),
                idx,
                role_str,
                &msg.content,
                None,
            );
            let hstry_msg = hstry_core::models::Message {
                id: stable_id,
                conversation_id: hstry_conv.id,
                idx,
                role: hstry_core::models::MessageRole::from(role_str),
                content: msg.content.clone(),
                parts_json,
                created_at: msg.created_at.and_then(|ts| {
                    chrono::DateTime::from_timestamp_millis(ts)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                }),
                model: msg.model.clone(),
                tokens: msg.tokens,
                cost_usd: msg.cost_usd,
                metadata: serde_json::Value::Object(serde_json::Map::default()),
                sender: None,
                provider: None,
                harness: None,
                client_id: None,
            };
            db.insert_message(&hstry_msg).await?;
            imported_msgs += 1;
        }

        imported_convs += 1;
    }

    // Update source last_sync_at
    let mut updated_source = source;
    updated_source.last_sync_at = Some(chrono::Utc::now());
    db.upsert_source(&updated_source).await?;

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(ImportResult {
                adapter: adapter_name,
                confidence,
                source_id,
                conversations: imported_convs,
                messages: imported_msgs,
                dry_run: false,
            }),
            error: None,
        });
    }

    println!(
        "Imported {imported_convs} conversations ({imported_msgs} messages) into source '{source_id}'"
    );
    Ok(())
}

async fn cmd_search_fast(
    config: &Config,
    query: &str,
    limit: i64,
    source: Option<String>,
    workspace: Option<String>,
    mode: SearchModeArg,
    scope: SearchScopeArg,
    remotes: Vec<String>,
    roles: Vec<SearchRoleArg>,
    no_tools: bool,
    dedup: bool,
    include_system: bool,
    after: Option<String>,
    before: Option<String>,
    model: Option<String>,
    harness_filter: Option<String>,
    tag: Option<String>,
    compact: bool,
    json: bool,
    budget: hstry_core::recall::Budget,
    raw: bool,
    offset: i64,
    trace_file: Option<PathBuf>,
) -> Result<()> {
    let started = std::time::Instant::now();
    budget.validate()?;
    if !(1..=1000).contains(&limit) || offset < 0 {
        anyhow::bail!("limit must be 1..1000 and offset nonnegative");
    }
    let scope = if !remotes.is_empty() && scope == SearchScopeArg::Local {
        SearchScopeArg::Remote
    } else {
        scope
    };
    if offset > 0
        && scope != SearchScopeArg::Local
        && !(scope == SearchScopeArg::Remote && remotes.len() == 1)
    {
        anyhow::bail!(
            "Pagination requires a local search or one named --remote; page each store separately"
        );
    }
    // Parse date strings into DateTime<Utc>
    let after_dt = after.as_deref().map(parse_date_filter).transpose()?;
    let before_dt = before.as_deref().map(parse_date_filter).transpose()?;

    let include_system = include_system || roles.iter().any(|r| matches!(r, SearchRoleArg::System));
    // Filter before retrieval limits and fallback decisions, including multiple requested roles.
    let db_role = if !roles.is_empty() {
        Some(
            roles
                .iter()
                .filter(|r| !no_tools || !matches!(r, SearchRoleArg::Tool))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
    } else if no_tools || !include_system {
        Some(
            ["user", "assistant", "system", "tool", "other"]
                .into_iter()
                .filter(|r| (include_system || *r != "system") && (!no_tools || *r != "tool"))
                .collect::<Vec<_>>()
                .join(","),
        )
    } else {
        None
    };

    // Request more results than needed if we're filtering, to ensure we get enough after filtering
    let has_filters = !include_system || !roles.is_empty() || no_tools || dedup;
    let fetch_limit = if has_filters {
        (limit * 4).min(1000)
    } else {
        limit
    };
    let opts = hstry_core::db::SearchOptions {
        source_id: source,
        workspace,
        limit: Some(fetch_limit),
        offset: Some(offset),
        mode: mode.into(),
        after: after_dt,
        before: before_dt,
        role: db_role,
        model,
        harness: harness_filter,
        tag,
    };
    let mut report = hstry_core::recall::SearchReport::default();

    if scope != SearchScopeArg::Remote {
        let service_expected = std::env::var("HSTRY_NO_SERVICE").is_err()
            && config.service.enabled
            && config.service.search_api;

        let local = if service_expected {
            if let Some(results) =
                hstry_core::service::try_service_search_report(query, &opts).await?
            {
                results
            } else {
                anyhow::bail!(
                    "Search service unavailable. Run `hstry service start` or set HSTRY_NO_SERVICE=1 to use local search."
                );
            }
        } else if let Some(results) = try_api_search(query, &opts, mode).await? {
            results
        } else {
            let db = Database::open(&config.database).await?;
            apply_storage_config(&db, config);
            db.search_report(query, opts.clone()).await?
        };
        report = local;
    }

    if scope != SearchScopeArg::Local {
        let remote_list = config.remotes_for_search(&remotes)?;
        for name in &remotes {
            if !remote_list.iter().any(|r| r.enabled && r.name == *name) {
                anyhow::bail!("Unknown or disabled remote: {name}");
            }
        }
        // Only a remote-only search has nothing left to report when no remote is
        // enabled. `--scope all` must still return the local report computed above.
        if scope == SearchScopeArg::Remote && !remote_list.iter().any(|r| r.enabled) {
            anyhow::bail!("No enabled remotes to search");
        }
        match hstry_core::remote::search_remotes(&remote_list, query, &opts).await {
            Ok(remote) => {
                if scope == SearchScopeArg::Remote {
                    report.filters = remote.filters;
                }
                report.hits.extend(remote.hits);
                report.stores.extend(remote.stores);
                report.attempts.extend(remote.attempts);
                report.warnings.extend(remote.warnings);
                report.has_more |= remote.has_more;
            }
            // `--scope all` (including satellite default) already has the local
            // report; an unreachable hub must not throw that away.
            Err(err) if scope != SearchScopeArg::Remote => {
                report.warnings.push(format!("Remote search failed: {err}"));
            }
            Err(err) => return Err(err.into()),
        }
    }
    report.scope = match scope {
        SearchScopeArg::Local => "local_snapshot".into(),
        SearchScopeArg::Remote if remotes.len() == 1 => format!("remote:{}", remotes[0]),
        SearchScopeArg::Remote => "remote".into(),
        SearchScopeArg::All => "local_snapshot_and_remote".into(),
    };
    report.offset = offset;
    if scope != SearchScopeArg::Local && !(scope == SearchScopeArg::Remote && remotes.len() == 1) {
        report
            .warnings
            .push("Multi-store discovery: narrow to local or one named remote to paginate".into());
    }
    report.available_remotes = config
        .remotes
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.name.clone())
        .collect();
    let mut messages = std::mem::take(&mut report.hits);
    // Core results are best-first (BM25 is negative); preserve conversation-aware ranking.

    // Filter out system context (AGENTS.md, etc.) unless explicitly requested
    if !include_system {
        messages.retain(|hit| hit.role != MessageRole::System);
        report.filters["exclude_system"] = serde_json::json!(true);
    }

    // Filter out tool messages if requested
    if no_tools {
        messages.retain(|hit| hit.role != MessageRole::Tool);
    }

    // Filter by role if specified
    if !roles.is_empty() {
        messages.retain(|hit| {
            roles.iter().any(|r| match r {
                SearchRoleArg::User => hit.role == MessageRole::User,
                SearchRoleArg::Assistant => hit.role == MessageRole::Assistant,
                SearchRoleArg::System => hit.role == MessageRole::System,
                SearchRoleArg::Tool => hit.role == MessageRole::Tool,
            })
        });
    }

    // Deduplicate by external_id (real session identifier) if requested
    if dedup {
        let mut seen = std::collections::HashSet::new();
        messages.retain(|hit| {
            // Use external_id if available, otherwise conversation_id
            let key = hit
                .external_id
                .clone()
                .unwrap_or_else(|| hit.conversation_id.to_string());
            seen.insert(key)
        });
    }

    // Group by external_id if compact mode is enabled
    if compact {
        use std::collections::BTreeMap;

        // Group by a unique key: prefer external_id, fall back to conversation_id
        // This ensures sessions that exist in multiple sources are grouped together
        let mut grouped: BTreeMap<String, (usize, hstry_core::models::SearchHit)> = BTreeMap::new();

        for hit in &messages {
            // Use external_id if available, otherwise conversation_id
            let key = hit
                .external_id
                .clone()
                .unwrap_or_else(|| hit.conversation_id.to_string());

            let entry = grouped.entry(key).or_insert((0, hit.clone()));
            entry.0 += 1; // increment occurrence count
            // Lower BM25 is better.
            if hit.score < entry.1.score {
                entry.1 = hit.clone();
            }
        }

        // Convert back to vector with occurrence counts set
        messages = grouped
            .into_values()
            .map(|(count, mut hit)| {
                hit.occurrences = Some(count as i32);
                hit
            })
            .collect();

        // Re-sort by score after grouping
        messages.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Apply the original limit after filtering
    let truncate_to = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
    report.has_more |= messages.len() > truncate_to;
    messages.truncate(truncate_to);
    if let Some(path) = trace_file {
        let value = serde_json::json!({"version":1,"elapsed_ms":started.elapsed().as_millis(),"attempts":report.attempts,"returned_hits":messages.len(),"store_count":report.stores.len(),"warning_count":report.warnings.len(),"has_more":report.has_more,"ranks":messages.iter().enumerate().map(|(i,h)|serde_json::json!({"rank":i+1,"score":h.score,"role":h.role})).collect::<Vec<_>>()});
        let mut file = tempfile::NamedTempFile::new_in(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        serde_json::to_writer(file.as_file_mut(), &value)?;
        file.persist_noclobber(path).map_err(|e| e.error)?;
    }

    if json {
        report.hits = messages;
        let envelope = hstry_core::recall::project(&report, budget, raw)?;
        println!("{envelope}");
        return Ok(());
    }
    eprintln!(
        "Searched {} via {}; completeness is snapshot-only (not proof of global absence).",
        report.scope,
        report.attempts.join(" → ")
    );

    if compact {
        pretty::print_search_results_compact(&messages);
    } else {
        pretty::print_search_results(&messages);
    }
    Ok(())
}

/// Detect if content is system context (AGENTS.md, etc.) that should be hidden by default.
fn is_system_context(content: &str) -> bool {
    // Strong markers - if any of these are present, it's system context
    let strong_markers = [
        "# AGENTS.md",
        "# Agent Configuration",
        "<available_skills>",
        "Guidance for coding agents",
        "<SYSTEM_PROMPT>",
        "</SYSTEM_PROMPT>",
    ];

    for marker in &strong_markers {
        if content.contains(marker) {
            return true;
        }
    }

    // Check for AGENTS.md file path pattern
    if content.contains("AGENTS.md") && content.contains("instructions") {
        return true;
    }

    false
}

/// Detect a resume/compaction continuation fragment whose first user message is
/// the synthetic "conversation history ... compacted" summary Claude Code
/// injects. These are hidden from `list` by default but stay fully searchable.
fn is_continuation_fragment(first_user: Option<&str>) -> bool {
    first_user.is_some_and(|content| {
        content
            .trim_start()
            .starts_with("The conversation history before this point was compacted")
    })
}

#[derive(Serialize)]
struct SearchApiQuery<'a> {
    query: &'a str,
    limit: Option<i64>,
    offset: Option<i64>,
    source: Option<&'a str>,
    workspace: Option<&'a str>,
    mode: SearchModeArg,
    raw: bool,
    after: Option<String>,
    before: Option<String>,
    role: Option<&'a str>,
    model: Option<&'a str>,
    harness: Option<&'a str>,
    tag: Option<&'a str>,
}

async fn try_api_search(
    query: &str,
    opts: &hstry_core::db::SearchOptions,
    mode: SearchModeArg,
) -> Result<Option<hstry_core::recall::SearchReport>> {
    if std::env::var("HSTRY_NO_API").is_ok() {
        return Ok(None);
    }

    let api_url =
        std::env::var("HSTRY_API_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());
    let url = format!("{base}/search", base = api_url.trim_end_matches('/'));

    let query_params = SearchApiQuery {
        query,
        limit: opts.limit,
        offset: opts.offset,
        source: opts.source_id.as_deref(),
        workspace: opts.workspace.as_deref(),
        mode,
        raw: true,
        after: opts.after.map(|d| d.to_rfc3339()),
        before: opts.before.map(|d| d.to_rfc3339()),
        role: opts.role.as_deref(),
        model: opts.model.as_deref(),
        harness: opts.harness.as_deref(),
        tag: opts.tag.as_deref(),
    };

    let client = reqwest::Client::new();
    let Ok(response) = client.get(url).query(&query_params).send().await else {
        return Ok(None);
    };

    if !response.status().is_success() {
        return Ok(None);
    }

    let body = response.text().await?;
    let envelope: serde_json::Value = serde_json::from_str(&body)?;
    let report = serde_json::from_value(envelope["result"].clone()).map_err(|_| {
        anyhow::anyhow!("Search API predates recall protocol; upgrade it or set HSTRY_NO_API=1")
    })?;
    Ok(Some(report))
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum SearchModeArg {
    Auto,
    Natural,
    Code,
    Exact,
    Needle,
    Regex,
    Recent,
}

impl From<SearchModeArg> for hstry_core::db::SearchMode {
    fn from(value: SearchModeArg) -> Self {
        match value {
            SearchModeArg::Auto => hstry_core::db::SearchMode::Auto,
            SearchModeArg::Natural => hstry_core::db::SearchMode::NaturalLanguage,
            SearchModeArg::Code => hstry_core::db::SearchMode::Code,
            SearchModeArg::Exact => hstry_core::db::SearchMode::Exact,
            SearchModeArg::Needle => hstry_core::db::SearchMode::Needle,
            SearchModeArg::Regex => hstry_core::db::SearchMode::Regex,
            SearchModeArg::Recent => hstry_core::db::SearchMode::Recent,
        }
    }
}

#[derive(
    Debug, Clone, Copy, clap::ValueEnum, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
enum SearchScopeArg {
    Local,
    Remote,
    All,
}

impl From<SearchScopeArg> for hstry_core::config::SearchScope {
    fn from(value: SearchScopeArg) -> Self {
        match value {
            SearchScopeArg::Local => Self::Local,
            SearchScopeArg::Remote => Self::Remote,
            SearchScopeArg::All => Self::All,
        }
    }
}

impl From<hstry_core::config::SearchScope> for SearchScopeArg {
    fn from(value: hstry_core::config::SearchScope) -> Self {
        match value {
            hstry_core::config::SearchScope::Local => Self::Local,
            hstry_core::config::SearchScope::Remote => Self::Remote,
            hstry_core::config::SearchScope::All => Self::All,
        }
    }
}

#[derive(
    Debug, Clone, Copy, clap::ValueEnum, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
enum SearchRoleArg {
    User,
    Assistant,
    System,
    Tool,
}

impl std::fmt::Display for SearchRoleArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchRoleArg::User => write!(f, "user"),
            SearchRoleArg::Assistant => write!(f, "assistant"),
            SearchRoleArg::System => write!(f, "system"),
            SearchRoleArg::Tool => write!(f, "tool"),
        }
    }
}

fn conversation_identity_key(conv: &Conversation) -> String {
    let harness = conv.harness.as_deref().unwrap_or_default();
    if let Some(external_id) = conv.external_id.as_deref().filter(|v| !v.is_empty()) {
        return format!("{harness}|external|{external_id}");
    }
    if let Some(readable_id) = conv.readable_id.as_deref().filter(|v| !v.is_empty()) {
        return format!("{harness}|readable|{readable_id}");
    }
    if let Some(platform_id) = conv.platform_id.as_deref().filter(|v| !v.is_empty()) {
        return format!("{harness}|platform|{platform_id}");
    }
    format!("{harness}|id|{}", conv.id)
}

fn conversation_sort_ts(conv: &Conversation) -> i64 {
    conv.updated_at.unwrap_or(conv.created_at).timestamp()
}

fn source_preference(source_id: &str, harness: Option<&str>) -> u8 {
    if let Some(harness) = harness {
        if source_id == harness {
            return 0;
        }
        if source_id == format!("import-{harness}") {
            return 2;
        }
    }

    if source_id.starts_with("import-") {
        2
    } else {
        1
    }
}

fn cursor_source_path_rank(path: &str) -> u8 {
    let normalized = path.replace('\\', "/").to_lowercase();
    if normalized.contains("globalstorage") {
        0
    } else if normalized.contains("workspacestorage") {
        2
    } else if normalized.contains("cursaves") {
        3
    } else {
        1
    }
}

fn source_quality_rank(
    source_id: &str,
    sources: &std::collections::HashMap<String, hstry_core::models::Source>,
    harness: Option<&str>,
) -> u8 {
    if let Some(source) = sources.get(source_id)
        && source.adapter == "cursor"
    {
        return cursor_source_path_rank(source.path.as_deref().unwrap_or(""));
    }
    source_preference(source_id, harness)
}

fn should_replace_conversation(candidate: &Conversation, current: &Conversation) -> bool {
    should_replace_conversation_with_sources(candidate, current, &std::collections::HashMap::new())
}

fn should_replace_conversation_with_sources(
    candidate: &Conversation,
    current: &Conversation,
    sources: &std::collections::HashMap<String, hstry_core::models::Source>,
) -> bool {
    let candidate_pref =
        source_quality_rank(&candidate.source_id, sources, candidate.harness.as_deref());
    let current_pref = source_quality_rank(&current.source_id, sources, current.harness.as_deref());

    if candidate_pref != current_pref {
        return candidate_pref < current_pref;
    }

    conversation_sort_ts(candidate) > conversation_sort_ts(current)
}

fn dedup_conversation_previews(
    previews: Vec<hstry_core::db::ConversationPreview>,
) -> Vec<hstry_core::db::ConversationPreview> {
    let mut best_by_key: HashMap<String, hstry_core::db::ConversationPreview> = HashMap::new();

    for preview in previews {
        let key = conversation_identity_key(&preview.conversation);
        match best_by_key.get(&key) {
            Some(existing)
                if !should_replace_conversation(&preview.conversation, &existing.conversation) => {}
            _ => {
                best_by_key.insert(key, preview);
            }
        }
    }

    let mut deduped: Vec<hstry_core::db::ConversationPreview> = best_by_key.into_values().collect();
    deduped.sort_by(|a, b| {
        conversation_sort_ts(&b.conversation).cmp(&conversation_sort_ts(&a.conversation))
    });
    deduped
}

fn expanded_list_limit(limit: i64) -> i64 {
    if limit <= 0 {
        return limit;
    }
    (limit.saturating_mul(4)).min(2000)
}
