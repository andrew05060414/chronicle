async fn cmd_scan(runner: &AdapterRunner, config: &Config, json: bool) -> Result<()> {
    if !json {
        println!("Scanning for chat history sources...\n");
    }

    let hits = scan_hits(runner, config).await?;

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(hits),
            error: None,
        });
    }

    for hit in hits {
        println!(
            "  {} {} (confidence: {:.0}%)",
            hit.display_name,
            hit.path,
            hit.confidence * 100.0
        );
    }

    Ok(())
}

fn upsert_adapter_config(config: &mut Config, name: &str, enabled: bool) {
    if let Some(entry) = config.adapters.iter_mut().find(|entry| entry.name == name) {
        entry.enabled = enabled;
    } else {
        config.adapters.push(hstry_core::config::AdapterConfig {
            name: name.to_string(),
            enabled,
        });
    }
}

fn read_input<T: DeserializeOwned>(input: Option<PathBuf>) -> Result<Option<T>> {
    let Some(path) = input else {
        return Ok(None);
    };
    let mut buf = String::new();
    if path.as_os_str() == "-" {
        std::io::stdin().read_to_string(&mut buf)?;
    } else {
        let mut file = std::fs::File::open(path)?;
        file.read_to_string(&mut buf)?;
    }
    let value = serde_json::from_str(&buf)?;
    Ok(Some(value))
}

fn emit_json<T: serde::Serialize>(value: T) -> Result<()> {
    let pretty = serde_json::to_string_pretty(&value)?;
    println!("{pretty}");
    Ok(())
}

/// Truncate a title for display, cleaning up whitespace and newlines.
fn truncate_title(title: &str, max_len: usize) -> String {
    // Replace newlines and collapse whitespace
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= max_len {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

fn display_title_for_list(title: Option<&str>, first_user: Option<&str>) -> String {
    let title = title.unwrap_or("");
    let first_user = first_user.unwrap_or("");

    if (title.is_empty() || is_system_context(title))
        && !first_user.trim().is_empty()
        && !is_continuation_fragment(Some(first_user))
    {
        return first_user.to_string();
    }

    if title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        title.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_log_filter_suppresses_sqlx_slow_query_warnings_without_verbose() {
        assert_eq!(
            default_log_filter(0),
            "warn,hstry_cli=info,hstry_core=info,sqlx=error"
        );
        assert_eq!(
            default_log_filter(1),
            "warn,hstry_cli=debug,hstry_core=debug,sqlx=warn"
        );
        assert_eq!(default_log_filter(2), "trace");
    }

    #[test]
    fn read_input_reads_json_file() {
        let mut file =
            tempfile::NamedTempFile::new().unwrap_or_else(|err| panic!("temp file: {err}"));
        writeln!(file, "{{\"id\":\"conv-1\"}}").unwrap_or_else(|err| panic!("write: {err}"));
        let value: Option<ShowInput> =
            read_input(Some(file.path().to_path_buf())).unwrap_or_else(|err| panic!("read: {err}"));
        let value = value.unwrap_or_else(|| panic!("missing parsed value"));
        assert_eq!(value.id, "conv-1");
    }

    #[test]
    fn is_system_context_detects_agents_md() {
        // Should detect AGENTS.md markers
        assert!(is_system_context(
            "# AGENTS.md\n\nGuidance for coding agents"
        ));
        assert!(is_system_context(
            "Some text\n<available_skills>\n</available_skills>"
        ));
        assert!(is_system_context(
            "# Agent Configuration\n\nSome instructions"
        ));
        assert!(is_system_context("AGENTS.md instructions for the agent"));

        // Should NOT detect normal content
        assert!(!is_system_context("Can you help me with this code?"));
        assert!(!is_system_context("The agent ran the command successfully"));
        assert!(!is_system_context("Check the AGENTS.md file")); // just filename mention

        // Compaction-continuation content stays searchable, so it is NOT
        // classified as system context (that path drives search filtering).
        assert!(!is_system_context(
            "The conversation history before this point was compacted into the following summary:"
        ));
    }

    #[test]
    fn continuation_fragments_are_detected_and_hidden_by_default() {
        assert!(is_continuation_fragment(Some(
            "The conversation history before this point was compacted into the following summary: ..."
        )));
        assert!(is_continuation_fragment(Some(
            "\n  The conversation history before this point was compacted"
        )));
        assert!(!is_continuation_fragment(Some("Central Server for ROMs")));
        assert!(!is_continuation_fragment(None));
    }

    #[test]
    fn embedded_web_runner_waits_for_an_authenticated_chatgpt_session() {
        let runner = include_str!("../assets/web-runner.ts");

        assert!(runner.contains("await waitForChatGPTAuthentication(page);"));
        assert!(runner.contains("Boolean(session?.user || session?.accessToken)"));
        assert!(
            !runner.contains(
                "if (provider === 'chatgpt') {\n    await page.waitForSelector('textarea'"
            )
        );
    }

    #[test]
    fn embedded_web_runner_sets_chatgpt_api_base_url() {
        let runner = include_str!("../assets/web-runner.ts");

        assert!(runner.contains("baseURL: providerUrls.chatgpt,"));
    }

    #[test]
    fn official_adapter_release_refs_advance_with_the_binary_version() {
        assert!(official_adapter_ref_needs_update(
            hstry_core::config::DEFAULT_ADAPTER_REPO,
            "v0.5.3",
            "v0.5.23"
        ));
        assert!(!official_adapter_ref_needs_update(
            hstry_core::config::DEFAULT_ADAPTER_REPO,
            "v0.5.23",
            "v0.5.23"
        ));
        assert!(!official_adapter_ref_needs_update(
            "https://example.com/custom-adapters",
            "v0.5.3",
            "v0.5.23"
        ));
    }

    #[test]
    fn bundled_adapter_manifest_matches_the_binary_version() {
        let manifest: adapter_manifest::AdapterManifest =
            serde_json::from_str(include_str!("../../../adapters/.hstry-adapters.json"))
                .unwrap_or_else(|err| panic!("parse bundled adapter manifest: {err}"));

        assert_eq!(manifest.hstry_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest.protocol_version,
            adapter_manifest::ADAPTER_PROTOCOL_VERSION
        );
    }

    #[test]
    fn parse_date_filter_accepts_relative_durations() {
        let now = chrono::Utc::now();
        let cases = [
            ("2", chrono::Duration::days(2)),
            ("1w", chrono::Duration::weeks(1)),
            ("2d", chrono::Duration::days(2)),
            ("3h", chrono::Duration::hours(3)),
            ("30m", chrono::Duration::minutes(30)),
            ("2mo", chrono::Duration::days(60)),
            ("1y", chrono::Duration::days(365)),
            ("1 week", chrono::Duration::weeks(1)),
            ("2 days", chrono::Duration::days(2)),
            ("3 hours ago", chrono::Duration::hours(3)),
        ];
        for (input, expected) in cases {
            let parsed =
                parse_date_filter(input).unwrap_or_else(|err| panic!("parse '{input}': {err}"));
            // Bare numbers snap to start-of-day, so allow a full day of slack.
            let delta = ((now - expected) - parsed).num_seconds().abs();
            assert!(
                delta <= 86_400,
                "input '{input}' parsed too far from expected (delta {delta}s)"
            );
        }
    }
}

async fn cmd_export(
    db: &Database,
    runner: &AdapterRunner,
    format: &str,
    conversations_arg: &str,
    source_filter: Option<String>,
    workspace_filter: Option<String>,
    role_filter: Vec<SearchRoleArg>,
    output: Option<PathBuf>,
    session_files: bool,
    pretty: bool,
    json_output: bool,
) -> Result<()> {
    use hstry_core::db::ListConversationsOptions;
    use std::fs;

    // Find the adapter for the target format
    // For universal formats (markdown, json), use any available adapter
    let adapter_path = if format == "markdown" || format == "json" {
        // Try to use the first available adapter that supports export
        let adapters = runner.list_adapters();
        adapters
            .into_iter()
            .find_map(|name| runner.find_adapter(&name))
            .ok_or_else(|| anyhow::anyhow!("No adapters available for export"))?
    } else {
        runner
            .find_adapter(format)
            .ok_or_else(|| anyhow::anyhow!("No adapter found for format '{format}'"))?
    };

    // Load conversations from database
    // Apply fuzzy matching for workspace filter (wrap with % for SQL LIKE)
    let workspace_filter = workspace_filter.map(|value| format!("%{value}%"));
    let conversations = if conversations_arg == "all" {
        db.list_conversations(ListConversationsOptions {
            source_id: source_filter.clone(),
            workspace: workspace_filter.clone(),
            after: None,
            before: None,
            updated_after: None,
            limit: None,
        })
        .await?
    } else {
        let mut convs = Vec::new();
        for id in conversations_arg
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            convs.push(resolve_conversation_by_id(db, id).await?);
        }
        convs
    };

    if conversations.is_empty() {
        if json_output {
            return emit_json(JsonResponse::<()> {
                ok: true,
                result: None,
                error: Some("No conversations found".to_string()),
            });
        }
        println!("No conversations found");
        return Ok(());
    }

    // Convert to export format
    let mut export_convs = Vec::new();
    for conv in &conversations {
        let messages = db.get_messages(conv.id).await?;
        let parsed_messages: Vec<ParsedMessage> = messages
            .into_iter()
            .filter(|m| {
                // Filter by role if specified
                if role_filter.is_empty() {
                    return true;
                }
                role_filter.iter().any(|r| match r {
                    SearchRoleArg::User => m.role == MessageRole::User,
                    SearchRoleArg::Assistant => m.role == MessageRole::Assistant,
                    SearchRoleArg::System => m.role == MessageRole::System,
                    SearchRoleArg::Tool => m.role == MessageRole::Tool,
                })
            })
            .map(|m| ParsedMessage {
                role: m.role.to_string(),
                content: m.content,
                created_at: m.created_at.map(|dt| dt.timestamp_millis()),
                model: m.model,
                tokens: m.tokens,
                cost_usd: m.cost_usd,
                parts: Some(m.parts_json),
                tool_calls: None, // TODO: load from tool_calls table
                metadata: Some(m.metadata),
            })
            .collect();

        export_convs.push(ExportConversation {
            external_id: conv.external_id.clone(),
            readable_id: conv.readable_id.clone(),
            title: conv.title.clone(),
            created_at: conv.created_at.timestamp_millis(),
            updated_at: conv.updated_at.map(|dt| dt.timestamp_millis()),
            model: conv.model.clone(),
            provider: conv.provider.clone(),
            workspace: conv.workspace.clone(),
            tokens_in: conv.tokens_in,
            tokens_out: conv.tokens_out,
            cost_usd: conv.cost_usd,
            messages: parsed_messages,
            metadata: Some(conv.metadata.clone()),
            version: Some(u64::try_from(conv.version).unwrap_or(0)),
            message_count: Some(u32::try_from(conv.message_count).unwrap_or(0)),
        });
    }

    if !json_output && session_files && (format == "markdown" || format == "json") {
        let output_dir = output.unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&output_dir)?;

        let mut written = 0usize;
        for (index, conv) in export_convs.iter().enumerate() {
            let single_result = runner
                .export(
                    &adapter_path,
                    vec![conv.clone()],
                    ExportOptions {
                        format: format.to_string(),
                        pretty: Some(pretty),
                        include_tools: Some(true),
                        include_attachments: Some(true),
                    },
                )
                .await?;

            if let Some(content) = &single_result.content {
                let filename = build_session_export_filename(conv, index, format);
                fs::write(output_dir.join(filename), content)?;
                written += 1;
                continue;
            }

            if let Some(files) = &single_result.files {
                for file in files {
                    let file_path = output_dir.join(&file.path);
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&file_path, &file.content)?;
                    written += 1;
                }
            }
        }

        println!(
            "Exported {} conversations to {} files in {}",
            conversations.len(),
            written,
            output_dir.display()
        );
        return Ok(());
    }

    let opts = ExportOptions {
        format: format.to_string(),
        pretty: Some(pretty),
        include_tools: Some(true),
        include_attachments: Some(true),
    };

    let result = runner.export(&adapter_path, export_convs, opts).await?;

    if json_output {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(&result),
            error: None,
        });
    }

    // Handle output
    if let Some(content) = &result.content {
        if let Some(output_path) = output {
            fs::write(&output_path, content)?;
            println!(
                "Exported {} conversations to {}",
                conversations.len(),
                output_path.display()
            );
        } else {
            println!("{content}");
        }
    } else if let Some(files) = &result.files {
        let output_dir = output.unwrap_or_else(|| PathBuf::from("."));
        for file in files {
            let file_path = output_dir.join(&file.path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&file_path, &file.content)?;
        }
        println!(
            "Exported {} conversations to {} files in {}",
            conversations.len(),
            files.len(),
            output_dir.display()
        );
    }

    Ok(())
}

fn build_session_export_filename(conv: &ExportConversation, index: usize, format: &str) -> String {
    let ext = match format {
        "markdown" => "md",
        "json" => "json",
        _ => "txt",
    };

    let stem = conv
        .readable_id
        .as_deref()
        .or(conv.external_id.as_deref())
        .or(conv.title.as_deref())
        .map(sanitize_filename_segment)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("conversation-{:03}", index + 1));

    format!("{:03}_{stem}.{ext}", index + 1)
}

fn sanitize_filename_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('-');
            last_was_sep = true;
        }
    }

    out.trim_matches('-').to_string()
}

/// Parse a natural-language or ISO date string into a `DateTime<Utc>`.
///
/// Supports:
/// - ISO dates: "2026-03-01", "2026-03-01T10:00:00Z"
/// - Relative: "today", "yesterday", "N days ago", "N weeks ago", "N months ago"
/// - Without "ago": "1 week", "2 days", "3 hours"
/// - Compact: "1w", "2d", "3h", "30m", "2mo", "1y"
/// - Bare number: "2" (interpreted as N days ago)
/// - Named: "last week", "last month"
fn unit_to_duration(n: i64, unit: &str) -> Option<chrono::Duration> {
    match unit.trim_end_matches('s') {
        "m" | "min" | "minute" => Some(chrono::Duration::minutes(n)),
        "h" | "hr" | "hour" => Some(chrono::Duration::hours(n)),
        "d" | "day" => Some(chrono::Duration::days(n)),
        "w" | "wk" | "week" => Some(chrono::Duration::weeks(n)),
        "mo" | "mon" | "month" => Some(chrono::Duration::days(n * 30)),
        "y" | "yr" | "year" => Some(chrono::Duration::days(n * 365)),
        _ => None,
    }
}

fn parse_date_filter(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let lower = s.trim().to_lowercase();

    // Handle relative dates that dateparser doesn't support
    let now = chrono::Utc::now();
    let start_of_day =
        |dt: chrono::DateTime<chrono::Utc>| -> Result<chrono::DateTime<chrono::Utc>> {
            dt.date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|value| value.and_utc())
                .ok_or_else(|| anyhow::anyhow!("Could not compute start-of-day timestamp"))
        };

    if lower == "today" {
        return start_of_day(now);
    }
    if lower == "yesterday" {
        return start_of_day(now - chrono::Duration::days(1));
    }
    if lower == "last week" {
        return start_of_day(now - chrono::Duration::weeks(1));
    }
    if lower == "last month" {
        return start_of_day(now - chrono::Duration::days(30));
    }

    // "N days/weeks/months ago" or just "N days/weeks" (with or without "ago")
    let relative = lower.strip_suffix(" ago").unwrap_or(&lower);
    let parts: Vec<&str> = relative.split_whitespace().collect();
    if parts.len() == 2
        && let Ok(n) = parts[0].parse::<i64>()
        && let Some(d) = unit_to_duration(n, parts[1])
    {
        return Ok(now - d);
    }

    // Compact durations like "1w", "2d", "3h", "30m", "2mo", "1y"
    if let Some(idx) = lower.find(|c: char| c.is_alphabetic())
        && idx > 0
        && let Ok(n) = lower[..idx].parse::<i64>()
        && let Some(d) = unit_to_duration(n, &lower[idx..])
    {
        return Ok(now - d);
    }

    // Bare number → N days ago
    if let Ok(n) = lower.parse::<i64>() {
        return start_of_day(now - chrono::Duration::days(n));
    }

    // Fall back to dateparser for ISO dates and other formats
    dateparser::parse(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| anyhow::anyhow!("Could not parse date '{s}': {e}"))
}

/// Run interactive fzf picker to select a conversation
async fn run_fzf_picker(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    source_filter: Option<String>,
    workspace_filter: Option<String>,
    after: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    limit: i64,
    agent_override: Option<String>,
    dry_run: bool,
    allow_unverified: bool,
    json_output: bool,
) -> Result<()> {
    use hstry_core::db::ListConversationsOptions;
    use std::process::{Command, Stdio};

    // Fetch conversations with filters
    let workspace_filter_like = workspace_filter.as_ref().map(|v| format!("%{v}%"));
    let conversations = db
        .list_conversation_summaries(ListConversationsOptions {
            source_id: source_filter,
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

    // Build fzf input lines
    let mut lines: Vec<String> = Vec::new();
    let mut id_map: std::collections::HashMap<String, uuid::Uuid> =
        std::collections::HashMap::new();

    for cs in &conversations {
        let title = cs
            .conversation
            .title
            .as_deref()
            .or(cs.first_user_message.as_deref())
            .unwrap_or("(untitled)");
        let source = &cs.conversation.source_id;
        let date = cs.conversation.created_at.format("%Y-%m-%d %H:%M");
        let workspace = cs.conversation.workspace.as_deref().unwrap_or("");
        let ws_short = workspace
            .strip_prefix("/Users/")
            .and_then(|s| s.split_once('/').map(|(_, rest)| format!("~/{rest}")))
            .unwrap_or_else(|| workspace.to_string());
        let id_short = cs.conversation.id.to_string()[..8].to_string();

        // Format: "[source] date  workspace  title  (id)"
        let line = format!(
            "[{}] {}  {}  {}  ({})",
            source, date, ws_short, title, id_short
        );
        id_map.insert(line.clone(), cs.conversation.id);
        lines.push(line);
    }

    // Write to temp file for fzf
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join(format!("hstry_picker_{}", std::process::id()));
    std::fs::write(&temp_file, lines.join("\n"))?;

    // Open temp file for stdin redirection
    let temp_file_handle = std::fs::File::open(&temp_file)?;

    // Build fzf command with preview - read from stdin
    let fzf_output = Command::new("fzf")
        .args([
            "--height=80%",
            "--reverse",
            "--inline-info",
            "--bind=ctrl-z:ignore",
            "--preview-window=down:60%",
            r#"--preview=echo {} | grep -oP '\([0-9a-f]{8}\)' | tr -d '()' | xargs -I {} hstry show {} 2>/dev/null | head -20"#,
            "--prompt=Resume conversation> ",
        ])
        .stdin(Stdio::from(temp_file_handle))
        .stdout(Stdio::piped())
        .output()?;

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_file);

    if !fzf_output.status.success() {
        // User cancelled (ESC or ctrl-c)
        return Ok(());
    }

    // Parse selected line
    let selected = String::from_utf8_lossy(&fzf_output.stdout);
    let selected = selected.trim();

    if selected.is_empty() {
        return Ok(());
    }

    let selected_id = id_map
        .get(selected)
        .ok_or_else(|| anyhow::anyhow!("Could not find selected conversation"))?;

    // Dispatch through the same path as an explicitly supplied ID so picker
    // selection performs the conversion/direct resume and launches the agent.
    Box::pin(cmd_resume(
        db,
        runner,
        config,
        Some(selected_id.to_string()),
        None,
        agent_override,
        None,
        None,
        None,
        None,
        limit,
        dry_run,
        allow_unverified,
        false,
        json_output,
    ))
    .await
}
