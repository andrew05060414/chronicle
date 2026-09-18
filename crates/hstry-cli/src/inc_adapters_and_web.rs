fn copy_adapters_from(
    src_root: &Path,
    dest_root: &Path,
    filter: Option<&str>,
    force: bool,
) -> Result<Vec<String>> {
    let mut adapters = Vec::new();

    if !src_root.exists() {
        anyhow::bail!("Adapter source path not found: {}", src_root.display());
    }

    let manifest_path = src_root.join(".hstry-adapters.json");
    if !manifest_path.exists() {
        anyhow::bail!(
            "Adapter manifest missing at {}. Ensure the repo matches the hstry version.",
            manifest_path.display()
        );
    }

    let mut items = Vec::new();
    if let Some(adapter) = filter {
        items.push(adapter.to_string());
    } else {
        for entry in std::fs::read_dir(src_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                items.push(name.to_string());
            }
        }
    }

    let mut entries_to_copy = Vec::new();
    if !items.iter().any(|item| item == "types") {
        entries_to_copy.push("types".to_string());
    }
    entries_to_copy.extend(items);

    for entry_name in entries_to_copy {
        let src_path = src_root.join(&entry_name);
        if !src_path.exists() {
            if filter.is_some() && !force {
                anyhow::bail!("Adapter not found: {entry_name}");
            }
            continue;
        }

        let dest_path = dest_root.join(&entry_name);
        if dest_path.exists() {
            std::fs::remove_dir_all(&dest_path)?;
        }
        copy_dir_recursive(&src_path, &dest_path)?;

        if entry_name != "types" {
            adapters.push(entry_name);
        }
    }

    let dest_manifest = dest_root.join(".hstry-adapters.json");
    let manifest = adapter_manifest::AdapterManifest {
        hstry_version: adapter_manifest::expected_hstry_version(),
        protocol_version: adapter_manifest::ADAPTER_PROTOCOL_VERSION.to_string(),
    };
    std::fs::write(&dest_manifest, serde_json::to_string_pretty(&manifest)?)?;

    Ok(adapters)
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let rel_path = path.strip_prefix(src)?;
        let target_path = dest.join(rel_path);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target_path)?;
        } else {
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &target_path)?;
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct QuickstartSummary {
    sources_added: Vec<serde_json::Value>,
    sources_skipped: usize,
    sync: SyncSummary,
}

#[derive(Debug, Serialize)]
struct WebInstallResult {
    browser: String,
    command: String,
}

#[derive(Debug, Serialize)]
struct WebLoginResult {
    provider: String,
    storage_state: String,
}

#[derive(Debug, Serialize)]
struct WebSyncResult {
    provider: String,
    export_path: String,
    sources: SyncSummary,
}

#[derive(Debug, Serialize)]
struct WebStatusEntry {
    provider: String,
    logged_in: bool,
    last_sync: Option<String>,
    export_path: Option<String>,
}

async fn cmd_web(
    db: &Database,
    config: &Config,
    config_path: &Path,
    command: WebCommand,
    json: bool,
) -> Result<()> {
    match command {
        WebCommand::Install { browser } => cmd_web_install(&browser, json),
        WebCommand::Login {
            provider,
            headful,
            browser,
        } => cmd_web_login(&provider, headful, browser.as_deref(), json),
        WebCommand::Sync {
            provider,
            headful,
            browser,
        } => {
            cmd_web_sync(
                db,
                config,
                config_path,
                provider.as_deref(),
                headful,
                browser.as_deref(),
                json,
            )
            .await
        }
        WebCommand::Status => cmd_web_status(json),
    }
}

fn cmd_web_install(browser: &str, json: bool) -> Result<()> {
    let browser = browser.to_lowercase();
    let supported = ["chromium", "firefox", "webkit"];
    if !supported.contains(&browser.as_str()) {
        anyhow::bail!("Unsupported browser: {browser}");
    }

    let web_dir = web_runner_dir()?;
    ensure_web_runner(&web_dir)?;

    let bun_path = which::which("bun").map_err(|_| {
        anyhow::anyhow!("Bun is required to install Playwright. Install bun and retry.")
    })?;

    let mut install_cmd = ProcessCommand::new(bun_path);
    install_cmd.arg("install").current_dir(&web_dir);
    let output = install_cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Playwright install failed: {stderr}");
    }

    let (command, mut cmd) = if which::which("playwright").is_ok() {
        let mut cmd = ProcessCommand::new("playwright");
        cmd.arg("install").arg(&browser);
        ("playwright install".to_string(), cmd)
    } else if which::which("bunx").is_ok() {
        let mut cmd = ProcessCommand::new("bunx");
        cmd.arg("playwright").arg("install").arg(&browser);
        ("bunx playwright install".to_string(), cmd)
    } else if which::which("npx").is_ok() {
        let mut cmd = ProcessCommand::new("npx");
        cmd.arg("playwright").arg("install").arg(&browser);
        ("npx playwright install".to_string(), cmd)
    } else {
        anyhow::bail!("Playwright not found. Install bun or node, then retry.");
    };

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Playwright install failed: {stderr}");
    }

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(WebInstallResult {
                browser: browser.to_string(),
                command,
            }),
            error: None,
        });
    }

    println!("Playwright installed for {browser}.");
    Ok(())
}

fn cmd_web_login(provider: &str, headful: bool, browser: Option<&str>, json: bool) -> Result<()> {
    let provider = normalize_provider(provider)?;
    let web_dir = web_runner_dir()?;
    ensure_web_runner(&web_dir)?;

    let storage_state = web_sessions_dir()?.join(format!("{provider}.json"));
    let headful = if headful {
        true
    } else {
        if !json {
            println!("Login requires a visible browser. Launching headful session...");
        }
        true
    };

    let args = build_web_args("login", &provider, headful, browser, &storage_state, None);

    run_web_runner(&web_dir, &args)?;

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(WebLoginResult {
                provider,
                storage_state: storage_state.to_string_lossy().to_string(),
            }),
            error: None,
        });
    }

    println!("Logged in to {provider}.");
    Ok(())
}

async fn cmd_web_sync(
    db: &Database,
    config: &Config,
    config_path: &Path,
    provider: Option<&str>,
    headful: bool,
    browser: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut config = config.clone();
    let providers = normalize_provider_list(provider)?;

    let web_dir = web_runner_dir()?;
    ensure_web_runner(&web_dir)?;

    let mut results = Vec::new();

    let runtime = Runtime::parse(&config.js_runtime).ok_or_else(|| {
        anyhow::anyhow!("No JavaScript runtime found. Install bun, deno, or node.")
    })?;
    let runner = AdapterRunner::new(runtime, config.adapter_paths.clone());

    for provider in providers {
        let storage_state = web_sessions_dir()?.join(format!("{provider}.json"));
        if !storage_state.exists() {
            anyhow::bail!(
                "No login session found for {provider}. Run 'hstry web login {provider}'."
            );
        }

        let export_dir = web_exports_dir()?.join(&provider);
        let export_path = export_dir.join("conversations.json");

        let args = build_web_args(
            "sync",
            &provider,
            headful,
            browser,
            &storage_state,
            Some(&export_path),
        );
        run_web_runner(&web_dir, &args)?;

        let adapter = match provider.as_str() {
            "chatgpt" => "chatgpt",
            "claude" => "claude-web",
            "gemini" => "gemini",
            _ => "chatgpt",
        };

        let source_id = format!("web-{provider}");
        if !config.sources.iter().any(|s| s.id == source_id) {
            config.sources.push(hstry_core::config::SourceConfig {
                id: source_id.clone(),
                adapter: adapter.to_string(),
                path: export_path.to_string_lossy().to_string(),
                auto_sync: true,
            });
            config.save_to_path(config_path)?;
        }

        ensure_config_sources(db, &runner, &config).await?;
        let stats =
            sync_sources(db, &runner, &config, Some(source_id.clone()), None, !json).await?;

        let summary = SyncSummary {
            total_sources: stats.len(),
            total_conversations: stats.iter().map(|s| s.conversations).sum(),
            total_messages: stats.iter().map(|s| s.messages).sum(),
            sources: stats,
        };

        results.push(WebSyncResult {
            provider: provider.to_string(),
            export_path: export_path.to_string_lossy().to_string(),
            sources: summary,
        });
    }

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(results),
            error: None,
        });
    }

    for result in &results {
        println!(
            "Synced {provider} to {path}.",
            provider = result.provider,
            path = result.export_path
        );
    }

    Ok(())
}

fn cmd_web_status(json: bool) -> Result<()> {
    let providers = normalize_provider_list(None)?;
    let mut statuses = Vec::new();

    for provider in providers {
        let storage_state = web_sessions_dir()?.join(format!("{provider}.json"));
        let export_path = web_exports_dir()?
            .join(&provider)
            .join("conversations.json");
        let logged_in = storage_state.exists();
        let last_sync = if export_path.exists() {
            std::fs::metadata(&export_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
        } else {
            None
        };

        statuses.push(WebStatusEntry {
            provider,
            logged_in,
            last_sync,
            export_path: export_path
                .exists()
                .then(|| export_path.to_string_lossy().to_string()),
        });
    }

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(statuses),
            error: None,
        });
    }

    for status in &statuses {
        let login_status = if status.logged_in {
            "logged in"
        } else {
            "not logged in"
        };
        println!("{provider}: {login_status}", provider = status.provider);
        if let Some(last_sync) = &status.last_sync {
            println!("  Last sync: {last_sync}");
        }
    }

    Ok(())
}

fn web_runner_dir() -> Result<PathBuf> {
    let config_dir = xdg_config_dir();
    Ok(config_dir.join("hstry").join("web"))
}

fn web_sessions_dir() -> Result<PathBuf> {
    Ok(xdg_data_dir().join("hstry").join("web-sessions"))
}

fn web_exports_dir() -> Result<PathBuf> {
    Ok(xdg_data_dir().join("hstry").join("web-exports"))
}

fn ensure_web_runner(web_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(web_dir)?;

    let version_path = web_dir.join(".hstry-web-version");
    let expected_version = env!("CARGO_PKG_VERSION");
    let current_version = std::fs::read_to_string(&version_path)
        .ok()
        .map(|value| value.trim().to_string());

    if current_version.as_deref() != Some(expected_version) {
        let node_modules = web_dir.join("node_modules");
        if node_modules.exists() {
            std::fs::remove_dir_all(node_modules)?;
        }
    }

    let script_path = web_dir.join("web-runner.ts");
    let package_path = web_dir.join("package.json");

    std::fs::write(script_path, include_str!("../assets/web-runner.ts"))?;
    std::fs::write(package_path, include_str!("../assets/web-package.json"))?;
    std::fs::write(version_path, expected_version)?;

    Ok(())
}

fn build_web_args(
    command: &str,
    provider: &str,
    headful: bool,
    browser: Option<&str>,
    storage_state: &Path,
    output: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "web-runner.ts".to_string(),
        command.to_string(),
        "--provider".to_string(),
        provider.to_string(),
        "--storage-state".to_string(),
        storage_state.to_string_lossy().to_string(),
    ];

    if headful {
        args.push("--headful".to_string());
    }

    if let Some(browser) = browser {
        args.push("--browser".to_string());
        args.push(browser.to_string());
    }

    if let Some(output) = output {
        args.push("--output".to_string());
        args.push(output.to_string_lossy().to_string());
    }

    args
}

fn run_web_runner(web_dir: &Path, args: &[String]) -> Result<()> {
    let bun_path = which::which("bun")
        .map_err(|_| anyhow::anyhow!("Bun is required to run web automation."))?;

    let output = ProcessCommand::new(bun_path)
        .args(args)
        .current_dir(web_dir)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Web runner failed: {stderr}");
    }

    Ok(())
}

fn normalize_provider(provider: &str) -> Result<String> {
    let provider = provider.to_lowercase();
    match provider.as_str() {
        "chatgpt" | "claude" | "gemini" => Ok(provider),
        _ => anyhow::bail!("Unsupported provider: {provider}"),
    }
}

fn normalize_provider_list(provider: Option<&str>) -> Result<Vec<String>> {
    if let Some(provider) = provider {
        return Ok(vec![normalize_provider(provider)?]);
    }

    Ok(vec![
        "chatgpt".to_string(),
        "claude".to_string(),
        "gemini".to_string(),
    ])
}

fn xdg_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    if cfg!(unix) {
        dirs::home_dir().map_or_else(|| PathBuf::from("."), |h| h.join(".config"))
    } else {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    }
}

fn xdg_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    if cfg!(unix) {
        dirs::home_dir().map_or_else(|| PathBuf::from("."), |h| h.join(".local").join("share"))
    } else {
        dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
    }
}

async fn cmd_quickstart(
    db: &Database,
    runner: &AdapterRunner,
    config: &Config,
    config_path: &Path,
    json: bool,
) -> Result<()> {
    let mut config = config.clone();

    if adapter_manifest::validate_adapter_manifest(&config.adapter_paths).is_err() {
        ensure_adapter_updates(&mut config, config_path, json)?;
    }

    let hits = scan_hits(runner, &config).await?;
    if hits.is_empty() {
        if json {
            return emit_json(JsonResponse {
                ok: true,
                result: Some(QuickstartSummary {
                    sources_added: Vec::new(),
                    sources_skipped: 0,
                    sync: SyncSummary {
                        sources: Vec::new(),
                        total_sources: 0,
                        total_conversations: 0,
                        total_messages: 0,
                    },
                }),
                error: None,
            });
        }
        println!("No sources detected.");
        return Ok(());
    }

    let mut sources_added = Vec::new();
    let mut sources_skipped = 0usize;

    // trx-gzfh: validate each hit before mutating config.toml so a bad
    // scan result doesn't poison the on-disk config file.
    let canonical_roots = resolve_canonical_roots(runner).await;

    for hit in &hits {
        if config
            .sources
            .iter()
            .any(|source| source.adapter == hit.adapter && source.path == hit.path)
        {
            sources_skipped += 1;
            continue;
        }

        if let Ok(Some(_)) = db.get_source_by_adapter_path(&hit.adapter, &hit.path).await {
            sources_skipped += 1;
            continue;
        }

        let source_id = generate_source_id(&config, &hit.adapter);

        // The source-registration chokepoint. Skip the hit (don't write
        // it to config.toml) if validation fails — scan is supposed to be
        // best-effort, not load-bearing.
        let existing_sources = db.list_sources().await?;
        if let Err(e) = hstry_core::source_registry::validate_new_source(
            &hit.adapter,
            &hit.path,
            source_id.clone(),
            serde_json::Value::Object(serde_json::Map::default()),
            &canonical_roots,
            &existing_sources,
            |p| p.is_dir(),
        ) {
            tracing::debug!(
                "quickstart skipped hit {adapter}:{path}: {e}",
                adapter = hit.adapter,
                path = hit.path
            );
            sources_skipped += 1;
            continue;
        }

        config.sources.push(hstry_core::config::SourceConfig {
            id: source_id.clone(),
            adapter: hit.adapter.clone(),
            path: hit.path.clone(),
            auto_sync: true,
        });

        sources_added.push(serde_json::json!({
            "id": source_id,
            "adapter": hit.adapter,
            "path": hit.path,
        }));
    }

    if !sources_added.is_empty() {
        config.save_to_path(config_path)?;
    }

    ensure_config_sources(db, runner, &config).await?;

    let stats = sync_sources(db, runner, &config, None, None, !json).await?;
    let sync_summary = SyncSummary {
        total_sources: stats.len(),
        total_conversations: stats.iter().map(|s| s.conversations).sum(),
        total_messages: stats.iter().map(|s| s.messages).sum(),
        sources: stats,
    };

    if json {
        return emit_json(JsonResponse {
            ok: true,
            result: Some(QuickstartSummary {
                sources_added,
                sources_skipped,
                sync: sync_summary,
            }),
            error: None,
        });
    }

    println!(
        "Quickstart added {} sources (skipped {}).",
        sources_added.len(),
        sources_skipped
    );
    Ok(())
}

fn generate_source_id(config: &Config, adapter: &str) -> String {
    let uuid = uuid::Uuid::new_v4().to_string();
    let short = uuid.split('-').next().unwrap_or(uuid.as_str());
    let mut candidate = format!("{adapter}-{short}");

    let mut idx = 1u32;
    while config.sources.iter().any(|s| s.id == candidate) {
        candidate = format!("{adapter}-{short}-{idx}");
        idx += 1;
    }

    candidate
}

fn ensure_adapter_updates(config: &mut Config, config_path: &Path, json: bool) -> Result<()> {
    let expected_ref = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut repos_to_update: Vec<_> = config
        .adapter_repos
        .iter()
        .filter(|r| r.enabled)
        .cloned()
        .collect();

    if repos_to_update.is_empty() {
        anyhow::bail!("No enabled adapter repositories. Run 'hstry adapters repo add-git'.");
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
        for r in &mut config.adapter_repos {
            if let AdapterRepoSource::Git { url, git_ref, .. } = &mut r.source
                && official_adapter_ref_needs_update(url, git_ref, &expected_ref)
            {
                *git_ref = expected_ref.clone();
            }
        }
        config.save_to_path(config_path)?;
    }

    let adapter_root = adapter_root_dir(config)?;
    std::fs::create_dir_all(&adapter_root)?;

    let mut updated_repos = Vec::new();
    for repo in &repos_to_update {
        let repo_result = update_repo_adapters(repo, &adapter_root, None, false)?;
        updated_repos.push(repo_result);
    }

    adapter_manifest::validate_adapter_manifest(&config.adapter_paths)?;

    if !json {
        println!("Updated adapters in {}", adapter_root.display());
        for repo_result in &updated_repos {
            println!(
                "  {name}: {count} adapters",
                name = repo_result.name,
                count = repo_result.adapters.len()
            );
        }
    }

    Ok(())
}

fn cmd_adapter_repo(
    config: &mut Config,
    config_path: &Path,
    command: AdapterRepoCommand,
    json: bool,
) -> Result<()> {
    match command {
        AdapterRepoCommand::List => {
            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(&config.adapter_repos),
                    error: None,
                });
            }
            if config.adapter_repos.is_empty() {
                println!("No adapter repositories configured.");
            } else {
                println!("Adapter repositories:");
                for repo in &config.adapter_repos {
                    let status = if repo.enabled { "enabled" } else { "disabled" };
                    let source_info = match &repo.source {
                        AdapterRepoSource::Git { url, git_ref, path } => {
                            format!("git {url} ({git_ref}) path={path}")
                        }
                        AdapterRepoSource::Archive { url, path } => {
                            format!("archive {url} path={path}")
                        }
                        AdapterRepoSource::Local { path } => format!("local {path}"),
                    };
                    println!("  {name} ({status}) - {source_info}", name = repo.name);
                }
            }
        }
        AdapterRepoCommand::AddGit {
            name,
            url,
            git_ref,
            path,
        } => {
            // Check if repo with this name already exists
            if config.adapter_repos.iter().any(|r| r.name == name) {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' already exists")),
                    });
                }
                anyhow::bail!("Repository '{name}' already exists");
            }

            let repo = AdapterRepo {
                name: name.clone(),
                source: AdapterRepoSource::Git { url, git_ref, path },
                enabled: true,
            };
            config.adapter_repos.push(repo.clone());
            config.save_to_path(config_path)?;

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(repo),
                    error: None,
                });
            }
            println!("Added git repository: {name}");
        }
        AdapterRepoCommand::AddArchive { name, url, path } => {
            if config.adapter_repos.iter().any(|r| r.name == name) {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' already exists")),
                    });
                }
                anyhow::bail!("Repository '{name}' already exists");
            }

            let repo = AdapterRepo {
                name: name.clone(),
                source: AdapterRepoSource::Archive { url, path },
                enabled: true,
            };
            config.adapter_repos.push(repo.clone());
            config.save_to_path(config_path)?;

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(repo),
                    error: None,
                });
            }
            println!("Added archive repository: {name}");
        }
        AdapterRepoCommand::AddLocal { name, path } => {
            if config.adapter_repos.iter().any(|r| r.name == name) {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' already exists")),
                    });
                }
                anyhow::bail!("Repository '{name}' already exists");
            }

            let repo = AdapterRepo {
                name: name.clone(),
                source: AdapterRepoSource::Local {
                    path: path.to_string_lossy().to_string(),
                },
                enabled: true,
            };
            config.adapter_repos.push(repo.clone());
            config.save_to_path(config_path)?;

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(repo),
                    error: None,
                });
            }
            println!("Added local repository: {name}");
        }
        AdapterRepoCommand::Remove { name } => {
            let original_len = config.adapter_repos.len();
            config.adapter_repos.retain(|r| r.name != name);

            if config.adapter_repos.len() == original_len {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' not found")),
                    });
                }
                anyhow::bail!("Repository '{name}' not found");
            }

            config.save_to_path(config_path)?;

            if json {
                return emit_json(JsonResponse {
                    ok: true,
                    result: Some(serde_json::json!({ "removed": name })),
                    error: None,
                });
            }
            println!("Removed repository: {name}");
        }
        AdapterRepoCommand::Enable { name } => {
            let repo = config.adapter_repos.iter_mut().find(|r| r.name == name);
            if let Some(repo) = repo {
                repo.enabled = true;
                config.save_to_path(config_path)?;

                if json {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({ "name": name, "enabled": true })),
                        error: None,
                    });
                }
                println!("Enabled repository: {name}");
            } else {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' not found")),
                    });
                }
                anyhow::bail!("Repository '{name}' not found");
            }
        }
        AdapterRepoCommand::Disable { name } => {
            let repo = config.adapter_repos.iter_mut().find(|r| r.name == name);
            if let Some(repo) = repo {
                repo.enabled = false;
                config.save_to_path(config_path)?;

                if json {
                    return emit_json(JsonResponse {
                        ok: true,
                        result: Some(serde_json::json!({ "name": name, "enabled": false })),
                        error: None,
                    });
                }
                println!("Disabled repository: {name}");
            } else {
                if json {
                    return emit_json(JsonResponse::<()> {
                        ok: false,
                        result: None,
                        error: Some(format!("Repository '{name}' not found")),
                    });
                }
                anyhow::bail!("Repository '{name}' not found");
            }
        }
    }

    Ok(())
}

/// Resolve canonical roots for every available adapter by asking each adapter
/// for its `defaultPaths` and expanding `~`/env-vars. Used by the
/// source-registration chokepoint (trx-gzfh) to enforce the territory rule.
///
/// Adapters that fail to load or return errors are simply omitted — the
/// invariant degrades to "no allowed roots" for that adapter, which means
/// registration will refuse anything (fail safe).
async fn resolve_canonical_roots(
    runner: &AdapterRunner,
) -> hstry_core::source_registry::CanonicalRoots {
    let mut roots: hstry_core::source_registry::CanonicalRoots = HashMap::new();
    for adapter_name in runner.list_adapters() {
        let Some(adapter_path) = runner.find_adapter(&adapter_name) else {
            continue;
        };
        let Ok(info) = runner.get_info(&adapter_path).await else {
            continue;
        };
        let expanded: Vec<PathBuf> = info
            .default_paths
            .iter()
            .map(|p| {
                let path = hstry_core::Config::expand_path(p);
                PathBuf::from(path.to_string_lossy().trim_end_matches('/').to_string())
            })
            .collect();
        roots.insert(adapter_name, expanded);
    }
    roots
}

async fn scan_hits(runner: &AdapterRunner, config: &Config) -> Result<Vec<ScanHit>> {
    let mut hits = Vec::new();
    for adapter_name in runner.list_adapters() {
        if !config.adapter_enabled(&adapter_name) {
            continue;
        }
        if let Some(adapter_path) = runner.find_adapter(&adapter_name)
            && let Ok(info) = runner.get_info(&adapter_path).await
        {
            for default_path in &info.default_paths {
                let expanded = hstry_core::Config::expand_path(default_path);
                if expanded.exists()
                    && let Ok(Some(confidence)) = runner
                        .detect(&adapter_path, &expanded.to_string_lossy())
                        .await
                    && confidence > 0.5
                {
                    hits.push(ScanHit {
                        adapter: adapter_name.clone(),
                        display_name: info.display_name.clone(),
                        path: expanded.to_string_lossy().to_string(),
                        confidence,
                    });
                }
            }
        }
    }

    Ok(hits)
}
