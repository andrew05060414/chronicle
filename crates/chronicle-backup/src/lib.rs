//! Native Chronicle backup.
//!
//! Search-database availability never gates these commands. Capture copies
//! only changed bytes into an immutable local snapshot, then Restic stores
//! versions. Credentials and unknown host versions are fail-closed.

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fs4::fs_std::FileExt;
use notify::{EventKind, RecursiveMode, Watcher};
use rusqlite::{Connection, OpenFlags, backup::Backup};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

mod codex;
mod cursor;

const VERSION: u32 = 1;

#[derive(Debug, Parser)]
#[command(name = "chronicle-native")]
pub struct NativeArgs {
    #[arg(long, global = true)]
    pub native_config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    #[arg(long, global = true)]
    pub home: Option<PathBuf>,
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: NativeCommand,
}

#[derive(Debug, Subcommand)]
pub enum NativeCommand {
    Discover(AppArgs),
    Capture(CaptureArgs),
    Watch(WatchArgs),
    Status,
    Snapshots,
    Replicate(ReplicateArgs),
    Verify { snapshot_id: String },
    Restore(RestoreArgs),
}

#[derive(Debug, Args, Default)]
pub struct AppArgs {
    #[arg(long)]
    pub app: Option<String>,
    #[arg(long, value_enum)]
    pub components: Vec<ComponentKind>,
}

#[derive(Debug, Args, Default)]
pub struct CaptureArgs {
    #[command(flatten)]
    pub app: AppArgs,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Args, Default)]
pub struct ReplicateArgs {
    pub snapshot_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    pub snapshot_id: Option<String>,
    #[arg(long)]
    pub target: Option<PathBuf>,
    #[arg(long)]
    pub into: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub apply_plan: Option<PathBuf>,
    #[arg(long)]
    pub map: Vec<String>,
    #[arg(long)]
    pub force: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ComponentKind {
    Sessions,
    Settings,
    Skills,
    Plugins,
    Projects,
    Integrations,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NativeConfig {
    pub data_root: Option<PathBuf>,
    pub restic: Option<PathBuf>,
    pub password_file: Option<PathBuf>,
    pub local_repository: Option<String>,
    pub remote_repository: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub components: Vec<ComponentKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub app: String,
    pub component: ComponentKind,
    pub slot: String,
    pub path: PathBuf,
    #[serde(default)]
    pub host_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    relative: String,
    source: String,
    bytes: u64,
    sha256: String,
    consistency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    snapshot_id: String,
    device: String,
    app_instance: String,
    captured_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    sources: Vec<SourceConfig>,
    files: Vec<FileEntry>,
    exclusions: Vec<String>,
    unsupported: Vec<String>,
    incremental: bool,
    local_restic_snapshot: Option<String>,
    remote_restic_snapshot: Option<String>,
    remote_confirmed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExtractPlan {
    kind: ExtractPlanKind,
    snapshot_id: String,
    target: PathBuf,
    target_fingerprint: String,
    files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtractPlanKind {
    Extract,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallPlan {
    kind: InstallPlanKind,
    version: u32,
    snapshot_id: String,
    app: String,
    host_version: String,
    target_fingerprint: String,
    actions: Vec<InstallAction>,
    path_map: BTreeMap<String, PathBuf>,
    rollback_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InstallPlanKind {
    Install,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallAction {
    kind: InstallActionKind,
    source: String,
    target: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InstallActionKind {
    Copy,
    CopyIfMissing,
    CursorSessionMerge,
    CodexIndexMerge,
    SessionIndexMerge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallJournal {
    snapshot_id: String,
    phase: String,
    completed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct Fingerprint {
    files: BTreeMap<String, (u64, String)>,
}

pub fn run(args: NativeArgs) -> Result<()> {
    let config = load_config(&args)?;
    let root = config.data_root.clone().unwrap_or_else(default_root);
    validate_store(&root)?;
    fs::create_dir_all(root.join("snapshots"))?;
    let output = match args.command {
        NativeCommand::Discover(app) => {
            serde_json::json!({"sources": selected_sources(&config, &app, args.home.as_deref())?})
        }
        NativeCommand::Capture(capture) => {
            capture_sources(&config, &root, &capture.app, args.home.as_deref())?
        }
        NativeCommand::Watch(watch) => {
            if watch.once {
                capture_sources(&config, &root, &AppArgs::default(), args.home.as_deref())?
            } else {
                watch_sources(&config, &root, &AppArgs::default(), args.home.as_deref())?
            }
        }
        NativeCommand::Status => status(&root)?,
        NativeCommand::Snapshots => serde_json::json!({"snapshots": list_manifests(&root)?}),
        NativeCommand::Replicate(replicate_args) => {
            replicate(&config, &root, replicate_args.snapshot_id.as_deref())?
        }
        NativeCommand::Verify { snapshot_id } => {
            verify_local(&root, &snapshot_id)?;
            serde_json::json!({"verified": snapshot_id})
        }
        NativeCommand::Restore(restore) => restore_snapshot(&root, restore, args.home.as_deref())?,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"ok": true, "result": output}))?
    );
    Ok(())
}

fn load_config(args: &NativeArgs) -> Result<NativeConfig> {
    let mut config = if let Some(path) = &args.native_config {
        toml::from_str(&fs::read_to_string(path)?)?
    } else {
        NativeConfig::default()
    };
    if let Some(root) = &args.root {
        config.data_root = Some(root.clone());
    }
    Ok(config)
}

fn selected_sources(
    config: &NativeConfig,
    args: &AppArgs,
    home: Option<&Path>,
) -> Result<Vec<SourceConfig>> {
    let components: BTreeSet<_> = if args.components.is_empty() {
        if config.components.is_empty() {
            [ComponentKind::Sessions].into()
        } else {
            config.components.iter().copied().collect()
        }
    } else {
        args.components.iter().copied().collect()
    };
    let mut sources = if config.sources.is_empty() {
        default_sources(home.or(dirs::home_dir().as_deref()))
    } else {
        config.sources.clone()
    };
    sources.retain(|source| {
        components.contains(&source.component)
            && args.app.as_ref().is_none_or(|app| app == &source.app)
    });
    for source in &sources {
        ensure!(
            safe_relative(&source.app) && !source.app.contains('/'),
            "invalid app name"
        );
        ensure!(
            safe_relative(&source.slot) && !source.slot.contains('/'),
            "invalid source slot"
        );
    }
    ensure!(!sources.is_empty(), "no matching native sources");
    Ok(sources)
}

fn default_sources(home: Option<&Path>) -> Vec<SourceConfig> {
    let Some(home) = home else { return Vec::new() };
    let mut output = Vec::new();
    let push = |output: &mut Vec<SourceConfig>,
                app: &str,
                component: ComponentKind,
                slot: &str,
                path: PathBuf| {
        output.push(SourceConfig {
            app: app.into(),
            component,
            slot: slot.into(),
            path,
            host_version: None,
        })
    };
    push(
        &mut output,
        "codex",
        ComponentKind::Sessions,
        "sessions",
        home.join(".codex/sessions"),
    );
    push(
        &mut output,
        "codex",
        ComponentKind::Sessions,
        "archived",
        home.join(".codex/archived_sessions"),
    );
    push(
        &mut output,
        "codex",
        ComponentKind::Sessions,
        "state",
        home.join(".codex/state_5.sqlite"),
    );
    push(
        &mut output,
        "codex",
        ComponentKind::Sessions,
        "thread-history",
        home.join(".codex/thread_history_1.sqlite"),
    );
    push(
        &mut output,
        "codex",
        ComponentKind::Sessions,
        "index",
        home.join(".codex/session_index.jsonl"),
    );
    push(
        &mut output,
        "claude-code",
        ComponentKind::Sessions,
        "projects",
        home.join(".claude/projects"),
    );
    push(
        &mut output,
        "claude-code",
        ComponentKind::Sessions,
        "tasks",
        home.join(".claude/tasks"),
    );
    push(
        &mut output,
        "claude-code",
        ComponentKind::Sessions,
        "file-history",
        home.join(".claude/file-history"),
    );
    push(
        &mut output,
        "cursor",
        ComponentKind::Sessions,
        "global-storage",
        cursor_global_storage(home).join("state.vscdb"),
    );
    push(
        &mut output,
        "antigravity",
        ComponentKind::Sessions,
        "app",
        home.join(".gemini/antigravity/conversations"),
    );
    push(
        &mut output,
        "antigravity",
        ComponentKind::Sessions,
        "cli",
        home.join(".gemini/antigravity-cli/conversations"),
    );
    push(
        &mut output,
        "antigravity",
        ComponentKind::Sessions,
        "brain",
        home.join(".gemini/antigravity-cli/brain"),
    );
    push(
        &mut output,
        "antigravity",
        ComponentKind::Sessions,
        "ide",
        home.join(".gemini/antigravity-ide"),
    );
    push(
        &mut output,
        "grok",
        ComponentKind::Sessions,
        "sessions",
        home.join(".grok/sessions"),
    );
    output
}

fn cursor_global_storage(home: &Path) -> PathBuf {
    let config = if dirs::home_dir().as_deref() == Some(home) {
        dirs::config_dir().unwrap_or_else(|| home.join(".config"))
    } else if cfg!(windows) {
        home.join("AppData/Roaming")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join(".config")
    };
    config.join("Cursor/User/globalStorage")
}

fn detect_host_version(app: &str, source: &Path, _home: Option<&Path>) -> Option<String> {
    if app == "cursor" {
        if let Some(version) = command_version("cursor") {
            return Some(format!("cursor {version}"));
        }
        let candidates = [
            source
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            dirs::home_dir().map(|home| home.join("AppData/Local/Programs/cursor")),
        ];
        for config in candidates.into_iter().flatten() {
            for relative in ["resources/app/product.json", "resources/app/package.json"] {
                let Ok(value) = fs::read_to_string(config.join(relative)) else {
                    continue;
                };
                let Ok(json) = serde_json::from_str::<serde_json::Value>(&value) else {
                    continue;
                };
                if let Some(version) = json.get("version").and_then(serde_json::Value::as_str) {
                    return Some(format!("cursor {version}"));
                }
            }
        }
    }
    let command = match app {
        "codex" => "codex",
        "claude-code" => "claude",
        "grok" => "grok",
        "antigravity" => "agy",
        _ => return None,
    };
    command_version(command).map(|version| format!("{command} {version}"))
}

fn command_version(command: &str) -> Option<String> {
    let output = Command::new(command).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.len() > 160 {
        return None;
    }
    Some(line.to_string())
}

fn capture_sources(
    config: &NativeConfig,
    root: &Path,
    args: &AppArgs,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    let _lock = exclusive_lock(&root.join("capture.lock"))?;
    let sources = selected_sources(config, args, home)?
        .into_iter()
        .map(|mut source| {
            if source.host_version.is_none() {
                source.host_version = detect_host_version(&source.app, &source.path, home);
            }
            source
        })
        .collect::<Vec<_>>();
    let previous = latest_manifest(root).ok();
    let fingerprint = fingerprint_sources(&sources)?;
    if previous.as_ref().is_some_and(|manifest| {
        saved_fingerprint(root, &manifest.snapshot_id).ok().as_ref() == Some(&fingerprint)
    }) {
        return Ok(
            serde_json::json!({"status": "unchanged", "snapshot_id": previous.unwrap().snapshot_id}),
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    let staging = root.join("staging").join(&id);
    fs::create_dir_all(&staging)?;
    let started = Utc::now();
    let mut files = Vec::new();
    let mut exclusions = Vec::new();
    let mut unsupported = Vec::new();
    let result = (|| -> Result<()> {
        for source in &sources {
            if !source.path.exists() {
                exclusions.push(format!("{} missing", source.path.display()));
                continue;
            }
            if source
                .host_version
                .as_deref()
                .is_none_or(|version| version == "unknown")
            {
                unsupported.push(format!("{} unknown version", source.app));
            }
            copy_source(source, &staging, &mut files, &mut exclusions)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let manifest = Manifest {
        version: VERSION,
        snapshot_id: id.clone(),
        device: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
        app_instance: "chronicle-native".into(),
        captured_at: started,
        finished_at: Utc::now(),
        sources,
        files,
        exclusions: exclusions.clone(),
        unsupported: unsupported.clone(),
        incremental: previous.is_some(),
        local_restic_snapshot: None,
        remote_restic_snapshot: None,
        remote_confirmed_at: None,
    };
    atomic_json(&staging.join("manifest.json"), &manifest)?;
    atomic_json(&staging.join("fingerprint.json"), &fingerprint)?;
    let destination = root.join("snapshots").join(&id);
    fs::create_dir_all(root.join("snapshots"))?;
    fs::rename(&staging, &destination)
        .with_context(|| format!("publish {} -> {}", staging.display(), destination.display()))?;
    Ok(
        serde_json::json!({"status": "captured", "snapshot_id": id, "files": manifest.files.len(), "exclusions": exclusions.iter().take(50).collect::<Vec<_>>(), "exclusion_count":exclusions.len(), "exclusions_truncated":exclusions.len()>50, "unsupported": unsupported}),
    )
}

fn watch_sources(
    config: &NativeConfig,
    root: &Path,
    args: &AppArgs,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    use std::sync::mpsc::{self, RecvTimeoutError};
    let sources = selected_sources(config, args, home)?;
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;
    for source in sources.iter().filter(|source| source.path.exists()) {
        watcher.watch(&source.path, RecursiveMode::Recursive)?;
    }
    std::thread::scope(|scope| -> Result<serde_json::Value> {
        let (replication_sender, replication_receiver) = mpsc::channel();
        scope.spawn(move || {
            let mut last_remote = Instant::now()
                .checked_sub(Duration::from_secs(300))
                .unwrap();
            loop {
                let remote_due = last_remote.elapsed() >= Duration::from_secs(300);
                if let Err(error) = replicate_pending(config, root, remote_due) {
                    eprintln!("native replication pending: {error:#}");
                }
                if remote_due {
                    last_remote = Instant::now();
                }
                match replication_receiver.recv_timeout(Duration::from_secs(5)) {
                    Err(RecvTimeoutError::Disconnected) => break,
                    _ => continue,
                }
            }
        });
        let mut last_capture = Instant::now();
        let mut first_event = Some(last_capture);
        let mut last_event = last_capture;
        // Startup reconciliation happens before waiting for filesystem events.
        match capture_sources(config, root, args, home) {
            Ok(_) => {
                first_event = None;
                let _ = replication_sender.send(());
            }
            Err(error) => eprintln!("native startup capture pending: {error:#}"),
        }
        loop {
            match receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event))
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    let now = Instant::now();
                    first_event.get_or_insert(now);
                    last_event = now;
                }
                Ok(Err(error)) => {
                    eprintln!("native watch error; reconciliation remains active: {error}");
                    first_event.get_or_insert(Instant::now());
                }
                Err(RecvTimeoutError::Disconnected) => bail!("watch channel disconnected"),
                _ => {}
            }
            if capture_due(
                last_capture.elapsed(),
                first_event.map(|time| time.elapsed()),
                last_event.elapsed(),
            ) {
                match capture_sources(config, root, args, home) {
                    Ok(_) => {
                        first_event = None;
                        let _ = replication_sender.send(());
                    }
                    Err(error) => {
                        eprintln!("native capture pending: {error:#}");
                        first_event = Some(Instant::now());
                        last_event = Instant::now();
                    }
                }
                last_capture = Instant::now();
            }
        }
    })
}

fn capture_due(
    since_capture: Duration,
    since_first: Option<Duration>,
    since_event: Duration,
) -> bool {
    since_capture >= Duration::from_secs(300)
        || since_first.is_some_and(|first| {
            first >= Duration::from_secs(60) || since_event >= Duration::from_secs(5)
        })
}

fn replicate_pending(config: &NativeConfig, root: &Path, include_remote: bool) -> Result<()> {
    if config.local_repository.is_none() {
        return Ok(());
    }
    let mut local_config = config.clone();
    if !include_remote {
        local_config.remote_repository = None;
    }
    let mut failures = Vec::new();
    for manifest in list_manifests(root)? {
        if (manifest.local_restic_snapshot.is_none()
            || (include_remote
                && config.remote_repository.is_some()
                && manifest.remote_restic_snapshot.is_none()))
            && let Err(error) = replicate(&local_config, root, Some(&manifest.snapshot_id))
        {
            failures.push(format!("{}: {error:#}", manifest.snapshot_id));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn copy_source(
    source: &SourceConfig,
    snapshot_root: &Path,
    files: &mut Vec<FileEntry>,
    exclusions: &mut Vec<String>,
) -> Result<()> {
    let root = safe_existing(&source.path)?;
    if root.is_file() {
        if source.component != ComponentKind::Sessions && credential_content(&root) {
            exclusions.push(format!(
                "{}/{}: credential-content",
                source.app, source.slot
            ));
            return Ok(());
        }
        return copy_one(
            &root,
            &snapshot_root
                .join(&source.app)
                .join(&source.slot)
                .join(root.file_name().unwrap()),
            &format!(
                "{}/{}/{}",
                source.app,
                source.slot,
                root.file_name().unwrap().to_string_lossy()
            ),
            files,
            exclusions,
        );
    }
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        if credential_path(entry.path()) {
            exclusions.push(format!("{relative}: credential"));
            continue;
        }
        if source.component != ComponentKind::Sessions && credential_content(entry.path()) {
            exclusions.push(format!("{relative}: credential-content"));
            continue;
        }
        if sqlite_sidecar(entry.path()) {
            exclusions.push(format!("{relative}: sqlite-sidecar"));
            continue;
        }
        if entry.file_type().is_symlink() {
            exclusions.push(format!("{relative}: symlink"));
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        copy_one(
            entry.path(),
            &snapshot_root
                .join(&source.app)
                .join(&source.slot)
                .join(&relative),
            &format!("{}/{}/{relative}", source.app, source.slot),
            files,
            exclusions,
        )?;
    }
    Ok(())
}

fn copy_one(
    source: &Path,
    destination: &Path,
    stored: &str,
    files: &mut Vec<FileEntry>,
    exclusions: &mut Vec<String>,
) -> Result<()> {
    if credential_path(source) {
        exclusions.push(format!("{stored}: credential"));
        return Ok(());
    }
    if stored.starts_with("cursor/")
        && source.file_name().and_then(|name| name.to_str()) != Some("state.vscdb")
    {
        exclusions.push(format!("{stored}: unsupported Cursor session artifact"));
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let consistency = if stored.starts_with("cursor/") && sqlite_path(source) {
        let omitted = cursor::backup_sessions(source, destination)?;
        exclusions.push(format!("{stored}: {omitted} non-session rows excluded"));
        "sqlite-session-projection"
    } else if sqlite_path(source) {
        backup_sqlite(source, destination)?;
        "sqlite-consistent"
    } else {
        fs::copy(source, destination)?;
        "file-copy"
    };
    let (bytes, sha) = hash_file(destination)?;
    files.push(FileEntry {
        relative: stored.replace('\\', "/"),
        source: source.display().to_string(),
        bytes,
        sha256: sha,
        consistency: consistency.into(),
    });
    Ok(())
}

fn backup_sqlite(source: &Path, destination: &Path) -> Result<()> {
    let from = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut to = Connection::open(destination)?;
    let backup = Backup::new(&from, &mut to)?;
    ensure!(
        matches!(backup.step(-1)?, rusqlite::backup::StepResult::Done),
        "SQLite backup did not complete; capture must be retried"
    );
    Ok(())
}

fn replicate(
    config: &NativeConfig,
    root: &Path,
    snapshot: Option<&str>,
) -> Result<serde_json::Value> {
    let id = snapshot
        .map(str::to_owned)
        .or_else(|| latest_manifest(root).ok().map(|m| m.snapshot_id))
        .context("no snapshot")?;
    verify_local(root, &id)?;
    let restic = config
        .restic
        .clone()
        .context("restic executable is not configured")?;
    let password = config
        .password_file
        .clone()
        .context("external password file is not configured")?;
    ensure!(
        password.is_file() && password.metadata()?.len() > 0,
        "password reference is invalid"
    );
    let local = config
        .local_repository
        .clone()
        .context("local repository is not configured")?;
    let _lock = exclusive_lock(&root.join("replicate.lock"))?;
    let mut manifest = load_manifest(&root.join("snapshots").join(&id))?;
    let source_id = if let Some(existing) = &manifest.local_restic_snapshot {
        existing.clone()
    } else {
        restic_backup(
            &restic,
            &password,
            &local,
            &root.join("snapshots").join(&id),
            &id,
            root,
        )?
    };
    manifest.local_restic_snapshot = Some(source_id.clone());
    atomic_json(
        &root.join("snapshots").join(&id).join("manifest.json"),
        &manifest,
    )?;
    if let Some(remote) = &config.remote_repository
        && manifest.remote_restic_snapshot.is_none()
    {
        restic_command(
            &restic,
            &password,
            root,
            &[
                "-r",
                remote,
                "copy",
                "--from-repo",
                &local,
                "--from-password-file",
                &password.display().to_string(),
                &source_id,
            ],
        )?;
        let copied = restic_json(
            &restic,
            &password,
            root,
            &["-r", remote, "snapshots", "--json"],
        )?;
        let remote_id = copied
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("original").and_then(|v| v.as_str()) == Some(&source_id)
                        || item.get("id").and_then(|v| v.as_str()) == Some(&source_id)
                })
            })
            .and_then(|item| item.get("id").and_then(|v| v.as_str()))
            .context("remote snapshot mapping missing")?
            .to_owned();
        manifest.remote_restic_snapshot = Some(remote_id);
        manifest.remote_confirmed_at = Some(Utc::now());
    }
    atomic_json(
        &root.join("snapshots").join(&id).join("manifest.json"),
        &manifest,
    )?;
    Ok(
        serde_json::json!({"snapshot_id": id, "local_restic_snapshot": manifest.local_restic_snapshot, "remote_restic_snapshot": manifest.remote_restic_snapshot, "remote_confirmed_at": manifest.remote_confirmed_at}),
    )
}

fn restic_backup(
    restic: &Path,
    password: &Path,
    repository: &str,
    snapshot: &Path,
    id: &str,
    cache_root: &Path,
) -> Result<String> {
    let output = Command::new(restic)
        .args([
            "-r",
            repository,
            "backup",
            "--json",
            "--tag",
            &format!("chronicle-native:{id}"),
        ])
        .arg(snapshot)
        .env("RESTIC_PASSWORD_FILE", password)
        .env("RESTIC_CACHE_DIR", cache_root.join("restic-cache"))
        .output()?;
    ensure!(
        output.status.success(),
        "restic backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|v| {
            v.get("snapshot_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .context("missing restic snapshot id")
}

fn restic_json(
    restic: &Path,
    password: &Path,
    cache_root: &Path,
    args: &[&str],
) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&restic_command(
        restic, password, cache_root, args,
    )?)?)
}
fn restic_command(
    restic: &Path,
    password: &Path,
    cache_root: &Path,
    args: &[&str],
) -> Result<String> {
    let output = Command::new(restic)
        .args(args)
        .env("RESTIC_PASSWORD_FILE", password)
        .env("RESTIC_CACHE_DIR", cache_root.join("restic-cache"))
        .output()?;
    ensure!(
        output.status.success(),
        "restic failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn restore_snapshot(
    root: &Path,
    args: RestoreArgs,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    if let Some(plan_path) = args.apply_plan {
        return apply_saved_plan(root, &plan_path, args.force);
    }
    let id = args.snapshot_id.context("snapshot id required")?;
    let manifest = verify_local(root, &id)?;
    if let Some(app) = args.into {
        ensure!(
            args.target.is_none(),
            "use --target for isolated extraction or --into for native installation planning"
        );
        let plan = build_install_plan(&manifest, &app, home, &args.map, root)?;
        if args.dry_run {
            return Ok(serde_json::json!({"mode": "install-dry-run", "plan": plan}));
        }
        let plan_path = root.join("plans").join(format!("{id}-{app}.install.json"));
        atomic_json(&plan_path, &plan)?;
        return Ok(
            serde_json::json!({"mode": "plan-ready", "apply_plan": plan_path, "actions": plan.actions.len()}),
        );
    }
    let target = args.target.context("restore target required")?;
    validate_store(&target)?;
    let plan = ExtractPlan {
        kind: ExtractPlanKind::Extract,
        snapshot_id: id.clone(),
        target: target.clone(),
        target_fingerprint: fingerprint_path(&target)?,
        files: manifest.files.iter().map(|f| f.relative.clone()).collect(),
    };
    if args.dry_run {
        return Ok(serde_json::json!({"mode": "extract-dry-run", "plan": plan}));
    }
    extract_snapshot(root, &id, &target)?;
    Ok(
        serde_json::json!({"mode": "extracted", "destination": target, "files": manifest.files.len()}),
    )
}

fn build_install_plan(
    manifest: &Manifest,
    app: &str,
    home: Option<&Path>,
    mappings: &[String],
    root: &Path,
) -> Result<InstallPlan> {
    ensure!(
        manifest.unsupported.is_empty(),
        "unknown host version: native installation is refused"
    );
    let defaults = default_sources(home.or(dirs::home_dir().as_deref()));
    let mut actions = Vec::new();
    let mut path_map = BTreeMap::new();
    let mut host_version = None;
    for source in manifest
        .sources
        .iter()
        .filter(|source| source.app == app && source.component == ComponentKind::Sessions)
    {
        let target_source = install_target_source(source, &defaults, mappings)?;
        let source_version = source
            .host_version
            .clone()
            .unwrap_or_else(|| "unknown".into());
        if let Some(existing) = host_version.replace(source_version.clone()) {
            ensure!(existing == source_version, "source host versions differ");
        }
        let prefix = format!("{}/{}/", source.app, source.slot);
        for file in manifest
            .files
            .iter()
            .filter(|file| file.relative.starts_with(&prefix))
        {
            let relative = &file.relative[prefix.len()..];
            let source_path = if source.path.is_file() {
                source.path.clone()
            } else {
                source.path.join(relative)
            };
            let target_path = if target_source.path.extension().is_some() {
                target_source.path.clone()
            } else {
                target_source.path.join(relative)
            };
            validate_store(&target_path)?;
            let name = target_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let kind = if app == "cursor" && name == "state.vscdb" {
                InstallActionKind::CursorSessionMerge
            } else if app == "codex" && source.slot == "state" {
                InstallActionKind::CodexIndexMerge
            } else if app == "codex" && source.slot == "index" {
                InstallActionKind::SessionIndexMerge
            } else if app == "codex" && source.slot == "thread-history" {
                InstallActionKind::CopyIfMissing
            } else {
                InstallActionKind::Copy
            };
            actions.push(InstallAction {
                kind,
                source: file.relative.clone(),
                target: target_path.clone(),
            });
            path_map.insert(source_path.display().to_string(), target_path);
        }
    }
    ensure!(!actions.is_empty(), "no installable files for {app}");
    actions.sort_by_key(|action| match action.kind {
        InstallActionKind::Copy | InstallActionKind::CopyIfMissing => 0,
        InstallActionKind::CursorSessionMerge => 1,
        InstallActionKind::CodexIndexMerge => 2,
        InstallActionKind::SessionIndexMerge => 3,
    });
    let host_version = host_version.context("host version missing")?;
    ensure!(
        host_version != "unknown",
        "unknown host version: native installation is refused"
    );
    Ok(InstallPlan {
        kind: InstallPlanKind::Install,
        version: VERSION,
        snapshot_id: manifest.snapshot_id.clone(),
        app: app.into(),
        host_version,
        target_fingerprint: install_fingerprint(&actions)?,
        actions,
        path_map,
        rollback_dir: root.join("rollback").join(&manifest.snapshot_id),
    })
}

fn install_target_source(
    source: &SourceConfig,
    defaults: &[SourceConfig],
    mappings: &[String],
) -> Result<SourceConfig> {
    let source_path = source
        .path
        .canonicalize()
        .unwrap_or_else(|_| source.path.clone());
    for mapping in mappings {
        let (from, to) = mapping
            .split_once('=')
            .context("mapping must be SOURCE=TARGET")?;
        if Path::new(from)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(from))
            == source_path
        {
            return Ok(SourceConfig {
                path: PathBuf::from(to),
                ..source.clone()
            });
        }
    }
    defaults
        .iter()
        .find(|target| target.app == source.app && target.slot == source.slot)
        .cloned()
        .with_context(|| format!("no install mapping for {}/{}", source.app, source.slot))
}

fn install_fingerprint(actions: &[InstallAction]) -> Result<String> {
    let mut files = BTreeMap::new();
    for action in actions {
        if action.target.exists() {
            validate_store(&action.target)?;
            files.insert(
                action.target.display().to_string(),
                Some(hash_file(&action.target)?),
            );
        } else {
            files.insert(action.target.display().to_string(), None);
        }
    }
    Ok(hex(&Sha256::digest(serde_json::to_vec(&files)?)))
}

fn apply_saved_plan(root: &Path, path: &Path, force: bool) -> Result<serde_json::Value> {
    let raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    if raw.get("kind").and_then(serde_json::Value::as_str) == Some("install") {
        let plan: InstallPlan = serde_json::from_value(raw)?;
        return apply_install_plan(root, plan, force);
    }
    let plan: ExtractPlan = serde_json::from_value(raw)?;
    let manifest = verify_local(root, &plan.snapshot_id)?;
    ensure!(
        plan.kind == ExtractPlanKind::Extract
            && plan.files
                == manifest
                    .files
                    .iter()
                    .map(|file| file.relative.clone())
                    .collect::<Vec<_>>(),
        "restore plan file set differs from snapshot"
    );
    let target = plan.target;
    ensure!(
        plan.target_fingerprint == fingerprint_path(&target)?,
        "restore target changed; generate a new plan"
    );
    extract_snapshot(root, &plan.snapshot_id, &target)?;
    Ok(
        serde_json::json!({"mode":"applied-files","snapshot_id":manifest.snapshot_id,"target":target,"client_index":"not-updated; use an install plan for native client registration"}),
    )
}

fn apply_install_plan(root: &Path, plan: InstallPlan, force: bool) -> Result<serde_json::Value> {
    let manifest = verify_local(root, &plan.snapshot_id)?;
    ensure!(
        verified_install_host(&plan.app, &plan.host_version),
        "native installation has not been verified for this host version; isolate files with --target and complete host acceptance first"
    );
    ensure!(
        plan.version == VERSION && plan.kind == InstallPlanKind::Install,
        "unsupported install plan"
    );
    ensure!(
        plan.target_fingerprint == install_fingerprint(&plan.actions)?,
        "restore target changed; generate a new plan"
    );
    ensure_host_stopped(&plan.app, force)?;
    for action in &plan.actions {
        ensure!(
            manifest
                .files
                .iter()
                .any(|file| file.relative == action.source),
            "install action is not in snapshot"
        );
    }
    fs::create_dir_all(&plan.rollback_dir)?;
    let mut prepared = Vec::new();
    for (index, action) in plan.actions.iter().enumerate() {
        validate_store(&action.target)?;
        let backup = plan.rollback_dir.join(format!("{index:04}.backup"));
        let existed = action.target.exists();
        if existed {
            if sqlite_path(&action.target) {
                backup_sqlite(&action.target, &backup)?;
            } else {
                fs::copy(&action.target, &backup)?;
            }
        }
        prepared.push((action, existed, backup));
    }
    let journal_path = plan.rollback_dir.join("journal.json");
    let mut journal = InstallJournal {
        snapshot_id: plan.snapshot_id.clone(),
        phase: "applying".into(),
        completed: Vec::new(),
    };
    atomic_json(&journal_path, &journal)?;
    let mut completed = 0;
    let result = (|| -> Result<()> {
        for action in &plan.actions {
            apply_install_action(root, &plan, action)?;
            completed += 1;
            journal.completed.push(action.target.display().to_string());
            atomic_json(&journal_path, &journal)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (action, existed, backup) in prepared.iter().take(completed + 1).rev() {
            restore_install_target(&action.target, *existed, backup)?;
        }
        journal.phase = "rolled-back-after-error".into();
        atomic_json(&journal_path, &journal)?;
        return Err(error);
    }
    journal.phase = "completed".into();
    atomic_json(&journal_path, &journal)?;
    Ok(
        serde_json::json!({"mode":"installed-files-and-indexes","snapshot_id":plan.snapshot_id,"app":plan.app,"actions":plan.actions.len(),"rollback":plan.rollback_dir,"file_verification":"passed","client_open":"not-verified","client_restart":"not-verified","continuation":"not-verified"}),
    )
}

fn verified_install_host(app: &str, version: &str) -> bool {
    // Fixture coverage is not a production version allowlist. Add a real host
    // version only with recorded isolated open/restart/continuation evidence.
    cfg!(test)
        && version == "fixture-v1"
        && matches!(
            app,
            "codex" | "cursor" | "claude-code" | "antigravity" | "grok"
        )
}

fn apply_install_action(root: &Path, plan: &InstallPlan, action: &InstallAction) -> Result<()> {
    let source = root
        .join("snapshots")
        .join(&plan.snapshot_id)
        .join(&action.source);
    match action.kind {
        InstallActionKind::Copy | InstallActionKind::CopyIfMissing => {
            if action.target.exists() {
                if action.kind == InstallActionKind::CopyIfMissing {
                    return Ok(());
                }
                let source_hash = hash_file(&source)?;
                ensure!(
                    source_hash == hash_file(&action.target)?,
                    "restore conflict: {}",
                    action.target.display()
                );
                return Ok(());
            }
            if let Some(parent) = action.target.parent() {
                fs::create_dir_all(parent)?;
            }
            validate_store(&action.target)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&action.target)?;
            std::io::copy(&mut File::open(&source)?, &mut output)?;
            output.sync_all()?;
            ensure!(
                hash_file(&source)? == hash_file(&action.target)?,
                "installed file hash mismatch"
            );
        }
        InstallActionKind::CursorSessionMerge => {
            if let Some(parent) = action.target.parent() {
                fs::create_dir_all(parent)?;
            }
            cursor::merge_sessions(&source, &action.target)?;
        }
        InstallActionKind::CodexIndexMerge => {
            codex::merge_index(&source, &action.target, &plan.path_map)?
        }
        InstallActionKind::SessionIndexMerge => {
            let registered = plan
                .actions
                .iter()
                .find(|action| action.kind == InstallActionKind::CodexIndexMerge)
                .map(|action| action.target.as_path());
            merge_session_index(&source, &action.target, registered)?
        }
    }
    Ok(())
}

fn restore_install_target(target: &Path, existed: bool, backup: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", target.display())));
    }
    if existed {
        fs::copy(backup, target)?;
    } else {
        let _ = fs::remove_file(target);
    }
    Ok(())
}

fn ensure_host_stopped(app: &str, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let expected: [&str; 3] = match app {
        "cursor" => ["cursor", "cursor agent", "cursor deeplinks"],
        "codex" => ["codex", "codex terminal", "codex desktop"],
        "claude-code" => ["claude", "claude code", "anthropic claude"],
        "antigravity" => ["antigravity", "agy", "gemini"],
        "grok" => ["grok", "grok cli", "xai grok"],
        _ => bail!("unsupported host {app}"),
    };
    let output = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()?
    } else {
        Command::new("ps").args(["-A", "-o", "comm="]).output()?
    };
    ensure!(output.status.success(), "process preflight failed");
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    ensure!(
        !expected
            .iter()
            .any(|name| text.lines().any(|line| line.contains(name))),
        "{app} is running; close it or explicitly use --force"
    );
    Ok(())
}

fn merge_session_index(
    source: &Path,
    target: &Path,
    registered_index: Option<&Path>,
) -> Result<()> {
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut ids = BTreeMap::new();
    for line in fs::read_to_string(source)?
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let value: serde_json::Value = serde_json::from_str(line)?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .context("session index entry missing id")?
            .to_owned();
        if let Some(index) = registered_index {
            let connection = Connection::open_with_flags(index, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let registered: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?1)",
                [&id],
                |row| row.get(0),
            )?;
            if !registered {
                continue;
            }
        }
        if let Some(previous) = ids.insert(id.clone(), value.clone()) {
            ensure!(
                previous == value,
                "session index duplicate id has different content: {id}"
            );
        }
        entries.push(value);
    }
    ensure!(!entries.is_empty(), "empty session index");
    if target.exists() {
        for line in fs::read_to_string(target)?
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let value: serde_json::Value = serde_json::from_str(line)?;
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .context("target session index entry missing id")?
                .to_owned();
            if let Some(recovered) = ids.get(&id) {
                ensure!(
                    *recovered == value,
                    "same session id has different index content: {id}"
                );
                continue;
            }
            entries.push(value);
        }
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = target.with_extension("tmp");
    let mut output = String::new();
    for entry in entries {
        output.push_str(&serde_json::to_string(&entry)?);
        output.push('\n');
    }
    fs::write(&temp, output)?;
    fs::rename(temp, target)?;
    Ok(())
}

fn verify_local(root: &Path, id: &str) -> Result<Manifest> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "invalid snapshot id");
    let snapshot = root.join("snapshots").join(id);
    validate_store(&snapshot)?;
    let manifest = load_manifest(&snapshot)?;
    ensure!(manifest.snapshot_id == id, "snapshot identity mismatch");
    let mut seen = BTreeSet::new();
    for file in &manifest.files {
        ensure!(
            safe_relative(&file.relative),
            "invalid snapshot relative path"
        );
        ensure!(
            seen.insert(file.relative.to_lowercase()),
            "duplicate snapshot path"
        );
        let path = snapshot.join(&file.relative);
        validate_store(&path)?;
        let (bytes, hash) = hash_file(&path)?;
        ensure!(
            hash == file.sha256 && bytes == file.bytes,
            "snapshot hash mismatch: {}",
            file.relative
        );
    }
    Ok(manifest)
}

fn extract_snapshot(root: &Path, id: &str, destination: &Path) -> Result<()> {
    let manifest = verify_local(root, id)?;
    let snapshot = root.join("snapshots").join(id);
    validate_store(destination)?;
    // Check every conflict before creating the first target. Identical files
    // make retry after an interruption idempotent.
    for file in &manifest.files {
        let target = destination.join(&file.relative);
        validate_store(&target)?;
        if target.exists() {
            let (bytes, hash) = hash_file(&target)?;
            ensure!(
                bytes == file.bytes && hash == file.sha256,
                "restore conflict: {}",
                target.display()
            );
        }
    }
    let mut created = Vec::new();
    let result = (|| -> Result<()> {
        for file in &manifest.files {
            let source = snapshot.join(&file.relative);
            let target = destination.join(&file.relative);
            if target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            validate_store(&target)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            created.push(target.clone());
            std::io::copy(&mut File::open(source)?, &mut output)?;
            output.sync_all()?;
            drop(output);
            let (_, hash) = hash_file(&target)?;
            ensure!(
                hash == file.sha256,
                "extracted file hash mismatch: {}",
                file.relative
            );
        }
        Ok(())
    })();
    if result.is_err() {
        for path in created.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
    result
}

fn status(root: &Path) -> Result<serde_json::Value> {
    let manifests = list_manifests(root)?;
    let latest = manifests.first();
    Ok(
        serde_json::json!({"local_snapshot": latest.map(|m| &m.snapshot_id), "local_restic_snapshot": latest.and_then(|m| m.local_restic_snapshot.clone()), "remote_confirmed_at": latest.and_then(|m| m.remote_confirmed_at), "remote_unknown_when_absent": latest.is_some_and(|m| m.remote_confirmed_at.is_none())}),
    )
}

fn list_manifests(root: &Path) -> Result<Vec<Manifest>> {
    let mut values = Vec::new();
    for entry in
        fs::read_dir(root.join("snapshots")).unwrap_or_else(|_| fs::read_dir(root).unwrap())
    {
        let path = entry?.path();
        if path.join("manifest.json").is_file() {
            values.push(load_manifest(&path)?);
        }
    }
    values.sort_by_key(|m| m.captured_at);
    values.reverse();
    Ok(values)
}
fn latest_manifest(root: &Path) -> Result<Manifest> {
    list_manifests(root)?
        .into_iter()
        .next()
        .context("no snapshots")
}
fn load_manifest(snapshot: &Path) -> Result<Manifest> {
    Ok(serde_json::from_str(&fs::read_to_string(
        snapshot.join("manifest.json"),
    )?)?)
}
fn saved_fingerprint(root: &Path, id: &str) -> Result<Fingerprint> {
    Ok(serde_json::from_str(&fs::read_to_string(
        root.join("snapshots").join(id).join("fingerprint.json"),
    )?)?)
}

fn fingerprint_sources(sources: &[SourceConfig]) -> Result<Fingerprint> {
    let mut files = BTreeMap::new();
    for source in sources {
        let identity = serde_json::to_vec(&(
            source.app.clone(),
            source.slot.clone(),
            source.path.clone(),
            source.host_version.clone(),
        ))?;
        files.insert(
            format!("@source/{}/{}", source.app, source.slot),
            (0, hex(&Sha256::digest(identity))),
        );
        if source.path.exists() {
            if source.app == "cursor" {
                for entry in WalkDir::new(&source.path).follow_links(false) {
                    let entry = entry?;
                    if entry.file_type().is_file()
                        && matches!(
                            entry.file_name().to_str(),
                            Some("state.vscdb" | "state.vscdb-wal")
                        )
                        && !credential_path(entry.path())
                    {
                        validate_store(entry.path())?;
                        let relative = if source.path.is_file() {
                            entry.file_name().to_string_lossy().to_string()
                        } else {
                            entry
                                .path()
                                .strip_prefix(&source.path)?
                                .to_string_lossy()
                                .to_string()
                        };
                        files.insert(
                            format!("{}/{}/{}", source.app, source.slot, relative),
                            file_stamp(entry.path())?,
                        );
                    }
                }
                if source.path.is_file() {
                    let wal = PathBuf::from(format!("{}-wal", source.path.display()));
                    if wal.exists() {
                        collect_fingerprint(
                            &wal,
                            &format!("{}/{}", source.app, source.slot),
                            &mut files,
                        )?;
                    }
                }
                continue;
            }
            collect_fingerprint(
                &safe_existing(&source.path)?,
                &format!("{}/{}", source.app, source.slot),
                &mut files,
            )?;
            if sqlite_path(&source.path) {
                let wal = PathBuf::from(format!("{}-wal", source.path.display()));
                if wal.exists() {
                    collect_fingerprint(
                        &wal,
                        &format!("{}/{}", source.app, source.slot),
                        &mut files,
                    )?;
                }
            }
        }
    }
    Ok(Fingerprint { files })
}
fn collect_fingerprint(
    path: &Path,
    prefix: &str,
    output: &mut BTreeMap<String, (u64, String)>,
) -> Result<()> {
    if path.is_file() {
        output.insert(
            format!("{prefix}/{}", path.file_name().unwrap().to_string_lossy()),
            file_stamp(path)?,
        );
        return Ok(());
    }
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && !credential_path(entry.path())
            && !entry.path().to_string_lossy().ends_with("-shm")
        {
            let relative = entry
                .path()
                .strip_prefix(path)?
                .to_string_lossy()
                .replace('\\', "/");
            output.insert(format!("{prefix}/{relative}"), file_stamp(entry.path())?);
        }
    }
    Ok(())
}
fn file_stamp(path: &Path) -> Result<(u64, String)> {
    hash_file(path)
}
fn fingerprint_path(path: &Path) -> Result<String> {
    validate_store(path)?;
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();
    let mut files = BTreeMap::new();
    if path.exists() {
        for entry in WalkDir::new(path).follow_links(false) {
            let entry = entry?;
            validate_store(entry.path())?;
            let relative = entry
                .path()
                .strip_prefix(path)?
                .to_string_lossy()
                .to_string();
            files.insert(
                relative,
                if entry.file_type().is_file() {
                    Some(hash_file(entry.path())?)
                } else {
                    None
                },
            );
        }
    }
    let value = serde_json::to_vec(&(canonical, path.exists(), files))?;
    Ok(hex(&Sha256::digest(value)))
}

fn credential_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let blocked = [
        "auth.json",
        "credentials.json",
        "credentials",
        "cookies",
        "cookies.sqlite",
        "token.json",
        "password",
        "password.txt",
        "secrets.json",
        ".env",
        "admin-api-token",
        "codex-accounts.json",
    ];
    blocked.contains(&name.as_str()) || path.components().any(|c| matches!(c, Component::Normal(v) if matches!(v.to_str().unwrap_or("").to_ascii_lowercase().as_str(), "auth" | "credentials" | "tokens" | "cookies" | "secrets")))
}

fn credential_content(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return true;
    };
    if bytes.contains(&0) {
        return false;
    }
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    if path
        .extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("json")))
    {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            return credential_json(&value);
        }
        return true;
    }
    if path
        .extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("toml")))
    {
        if let Ok(value) = toml::from_slice::<toml::Value>(&bytes) {
            return credential_toml(&value);
        }
        return true;
    }
    text.lines().any(|line| {
        let line = line.trim();
        let credential = [
            "password",
            "passwd",
            "secret",
            "token",
            "api_key",
            "apikey",
            "access_token",
            "authorization",
            "cookie",
        ]
        .iter()
        .any(|word| line.contains(word));
        credential && line.contains(['=', ':'])
    })
}

fn credential_json(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "password"
                    | "passwd"
                    | "secret"
                    | "token"
                    | "api_key"
                    | "apikey"
                    | "access_token"
                    | "authorization"
                    | "cookie"
            ) || credential_json(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(credential_json),
        _ => false,
    }
}

fn credential_toml(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(map) => map.iter().any(|(key, value)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "password"
                    | "passwd"
                    | "secret"
                    | "token"
                    | "api_key"
                    | "apikey"
                    | "access_token"
                    | "authorization"
                    | "cookie"
            ) || credential_toml(value)
        }),
        toml::Value::Array(values) => values.iter().any(credential_toml),
        _ => false,
    }
}
fn sqlite_sidecar(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        matches!(
            ext.to_str(),
            Some("db-wal" | "db-shm" | "sqlite-wal" | "sqlite-shm" | "vscdb-wal" | "vscdb-shm")
        )
    })
}
fn sqlite_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| matches!(ext.to_str(), Some("db" | "sqlite" | "vscdb")))
}
fn safe_existing(path: &Path) -> Result<PathBuf> {
    ensure!(path.exists(), "missing source");
    ensure!(
        !path.is_symlink(),
        "source symlink rejected: {}",
        path.display()
    );
    Ok(path.to_path_buf())
}
fn validate_store(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    !meta.is_symlink(),
                    "symlink rejected: {}",
                    ancestor.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse point rejected: {}",
                        ancestor.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':'])
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}
fn default_root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("D:/Data/chronicle-native")
    } else {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("chronicle-native")
    }
}
fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 1024 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((bytes, hex(hasher.finalize().as_slice())))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temp, path)?;
    Ok(())
}
fn exclusive_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_home_keeps_cursor_discovery_inside_isolated_profile() {
        let temp = tempfile::tempdir().unwrap();
        let sources = default_sources(Some(temp.path()));
        let cursor = sources
            .iter()
            .find(|source| source.app == "cursor")
            .unwrap();
        assert!(cursor.path.starts_with(temp.path()));
        assert_eq!(cursor.path.file_name().unwrap(), "state.vscdb");
    }
    #[test]
    fn restore_retries_identically_rejects_conflicts_and_checks_saved_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.jsonl"), b"first").unwrap();
        fs::write(source.join("z.jsonl"), b"last").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "fixture".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source,
                host_version: None,
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let target = temp.path().join("target");
        extract_snapshot(&root, id, &target).unwrap();
        let before = fingerprint_path(&target).unwrap();
        extract_snapshot(&root, id, &target).unwrap();
        assert_eq!(fingerprint_path(&target).unwrap(), before);
        let manifest = verify_local(&root, id).unwrap();
        let plan = ExtractPlan {
            kind: ExtractPlanKind::Extract,
            snapshot_id: id.into(),
            target: target.clone(),
            target_fingerprint: before,
            files: manifest.files.iter().map(|f| f.relative.clone()).collect(),
        };
        let plan_path = temp.path().join("plan.json");
        atomic_json(&plan_path, &plan).unwrap();
        fs::write(target.join("fixture/sessions/z.jsonl"), b"changed").unwrap();
        assert!(apply_saved_plan(&root, &plan_path, true).is_err());
        fs::remove_file(target.join("fixture/sessions/a.jsonl")).unwrap();
        assert!(extract_snapshot(&root, id, &target).is_err());
        assert!(!target.join("fixture/sessions/a.jsonl").exists());
        let mut bad = manifest;
        bad.files[0].relative = "../escaped".into();
        atomic_json(&root.join("snapshots").join(id).join("manifest.json"), &bad).unwrap();
        assert!(verify_local(&root, id).is_err());
        assert!(verify_local(&root, "..").is_err());
    }
    #[test]
    fn capture_schedule_retains_early_events_and_bounds_continuous_changes() {
        let seconds = Duration::from_secs;
        assert!(!capture_due(seconds(2), Some(seconds(2)), seconds(2)));
        assert!(capture_due(seconds(5), Some(seconds(5)), seconds(5)));
        assert!(!capture_due(seconds(59), Some(seconds(59)), seconds(1)));
        assert!(capture_due(seconds(60), Some(seconds(60)), seconds(1)));
        assert!(!capture_due(seconds(299), None, seconds(299)));
        assert!(capture_due(seconds(300), None, seconds(300)));
    }

    #[test]
    fn cursor_install_plan_merges_sessions_preserves_auth_and_rolls_back_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_root = home.join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_db = source_root.join("state.vscdb");
        let source = Connection::open(&source_db).unwrap();
        source.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT); INSERT INTO ItemTable VALUES('cursor.accessToken','FAKE_SOURCE_AUTH'); INSERT INTO ItemTable VALUES('composerData:session-1','restored-message'); INSERT INTO composerHeaders VALUES('session-1','workspace-1',1,2,0,0,3,4,'restored-header',NULL);").unwrap();
        drop(source);
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "cursor".into(),
                component: ComponentKind::Sessions,
                slot: "global-storage".into(),
                path: source_db,
                host_version: Some("fixture-v1".into()),
            }],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();
        let target = home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let target_db = Connection::open(&target).unwrap();
        target_db.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value BLOB); CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT); INSERT INTO ItemTable VALUES('cursor.accessToken','FAKE_TARGET_AUTH');").unwrap();
        drop(target_db);
        let plan = build_install_plan(&manifest, "cursor", Some(&home), &[], &root).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].kind, InstallActionKind::CursorSessionMerge);
        assert_eq!(plan.actions[0].target, target);
        let mut stale = plan.clone();
        stale.actions[0].target = temp.path().join("changed-after-plan");
        assert!(apply_install_plan(&root, stale, true).is_err());
        assert_eq!(
            apply_install_plan(&root, plan, true).unwrap()["file_verification"],
            "passed"
        );
        let target_db = Connection::open(&target).unwrap();
        assert_eq!(
            target_db
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='cursor.accessToken'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "FAKE_TARGET_AUTH"
        );
        assert_eq!(
            target_db
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='composerData:session-1'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "restored-message"
        );
    }

    #[test]
    fn codex_install_registers_only_verified_rollouts_and_filters_ghost_index_entries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source_root = home.join("source/.codex");
        let rollout = source_root.join("sessions/rollouts/thread-1.jsonl");
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(&rollout, b"{\"type\":\"session_meta\"}\n").unwrap();
        let source_state = source_root.join("state/state_5.sqlite");
        fs::create_dir_all(source_state.parent().unwrap()).unwrap();
        let state_db = Connection::open(&source_state).unwrap();
        state_db.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,metadata TEXT NOT NULL,position INTEGER NOT NULL,created_at_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL); CREATE TABLE thread_sections(id TEXT PRIMARY KEY,name TEXT,updated_at INTEGER); CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,source TEXT NOT NULL,model_provider TEXT NOT NULL,cwd TEXT NOT NULL,title TEXT NOT NULL); CREATE TABLE thread_dynamic_tools(thread_id TEXT,position INTEGER,name TEXT,PRIMARY KEY(thread_id,position)); CREATE TABLE thread_spawn_edges(child_thread_id TEXT PRIMARY KEY,parent_thread_id TEXT,status TEXT);").unwrap();
        state_db.execute("INSERT INTO threads VALUES('thread-1',?1,1,2,'user','openai','C:/work','recovered')",[rollout.display().to_string()]).unwrap();
        state_db
            .execute(
                "INSERT INTO threads VALUES('ghost',?1,1,2,'user','openai','C:/work','ghost')",
                [source_root
                    .join("sessions/rollouts/missing.jsonl")
                    .display()
                    .to_string()],
            )
            .unwrap();
        drop(state_db);
        let source_index = source_root.join("index/session_index.jsonl");
        fs::create_dir_all(source_index.parent().unwrap()).unwrap();
        fs::write(&source_index,"{\"id\":\"thread-1\",\"thread_name\":\"recovered\"}\n{\"id\":\"ghost\",\"thread_name\":\"ghost\"}\n").unwrap();
        let source_history = source_root.join("thread-history/thread_history_1.sqlite");
        fs::create_dir_all(source_history.parent().unwrap()).unwrap();
        Connection::open(&source_history)
            .unwrap()
            .execute_batch("CREATE TABLE projection(value TEXT)")
            .unwrap();
        let sources = vec![
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: source_root.join("sessions"),
                host_version: Some("fixture-v1".into()),
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "state".into(),
                path: source_state,
                host_version: Some("fixture-v1".into()),
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "thread-history".into(),
                path: source_history,
                host_version: Some("fixture-v1".into()),
            },
            SourceConfig {
                app: "codex".into(),
                component: ComponentKind::Sessions,
                slot: "index".into(),
                path: source_index,
                host_version: Some("fixture-v1".into()),
            },
        ];
        let root = temp.path().join("backup");
        let captured = capture_sources(
            &NativeConfig {
                sources,
                ..Default::default()
            },
            &root,
            &AppArgs::default(),
            Some(&home),
        )
        .unwrap();
        let id = captured["snapshot_id"].as_str().unwrap();
        let manifest = verify_local(&root, id).unwrap();
        let target_state = home.join(".codex/state_5.sqlite");
        fs::create_dir_all(target_state.parent().unwrap()).unwrap();
        let target_db = Connection::open(&target_state).unwrap();
        target_db.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,metadata TEXT NOT NULL,position INTEGER NOT NULL,created_at_ms INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL); CREATE TABLE thread_sections(id TEXT PRIMARY KEY,name TEXT,updated_at INTEGER); CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,source TEXT NOT NULL,model_provider TEXT NOT NULL,cwd TEXT NOT NULL,title TEXT NOT NULL); CREATE TABLE thread_dynamic_tools(thread_id TEXT,position INTEGER,name TEXT,PRIMARY KEY(thread_id,position)); CREATE TABLE thread_spawn_edges(child_thread_id TEXT PRIMARY KEY,parent_thread_id TEXT,status TEXT);").unwrap();
        drop(target_db);
        let plan = build_install_plan(&manifest, "codex", Some(&home), &[], &root).unwrap();
        assert_eq!(
            apply_install_plan(&root, plan, true).unwrap()["file_verification"],
            "passed"
        );
        let target_db = Connection::open(&target_state).unwrap();
        assert_eq!(
            target_db
                .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let index = fs::read_to_string(home.join(".codex/session_index.jsonl")).unwrap();
        assert!(index.contains("thread-1"));
        assert!(!index.contains("ghost"));
        assert!(
            home.join(".codex/sessions/rollouts/thread-1.jsonl")
                .is_file()
        );
    }

    #[test]
    fn file_only_hosts_install_to_explicit_home_without_cross_host_paths() {
        for (app, slot, target) in [
            (
                "claude-code",
                "projects",
                PathBuf::from(".claude/projects/session.jsonl"),
            ),
            (
                "antigravity",
                "app",
                PathBuf::from(".gemini/antigravity/conversations/session.db"),
            ),
            (
                "grok",
                "sessions",
                PathBuf::from(".grok/sessions/session.jsonl"),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let source = home.join("source").join(target.file_name().unwrap());
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            if source
                .extension()
                .is_some_and(|extension| extension == "db")
            {
                Connection::open(&source)
                    .unwrap()
                    .execute_batch("CREATE TABLE session(value TEXT)")
                    .unwrap();
            } else {
                fs::write(&source, b"native session").unwrap();
            }
            let root = temp.path().join("backup");
            let config = NativeConfig {
                sources: vec![SourceConfig {
                    app: app.into(),
                    component: ComponentKind::Sessions,
                    slot: slot.into(),
                    path: source,
                    host_version: Some("fixture-v1".into()),
                }],
                ..Default::default()
            };
            let captured =
                capture_sources(&config, &root, &AppArgs::default(), Some(&home)).unwrap();
            let manifest = verify_local(&root, captured["snapshot_id"].as_str().unwrap()).unwrap();
            let plan = build_install_plan(&manifest, app, Some(&home), &[], &root).unwrap();
            let expected = home.join(&target);
            assert_eq!(plan.actions[0].target, expected);
            assert_eq!(plan.actions[0].kind, InstallActionKind::Copy);
            assert_eq!(
                apply_install_plan(&root, plan, true).unwrap()["file_verification"],
                "passed"
            );
            assert!(expected.is_file());
            assert!(expected.starts_with(&home));
        }
    }

    #[test]
    fn optional_components_exclude_credential_values_even_with_safe_filenames() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("settings");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("safe.json"), b"{\"theme\":\"dark\"}").unwrap();
        fs::write(
            source.join("looks-safe.json"),
            b"{\"theme\":\"dark\",\"nested\":{\"apiKey\":\"FAKE_SECRET\"}}",
        )
        .unwrap();
        fs::write(source.join("plugin.js"), b"const password = 'FAKE_SECRET';").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "fixture".into(),
                component: ComponentKind::Settings,
                slot: "settings".into(),
                path: source,
                host_version: Some("fixture-v1".into()),
            }],
            components: vec![ComponentKind::Settings],
            ..Default::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(captured["files"], 1);
        let exclusions = captured["exclusions"].to_string();
        assert!(exclusions.contains("credential-content"));
        let snapshot = root
            .join("snapshots")
            .join(captured["snapshot_id"].as_str().unwrap());
        assert!(snapshot.join("fixture/settings/safe.json").is_file());
        assert!(!snapshot.join("fixture/settings/looks-safe.json").exists());
        assert!(!snapshot.join("fixture/settings/plugin.js").exists());
    }

    #[test]
    fn cursor_capture_uses_projection_and_tracks_live_wal_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let db = Connection::open(source_root.join("state.vscdb")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value TEXT); INSERT INTO ItemTable VALUES ('composerData:abc','first'); INSERT INTO ItemTable VALUES ('cursor.accessToken','FAKE_SECRET');").unwrap();
        fs::write(source_root.join("storage.json"), "FAKE_SECRET").unwrap();
        let root = temp.path().join("backup");
        let config = NativeConfig {
            sources: vec![SourceConfig {
                app: "cursor".into(),
                component: ComponentKind::Sessions,
                slot: "global-storage".into(),
                path: source_root.clone(),
                host_version: None,
            }],
            ..Default::default()
        };
        let first = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(first["files"], 1);
        let before = hash_file(&source_root.join("state.vscdb")).unwrap();
        db.execute(
            "UPDATE ItemTable SET value='later' WHERE key='composerData:abc'",
            [],
        )
        .unwrap();
        assert_eq!(hash_file(&source_root.join("state.vscdb")).unwrap(), before);
        let second = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(second["status"], "captured");
        let manifest = verify_local(&root, second["snapshot_id"].as_str().unwrap()).unwrap();
        assert_eq!(manifest.files[0].consistency, "sqlite-session-projection");
        let output = root
            .join("snapshots")
            .join(&manifest.snapshot_id)
            .join(&manifest.files[0].relative);
        assert!(!String::from_utf8_lossy(&fs::read(&output).unwrap()).contains("FAKE_SECRET"));
        let recovered = Connection::open(output).unwrap();
        assert_eq!(
            recovered
                .query_row(
                    "SELECT value FROM ItemTable WHERE key='composerData:abc'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "later"
        );
    }

    #[test]
    fn excludes_credentials_and_skips_unchanged_capture() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("sessions/auth")).unwrap();
        fs::write(
            temp.path().join("sessions/chat.jsonl"),
            b"{\"unknown\":1}\n",
        )
        .unwrap();
        fs::write(temp.path().join("sessions/auth/token.json"), b"secret").unwrap();
        let root_temp = tempfile::tempdir().unwrap();
        let root = root_temp.path().to_path_buf();
        let config = NativeConfig {
            data_root: Some(root.clone()),
            sources: vec![SourceConfig {
                app: "fixture".into(),
                component: ComponentKind::Sessions,
                slot: "sessions".into(),
                path: temp.path().join("sessions"),
                host_version: Some("fixture".into()),
            }],
            ..NativeConfig::default()
        };
        let first = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(first["files"], 1);
        assert!(first["exclusions"].to_string().contains("credential"));
        let second = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(second["status"], "unchanged");
        fs::write(temp.path().join("sessions/chat.jsonl"), b"changed").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let third = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        assert_eq!(third["status"], "captured");
        let snapshot = root
            .join("snapshots")
            .join(third["snapshot_id"].as_str().unwrap());
        assert!(!snapshot.join("fixture/sessions/auth").exists());
        fs::write(snapshot.join("fixture/sessions/chat.jsonl"), b"corrupt").unwrap();
        assert!(verify_local(&root, third["snapshot_id"].as_str().unwrap()).is_err());
    }

    #[test]
    fn sqlite_backup_survives_live_wal_and_restic_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let root_temp = tempfile::tempdir().unwrap();
        let root = root_temp.path().to_path_buf();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let db_path = source_root.join("conversation.db");
        let connection = Connection::open(&db_path).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE steps(payload BLOB); INSERT INTO steps VALUES (X'0102FF');").unwrap();
        let source = SourceConfig {
            app: "fixture".into(),
            component: ComponentKind::Sessions,
            slot: "sessions".into(),
            path: source_root,
            host_version: Some("fixture".into()),
        };
        let password = temp.path().join("password");
        fs::write(&password, "synthetic-test-password").unwrap();
        let local_repo = temp.path().join("local-repo");
        let remote_repo = temp.path().join("remote-repo");
        let restic = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tmp/native-tools/restic_0.19.1_windows_amd64.exe");
        if !restic.is_file() {
            return;
        }
        for repo in [&local_repo, &remote_repo] {
            let _ = Command::new(&restic)
                .args(["-r"])
                .arg(repo)
                .arg("init")
                .env("RESTIC_PASSWORD_FILE", &password)
                .status()
                .unwrap();
        }
        let config = NativeConfig {
            data_root: Some(root.clone()),
            restic: Some(restic),
            password_file: Some(password),
            local_repository: Some(local_repo.display().to_string()),
            remote_repository: Some(remote_repo.display().to_string()),
            sources: vec![source],
            ..NativeConfig::default()
        };
        let captured = capture_sources(&config, &root, &AppArgs::default(), None).unwrap();
        let snapshot = captured["snapshot_id"].as_str().unwrap();
        let manifest = load_manifest(&root.join("snapshots").join(snapshot)).unwrap();
        assert!(
            manifest.files.iter().all(|file| root
                .join("snapshots")
                .join(snapshot)
                .join(&file.relative)
                .exists()),
            "manifest paths={:?}",
            manifest.files
        );
        connection
            .execute("INSERT INTO steps VALUES (X'0304')", [])
            .unwrap();
        let replicated = replicate(&config, &root, Some(snapshot)).unwrap();
        assert!(replicated["local_restic_snapshot"].is_string());
        assert!(replicated["remote_restic_snapshot"].is_string());
        assert_ne!(
            replicated["local_restic_snapshot"],
            replicated["remote_restic_snapshot"]
        );
        let extracted = temp.path().join("extracted");
        extract_snapshot(&root, snapshot, &extracted).unwrap();
        let restored_path = extracted.join("fixture/sessions/conversation.db");
        assert!(
            restored_path.exists(),
            "missing {}\nlayout={:?}",
            restored_path.display(),
            WalkDir::new(&extracted)
                .into_iter()
                .filter_map(Result::ok)
                .map(|entry| entry.path().display().to_string())
                .collect::<Vec<_>>()
        );
        let restored = Connection::open(restored_path).unwrap();
        assert_eq!(
            restored
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            restored
                .query_row("SELECT count(*) FROM steps", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
