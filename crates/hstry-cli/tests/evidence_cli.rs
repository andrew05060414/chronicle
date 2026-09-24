use hstry_core::{Config, Database, config::SyncMode, ingest::ingest_batch, models::Source};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

async fn fixture() -> anyhow::Result<(tempfile::TempDir, PathBuf, String)> {
    let dir = tempfile::tempdir()?;
    hstry_core::test_guard::assert_test_path_safe(&dir.path().join("history.db"));
    let mut config = Config {
        database: dir.path().join("history.db"),
        ..Default::default()
    };
    config.adapter_paths = vec![
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../adapters")
            .canonicalize()?,
    ];
    config.resume.agents.get_mut("pi").unwrap().session_dir = dir
        .path()
        .join("sessions with spaces")
        .to_string_lossy()
        .into_owned();
    let db = Database::open(&config.database).await?;
    db.upsert_source(&Source {
        id: "test".into(),
        adapter: "codex".into(),
        path: None,
        last_sync_at: None,
        config: json!({}),
    })
    .await?;
    let conv = serde_json::from_value(
        json!({"externalId":"foreign-id","workspace":dir.path(),"createdAt":1767225600000_i64,"messages":[{"role":"user","content":"fixture request"},{"role":"assistant","content":"fixture answer"},{"role":"tool","content":"x".repeat(15000)}]}),
    )?;
    ingest_batch(&db, "test", vec![conv]).await?;
    let id = db.list_conversations(Default::default()).await?[0]
        .id
        .to_string();
    config.remotes.push(serde_json::from_value(
        json!({"name":"peer","host":"fixture-peer"}),
    )?);
    let path = dir.path().join("config.toml");
    fs::write(&path, toml::to_string(&config)?)?;
    db.close().await;
    Ok((dir, path, id))
}
fn cmd(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hstry"));
    command
        .arg("--config")
        .arg(config)
        .env("HSTRY_NO_SERVICE", "1")
        .env("HSTRY_NO_API", "1");
    command
}
fn output(mut command: Command) -> anyhow::Result<Value> {
    let r = command.output()?;
    anyhow::ensure!(
        r.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    Ok(serde_json::from_slice(&r.stdout)?)
}
#[tokio::test]
async fn cli_reads_are_bounded_and_full_requires_opt_in() -> anyhow::Result<()> {
    let (_dir, config, id) = fixture().await?;
    let mut c = cmd(&config);
    c.args([
        "read",
        &id,
        "--message-idx",
        "2",
        "--max-chars",
        "1100",
        "--json",
    ]);
    let page = output(c)?;
    assert!(page.to_string().chars().count() < 1100);
    assert!(page["result"]["records"][0]["next_offset_chars"].is_number());
    let mut c = cmd(&config);
    c.args(["show", &id, "--json"]);
    let page = output(c)?;
    assert!(page.to_string().chars().count() < 3000);
    let mut c = cmd(&config);
    c.args(["show", &id, "--json", "--full"]);
    assert_eq!(
        output(c)?["result"]["messages"].as_array().unwrap().len(),
        3
    );
    Ok(())
}
#[tokio::test]
async fn trace_files_exclude_queries_identifiers_and_payloads() -> anyhow::Result<()> {
    let (dir, config, id) = fixture().await?;
    let trace = dir.path().join("trace.json");
    let mut c = cmd(&config);
    c.args(["search", "fixture request", "--json", "--trace-file"])
        .arg(&trace);
    output(c)?;
    let text = fs::read_to_string(&trace)?;
    assert!(!text.contains("fixture"));
    assert!(!text.contains(&id));
    let value: Value = serde_json::from_str(&text)?;
    assert!(value["returned_hits"].as_u64().unwrap() > 0);
    assert!(value["ranks"][0].get("score").is_some());
    Ok(())
}

#[tokio::test]
async fn local_search_falls_back_when_service_is_unavailable() -> anyhow::Result<()> {
    let (dir, config, _id) = fixture().await?;
    let mut parsed: Config = toml::from_str(&fs::read_to_string(&config)?)?;
    parsed.service.enabled = true;
    parsed.service.search_api = true;
    let service_config = dir.path().join("service-enabled.toml");
    fs::write(&service_config, toml::to_string(&parsed)?)?;

    let mut c = cmd(&service_config);
    c.args(["search", "fixture request", "--json"])
        .env_remove("HSTRY_NO_SERVICE")
        .env("HSTRY_SERVICE_PORT", "1")
        .env("HOME", dir.path().join("isolated-home"))
        .env("XDG_CONFIG_HOME", dir.path().join("isolated-config"))
        .env("XDG_STATE_HOME", dir.path().join("isolated-state"))
        .env("XDG_RUNTIME_DIR", dir.path().join("isolated-runtime"));

    let page = output(c)?;
    assert!(!page["result"]["hits"].as_array().unwrap().is_empty());
    assert!(
        page["result"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| {
                w.as_str().is_some_and(|s| {
                    s.contains("Search service unavailable; used the local archive")
                })
            })
    );
    Ok(())
}

#[tokio::test]
async fn local_search_does_not_create_a_missing_archive() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("missing.db");
    hstry_core::test_guard::assert_test_path_safe(&database);
    let config = Config {
        database: database.clone(),
        ..Default::default()
    };
    let config_path = dir.path().join("config.toml");
    fs::write(&config_path, toml::to_string(&config)?)?;

    let mut c = cmd(&config_path);
    c.args(["search", "anything", "--json"]);
    let result = c.output()?;
    anyhow::ensure!(
        !result.status.success(),
        "search must fail when the configured archive is missing"
    );
    anyhow::ensure!(
        !database.exists(),
        "a read-only search must not create a missing archive"
    );
    Ok(())
}

#[tokio::test]
async fn resume_json_is_non_destructive_and_uses_a_new_target_id() -> anyhow::Result<()> {
    let (dir, config, id) = fixture().await?;
    let mut c = cmd(&config);
    c.args(["resume", &id, "--agent", "pi", "--json"]);
    let page = output(c)?;
    assert_eq!(page["result"]["launched"], false);
    assert_eq!(page["result"]["native_verification"], "not_checked");
    assert_ne!(page["result"]["target_session_id"], "foreign-id");
    assert_eq!(page["result"]["argv"].as_array().unwrap().len(), 3);
    let mut c = cmd(&config);
    c.args(["resume", &id, "--agent", "pi"]);
    assert!(!c.output()?.status.success());
    assert!(!dir.path().join("sessions with spaces").exists());
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn ssh_reads_enforce_peer_side_budgets_and_reject_old_peers() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (dir, config, id) = fixture().await?;
    let bin = dir.path().join("bin");
    fs::create_dir(&bin)?;
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nexec \"$HSTRY_PEER_BIN\" --config \"$HSTRY_PEER_CONFIG\" read --json --input -\n",
    )?;
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755))?;
    let paths = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    let mut c = cmd(&config);
    c.args([
        "read",
        &id,
        "--remote",
        "peer",
        "--message-idx",
        "2",
        "--max-chars",
        "1100",
    ])
    .env("PATH", &paths)
    .env("HSTRY_PEER_BIN", env!("CARGO_BIN_EXE_hstry"))
    .env("HSTRY_PEER_CONFIG", &config);
    let r = output(c)?;
    assert!(r.to_string().chars().count() < 1100);
    assert_eq!(r["result"]["protocol"], 1);
    assert_eq!(r["result"]["machine"], "peer");
    fs::write(
        &ssh,
        "#!/bin/sh\nprintf '{\"ok\":true,\"result\":{\"messages\":[]}}'\n",
    )?;
    let mut c = cmd(&config);
    c.args(["read", &id, "--remote", "peer"])
        .env("PATH", &paths);
    assert!(!c.output()?.status.success());
    Ok(())
}

#[tokio::test]
async fn checkpoint_restore_live_closes_handles_and_creates_safety_snapshot() -> anyhow::Result<()>
{
    let (dir, config_path, _id) = fixture().await?;
    let mut config: hstry_core::Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
    config.checkpoint.enabled = true;
    let chk_dir = dir.path().join("checkpoints");
    config.checkpoint.dir = Some(chk_dir.clone());
    fs::write(&config_path, toml::to_string(&config)?)?;

    // Create an initial checkpoint
    let mut c = cmd(&config_path);
    c.args(["checkpoint", "create", "--json"]);
    let out = output(c)?;
    assert_eq!(out["ok"], true);
    let stem = out["result"]["stem"].as_str().unwrap().to_string();

    // Verify initial count is 1 conversation
    let db = Database::open(&config.database).await?;
    let convs_before = db.list_conversations(Default::default()).await?;
    assert_eq!(convs_before.len(), 1);

    // Ingest a second conversation into the live database
    let conv2 = serde_json::from_value(
        json!({"externalId":"second-conv","createdAt":1767225700000_i64,"messages":[{"role":"user","content":"second query"}]}),
    )?;
    ingest_batch(&db, "test", vec![conv2]).await?;
    let convs_after = db.list_conversations(Default::default()).await?;
    assert_eq!(convs_after.len(), 2);
    db.close().await;

    // Perform live restore of the first checkpoint
    let mut c = cmd(&config_path);
    c.args(["checkpoint", "restore", &stem, "--live", "--json"]);
    let restore_out = output(c)?;
    assert_eq!(restore_out["ok"], true);
    assert_eq!(restore_out["result"]["live"], true);

    // Verify live DB was restored back to 1 conversation and passes integrity check
    let db = Database::open(&config.database).await?;
    assert_eq!(db.integrity_check().await?, "ok");
    let convs_restored = db.list_conversations(Default::default()).await?;
    assert_eq!(convs_restored.len(), 1);
    assert_eq!(
        convs_restored[0].id.to_string(),
        convs_before[0].id.to_string()
    );
    assert_eq!(convs_restored[0].external_id.as_deref(), Some("foreign-id"));
    db.close().await;

    // Verify that the pre-restore safety snapshot was created in checkpoints directory (total 2 checkpoints)
    let mut c = cmd(&config_path);
    c.args(["checkpoint", "list", "--json"]);
    let list_out = output(c)?;
    assert_eq!(list_out["ok"], true);
    let manifests = list_out["result"].as_array().unwrap();
    assert_eq!(
        manifests.len(),
        2,
        "must retain initial checkpoint and safety checkpoint"
    );
    let conv_counts: Vec<u64> = manifests
        .iter()
        .map(|m| m["conversations"].as_u64().unwrap())
        .collect();
    assert!(
        conv_counts.contains(&1) && conv_counts.contains(&2),
        "manifests must record initial (1 conv) and pre-restore safety snapshot (2 convs)"
    );

    Ok(())
}

#[tokio::test]
async fn checkpoint_restore_refuses_staging_db() -> anyhow::Result<()> {
    let (dir, config_path, _id) = fixture().await?;
    let mut config: hstry_core::Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
    let staging_path = dir.path().join("staging.db");
    config.database = staging_path.clone();
    fs::write(&config_path, toml::to_string(&config)?)?;

    let mut c = cmd(&config_path);
    c.args(["checkpoint", "restore", "dummy-stem", "--live"]);
    let res = c.output()?;
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("refusing to restore a checkpoint onto staging.db"),
        "stderr was: {stderr}"
    );
    Ok(())
}

/// Scope-all must still surface local results when no remote is enabled; only `--scope remote` has nothing left to return.
#[tokio::test]
async fn scope_all_keeps_local_hits_when_no_remote_is_enabled() -> anyhow::Result<()> {
    let (dir, config, _id) = fixture().await?;
    let mut parsed: Config = toml::from_str(&fs::read_to_string(&config)?)?;
    for remote in &mut parsed.remotes {
        remote.enabled = false;
    }
    assert!(!parsed.remotes.is_empty());
    let disabled = dir.path().join("config-no-enabled-remotes.toml");
    fs::write(&disabled, toml::to_string(&parsed)?)?;

    let mut c = cmd(&disabled);
    c.args(["search", "fixture request", "--scope", "all", "--json"]);
    let page = output(c)?;
    assert_eq!(page["result"]["scope"], "local_snapshot_and_remote");
    assert!(!page["result"]["hits"].as_array().unwrap().is_empty());
    assert!(
        page["result"]["available_remotes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let mut c = cmd(&disabled);
    c.args(["search", "fixture request", "--scope", "remote", "--json"]);
    let remote_only = c.output()?;
    assert!(!remote_only.status.success());
    assert!(String::from_utf8_lossy(&remote_only.stderr).contains("No enabled remotes to search"));
    Ok(())
}

/// Scripts that pass the restored global `--no-color` must not die on clap.
#[test]
fn global_no_color_is_accepted() {
    let help = Command::new(env!("CARGO_BIN_EXE_hstry"))
        .args(["--no-color", "--help"])
        .output()
        .expect("spawn hstry --no-color --help");
    assert!(
        help.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    let stderr = String::from_utf8_lossy(&help.stderr);
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        !stderr.contains("unexpected argument") && !stdout.contains("unexpected argument"),
        "clap rejected --no-color: stdout={stdout} stderr={stderr}"
    );
}

#[tokio::test]
async fn global_no_color_search_does_not_die() -> anyhow::Result<()> {
    let (_dir, config, _id) = fixture().await?;
    let mut c = cmd(&config);
    c.args(["--no-color", "search", "fixture request", "--json"]);
    let page = output(c)?;
    assert!(!page["result"]["hits"].as_array().unwrap().is_empty());
    Ok(())
}

fn write_satellite_config(
    dir: &tempfile::TempDir,
    mut parsed: Config,
    host: &str,
    port: Option<u16>,
    enabled: bool,
) -> anyhow::Result<PathBuf> {
    parsed.sync.mode = SyncMode::Satellite;
    parsed.sync.hub_remote = Some("peer".into());
    for remote in &mut parsed.remotes {
        if remote.name == "peer" {
            remote.host = host.into();
            remote.port = port;
            remote.enabled = enabled;
        }
    }
    let path = dir.path().join("config-satellite.toml");
    fs::write(&path, toml::to_string(&parsed)?)?;
    Ok(path)
}

/// Satellite default search (no `--scope`) must read the local DB when the hub is disabled.
/// Explicit `--scope remote` on the same config still fails loudly (PR #28).
#[tokio::test]
async fn satellite_default_search_reads_local_when_hub_disabled() -> anyhow::Result<()> {
    let (dir, config, _id) = fixture().await?;
    let parsed: Config = toml::from_str(&fs::read_to_string(&config)?)?;
    let satellite = write_satellite_config(&dir, parsed, "fixture-peer", None, false)?;

    let mut c = cmd(&satellite);
    c.args(["search", "fixture request", "--json"]);
    let page = output(c)?;
    assert_eq!(page["result"]["scope"], "local_snapshot");
    assert!(!page["result"]["hits"].as_array().unwrap().is_empty());
    let warnings = page["result"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .is_some_and(|s| s.contains("No enabled remotes to search"))),
        "expected a local-fallback warning, got {warnings:?}"
    );
    assert!(
        warnings.iter().all(|w| w
            .as_str()
            .is_none_or(|s| !s.contains("Multi-store discovery"))),
        "local-only fallback must not keep remote multi-store warnings, got {warnings:?}"
    );

    let mut c = cmd(&satellite);
    c.args(["search", "fixture request", "--scope", "remote", "--json"]);
    let remote_only = c.output()?;
    assert!(!remote_only.status.success());
    assert!(String::from_utf8_lossy(&remote_only.stderr).contains("No enabled remotes to search"));

    let mut c = cmd(&satellite);
    c.args(["search", "fixture request", "--remote", "peer", "--json"]);
    let named_remote = c.output()?;
    assert!(!named_remote.status.success());
    let named_err = String::from_utf8_lossy(&named_remote.stderr);
    assert!(
        named_err.contains("Unknown or disabled remote")
            || named_err.contains("No enabled remotes to search"),
        "named --remote must fail loudly, got {named_err}"
    );
    Ok(())
}

/// Hub unreachable must not discard local hits on satellite default search.
#[cfg(unix)]
#[tokio::test]
async fn satellite_default_search_keeps_local_when_hub_unreachable() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (dir, config, _id) = fixture().await?;
    let parsed: Config = toml::from_str(&fs::read_to_string(&config)?)?;
    let satellite = write_satellite_config(&dir, parsed, "127.0.0.1", Some(1), true)?;

    let bin = dir.path().join("bin");
    fs::create_dir(&bin)?;
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\necho 'ssh: connection refused' >&2\nexit 255\n",
    )?;
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755))?;
    let paths = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;

    let mut c = cmd(&satellite);
    c.args([
        "search",
        "fixture request",
        "--json",
        "--max-chars",
        "100000",
    ])
    .env("PATH", &paths);
    let page = output(c)?;
    assert_eq!(page["result"]["scope"], "local_snapshot");
    assert!(!page["result"]["hits"].as_array().unwrap().is_empty());
    let warnings = page["result"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .is_some_and(|s| s.contains("Remote search failed"))),
        "expected a remote-failure warning, got {warnings:?}"
    );
    assert!(
        warnings.iter().all(|w| w
            .as_str()
            .is_none_or(|s| !s.contains("Multi-store discovery"))),
        "local-only fallback must not keep remote multi-store warnings, got {warnings:?}"
    );

    let mut c = cmd(&satellite);
    c.args([
        "search",
        "fixture request",
        "--scope",
        "all",
        "--json",
        "--max-chars",
        "100000",
    ])
    .env("PATH", &paths);
    let all_scope = c.output()?;
    assert!(
        !all_scope.status.success(),
        "explicit --scope all must not swallow remote errors: stdout={} stderr={}",
        String::from_utf8_lossy(&all_scope.stdout),
        String::from_utf8_lossy(&all_scope.stderr)
    );
    Ok(())
}
